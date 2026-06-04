//! Shell session management.
//!
//! This module provides the main `ShellSession` type that orchestrates
//! PTY lifecycle, terminal I/O, and status line rendering.

use crate::escape::EscapeScanner;
use crate::escape_state::{
    EscapeState, cleanup_forwarded_modes as escape_state_cleanup,
    process_escape_events as escape_state_process,
};
use crate::protocol::{ShellCommand, ShellEvent};
use crate::pty::{Pty, PtyError, get_terminal_size};
use crate::status_line::{SPINNER_INTERVAL_MS, StatusLine};
use crate::terminal::RawModeGuard;
use crate::terminal_commands::{
    InBandResizeNotification, ORIGIN_MODE, ResetDecMode, ResetScrollRegion, SetScrollRegion,
};
use crate::utf8_accumulator::Utf8Accumulator;
use crate::vt_utils::{
    CursorState, DEFAULT_MAX_SCROLLBACK, active_point, cells_in_row, point_with_x, push_cell_text,
    screen_point,
};
use crossterm::{
    Command, cursor, queue,
    style::ResetColor,
    terminal::{self, Clear, ClearType},
};
use libghostty_vt::render::RenderState;
use libghostty_vt::screen::{Cell, CellContentTag, CellWide, GridRef};
use libghostty_vt::style::{Style, StyleColor, Underline};
use libghostty_vt::terminal::{Options as TerminalOptions, Point, Terminal};
use portable_pty::PtySize;
use std::fmt::Write as FmtWrite;
use std::io::{self, IsTerminal, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Keybind byte sequences (ESC + Ctrl key).
const KEYBIND_TOGGLE_PAUSE: [u8; 2] = [0x1b, 0x04]; // Ctrl-Alt-D
const KEYBIND_LIST_WATCHED: [u8; 2] = [0x1b, 0x17]; // Ctrl-Alt-W
const KEYBIND_TOGGLE_ERROR: [u8; 2] = [0x1b, 0x05]; // Ctrl-Alt-E

fn dump_row_from_cells(buf: &mut String, vt: &Terminal<'_, '_>, point: Point, cells: &[Cell]) {
    // TODO(libghostty-rs): Style::is_default() returns true for RGB-bg-only styles
    // because StyleColor::Rgb is mistagged as NONE in the FFI From conversion.
    // Compare via PartialEq against Style::default() until upstream is fixed.
    let default_style = Style::default();
    let mut cur_style = default_style;
    let mut blank_cells: usize = 0;
    for (x, cell) in cells.iter().enumerate() {
        if matches!(
            cell.wide().ok(),
            Some(CellWide::SpacerTail | CellWide::SpacerHead)
        ) {
            continue;
        }
        let Ok(cell_ref) = vt.grid_ref(point_with_x(point, x as u16)) else {
            continue;
        };
        let has_text = cell.has_text().unwrap_or(false);
        let has_styling = cell.has_styling().unwrap_or(false);
        let tag = cell.content_tag().ok();
        let is_bg_only = matches!(
            tag,
            Some(CellContentTag::BgColorPalette | CellContentTag::BgColorRgb)
        );
        if !has_text && !has_styling && !is_bg_only {
            blank_cells += 1;
            continue;
        }
        if blank_cells > 0 {
            for _ in 0..blank_cells {
                buf.push(' ');
            }
            blank_cells = 0;
        }
        let new_style = cell_style(cell, &cell_ref, tag, has_styling, default_style);
        if new_style != cur_style {
            if new_style == default_style {
                buf.push_str("\x1b[0m");
            } else {
                dump_style(buf, &new_style);
            }
            cur_style = new_style;
        }
        push_cell_text(buf, cell, &cell_ref);
    }
    if cur_style != default_style {
        buf.push_str("\x1b[0m");
    }
}

/// Whether every cell in a row is blank — no text, no styling, no background
/// fill — i.e. `dump_row_from_cells` would emit nothing for it. Mirrors the
/// per-cell blank test there (lines `!has_text && !has_styling && !is_bg_only`).
/// Unknown queries count as non-blank so we redraw to stay correct. Used by the
/// post-clear fast path: after a single native CSI 2J blanks the real screen,
/// rows that read back blank need no per-row redraw.
fn is_blank_cells(cells: &[Cell]) -> bool {
    cells.iter().all(|cell| {
        let has_text = cell.has_text().unwrap_or(true);
        let has_styling = cell.has_styling().unwrap_or(true);
        let is_bg_only = matches!(
            cell.content_tag().ok(),
            Some(CellContentTag::BgColorPalette | CellContentTag::BgColorRgb)
        );
        !has_text && !has_styling && !is_bg_only
    })
}

fn cell_style(
    cell: &Cell,
    cell_ref: &GridRef<'_>,
    tag: Option<CellContentTag>,
    has_styling: bool,
    default: Style,
) -> Style {
    match tag {
        Some(CellContentTag::Codepoint | CellContentTag::CodepointGrapheme) => {
            if has_styling {
                cell_ref.style().unwrap_or(default)
            } else {
                default
            }
        }
        Some(CellContentTag::BgColorPalette) => {
            let mut s = default;
            if let Ok(idx) = cell.bg_color_palette() {
                s.bg_color = StyleColor::Palette(idx);
            }
            s
        }
        Some(CellContentTag::BgColorRgb) => {
            let mut s = default;
            if let Ok(rgb) = cell.bg_color_rgb() {
                s.bg_color = StyleColor::Rgb(rgb);
            }
            s
        }
        None => default,
    }
}

/// Render a VT row as a string with SGR escape sequences (fetches cells internally).
fn dump_row(buf: &mut String, vt: &Terminal<'_, '_>, point: Point) {
    let cells = cells_in_row(vt, point);
    dump_row_from_cells(buf, vt, point, &cells);
}

fn dump_style(s: &mut String, style: &Style) {
    s.push_str("\x1b[0");
    if style.fg_color != StyleColor::None {
        s.push(';');
        dump_color(s, &style.fg_color, 30);
    }
    if style.bg_color != StyleColor::None {
        s.push(';');
        dump_color(s, &style.bg_color, 40);
    }
    if style.bold {
        s.push_str(";1");
    }
    if style.faint {
        s.push_str(";2");
    }
    if style.italic {
        s.push_str(";3");
    }
    match style.underline {
        Underline::None => {}
        Underline::Single => s.push_str(";4"),
        Underline::Double => s.push_str(";4:2"),
        Underline::Curly => s.push_str(";4:3"),
        Underline::Dotted => s.push_str(";4:4"),
        Underline::Dashed => s.push_str(";4:5"),
        _ => {}
    }
    if style.blink {
        s.push_str(";5");
    }
    if style.inverse {
        s.push_str(";7");
    }
    if style.strikethrough {
        s.push_str(";9");
    }
    s.push('m');
}

fn dump_color(s: &mut String, color: &StyleColor, base: u8) {
    match color {
        StyleColor::Palette(p) if p.0 < 8 => {
            let _ = write!(s, "{}", base + p.0);
        }
        StyleColor::Palette(p) if p.0 < 16 => {
            let _ = write!(s, "{}", base + 52 + p.0);
        }
        StyleColor::Palette(p) => {
            let _ = write!(s, "{};5;{}", base + 8, p.0);
        }
        StyleColor::Rgb(rgb) => {
            let _ = write!(s, "{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b);
        }
        StyleColor::None => {}
    }
}

/// Feed text into VT and return the scroll count (lines that scrolled off the viewport).
///
/// Scroll count is the delta of `total_rows()`, which is exact as long as VT
/// scrollback hasn't hit its `max_scrollback` cap. We keep VT scrollback well
/// below the cap by wiping it via CSI 3J after every successful flush in
/// `render_with_scroll`, so the delta stays accurate in practice.
fn feed_vt(vt: &mut Terminal<'_, '_>, text: &str) -> usize {
    let total_before = vt.total_rows().unwrap_or(0);
    vt.vt_write(text.as_bytes());
    let total_after = vt.total_rows().unwrap_or(0);
    total_after.saturating_sub(total_before)
}

/// Differential renderer that draws VT state to a bounded terminal region.
///
/// Instead of passing raw PTY output to stdout (which conflicts with the status
/// line's scroll region), this renderer mediates all terminal output through
/// the VT state machine — similar to how tmux works.
struct Renderer {
    /// Previous frame for diffing — one line of cells per row.
    prev_lines: Vec<Vec<Cell>>,
    /// Previous cursor state.
    prev_cursor: CursorState,
    /// Row offset for the initial phase after TUI handoff.
    /// When > 0, VT row N maps to real terminal row (N + 1 + row_offset)
    /// instead of (N + 1). Gradually consumed as VT content scrolls,
    /// or reset to 0 immediately on terminal resize or alternate screen.
    row_offset: u16,
    /// Number of usable content rows on the real terminal (excludes status line).
    /// Used to clip rendering so offset VT rows don't overwrite the status line.
    content_rows: u16,
    /// Number of VT scrollback lines already pushed to native terminal scrollback.
    /// Used to flush only new (unflushed) scrollback lines in `render_with_scroll`.
    /// Reset to 0 after each flush.
    scrollback_flushed: usize,
    /// Reusable buffer for SGR line rendering (avoids per-line allocation).
    line_buf: String,
    /// libghostty render state, reused across frames. `update` consumes the
    /// terminal's per-row dirty bits so the next frame only reports rows that
    /// changed, letting render skip the cell readback for unchanged rows.
    render_state: RenderState<'static>,
    /// Force the next render to redraw every row regardless of dirty tracking
    /// (set on invalidate: alt-screen toggle, resize, scroll, handoff).
    force_full: bool,
    /// Set by `native_clear` when a one-shot CSI 2J was emitted this batch.
    /// Makes the next `render` suppress the per-row redraw of rows that read
    /// back blank (the native clear already blanked them), so a full-screen
    /// erase costs one escape instead of a MoveTo+Clear+reset per row.
    cleared: bool,
}

impl Renderer {
    fn new(content_rows: u16) -> Result<Self, libghostty_vt::Error> {
        Ok(Self {
            prev_lines: Vec::new(),
            prev_cursor: CursorState {
                col: 0,
                row: 0,
                visible: true,
            },
            row_offset: 0,
            content_rows,
            scrollback_flushed: 0,
            line_buf: String::new(),
            render_state: RenderState::new()?,
            force_full: false,
            cleared: false,
        })
    }

    /// Discard any VT scrollback without emitting it to the native terminal
    /// (e.g., after resize where the post-resize state is the new baseline).
    fn discard_vt_scrollback(&mut self, vt: &mut Terminal<'_, '_>) {
        vt.vt_write(b"\x1b[3J");
        self.scrollback_flushed = 0;
    }

    /// Number of VT rows that fit on-screen given the current offset.
    fn visible_rows(&self) -> usize {
        (self.content_rows as usize).saturating_sub(self.row_offset as usize)
    }

    /// Scroll the real terminal by `count` lines within a temporary DECSTBM
    /// scroll region, pushing content into native scrollback while protecting
    /// the status line row.
    fn scroll_region(stdout: &mut impl Write, content_rows: u16, count: usize) -> io::Result<()> {
        if count == 0 || content_rows == 0 {
            return Ok(());
        }
        queue!(
            stdout,
            SetScrollRegion {
                top: 1,
                bottom: content_rows
            },
            cursor::MoveTo(0, content_rows - 1)
        )?;
        for _ in 0..count {
            stdout.write_all(b"\n")?;
        }
        queue!(stdout, ResetScrollRegion)
    }

    /// Write a single VT row's content (SGR-formatted text + reset) to stdout.
    fn write_row_content(
        &mut self,
        stdout: &mut impl Write,
        vt: &Terminal<'_, '_>,
        point: Point,
    ) -> io::Result<()> {
        self.line_buf.clear();
        dump_row(&mut self.line_buf, vt, point);
        stdout.write_all(self.line_buf.as_bytes())?;
        queue!(stdout, ResetColor)
    }

    /// Write a row using pre-fetched cells (avoids re-iterating cells via FFI).
    fn write_row_from_cells(
        &mut self,
        stdout: &mut impl Write,
        vt: &Terminal<'_, '_>,
        point: Point,
        cells: &[Cell],
    ) -> io::Result<()> {
        self.line_buf.clear();
        dump_row_from_cells(&mut self.line_buf, vt, point, cells);
        stdout.write_all(self.line_buf.as_bytes())?;
        queue!(stdout, ResetColor)
    }

    /// Read each viewport row's dirty flag from the VT grid (one cheap FFI
    /// probe per row, vs reading every cell), then consume the terminal's dirty
    /// state via `RenderState::update` so the next frame sees only newly-changed
    /// rows. Returns, per row in `0..limit`, whether it changed since last call.
    fn collect_dirty(&mut self, vt: &Terminal<'static, 'static>, limit: usize) -> Vec<bool> {
        // Legacy mode: every row dirty, no render-state update — reproduces the
        // pre-optimization full-screen readback for before/after benchmarking.
        if crate::perf::legacy() {
            return vec![true; limit];
        }
        let dirty: Vec<bool> = (0..limit)
            .map(|row_idx| {
                vt.grid_ref(active_point(row_idx as u32))
                    .ok()
                    .and_then(|g| g.row().ok())
                    .and_then(|r| r.is_dirty().ok())
                    // Unknown dirtiness → redraw to stay correct.
                    .unwrap_or(true)
            })
            .collect();
        // Consume the grid dirty bits so the next frame only reports new changes.
        let _ = self.render_state.update(vt);
        dirty
    }

    /// Render changed VT lines to stdout. Uses libghostty per-row dirty tracking
    /// to read back cells only for rows that changed (the rest are skipped
    /// without a cell read), clips rows outside the visible area, and keeps a
    /// `prev_lines` content compare as a safety net for the emit decision.
    fn render(
        &mut self,
        stdout: &mut impl Write,
        vt: &Terminal<'static, 'static>,
    ) -> io::Result<()> {
        let offset = self.row_offset as usize;
        let max_row = self.visible_rows();
        let rows = vt.rows().unwrap_or(0) as usize;
        let limit = rows.min(max_row);

        let force = std::mem::take(&mut self.force_full);
        let cleared = std::mem::take(&mut self.cleared);
        let dirty = self.collect_dirty(vt, limit);

        for row_idx in 0..limit {
            if !force && !dirty.get(row_idx).copied().unwrap_or(true) {
                // libghostty says this row is unchanged — skip the cell readback.
                crate::perf::record_row(false);
                continue;
            }
            let point = active_point(row_idx as u32);
            let cells = cells_in_row(vt, point);
            // Post native-clear: the real screen was just blanked by one CSI 2J,
            // so any row that reads back blank needs no per-row redraw. Record
            // the real (blank) cells as the new baseline so the next frame still
            // diffs correctly.
            if cleared && is_blank_cells(&cells) {
                crate::perf::record_row(false);
                if row_idx >= self.prev_lines.len() {
                    self.prev_lines.resize_with(row_idx + 1, Vec::new);
                }
                self.prev_lines[row_idx] = cells;
                continue;
            }
            if row_idx < self.prev_lines.len() && cells == self.prev_lines[row_idx] {
                crate::perf::record_row(false);
                continue;
            }
            crate::perf::record_row(true);
            queue!(
                stdout,
                cursor::MoveTo(0, (row_idx + offset) as u16),
                Clear(ClearType::CurrentLine)
            )?;
            self.write_row_from_cells(stdout, vt, point, &cells)?;
            if row_idx >= self.prev_lines.len() {
                self.prev_lines.resize_with(row_idx + 1, Vec::new);
            }
            self.prev_lines[row_idx] = cells;
        }
        self.update_cursor(stdout, vt)
    }

    /// Push unflushed VT scrollback lines into native terminal scrollback,
    /// then render the viewport.
    ///
    /// Instead of blindly scrolling the previous screen content (which loses
    /// the actual scrolled-off text), this draws VT scrollback lines onto the
    /// real terminal and then scrolls them off via newlines inside a DECSTBM
    /// region that protects the status line.
    fn render_with_scroll(
        &mut self,
        stdout: &mut impl Write,
        vt: &mut Terminal<'static, 'static>,
    ) -> io::Result<()> {
        let vt_scrollback = vt.scrollback_rows().unwrap_or(0);
        let unflushed = vt_scrollback.saturating_sub(self.scrollback_flushed);

        if unflushed > 0 && self.content_rows > 0 {
            let batch_size = self.content_rows as usize;

            // Set scroll region to protect the status line row.
            queue!(
                stdout,
                SetScrollRegion {
                    top: 1,
                    bottom: self.content_rows
                }
            )?;

            // Iterate scrollback lines starting from the first unflushed one.
            // Uses Screen coordinates where y=0 is the oldest scrollback line.
            let mut screen_y = self.scrollback_flushed;
            let mut remaining = unflushed;

            while remaining > 0 {
                let count = remaining.min(batch_size);
                let mut drawn = 0;

                // Pre-clear destination rows so soft-wrapped continuation rows
                // (which skip MoveTo+Clear below) land on clean rows.
                for i in 0..count {
                    queue!(
                        stdout,
                        cursor::MoveTo(0, i as u16),
                        Clear(ClearType::CurrentLine)
                    )?;
                }

                // After a soft-wrapped row, skip MoveTo so the outer terminal's
                // pending-wrap carries over instead of becoming a hard newline.
                let mut prev_was_wrap_source = false;

                for i in 0..count {
                    if screen_y >= vt_scrollback {
                        break;
                    }
                    let row_point = screen_point(screen_y as u32);
                    let row = vt.grid_ref(row_point).and_then(|gr| gr.row()).ok();
                    let is_continuation = row
                        .and_then(|r| r.is_wrap_continuation().ok())
                        .unwrap_or(false);
                    let is_wrap_source = row.and_then(|r| r.is_wrapped().ok()).unwrap_or(false);

                    if !(is_continuation && prev_was_wrap_source) {
                        queue!(stdout, cursor::MoveTo(0, i as u16))?;
                    }
                    self.write_row_content(stdout, vt, row_point)?;
                    screen_y += 1;
                    drawn += 1;
                    prev_was_wrap_source = is_wrap_source;
                }

                if drawn > 0 {
                    // Scroll drawn content into native scrollback.
                    queue!(stdout, cursor::MoveTo(0, self.content_rows - 1))?;
                    for _ in 0..drawn {
                        stdout.write_all(b"\n")?;
                    }
                }

                remaining -= count;
                if drawn < count {
                    break;
                }
            }

            queue!(stdout, ResetScrollRegion)?;

            // Wipe VT scrollback so it never approaches `max_scrollback` and
            // triggers internal GC. The user-visible scrollback lives in the
            // native terminal we just flushed to; VT scrollback is only an
            // internal staging area for not-yet-flushed lines. CSI 3J
            // (`EraseDisplay::scrollback`) preserves viewport, cursor, styles,
            // and modes — it only frees the history pages. See
            // `ghostty/src/terminal/Terminal.zig::eraseDisplay` and
            // `Screen.zig::eraseHistory`.
            vt.vt_write(b"\x1b[3J");
            self.scrollback_flushed = 0;
            // The viewport scrolled on the real terminal; force a full redraw
            // (clears prev_lines and overrides dirty tracking for this frame).
            self.invalidate();
        }
        self.render(stdout, vt)
    }

    /// Full redraw of all VT lines (after resize or initialization).
    fn render_full(
        &mut self,
        stdout: &mut impl Write,
        vt: &Terminal<'static, 'static>,
    ) -> io::Result<()> {
        self.invalidate();
        self.render(stdout, vt)
    }

    /// Mirror a full-screen erase (CSI 2J) with a single native clear instead
    /// of repainting every row. Emits one `Clear(All)` + home, resets the diff
    /// baseline to "screen is blank", and sets `cleared` so the following
    /// `render` redraws only the rows that still have content (e.g. the prompt)
    /// and skips the now-blank ones. Deliberately does *not* call `invalidate`:
    /// `force_full` would force-read every row and defeat the dirty skip on the
    /// frames that follow. Cursor visibility is preserved (only `update_cursor`
    /// emits Show/Hide, by diffing against `prev_cursor`).
    fn native_clear(&mut self, stdout: &mut impl Write) -> io::Result<()> {
        queue!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
        self.prev_lines.clear();
        self.prev_cursor = CursorState {
            col: 0,
            row: 0,
            visible: self.prev_cursor.visible,
        };
        self.cleared = true;
        Ok(())
    }

    /// Position the real terminal cursor to match the VT cursor.
    fn update_cursor(&mut self, stdout: &mut impl Write, vt: &Terminal<'_, '_>) -> io::Result<()> {
        let offset = self.row_offset as usize;
        let cur = CursorState::from_terminal(vt);
        if cur != self.prev_cursor {
            if cur.visible && !self.prev_cursor.visible {
                queue!(stdout, cursor::Show)?;
            } else if !cur.visible && self.prev_cursor.visible {
                queue!(stdout, cursor::Hide)?;
            }
            queue!(
                stdout,
                cursor::MoveTo(cur.col, (cur.row as usize + offset) as u16)
            )?;
            self.prev_cursor = cur;
        }
        Ok(())
    }

    /// Write the VT cursor position to stdout (unconditional, no diffing).
    ///
    /// Used to restore the real terminal cursor after status line draws
    /// or other operations that move it away from the VT position.
    fn write_cursor(&self, stdout: &mut impl Write, vt: &Terminal<'_, '_>) -> io::Result<()> {
        let cur = CursorState::from_terminal(vt);
        let offset = self.row_offset as usize;
        queue!(
            stdout,
            cursor::MoveTo(cur.col, (cur.row as usize + offset) as u16)
        )
    }

    /// Mark all lines as stale so the next render redraws everything,
    /// overriding dirty tracking for that frame.
    fn invalidate(&mut self) {
        self.prev_lines.clear();
        self.force_full = true;
    }

    /// Snapshot VT state into prev_lines without writing anything to stdout.
    /// Used after TUI handoff to establish a baseline for diff rendering
    /// while preserving existing terminal content.
    fn sync(&mut self, vt: &Terminal<'_, '_>) {
        self.prev_lines.clear();
        let rows = vt.rows().unwrap_or(0);
        for y in 0..rows {
            let cells = cells_in_row(vt, active_point(y as u32));
            self.prev_lines.push(cells);
        }
        self.prev_cursor = CursorState::from_terminal(vt);
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("PTY error: {0}")]
    Pty(#[from] PtyError),
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("terminal error: {0}")]
    Terminal(#[from] libghostty_vt::Error),
    #[error("channel closed")]
    ChannelClosed,
    #[error("unexpected command: expected Spawn, got {0}")]
    UnexpectedCommand(String),
}

/// Configuration for TUI handoff.
///
/// When running with TUI, the shell session needs to coordinate
/// terminal ownership with the TUI.
pub struct TuiHandoff {
    /// Signal the renderer to stop. Sending — or dropping — the sender is the
    /// signal (a closed channel is a delivered "stop", which makes the
    /// panic/early-return path safe without a guard).
    pub backend_done: oneshot::Sender<()>,
    /// Wait for the renderer to release the terminal. The TUI's final render
    /// height is carried but unused here.
    pub terminal_ready_rx: oneshot::Receiver<u16>,
}

/// Shell session configuration.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Show status line at bottom of terminal.
    pub show_status_line: bool,
    /// Initial terminal size (auto-detected if None).
    pub size: Option<PtySize>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            show_status_line: true,
            size: None,
        }
    }
}

