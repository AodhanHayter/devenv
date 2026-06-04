//! Dependency-free render-pipeline benchmark.
//!
//! `devenv shell` mediates every PTY-output batch through a VT emulator and
//! re-renders the visible screen (tmux-style), which is what makes nested
//! full-screen TUIs (nvim, claude-code) feel laggy. This benchmark drives fixed
//! byte streams through that exact pipeline (escape scan → vt_write → render →
//! status) via [`devenv_shell::RenderHarness`], with no PTY, threads, or real
//! terminal, so the core render cost is measured deterministically.
//!
//! Run: `cargo bench -p devenv-shell --features bench-internals`
//!
//! Real fixtures: drop raw PTY-master captures as `benches/fixtures/*.bin` and
//! they are picked up automatically. Capture command is in
//! `benches/fixtures/README.md`.
//!
//! The `legacy` (unchanged) side re-renders every frame through the VT; the
//! optimized side adds per-row dirty tracking and, crucially, alt-screen
//! passthrough — a batch that starts and stays in the alternate screen is copied
//! straight through, so a nested full-screen app pays ~zero render cost. The
//! before/after on the `alt_*` and `fixture:nvim*` corpora is that passthrough.
//!
//! Metrics per corpus:
//! - MiB/s        — input throughput the pipeline sustains
//! - us/frame     — wall time per rendered batch (the latency a nested app sees)
//! - amplify      — bytes re-emitted / bytes in (output blow-up; ~1.0 = passthrough)
//! - cells/frame  — libghostty FFI cell reads per frame (the dominant cost)
//! - redraw/skip  — rows re-serialized vs rows the diff skipped (still read)

use devenv_shell::{RenderHarness, perf};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

/// Default terminal geometry; override with `DEVENV_BENCH_COLS`/`DEVENV_BENCH_ROWS`.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// PTY read buffer size — raw fixtures are chunked at this size to mirror how
/// the real reader delivers bytes (one chunk ≈ one un-coalesced render).
const PTY_CHUNK: usize = 4096;
const TIMING_PASSES: usize = 12;