/// Injectable I/O for the shell session.
/// When fields are None, real stdin/stdout are used.
#[derive(Default)]
pub struct SessionIo {
    pub stdin: Option<Box<dyn std::io::Read + Send>>,
    pub stdout: Option<Box<dyn std::io::Write + Send>>,
}

/// Internal events for the shell session event loop.
enum Event {
    Stdin(Vec<u8>),
    PtyOutput(Vec<u8>),
    PtyExit(Option<u32>),
    Command(ShellCommand),
    Resize,
}

/// Interactive shell session with hot-reload support.
///
/// Manages PTY lifecycle, terminal I/O, and status line rendering.
pub struct ShellSession {
    config: SessionConfig,
    size: PtySize,
    status_line: StatusLine,
    shutdown_token: Option<CancellationToken>,
}

impl ShellSession {
    /// Create a new shell session with the given configuration.
    pub fn new(config: SessionConfig) -> Self {
        let size = config.size.unwrap_or_else(get_terminal_size);
        let mut status_line = StatusLine::new();
        status_line.set_enabled(config.show_status_line);

        Self {
            config,
            size,
            status_line,
            shutdown_token: None,
        }
    }

    /// Get the PTY size, reserving 1 row for status line if enabled.
    fn pty_size(&self) -> PtySize {
        if self.config.show_status_line {
            PtySize {
                rows: self.size.rows.saturating_sub(1).max(1),
                cols: self.size.cols,
                ..self.size
            }
        } else {
            self.size
        }
    }

    /// Create a new shell session with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(SessionConfig::default())
    }

    /// Set whether to show the status line.
    pub fn with_status_line(mut self, show: bool) -> Self {
        self.config.show_status_line = show;
        self.status_line.set_enabled(show);
        self
    }

    /// Wire a shutdown token. On cancellation the session kills the inner
    /// shell so devenv can exit instead of orphaning it after a terminal
    /// hangup or SIGHUP/SIGINT/SIGTERM.
    pub fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// Run the shell session.
    ///
    /// This function takes over the terminal and runs until the shell exits
    /// or the coordinator sends a shutdown command.
    ///
    /// # Arguments
    /// * `command_rx` - Receives commands from coordinator
    /// * `event_tx` - Sends events to coordinator
    /// * `handoff` - Optional TUI handoff configuration
    pub async fn run(
        mut self,
        mut command_rx: tokio_mpsc::Receiver<ShellCommand>,
        event_tx: tokio_mpsc::Sender<ShellEvent>,
        handoff: Option<TuiHandoff>,
        io: SessionIo,
    ) -> Result<Option<u32>, SessionError> {
        // Wait for the initial Spawn command
        let (initial_cmd, _watch_files) = match command_rx.recv().await {
            Some(ShellCommand::Spawn {
                command,
                watch_files,
            }) => {
                self.status_line
                    .state_mut()
                    .set_watched_file_count(watch_files.len());
                (command, watch_files)
            }
            Some(ShellCommand::Shutdown) | None => {
                if let Some(h) = handoff {
                    let _ = h.backend_done.send(());
                }
                return Ok(None);
            }
            Some(other) => {
                if let Some(h) = handoff {
                    let _ = h.backend_done.send(());
                }
                return Err(SessionError::UnexpectedCommand(format!("{:?}", other)));
            }
        };

        // Spawn PTY
        // Reserve 1 row for status line if enabled
        let pty_size = self.pty_size();

        let pty = Arc::new(Pty::spawn(initial_cmd, pty_size)?);

        // TUI handoff. Wait for the renderer to release the terminal, but
        // yield to shutdown so a SIGHUP during this await can't hang us.
        if let Some(handoff) = handoff {
            tracing::trace!("session: sending backend_done");
            let _ = handoff.backend_done.send(());

            tracing::trace!("session: waiting for terminal_ready_rx");
            let cancelled = async {
                match &self.shutdown_token {
                    Some(t) => t.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = handoff.terminal_ready_rx => {
                    tracing::trace!("session: terminal_ready_rx received");
                }
                _ = cancelled => {
                    tracing::debug!("session: shutdown during handoff, aborting");
                    let _ = pty.kill();
                    return Ok(None);
                }
            }
        }

        // Enter raw mode
        tracing::trace!("session: entering raw mode");
        let _raw_guard = RawModeGuard::new()?;
        tracing::trace!("session: raw mode active");

        crate::perf::init_from_env();

        let injected_stdin = io.stdin.is_some();
        let stdout_raw: Box<dyn Write + Send> = io.stdout.unwrap_or_else(|| Box::new(io::stdout()));
        // CountingWriter sits between the BufWriter and the real terminal fd so
        // the perf profiler sees the bytes/syscalls actually reaching the
        // terminal (including mid-frame BufWriter auto-flushes).
        //
        // 256 KiB capacity so a full-screen repaint with dense SGR doesn't
        // overflow the default 8 KiB buffer and auto-flush mid-frame (extra
        // blocking write() syscalls that also split the synchronized update).
        // Legacy mode uses the old small buffer for a faithful before/after.
        let stdout_cap = if crate::perf::legacy() {
            8 * 1024
        } else {
            256 * 1024
        };
        let mut stdout: Box<dyn Write + Send> = Box::new(io::BufWriter::with_capacity(
            stdout_cap,
            crate::perf::CountingWriter::new(stdout_raw),
        ));
        let stdin_source: Box<dyn Read + Send> = io.stdin.unwrap_or_else(|| Box::new(io::stdin()));

        // Query cursor position FIRST before any terminal resets.
        // This tells us where TUI left the cursor after its final render.
        // Skip when stdin is injected (not a real terminal) — the response comes
        // via stdin, so this would hang if stdin is not a TTY.
        // crossterm::cursor::position() handles the DSR query, parsing, and has a
        // built-in 2s timeout for environments that don't respond (Docker, CI).
        let cursor_row = if !injected_stdin && io::stdin().is_terminal() {
            match crossterm::cursor::position() {
                Ok((_col, row)) => row + 1, // crossterm returns 0-based, we need 1-based
                Err(e) => {
                    tracing::debug!("session: cursor position query failed: {e}, assuming row 1");
                    1
                }
            }
        } else {
            1
        };
        tracing::trace!("session: cursor position after TUI: row {}", cursor_row);

        // TUI renderers may leave a non-default scroll region/origin mode.
        // Reset both before we start cursor-addressed rendering, otherwise
        // the first shell draw can land in the wrong area and overlap TUI output.
        queue!(stdout, ResetScrollRegion, ResetDecMode(ORIGIN_MODE))?;
        stdout.flush()?;

        // Get terminal size.
        // TODO: query the size from the actual stdout fd (e.g. TIOCGWINSZ on the
        // writer) instead of crossterm::terminal::size() which always uses the
        // process's controlling terminal. That would make this work correctly even
        // with injected I/O and remove the need for the config.size guard.
        if self.config.size.is_none()
            && let Ok((cols, rows)) = terminal::size()
        {
            self.size = PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            };
        }
        tracing::trace!(
            "session: terminal size: {}x{}",
            self.size.cols,
            self.size.rows
        );
        // Both PTY and VT stay at full terminal size so that:
        // - Programs see the real dimensions (no unnecessary pager invocations)
        // - Alternate screen save/restore works correctly (same buffer size)
        // The renderer clips output to the visible area below cursor_row
        // and gradually consumes offset as the cursor moves down.
        let row_offset = cursor_row.saturating_sub(1);
        let pty_size = self.pty_size();
        let _ = pty.resize(pty_size);

        // Set up event channel
        let (event_tx_internal, event_rx_internal) = std::sync::mpsc::channel::<Event>();

        // On shutdown, kill the inner shell *and* inject a synthetic `PtyExit`:
        // if the child has already exited, `kill` returns ESRCH and on macOS
        // the PTY reader can stay blocked, so the event loop never sees the
        // real `PtyExit`. Signalled exit code is recovered upstream from
        // `Shutdown::last_signal()`.
        if let Some(token) = self.shutdown_token.clone() {
            let pty_killer = Arc::clone(&pty);
            let exit_tx = event_tx_internal.clone();
            tokio::spawn(async move {
                token.cancelled().await;
                tracing::debug!("session: shutdown requested, tearing down inner shell");
                if let Err(e) = pty_killer.kill() {
                    tracing::debug!("session: inner shell kill returned {e}");
                }
                let _ = exit_tx.send(Event::PtyExit(None));
            });
        }

        // Spawn stdin reader thread.
        let stdin_tx = event_tx_internal.clone();
        std::thread::Builder::new()
            .name("session-stdin".into())
            .spawn(move || {
                let mut stdin = stdin_source;
                let mut buf = [0u8; 1024];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if stdin_tx.send(Event::Stdin(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("session: stdin read error: {}", e);
                            break;
                        }
                    }
                }
            })
            .expect("failed to spawn session-stdin thread");

        // Spawn PTY reader thread
        let pty_tx = event_tx_internal.clone();
        let pty_reader = Arc::clone(&pty);
        std::thread::Builder::new()
            .name("session-pty".into())
            .spawn(move || {
                // 64 KiB so a single full-screen TUI frame (often >4 KiB) is
                // delivered in one read instead of fragmenting into many
                // syscalls, allocations, channel sends, and scan/accumulate
                // passes. Heap-allocated to keep the thread stack small.
                // Legacy mode uses the old 4 KiB buffer for a faithful A/B.
                let buf_len = if crate::perf::legacy() {
                    4096
                } else {
                    64 * 1024
                };
                let mut buf = vec![0u8; buf_len];
                loop {
                    match pty_reader.read(&mut buf) {
                        Ok(0) => {
                            let exit_code =
                                pty_reader.try_wait().ok().flatten().map(|s| s.exit_code());
                            let _ = pty_tx.send(Event::PtyExit(exit_code));
                            break;
                        }
                        Ok(n) => {
                            if pty_tx.send(Event::PtyOutput(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("session: PTY read error: {}", e);
                            let exit_code =
                                pty_reader.try_wait().ok().flatten().map(|s| s.exit_code());
                            let _ = pty_tx.send(Event::PtyExit(exit_code));
                            break;
                        }
                    }
                }
            })
            .expect("failed to spawn session-pty thread");

        // Forward coordinator commands to internal event channel
        let cmd_tx = event_tx_internal.clone();
        tokio::spawn(async move {
            while let Some(cmd) = command_rx.recv().await {
                if cmd_tx.send(Event::Command(cmd)).is_err() {
                    break;
                }
            }
        });

        // Listen for SIGWINCH to handle terminal resize immediately
        #[cfg(unix)]
        {
            let resize_tx = event_tx_internal.clone();
            tokio::spawn(async move {
                let mut sigwinch =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                        .expect("failed to register SIGWINCH handler");
                loop {
                    sigwinch.recv().await;
                    if resize_tx.send(Event::Resize).is_err() {
                        break;
                    }
                }
            });
        }

        // Move VT processing and rendering to a dedicated thread.
        // Terminal is !Send, so all VT access must stay on one thread.
        let coordinator_tx = event_tx.clone();
        let pty_for_thread = Arc::clone(&pty);
        let vt_handle = std::thread::spawn(move || -> Result<Option<u32>, SessionError> {
            // Create the VT on this thread (Terminal is !Send). Owned with a
            // NULL allocator, so its lifetimes are 'static — required to store a
            // RenderState alongside it in the Renderer.
            let mut vt: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
                cols: pty_size.cols,
                rows: pty_size.rows,
                max_scrollback: DEFAULT_MAX_SCROLLBACK,
            })?;

            // Reset the VT after resize so any stale PTY output (the shell's
            // PROMPT_COMMAND after task execution, SIGWINCH redraw from the
            // resize above) starts on a clean slate. The event loop will
            // process any pending PTY output normally.
            if let Err(e) = vt.resize(pty_size.cols, pty_size.rows, 0, 0) {
                tracing::warn!("failed to resize terminal: {e}");
            }
            vt.vt_write(b"\x1b[2J\x1b[H");

            // Initialize the renderer and do a full initial draw
            let mut renderer = Renderer::new(pty_size.rows)?;
            if row_offset > 0 {
                renderer.row_offset = row_offset;
                renderer.sync(&vt);
            } else {
                renderer.render_full(&mut stdout, &vt)?;
            }
            if self.config.show_status_line {
                self.status_line
                    .draw(&mut stdout, self.size.cols, self.size.rows)?;
            }
            renderer.write_cursor(&mut stdout, &vt)?;
            stdout.flush()?;

            self.event_loop(
                &pty_for_thread,
                &mut vt,
                &mut renderer,
                event_rx_internal,
                &coordinator_tx,
                &mut stdout,
            )
        });

        // Wait for VT thread without blocking the tokio runtime
        let session_start = Instant::now();
        let exit_code = tokio::task::spawn_blocking(move || {
            vt_handle.join().unwrap_or(Err(SessionError::ChannelClosed))
        })
        .await
        .map_err(|_| SessionError::ChannelClosed)??;
        crate::perf::add_session(session_start.elapsed());
        crate::perf::dump();

        let _ = pty.kill();

        // Notify coordinator that shell exited
        if let Err(e) = event_tx.try_send(ShellEvent::Exited { exit_code }) {
            tracing::trace!("failed to send Exited event: {e}");
        }

        Ok(exit_code)
    }

    /// Main event loop handling stdin, PTY output, and coordinator commands.
    /// Returns the exit code from the PTY child process, if available.
    fn event_loop(
        &mut self,
        pty: &Arc<Pty>,
        vt: &mut Terminal<'static, 'static>,
        renderer: &mut Renderer,
        event_rx: std::sync::mpsc::Receiver<Event>,
        coordinator_tx: &tokio_mpsc::Sender<ShellEvent>,
        stdout: &mut Box<dyn Write + Send>,
    ) -> Result<Option<u32>, SessionError> {
        let spinner_interval = Duration::from_millis(SPINNER_INTERVAL_MS);
        let mut scanner = EscapeScanner::new();
        let mut utf8_acc = Utf8Accumulator::new();
        let mut esc = EscapeState::new();
        let mut resize_pending = false;
        let mut esc_events = Vec::new();
        // Reusable buffers for the alt-screen passthrough path: `fwd_buf` holds
        // the escapes still owed to the terminal when we exit alt-screen, and
        // `batch_chunks` holds the raw bytes copied straight through while a
        // nested app owns the alt-screen.
        let mut fwd_buf: Vec<u8> = Vec::new();
        let mut batch_chunks: Vec<Vec<u8>> = Vec::new();

        loop {
            // Use select! to handle both events and spinner animation
            let event = if resize_pending {
                resize_pending = false;
                Some(Event::Resize)
            } else if self.status_line.state().building {
                match event_rx.recv_timeout(spinner_interval) {
                    Ok(event) => Some(event),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if self.config.show_status_line
                            && (crate::perf::legacy() || !esc.in_alternate_screen)
                        {
                            queue!(stdout, terminal::BeginSynchronizedUpdate)?;
                            self.status_line
                                .draw(stdout, self.size.cols, self.size.rows)?;
                            renderer.write_cursor(stdout, vt)?;
                            queue!(stdout, terminal::EndSynchronizedUpdate)?;
                            stdout.flush()?;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
                }
            } else if let Some(remaining) = self.status_line.state().reloaded_remaining() {
                match event_rx.recv_timeout(remaining) {
                    Ok(event) => Some(event),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        self.status_line.state_mut().clear_reloaded();
                        if self.config.show_status_line
                            && (crate::perf::legacy() || !esc.in_alternate_screen)
                        {
                            queue!(stdout, terminal::BeginSynchronizedUpdate)?;
                            self.status_line
                                .draw(stdout, self.size.cols, self.size.rows)?;
                            renderer.write_cursor(stdout, vt)?;
                            queue!(stdout, terminal::EndSynchronizedUpdate)?;
                            stdout.flush()?;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
                }
            } else {
                event_rx.recv().ok()
            };

            let Some(event) = event else {
                break;
            };

            match event {
                Event::Stdin(data) => {
                    if data.as_slice() == KEYBIND_TOGGLE_PAUSE {
                        if let Err(e) = coordinator_tx.try_send(ShellEvent::TogglePause) {
                            tracing::trace!("failed to send TogglePause event: {e}");
                        }
                        continue;
                    }
                    if data.as_slice() == KEYBIND_LIST_WATCHED {
                        if let Err(e) = coordinator_tx.try_send(ShellEvent::ListWatchedFiles) {
                            tracing::trace!("failed to send ListWatchedFiles event: {e}");
                        }
                        continue;
                    }
                    if data.as_slice() == KEYBIND_TOGGLE_ERROR {
                        let state = self.status_line.state_mut();
                        if state.error.is_some() {
                            state.show_error = !state.show_error;
                            if state.show_error {
                                let error = state.error.clone().unwrap();
                                let mut error_text =
                                    String::from("\r\n\x1b[1;31mBuild error:\x1b[0m\r\n");
                                for line in error.lines() {
                                    error_text.push_str(&format!("  {}\r\n", line));
                                }
                                error_text.push_str("\r\n");
                                feed_vt(vt, &error_text);
                                if renderer.row_offset > 0 {
                                    renderer.render(stdout, vt)?;
                                } else {
                                    renderer.render_with_scroll(stdout, vt)?;
                                }
                            } else {
                                pty.write_all(&[0x0C])?;
                                pty.flush()?;
                            }
                            self.status_line
                                .draw(stdout, self.size.cols, self.size.rows)?;
                            renderer.write_cursor(stdout, vt)?;
                            stdout.flush()?;
                        }
                        continue;
                    }
                    if !&data.is_empty() {
                        pty.write_all(&data)?;
                        pty.flush()?;
                    }
                }

                Event::PtyOutput(data) => {
                    let was_in_alt = esc.in_alternate_screen;
                    esc.reset_batch();
                    let mut perf_bytes_in = data.len();
                    let mut perf_chunks = 1usize;

                    // While a nested full-screen app owns the alt-screen, copy its
                    // raw output straight to the terminal instead of re-rendering
                    // it through the VT (handled after the drain loop). Engaged
                    // only when the batch *starts* in alt-screen; the enter/exit
                    // frames still flow through the VT so the primary grid the
                    // terminal restores on exit stays coherent.
                    let passthrough = was_in_alt && crate::perf::passthrough();

                    fwd_buf.clear();
                    batch_chunks.clear();
                    let mut total_scroll = 0usize;

                    // First chunk: scan escapes — forwarding straight to the
                    // terminal normally, or into `fwd_buf` while buffering a
                    // passthrough batch — and feed the VT unless we are passing the
                    // batch through untouched.
                    let t = crate::perf::enabled().then(Instant::now);
                    if passthrough {
                        escape_state_process(
                            &mut scanner,
                            &data,
                            &mut esc,
                            &mut fwd_buf,
                            &**pty,
                            self.pty_size(),
                            &mut esc_events,
                        )?;
                    } else {
                        escape_state_process(
                            &mut scanner,
                            &data,
                            &mut esc,
                            stdout,
                            &**pty,
                            self.pty_size(),
                            &mut esc_events,
                        )?;
                    }
                    if let Some(t) = t {
                        crate::perf::add_scan(t.elapsed());
                    }
                    if passthrough {
                        batch_chunks.push(data);
                    } else {
                        let text = utf8_acc.accumulate(&data);
                        let t = crate::perf::enabled().then(Instant::now);
                        total_scroll += feed_vt(vt, &text);
                        if let Some(t) = t {
                            crate::perf::add_vt(t.elapsed());
                        }
                    }

                    // Batch: drain any additional pending events.
                    while let Ok(event) = event_rx.try_recv() {
                        match event {
                            Event::PtyOutput(more) => {
                                perf_bytes_in += more.len();
                                perf_chunks += 1;
                                let t = crate::perf::enabled().then(Instant::now);
                                if passthrough {
                                    escape_state_process(
                                        &mut scanner,
                                        &more,
                                        &mut esc,
                                        &mut fwd_buf,
                                        &**pty,
                                        self.pty_size(),
                                        &mut esc_events,
                                    )?;
                                } else {
                                    escape_state_process(
                                        &mut scanner,
                                        &more,
                                        &mut esc,
                                        stdout,
                                        &**pty,
                                        self.pty_size(),
                                        &mut esc_events,
                                    )?;
                                }
                                if let Some(t) = t {
                                    crate::perf::add_scan(t.elapsed());
                                }
                                if passthrough {
                                    batch_chunks.push(more);
                                } else {
                                    let text = utf8_acc.accumulate(&more);
                                    let t = crate::perf::enabled().then(Instant::now);
                                    total_scroll += feed_vt(vt, &text);
                                    if let Some(t) = t {
                                        crate::perf::add_vt(t.elapsed());
                                    }
                                }
                            }
                            Event::PtyExit(exit_code) => {
                                if passthrough && esc.in_alternate_screen {
                                    // Shell exited mid-passthrough: leave alt-screen
                                    // and reset modes; the VT grid is frozen/stale,
                                    // so don't render it. The unflushed buffered
                                    // bytes are intentionally dropped — the program
                                    // is gone and we're restoring the primary screen.
                                    // cleanup resets every tracked mode (resets are
                                    // idempotent), so the terminal is left clean.
                                    escape_state_cleanup(&esc, stdout)?;
                                    stdout.flush()?;
                                    return Ok(exit_code);
                                }
                                if passthrough {
                                    // Exited alt-screen, then the shell exited in the
                                    // same batch: forward the tracked escapes and
                                    // fold the buffered bytes into the VT so the
                                    // final render is correct.
                                    stdout.write_all(&fwd_buf)?;
                                    for chunk in batch_chunks.drain(..) {
                                        let text = utf8_acc.accumulate(&chunk);
                                        feed_vt(vt, &text);
                                    }
                                }
                                escape_state_cleanup(&esc, stdout)?;
                                renderer.render_with_scroll(stdout, vt)?;
                                return Ok(exit_code);
                            }
                            Event::Stdin(stdin_data) => {
                                if !&stdin_data.is_empty() {
                                    pty.write_all(&stdin_data)?;
                                    pty.flush()?;
                                }
                            }
                            Event::Command(cmd) => {
                                // In passthrough the VT is frozen and unrendered, so
                                // ignore any scroll the command would have produced.
                                let scrolled = self.handle_command(cmd, vt)?;
                                if !passthrough {
                                    total_scroll += scrolled;
                                }
                            }
                            Event::Resize => {
                                resize_pending = true;
                                break;
                            }
                        }
                    }

                    // Pure alt-screen batch: copy the raw bytes through and skip
                    // the VT render entirely — the nested app drew the terminal
                    // directly, so there is nothing left for us to re-render.
                    //
                    // `batch_chunks` is the program's verbatim output, so every
                    // byte it emitted — including mode-setting escapes (mouse,
                    // bracketed paste, kitty keyboard, …) — reaches the terminal
                    // here. `fwd_buf` holds the renderer's *duplicate* copies of
                    // the forwarded escapes; it is dropped precisely so those bytes
                    // aren't emitted twice. The escape-state tracking stays in sync
                    // with the terminal because both saw the same raw stream.
                    //
                    // Assumes at most one alt-screen transition per batch: a batch
                    // that exits and re-enters within itself (even number of
                    // toggles in one ~64 KiB PTY read — not something real apps do)
                    // would leave the frozen VT's saved primary grid out of step
                    // until the next clean exit reconciles it.
                    if passthrough && esc.in_alternate_screen {
                        let t = crate::perf::enabled().then(Instant::now);
                        for chunk in &batch_chunks {
                            stdout.write_all(chunk)?;
                        }
                        stdout.flush()?;
                        if let Some(t) = t {
                            crate::perf::add_render(t.elapsed());
                        }
                        crate::perf::record_passthrough();
                        crate::perf::record_batch(perf_bytes_in, perf_chunks);
                        continue;
                    }

                    if passthrough {
                        // Exited alt-screen during this batch. Forward the escapes
                        // we tracked (this carries the alt-screen reset, so the
                        // terminal restores its primary buffer) and fold the
                        // buffered raw bytes into the VT so its primary grid catches
                        // up before the authoritative re-render below.
                        stdout.write_all(&fwd_buf)?;
                        for chunk in batch_chunks.drain(..) {
                            let text = utf8_acc.accumulate(&chunk);
                            total_scroll += feed_vt(vt, &text);
                        }
                    }

                    // Begin synchronized output so the terminal buffers
                    // all writes atomically (mode 2026).
                    queue!(stdout, terminal::BeginSynchronizedUpdate)?;

                    // Handle alternate screen transitions
                    if was_in_alt != esc.in_alternate_screen {
                        renderer.invalidate();
                        // The status line is suppressed while in alt-screen, so
                        // force a redraw on the way back to the primary buffer
                        // (where the row may be stale).
                        self.status_line.mark_dirty();
                    }

                    // Consume offset if needed: when cursor would land
                    // off-screen or VT scrolled, push old TUI content
                    // into native scrollback to make room.
                    if renderer.row_offset > 0 {
                        let content_rows = renderer.content_rows;
                        let visible_rows = renderer.visible_rows();
                        let cursor_row = vt.cursor_y().map(|r| r as usize).unwrap_or(0);
                        let cursor_excess = (cursor_row + 1).saturating_sub(visible_rows);
                        let need = total_scroll.max(cursor_excess);

                        let consumed = if esc.in_alternate_screen || esc.erase_display {
                            // Alternate screen or explicit screen clear (CSI 2J):
                            // consume the entire offset so the shell owns the
                            // full visible area.
                            renderer.row_offset as usize
                        } else {
                            need.min(renderer.row_offset as usize)
                        };
                        if consumed > 0 {
                            Renderer::scroll_region(stdout, content_rows, consumed)?;
                            renderer.row_offset -= consumed as u16;
                            renderer.invalidate();
                        }
                    }

                    if esc.clear_scrollback {
                        queue!(stdout, Clear(ClearType::Purge))?;
                    }

                    // Full-screen erase fast-path: when the inner program cleared
                    // the whole viewport (CSI 2J) on the primary screen, mirror it
                    // with a single native clear instead of repainting every row.
                    // render() then redraws only the rows with content (e.g. the
                    // prompt) and skips the now-blank ones. Gated off in alt-screen
                    // (a nested TUI owns and refills that buffer, so the clear is
                    // pure overhead there) and during the row_offset hand-off phase
                    // (the offset block above scrolls old content into scrollback
                    // first). Legacy mode keeps the full per-row redraw.
                    if !crate::perf::legacy()
                        && esc.erase_display
                        && !esc.in_alternate_screen
                        && renderer.row_offset == 0
                    {
                        renderer.native_clear(stdout)?;
                        self.status_line.mark_dirty();
                    }

                    let t = crate::perf::enabled().then(Instant::now);
                    if esc.in_alternate_screen || renderer.row_offset > 0 {
                        renderer.render(stdout, vt)?;
                    } else {
                        renderer.render_with_scroll(stdout, vt)?;
                    }
                    if let Some(t) = t {
                        crate::perf::add_render(t.elapsed());
                    }

                    // Suppress the status line while a nested app owns the
                    // alt-screen — it would fight the TUI for the bottom row and
                    // cost an iocraft layout + redraw every frame. (Legacy mode
                    // keeps drawing it for a faithful before/after.)
                    if self.config.show_status_line
                        && (crate::perf::legacy() || !esc.in_alternate_screen)
                    {
                        let t = crate::perf::enabled().then(Instant::now);
                        let drew = self
                            .status_line
                            .draw(stdout, self.size.cols, self.size.rows)?;
                        if let Some(t) = t {
                            crate::perf::add_status(t.elapsed());
                        }
                        crate::perf::record_status(drew);
                    }
                    renderer.write_cursor(stdout, vt)?;

                    // End synchronized output and flush.
                    queue!(stdout, terminal::EndSynchronizedUpdate)?;
                    stdout.flush()?;
                    crate::perf::record_batch(perf_bytes_in, perf_chunks);
                }

                Event::PtyExit(exit_code) => {
                    self.clear_status_row(stdout, esc.in_alternate_screen)?;
                    escape_state_cleanup(&esc, stdout)?;
                    stdout.flush()?;
                    return Ok(exit_code);
                }

                Event::Command(cmd) => {
                    self.handle_command(cmd, vt)?;
                    // While a nested app owns the alt-screen the VT is frozen and
                    // the terminal is drawn directly by that app; re-rendering the
                    // VT here would paint stale content over it. Skip the draw —
                    // the status row is suppressed in alt-screen anyway.
                    if !esc.in_alternate_screen || crate::perf::legacy() {
                        queue!(stdout, terminal::BeginSynchronizedUpdate)?;
                        if renderer.row_offset > 0 {
                            renderer.render(stdout, vt)?;
                        } else {
                            renderer.render_with_scroll(stdout, vt)?;
                        }
                        self.draw_status_and_cursor(stdout, vt, renderer, esc.in_alternate_screen)?;
                        queue!(stdout, terminal::EndSynchronizedUpdate)?;
                        stdout.flush()?;
                    }
                }

                Event::Resize => {
                    if let Ok((cols, rows)) = terminal::size()
                        && (cols != self.size.cols || rows != self.size.rows)
                    {
                        self.size = PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        };
                        // Terminal resize ends the offset phase
                        renderer.row_offset = 0;
                        let pty_size = self.pty_size();
                        renderer.content_rows = pty_size.rows;
                        let _ = pty.resize(pty_size);
                        // Send a mode 2048 in-band resize notification
                        // through the PTY, but only if the program has
                        // enabled mode 2048. Sending it unconditionally
                        // causes shells that don't understand it to display
                        // the raw escape sequence as input text.
                        if esc.in_band_resize {
                            let cmd = InBandResizeNotification {
                                rows: pty_size.rows,
                                cols: pty_size.cols,
                            };
                            let mut buf = String::new();
                            cmd.write_ansi(&mut buf).unwrap();
                            let _ = pty.write_all(buf.as_bytes());
                        }
                        if let Err(e) = vt.resize(pty_size.cols, pty_size.rows, 0, 0) {
                            tracing::warn!("failed to resize terminal: {e}");
                        }
                        // Row geometry changed — force a status redraw even if the
                        // content text is unchanged.
                        self.status_line.mark_dirty();
                        if esc.in_alternate_screen && crate::perf::passthrough() {
                            // Passthrough: the nested app repaints itself on its
                            // SIGWINCH (delivered by the PTY resize above), so don't
                            // paint the frozen VT over it. The VT was resized so the
                            // primary grid is correct when we exit alt-screen; force
                            // a full redraw then.
                            renderer.invalidate();
                        } else {
                            renderer.discard_vt_scrollback(vt);
                            renderer.render_full(stdout, vt)?;
                            if self.config.show_status_line && !esc.in_alternate_screen {
                                self.status_line.draw(stdout, cols, rows)?;
                            }
                            renderer.write_cursor(stdout, vt)?;
                            stdout.flush()?;
                        }
                        if let Err(e) = coordinator_tx.try_send(ShellEvent::Resize {
                            cols: pty_size.cols,
                            rows: pty_size.rows,
                        }) {
                            tracing::trace!("failed to send Resize event: {e}");
                        }
                    }
                }
            }
        }

        self.clear_status_row(stdout, esc.in_alternate_screen)?;
        escape_state_cleanup(&esc, stdout)?;
        stdout.flush()?;
        Ok(None)
    }

    /// Handle a command from the coordinator.
    ///
    /// Updates state and, for some commands (e.g. `PrintWatchedFiles`), feeds
    /// text into the VT. Does not write to stdout. Returns the scroll count
    /// so the caller can fold it into its render pass.
    fn handle_command(
        &mut self,
        cmd: ShellCommand,
        vt: &mut Terminal<'_, '_>,
    ) -> Result<usize, SessionError> {
        match cmd {
            ShellCommand::ReloadReady { changed_files } => {
                self.status_line.state_mut().set_reload_ready(changed_files);
            }

            ShellCommand::Building { changed_files } => {
                self.status_line.state_mut().set_building(changed_files);
            }

            ShellCommand::BuildFailed {
                changed_files,
                error,
            } => {
                self.status_line
                    .state_mut()
                    .set_build_failed(changed_files, error);
            }

            ShellCommand::ReloadApplied => {
                self.status_line.state_mut().set_reloaded();
            }

            ShellCommand::WatchedFiles { files } => {
                self.status_line
                    .state_mut()
                    .set_watched_file_count(files.len());
            }

            ShellCommand::WatchingPaused { paused } => {
                self.status_line.state_mut().set_paused(paused);
            }

            ShellCommand::PrintWatchedFiles { files } => {
                let mut text = format!("\r\n\x1b[1mWatched files ({}):\x1b[0m\r\n", files.len());
                for file in &files {
                    text.push_str(&format!("  {}\r\n", file.display()));
                }
                return Ok(feed_vt(vt, &text));
            }

            ShellCommand::Shutdown => {
                // Will be handled by returning from event loop
            }

            ShellCommand::Spawn { .. } => {
                // Shouldn't receive Spawn after initial
            }
        }

        Ok(0)
    }

    /// Draw status line and reposition cursor.
    ///
    /// Does not flush — callers flush after ending their sync block.
    fn draw_status_and_cursor(
        &mut self,
        stdout: &mut impl Write,
        vt: &Terminal<'_, '_>,
        renderer: &Renderer,
        in_alternate_screen: bool,
    ) -> Result<(), SessionError> {
        // Suppressed in alt-screen; render already positioned the cursor there.
        // (Legacy mode keeps drawing it for a faithful before/after.)
        if self.config.show_status_line && (crate::perf::legacy() || !in_alternate_screen) {
            self.status_line
                .draw(stdout, self.size.cols, self.size.rows)?;
            renderer.write_cursor(stdout, vt)?;
        }
        Ok(())
    }

    /// Clear the status line row (e.g. on exit).
    fn clear_status_row(
        &self,
        stdout: &mut impl Write,
        in_alternate_screen: bool,
    ) -> io::Result<()> {
        if self.config.show_status_line && !in_alternate_screen {
            // Save cursor, clear the status row, restore cursor.
            queue!(
                stdout,
                cursor::SavePosition,
                cursor::MoveTo(0, self.size.rows - 1),
                Clear(ClearType::CurrentLine),
                cursor::RestorePosition,
            )?;
        }
        Ok(())
    }
}