fn env_u16(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Corpus {
    name: String,
    cols: u16,
    rows: u16,
    status_line: bool,
    /// Each entry is one logical update the inner program emits → one render.
    frames: Vec<Vec<u8>>,
}

impl Corpus {
    fn bytes_in(&self) -> usize {
        self.frames.iter().map(|f| f.len()).sum()
    }
}

/// A full-screen repaint every frame: every row moved-to, recolored, rewritten
/// with content that changes each frame. Models nvim/claude-code scrolling or
/// redrawing the whole viewport — the throughput-ceiling case.
fn gen_alt_full_repaint(cols: u16, rows: u16, frames: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(frames);
    for fi in 0..frames {
        let mut b = Vec::new();
        if fi == 0 {
            b.extend_from_slice(b"\x1b[?1049h"); // enter alternate screen
        }
        b.extend_from_slice(b"\x1b[H");
        for r in 0..rows {
            let color = 16 + ((r as u32 + fi as u32) % 200);
            write!(b, "\x1b[{};1H\x1b[38;5;{color}m", r + 1).unwrap();
            for c in 0..cols {
                let ch = b'!' + (((c as u32 + r as u32 + fi as u32) % 90) as u8);
                b.push(ch);
            }
        }
        b.extend_from_slice(b"\x1b[0m");
        out.push(b);
    }
    out
}

/// One full repaint, then frames that change a single cell each (cursor move +
/// one char). Models typing in nvim — exposes the cost of reading the whole
/// screen just to diff a one-cell change.
fn gen_alt_single_cell(cols: u16, rows: u16, frames: usize) -> Vec<Vec<u8>> {
    let mut out = gen_alt_full_repaint(cols, rows, 1);
    for fi in 1..frames {
        let mut b = Vec::new();
        let r = (fi as u16) % rows + 1;
        let c = (fi as u16 * 7) % cols + 1;
        let ch = b'a' + ((fi % 26) as u8);
        write!(b, "\x1b[{r};{c}H").unwrap();
        b.push(ch);
        out.push(b);
    }
    out
}

/// Plain scrolling output (no alt screen) — exercises render_with_scroll and the
/// scrollback flush path. Models `cat` / build logs under the status line.
fn gen_scroll_flood(cols: u16, lines: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(lines);
    for i in 0..lines {
        let mut b = Vec::new();
        let prefix = format!("line {i:>6}: ");
        b.extend_from_slice(prefix.as_bytes());
        let fill = (cols as usize).saturating_sub(prefix.len());
        for c in 0..fill {
            b.push(b'.' + ((c % 10) as u8));
        }
        b.extend_from_slice(b"\r\n");
        out.push(b);
    }
    out
}

/// Per-character SGR color changes — stresses the escape scanner and the SGR
/// re-serialization in dump_row.
fn gen_heavy_sgr(cols: u16, rows: u16, frames: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(frames);
    for fi in 0..frames {
        let mut b = Vec::new();
        if fi == 0 {
            b.extend_from_slice(b"\x1b[?1049h");
        }
        b.extend_from_slice(b"\x1b[H");
        for r in 0..rows {
            write!(b, "\x1b[{};1H", r + 1).unwrap();
            for c in 0..cols {
                let color = 16 + ((c as u32 + fi as u32) % 216);
                write!(b, "\x1b[38;5;{color}m#").unwrap();
            }
        }
        b.extend_from_slice(b"\x1b[0m");
        out.push(b);
    }
    out
}

/// CJK / wide-character grid — exercises the grapheme/wide-cell readback path.
fn gen_cjk_wide(cols: u16, rows: u16, frames: usize) -> Vec<Vec<u8>> {
    const GLYPHS: [&str; 6] = ["日", "本", "語", "你", "好", "世"];
    let mut out = Vec::with_capacity(frames);
    for fi in 0..frames {
        let mut b = Vec::new();
        if fi == 0 {
            b.extend_from_slice(b"\x1b[?1049h");
        }
        b.extend_from_slice(b"\x1b[H");
        for r in 0..rows {
            write!(b, "\x1b[{};1H", r + 1).unwrap();
            // wide glyphs take 2 columns each
            for c in 0..(cols / 2) {
                let g = GLYPHS[(c as usize + r as usize + fi) % GLYPHS.len()];
                b.extend_from_slice(g.as_bytes());
            }
        }
        out.push(b);
    }
    out
}

/// Primary-screen clear storm: alternate a full-viewport repaint with a
/// CSI 2J + home + short prompt. Models a screen full of output then `clear` /
/// Ctrl-L — the case devenv re-renders row-by-row (every row dirty, all blank)
/// where a native terminal does a single 2J. The clear frames (odd) are where
/// the fast-path wins; the fill frames (even) leave content for the next clear
/// to erase. No alt-screen and positioned writes keep it on the primary
/// viewport so the render_with_scroll path is exercised.
fn gen_clear_storm(cols: u16, rows: u16, frames: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(frames);
    for fi in 0..frames {
        let mut b = Vec::new();
        if fi % 2 == 0 {
            for r in 0..rows {
                let color = 16 + ((r as u32 + fi as u32) % 200);
                write!(b, "\x1b[{};1H\x1b[38;5;{color}m", r + 1).unwrap();
                for c in 0..cols {
                    let ch = b'!' + (((c as u32 + r as u32 + fi as u32) % 90) as u8);
                    b.push(ch);
                }
            }
            b.extend_from_slice(b"\x1b[0m\x1b[H");
        } else {
            b.extend_from_slice(b"\x1b[2J\x1b[H");
            b.extend_from_slice(b"user@host:~/project$ ");
        }
        out.push(b);
    }
    out
}

fn synthetic_corpora(cols: u16, rows: u16) -> Vec<Corpus> {
    let mk = |name: &str, status: bool, frames: Vec<Vec<u8>>| Corpus {
        name: name.to_string(),
        cols,
        rows,
        status_line: status,
        frames,
    };
    vec![
        mk(
            "alt_full_repaint",
            false,
            gen_alt_full_repaint(cols, rows, 200),
        ),
        mk(
            "alt_full_repaint+status",
            true,
            gen_alt_full_repaint(cols, rows, 200),
        ),
        mk(
            "alt_single_cell",
            false,
            gen_alt_single_cell(cols, rows, 400),
        ),
        mk(
            "alt_single_cell+status",
            true,
            gen_alt_single_cell(cols, rows, 400),
        ),
        mk("scroll_flood+status", true, gen_scroll_flood(cols, 600)),
        mk("heavy_sgr", false, gen_heavy_sgr(cols, rows, 150)),
        mk("cjk_wide", false, gen_cjk_wide(cols, rows, 150)),
        mk("clear_storm", false, gen_clear_storm(cols, rows, 200)),
        mk("clear_storm+status", true, gen_clear_storm(cols, rows, 200)),
    ]
}

/// Load `benches/fixtures/*.bin` raw PTY captures, chunked at PTY_CHUNK to mirror
/// the reader. A capture's escape stream assumes the geometry it was recorded
/// at, so a fixture is only included when the run geometry matches: name files
/// `<name>.<cols>x<rows>.bin` (untagged files are accepted at any size).
fn fixture_corpora(cols: u16, rows: u16) -> Vec<Corpus> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/fixtures");
    let mut corpora = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return corpora;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("bin"))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("fixture")
            .to_string();
        // Skip fixtures tagged with a geometry that doesn't match this run.
        if let Some((_, geo)) = stem.rsplit_once('.')
            && let Some((c, r)) = geo.split_once('x')
            && let (Ok(fc), Ok(fr)) = (c.parse::<u16>(), r.parse::<u16>())
            && (fc != cols || fr != rows)
        {
            continue;
        }
        let frames: Vec<Vec<u8>> = bytes.chunks(PTY_CHUNK).map(|c| c.to_vec()).collect();
        for status in [false, true] {
            corpora.push(Corpus {
                name: format!("fixture:{stem}{}", if status { "+status" } else { "" }),
                cols,
                rows,
                status_line: status,
                frames: frames.clone(),
            });
        }
    }
    corpora
}