impl Default for ShellSession {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// PTY-free driver for the per-batch render pipeline, used by benches to push a
/// fixed byte stream through scan → vt_write → render → status into an in-memory
/// sink. Mirrors the `Event::PtyOutput` arm of [`ShellSession::event_loop`]
/// without the PTY, threads, or channels so the core render cost can be measured
/// deterministically. Feed inputs must not contain a TextAreaSizeQuery (CSI 18t);
/// the no-op responder simply drops any reply.
#[cfg(any(test, feature = "bench-internals"))]
pub struct RenderHarness {
    vt: Terminal<'static, 'static>,
    renderer: Renderer,
    scanner: EscapeScanner,
    utf8_acc: Utf8Accumulator,
    esc: EscapeState,
    status: Option<StatusLine>,
    esc_events: Vec<crate::escape::SequenceEvent>,
    size: PtySize,
    out: Vec<u8>,
    fwd_buf: Vec<u8>,
}

#[cfg(any(test, feature = "bench-internals"))]
struct NoResponder;

#[cfg(any(test, feature = "bench-internals"))]
impl crate::escape_state::QueryResponder for NoResponder {
    fn respond(&self, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(any(test, feature = "bench-internals"))]
impl RenderHarness {
    /// Build a harness for a `cols`x`rows` terminal. `status_line` toggles the
    /// per-batch status-line draw so its cost can be isolated.
    pub fn new(cols: u16, rows: u16, status_line: bool) -> Self {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut vt = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: DEFAULT_MAX_SCROLLBACK,
        })
        .expect("create VT");
        vt.vt_write(b"\x1b[2J\x1b[H");
        // StatusLine::new() is enabled by default; render the realistic
        // "watching N files" idle line so the bench captures its layout cost.
        let status = status_line.then(|| {
            let mut s = StatusLine::new();
            s.state_mut().set_watched_file_count(3);
            s
        });
        Self {
            vt,
            renderer: Renderer::new(rows).expect("create renderer"),
            scanner: EscapeScanner::new(),
            utf8_acc: Utf8Accumulator::new(),
            esc: EscapeState::new(),
            status,
            esc_events: Vec::new(),
            size,
            out: Vec::with_capacity(64 * 1024),
            fwd_buf: Vec::new(),
        }
    }

    /// Push one chunk of inner-program output through the full pipeline,
    /// appending the re-emitted terminal bytes to the internal sink. Mirrors the
    /// session's per-batch logic, including alt-screen passthrough (a chunk that
    /// starts and stays in alt-screen is copied straight through, with no VT
    /// render).
    pub fn feed(&mut self, chunk: &[u8]) {
        let was_in_alt = self.esc.in_alternate_screen;
        self.esc.reset_batch();
        let passthrough = was_in_alt && crate::perf::passthrough();

        if passthrough {
            self.fwd_buf.clear();
            escape_state_process(
                &mut self.scanner,
                chunk,
                &mut self.esc,
                &mut self.fwd_buf,
                &NoResponder,
                self.size,
                &mut self.esc_events,
            )
            .expect("scan");
            if self.esc.in_alternate_screen {
                // Pure alt-screen batch: copy raw, skip the VT render entirely.
                self.out.extend_from_slice(chunk);
                crate::perf::record_passthrough();
                return;
            }
            // Exited alt-screen: forward tracked escapes, fold into the VT, render.
            self.out.extend_from_slice(&self.fwd_buf);
            let text = self.utf8_acc.accumulate(chunk);
            feed_vt(&mut self.vt, &text);
            self.renderer.invalidate();
        } else {
            escape_state_process(
                &mut self.scanner,
                chunk,
                &mut self.esc,
                &mut self.out,
                &NoResponder,
                self.size,
                &mut self.esc_events,
            )
            .expect("scan");
            let text = self.utf8_acc.accumulate(chunk);
            feed_vt(&mut self.vt, &text);
            if was_in_alt != self.esc.in_alternate_screen {
                self.renderer.invalidate();
            }
        }
        // Mirror the event loop's full-screen-erase fast-path so the bench
        // measures it (see `event_loop`). The harness has no offset/3J handling,
        // so the gate is just the primary-screen 2J case.
        if !crate::perf::legacy()
            && self.esc.erase_display
            && !self.esc.in_alternate_screen
            && self.renderer.row_offset == 0
        {
            self.renderer
                .native_clear(&mut self.out)
                .expect("native clear");
            if let Some(status) = &mut self.status {
                status.mark_dirty();
            }
        }
        if self.esc.in_alternate_screen || self.renderer.row_offset > 0 {
            self.renderer
                .render(&mut self.out, &self.vt)
                .expect("render");
        } else {
            self.renderer
                .render_with_scroll(&mut self.out, &mut self.vt)
                .expect("render");
        }
        // Mirror the session: the status line is suppressed in alt-screen
        // (legacy mode draws it every frame regardless, for before/after).
        if let Some(status) = &mut self.status
            && (crate::perf::legacy() || !self.esc.in_alternate_screen)
        {
            status
                .draw(&mut self.out, self.size.cols, self.size.rows)
                .expect("status");
        }
        self.renderer
            .write_cursor(&mut self.out, &self.vt)
            .expect("cursor");
    }