struct Result {
    name: String,
    frames: usize,
    bytes_in: usize,
    best: Duration,
    out_len: usize,
    snap: perf::Snapshot,
    status: bool,
}

/// `legacy` reverts the renderer to its pre-optimization behavior so the same
/// binary measures unchanged-vs-optimized on identical input.
fn bench(c: &Corpus, legacy: bool) -> Result {
    perf::set_legacy(legacy);

    // Structural pass with perf on: counts FFI cell reads + row diff outcomes.
    perf::set_enabled(true);
    perf::reset();
    let mut h = RenderHarness::new(c.cols, c.rows, c.status_line);
    for f in &c.frames {
        h.feed(f);
    }
    let snap = perf::snapshot();
    let out_len = h.output_len();
    perf::set_enabled(false);

    // Timing passes with perf off: fresh harness each pass (VT state mutates),
    // take the best (least-noise) wall time.
    let mut best = Duration::MAX;
    for _ in 0..TIMING_PASSES {
        let mut h = RenderHarness::new(c.cols, c.rows, c.status_line);
        let t = Instant::now();
        for f in &c.frames {
            h.feed(f);
        }
        std::hint::black_box(h.output_len());
        best = best.min(t.elapsed());
    }

    perf::set_legacy(false);

    Result {
        name: c.name.clone(),
        frames: c.frames.len(),
        bytes_in: c.bytes_in(),
        best,
        out_len,
        snap,
        status: c.status_line,
    }
}

fn main() {
    let cols = env_u16("DEVENV_BENCH_COLS", DEFAULT_COLS);
    let rows = env_u16("DEVENV_BENCH_ROWS", DEFAULT_ROWS);
    let mut corpora = synthetic_corpora(cols, rows);
    corpora.extend(fixture_corpora(cols, rows));

    let us = |r: &Result| r.best.as_micros() as f64 / r.frames.max(1) as f64;
    let cells = |r: &Result| r.snap.cell_reads as f64 / r.frames.max(1) as f64;
    let amp = |r: &Result| {
        if r.bytes_in > 0 {
            r.out_len as f64 / r.bytes_in as f64
        } else {
            0.0
        }
    };

    println!(
        "\n{cols}x{rows} terminal · {TIMING_PASSES} timing passes · best-of · legacy(unchanged) vs optimized\n\
         {:<24} {:>4} {:>17} {:>8} {:>15} {:>15}",
        "corpus", "stat", "us/frame old→new", "speedup", "cells/fr o→n", "amplify o→n",
    );
    println!("{}", "─".repeat(92));

    for c in &corpora {
        let old = bench(c, true);
        let new = bench(c, false);
        let speedup = us(&old) / us(&new).max(1e-9);
        println!(
            "{:<24} {:>4} {:>7.1}→{:<7.1} {:>6.1}x {:>6.0}→{:<6.0} {:>5.1}x→{:.1}x",
            new.name,
            if new.status { "on" } else { "off" },
            us(&old),
            us(&new),
            speedup,
            cells(&old),
            cells(&new),
            amp(&old),
            amp(&new),
        );
    }
    println!();
}