    /// Total bytes re-emitted so far (the numerator of output amplification).
    #[cfg(feature = "bench-internals")]
    pub fn output_len(&self) -> usize {
        self.out.len()
    }

    /// Bytes re-emitted so far, for assertions in tests.
    #[cfg(test)]
    pub fn output_bytes(&self) -> &[u8] {
        &self.out
    }

    /// Drop accumulated output without resetting VT/renderer state.
    pub fn reset_output(&mut self) {
        self.out.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::CommandBuilder;

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Alt-screen passthrough: once a nested app owns the alt-screen, its updates
    /// are copied to the terminal verbatim instead of being re-rendered through
    /// the VT. This is the core of "option D" — the nested program effectively
    /// talks to the terminal directly.
    #[test]
    fn passthrough_copies_raw_in_alt_screen() {
        crate::perf::set_legacy(false);
        crate::perf::set_passthrough(true);
        let mut h = RenderHarness::new(80, 24, false);
        // Enter alt-screen with an initial paint — this frame still flows through
        // the VT so the primary grid stays coherent for the eventual exit.
        h.feed(b"\x1b[?1049h\x1b[2J\x1b[Hhello");
        h.reset_output();
        // Steady-state alt-screen update: copied through byte-for-byte.
        let update = b"\x1b[5;3Hworld";
        h.feed(update);
        assert_eq!(
            h.output_bytes(),
            update,
            "alt-screen update should pass through raw"
        );
    }

    /// A mode-setting escape (here mouse tracking) emitted mid alt-screen must
    /// still reach the terminal — it rides through inside the raw copy. The
    /// renderer also tracks it in a forwarded-escape buffer, but that buffer is a
    /// redundant duplicate dropped on the pure-alt path; the raw passthrough is
    /// the single source of truth, so the terminal never diverges from the
    /// tracked mode state.
    #[test]
    fn passthrough_carries_mode_escapes_verbatim() {
        crate::perf::set_legacy(false);
        crate::perf::set_passthrough(true);
        let mut h = RenderHarness::new(80, 24, false);
        h.feed(b"\x1b[?1049h\x1b[2J\x1b[Hhi");
        h.reset_output();
        let update = b"\x1b[?1000h\x1b[5;3Hx";
        h.feed(update);
        assert_eq!(
            h.output_bytes(),
            update,
            "mode escape + content must pass through verbatim (no duplication, no loss)"
        );
    }

    /// With passthrough disabled the same alt-screen update is re-rendered through
    /// the VT (absolute MoveTo per row, ResetColor, …), so the emitted bytes are
    /// not a verbatim copy of the input.
    #[test]
    fn passthrough_disabled_re_renders() {
        crate::perf::set_legacy(false);
        crate::perf::set_passthrough(false);
        let mut h = RenderHarness::new(80, 24, false);
        h.feed(b"\x1b[?1049h\x1b[2J\x1b[Hhello");
        h.reset_output();
        let update = b"\x1b[5;3Hworld";
        h.feed(update);
        assert_ne!(h.output_bytes(), update);
        assert!(!h.output_bytes().is_empty());
        crate::perf::set_passthrough(true);
    }

    /// Legacy mode forces the pre-optimization full re-render, so passthrough is
    /// off even though the flag is on.
    #[test]
    fn legacy_disables_passthrough() {
        crate::perf::set_passthrough(true);
        crate::perf::set_legacy(true);
        let mut h = RenderHarness::new(80, 24, false);
        h.feed(b"\x1b[?1049h\x1b[2J\x1b[Hhello");
        h.reset_output();
        let update = b"\x1b[5;3Hworld";
        h.feed(update);
        assert_ne!(
            h.output_bytes(),
            update,
            "legacy mode should re-render, not pass through"
        );
        crate::perf::set_legacy(false);
    }

    /// Exiting the alt-screen out of a passthrough run reconciles the terminal to
    /// the VT's primary grid: the alt-screen reset is forwarded and the
    /// primary-screen content is rendered (not copied raw).
    #[test]
    fn passthrough_exit_reconciles_to_primary() {
        crate::perf::set_legacy(false);
        crate::perf::set_passthrough(true);
        let mut h = RenderHarness::new(80, 24, false);
        h.feed(b"\x1b[?1049h\x1b[2J\x1b[Halt-content");
        h.feed(b"\x1b[3;3Hmore-alt"); // steady-state passthrough
        h.reset_output();
        // Exit alt-screen and print a primary-screen prompt.
        h.feed(b"\x1b[?1049l\r\n$ ready");
        let out = h.output_bytes();
        assert!(
            contains(out, b"\x1b[?1049l"),
            "alt-screen reset should be forwarded on exit"
        );
        assert!(
            contains(out, b"ready"),
            "primary prompt should be re-rendered after exit"
        );
    }

    /// Nails the libghostty dirty-tracking protocol the renderer depends on:
    /// reading per-row grid dirty then consuming it via RenderState::update so
    /// the next unchanged frame is clean and a single-cell change dirties one row.
    #[test]
    fn grid_dirty_is_consumed_by_update() {
        let mut vt: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: DEFAULT_MAX_SCROLLBACK,
        })
        .unwrap();
        let mut rs = RenderState::new().unwrap();

        fn count_grid_dirty(vt: &Terminal<'static, 'static>, rows: u16) -> u32 {
            (0..rows)
                .filter(|&y| {
                    vt.grid_ref(active_point(y as u32))
                        .ok()
                        .and_then(|g| g.row().ok())
                        .and_then(|r| r.is_dirty().ok())
                        .unwrap_or(true)
                })
                .count() as u32
        }

        vt.vt_write(b"\x1b[2J\x1b[Hhello");
        let n1 = count_grid_dirty(&vt, 24);
        let _ = rs.update(&vt); // consume
        let n2 = count_grid_dirty(&vt, 24);

        vt.vt_write(b"\x1b[10;5HZ");
        let n3 = count_grid_dirty(&vt, 24);
        let _ = rs.update(&vt);
        let n4 = count_grid_dirty(&vt, 24);

        assert!(n1 >= 1, "write should dirty >=1 row, got {n1}");
        assert_eq!(n2, 0, "update should consume grid dirty, got {n2}");
        // A 1-cell write dirties the written row (and possibly the cursor's
        // previous row); the point is it's a small subset, not the whole screen.
        assert!(
            (1..=3).contains(&n3),
            "single-cell change should dirty a small subset, got {n3}"
        );
        assert_eq!(n4, 0, "update should consume grid dirty again, got {n4}");
    }

    /// The full-screen-erase fast-path: after a screen of content, a CSI 2J +
    /// prompt is re-emitted as a SINGLE native clear plus the prompt row — not a
    /// per-row MoveTo+Clear(CurrentLine) for every blank row (the slow path this
    /// optimizes away). Blank rows are skipped; the prompt row still renders.
    #[test]
    fn native_clear_collapses_full_erase() {
        fn count(haystack: &[u8], needle: &[u8]) -> usize {
            haystack
                .windows(needle.len())
                .filter(|w| *w == needle)
                .count()
        }

        let mut vt: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: DEFAULT_MAX_SCROLLBACK,
        })
        .unwrap();
        let mut renderer = Renderer::new(24).unwrap();
        let mut out: Vec<u8> = Vec::new();

        // Fill the viewport, render it as the baseline.
        for r in 1..=24 {
            feed_vt(&mut vt, &format!("\x1b[{r};1Hrow {r} content here"));
        }
        renderer.render_full(&mut out, &vt).unwrap();
        out.clear();

        // Clear the whole screen and draw a prompt, the way `clear`/Ctrl-L does.
        feed_vt(&mut vt, "\x1b[2J\x1b[Huser@host:~$ ");
        renderer.native_clear(&mut out).unwrap();
        renderer.render(&mut out, &vt).unwrap();

        assert_eq!(
            count(&out, b"\x1b[2J"),
            1,
            "expected exactly one native screen clear, got {:?}",
            String::from_utf8_lossy(&out)
        );
        // Only the (single) prompt row gets a per-line clear; the 23 blank rows
        // are skipped. The slow path would emit ~24 of these.
        assert_eq!(
            count(&out, b"\x1b[2K"),
            1,
            "blank rows must be skipped, got {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            out.windows(12).any(|w| w == b"user@host:~$"),
            "prompt row must still be drawn, got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// Regression test for devenv#2845: when the process-wide shutdown token is
    /// cancelled (e.g. from the SIGHUP/SIGINT/SIGTERM handler), the inner shell
    /// must die with it. Otherwise the PTY (in its own session via setsid)
    /// outlives devenv and orphans, burning CPU after the terminal closes.
    ///
    /// Exercises the same wiring `ShellSession::run` installs after PTY spawn:
    /// a tokio task that, on `token.cancelled()`, calls `pty.kill()`.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_token_kills_inner_shell() {
        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("5");
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pty = Arc::new(Pty::spawn(cmd, size).expect("spawn inner pty"));

        let token = CancellationToken::new();
        let pty_killer = Arc::clone(&pty);
        let token_for_task = token.clone();
        tokio::spawn(async move {
            token_for_task.cancelled().await;
            let _ = pty_killer.kill();
        });

        token.cancel();

        // The kill is asynchronous; poll briefly for the child to reap.
        let mut status = None;
        for _ in 0..500 {
            status = pty.try_wait().expect("try_wait");
            if status.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            status.is_some(),
            "inner shell still running after shutdown token cancellation"
        );
    }
}
