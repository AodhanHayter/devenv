//! Env-gated render-performance counters for the shell session.
//!
//! `devenv shell` does not pass the inner program's output straight through: it
//! mediates every PTY-output batch through a VT emulator and re-renders the
//! visible screen (see [`crate::session`]). That makes nested full-screen TUIs
//! (nvim, claude-code) feel laggy. This module measures where the per-batch time
//! and work actually go so optimizations can be quantified.
//!
//! Enable with `DEVENV_SHELL_PERF=1`. When disabled (the default) every hook is a
//! single relaxed atomic load that returns immediately, so the normal path pays
//! no measurable cost. When enabled, the session loop attributes time and work
//! across the pipeline stages (escape scan, VT write, render, status line) and
//! prints a summary to stderr when the shell exits.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// When set, the renderer reverts to its pre-optimization behavior: read back
/// every row each frame (no dirty-tracking skip), and draw the status line every
/// frame including in alt-screen (no skip-if-unchanged, no suppression). Lets the
/// benchmark produce a true before/after on identical inputs in one binary.
/// Also settable at runtime via `DEVENV_SHELL_LEGACY_RENDER`.
static LEGACY: AtomicBool = AtomicBool::new(false);

struct Counters {
    /// `PtyOutput` batches processed (one render per batch).
    frames: AtomicU64,
    /// Raw PTY chunks fed into the pipeline, including chunks coalesced into a
    /// batch via `try_recv`. `chunks / frames` is the coalescing ratio.
    chunks: AtomicU64,
    /// Bytes read from the PTY master (what the inner program emitted).
    bytes_in: AtomicU64,
    /// Bytes written to the real terminal (what we re-emit). `bytes_out /
    /// bytes_in` is the output amplification.
    bytes_out: AtomicU64,
    /// `write()` calls reaching the real terminal fd, including BufWriter
    /// auto-flushes when its buffer fills mid-frame.
    writes: AtomicU64,
    /// Explicit `flush()` calls (one per frame is the intended steady state).
    flushes: AtomicU64,
    /// libghostty cell reads via `cells_in_row` — the per-frame FFI cost.
    cell_reads: AtomicU64,
    /// Rows re-serialized to the terminal because the diff saw a change.
    rows_redrawn: AtomicU64,
    /// Rows the diff skipped (still paid the cell readback to discover that).
    rows_skipped: AtomicU64,
    /// Status-line draws emitted.
    status_draws: AtomicU64,
    /// Status-line draws skipped (content unchanged) — 0 until that optimization
    /// lands; lets a before/after show the win.
    status_skipped: AtomicU64,
    scan_ns: AtomicU64,
    vt_ns: AtomicU64,
    render_ns: AtomicU64,
    status_ns: AtomicU64,
    /// Wall time the session spent with the event loop active.
    session_ns: AtomicU64,
}

static C: Counters = Counters {
    frames: AtomicU64::new(0),
    chunks: AtomicU64::new(0),
    bytes_in: AtomicU64::new(0),
    bytes_out: AtomicU64::new(0),
    writes: AtomicU64::new(0),
    flushes: AtomicU64::new(0),
    cell_reads: AtomicU64::new(0),
    rows_redrawn: AtomicU64::new(0),
    rows_skipped: AtomicU64::new(0),
    status_draws: AtomicU64::new(0),
    status_skipped: AtomicU64::new(0),
    scan_ns: AtomicU64::new(0),
    vt_ns: AtomicU64::new(0),
    render_ns: AtomicU64::new(0),
    status_ns: AtomicU64::new(0),
    session_ns: AtomicU64::new(0),
};

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0" && v != "false")
        .unwrap_or(false)
}

/// Read `DEVENV_SHELL_PERF` / `DEVENV_SHELL_LEGACY_RENDER` and arm the
/// corresponding flags if set to a truthy value.
pub fn init_from_env() {
    ENABLED.store(env_truthy("DEVENV_SHELL_PERF"), Relaxed);
    LEGACY.store(env_truthy("DEVENV_SHELL_LEGACY_RENDER"), Relaxed);
}

/// Force the profiler on/off (used by tests and benches).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Force pre-optimization rendering on/off (benches drive this for before/after).
pub fn set_legacy(on: bool) {
    LEGACY.store(on, Relaxed);
}

#[inline]
pub fn legacy() -> bool {
    LEGACY.load(Relaxed)
}

/// Structural per-run counters, read by benches to report the work done
/// independent of wall-clock timing.
#[derive(Clone, Copy, Default)]
pub struct Snapshot {
    pub frames: u64,
    pub cell_reads: u64,
    pub rows_redrawn: u64,
    pub rows_skipped: u64,
}

/// Zero all counters. Used by benches between corpora.
pub fn reset() {
    for c in [
        &C.frames,
        &C.chunks,
        &C.bytes_in,
        &C.bytes_out,
        &C.writes,
        &C.flushes,
        &C.cell_reads,
        &C.rows_redrawn,
        &C.rows_skipped,
        &C.status_draws,
        &C.status_skipped,
        &C.scan_ns,
        &C.vt_ns,
        &C.render_ns,
        &C.status_ns,
        &C.session_ns,
    ] {
        c.store(0, Relaxed);
    }
}

/// Read the structural counters.
pub fn snapshot() -> Snapshot {
    Snapshot {
        frames: C.frames.load(Relaxed),
        cell_reads: C.cell_reads.load(Relaxed),
        rows_redrawn: C.rows_redrawn.load(Relaxed),
        rows_skipped: C.rows_skipped.load(Relaxed),
    }
}

#[inline]
pub fn record_batch(bytes_in: usize, chunks: usize) {
    if !enabled() {
        return;
    }
    C.frames.fetch_add(1, Relaxed);
    C.chunks.fetch_add(chunks as u64, Relaxed);
    C.bytes_in.fetch_add(bytes_in as u64, Relaxed);
}

#[inline]
pub fn record_cell_reads(n: usize) {
    if !enabled() {
        return;
    }
    C.cell_reads.fetch_add(n as u64, Relaxed);
}

#[inline]
pub fn record_row(redrawn: bool) {
    if !enabled() {
        return;
    }
    if redrawn {
        C.rows_redrawn.fetch_add(1, Relaxed);
    } else {
        C.rows_skipped.fetch_add(1, Relaxed);
    }
}

#[inline]
pub fn record_status(drawn: bool) {
    if !enabled() {
        return;
    }
    if drawn {
        C.status_draws.fetch_add(1, Relaxed);
    } else {
        C.status_skipped.fetch_add(1, Relaxed);
    }
}

#[inline]
pub fn add_scan(d: Duration) {
    C.scan_ns.fetch_add(d.as_nanos() as u64, Relaxed);
}

#[inline]
pub fn add_vt(d: Duration) {
    C.vt_ns.fetch_add(d.as_nanos() as u64, Relaxed);
}

#[inline]
pub fn add_render(d: Duration) {
    C.render_ns.fetch_add(d.as_nanos() as u64, Relaxed);
}

#[inline]
pub fn add_status(d: Duration) {
    C.status_ns.fetch_add(d.as_nanos() as u64, Relaxed);
}

#[inline]
pub fn add_session(d: Duration) {
    C.session_ns.fetch_add(d.as_nanos() as u64, Relaxed);
}

/// A `Write` wrapper placed between the BufWriter and the real terminal fd so we
/// count the bytes and `write()`/`flush()` syscalls actually reaching the
/// terminal — the true output amplification, including mid-frame auto-flushes.
pub struct CountingWriter<W> {
    inner: W,
}

impl<W: Write> CountingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if enabled() {
            C.bytes_out.fetch_add(n as u64, Relaxed);
            C.writes.fetch_add(1, Relaxed);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if enabled() {
            C.flushes.fetch_add(1, Relaxed);
        }
        self.inner.flush()
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Print the accumulated counters to stderr. No-op when disabled or when no
/// frames were processed (e.g. a shell that exited immediately).
pub fn dump() {
    if !enabled() {
        return;
    }
    let frames = C.frames.load(Relaxed);
    if frames == 0 {
        return;
    }
    let chunks = C.chunks.load(Relaxed);
    let bytes_in = C.bytes_in.load(Relaxed);
    let bytes_out = C.bytes_out.load(Relaxed);
    let writes = C.writes.load(Relaxed);
    let flushes = C.flushes.load(Relaxed);
    let cell_reads = C.cell_reads.load(Relaxed);
    let rows_redrawn = C.rows_redrawn.load(Relaxed);
    let rows_skipped = C.rows_skipped.load(Relaxed);
    let status_draws = C.status_draws.load(Relaxed);
    let status_skipped = C.status_skipped.load(Relaxed);
    let scan_ms = C.scan_ns.load(Relaxed) as f64 / 1e6;
    let vt_ms = C.vt_ns.load(Relaxed) as f64 / 1e6;
    let render_ms = C.render_ns.load(Relaxed) as f64 / 1e6;
    let status_ms = C.status_ns.load(Relaxed) as f64 / 1e6;
    let session_ms = C.session_ns.load(Relaxed) as f64 / 1e6;

    let stage_total = scan_ms + vt_ms + render_ms + status_ms;
    let pct = |ms: f64| {
        if stage_total > 0.0 {
            ms / stage_total * 100.0
        } else {
            0.0
        }
    };
    let amplification = if bytes_in > 0 {
        bytes_out as f64 / bytes_in as f64
    } else {
        0.0
    };
    let rows_total = rows_redrawn + rows_skipped;
    let skip_pct = if rows_total > 0 {
        rows_skipped as f64 / rows_total as f64 * 100.0
    } else {
        0.0
    };
    let coalesce = if frames > 0 {
        chunks as f64 / frames as f64
    } else {
        0.0
    };
    let in_throughput = if session_ms > 0.0 {
        bytes_in as f64 / (session_ms / 1000.0) / 1024.0 / 1024.0
    } else {
        0.0
    };

    let mut out = String::new();
    out.push_str("\n┌─ devenv shell render perf ─────────────────────────────────\n");
    out.push_str(&format!(
        "│ frames(batches): {frames}   chunks: {chunks}   coalesce: {coalesce:.1} chunks/frame\n"
    ));
    out.push_str(&format!(
        "│ bytes in:  {:>10}   out: {:>10}   amplification: {amplification:.1}x\n",
        human_bytes(bytes_in),
        human_bytes(bytes_out),
    ));
    out.push_str(&format!(
        "│ terminal writes: {writes}   explicit flushes: {flushes}  ({:.1} writes/frame)\n",
        if frames > 0 {
            writes as f64 / frames as f64
        } else {
            0.0
        },
    ));
    out.push_str(&format!(
        "│ FFI cell reads: {cell_reads}   ({:.0}/frame)\n",
        if frames > 0 {
            cell_reads as f64 / frames as f64
        } else {
            0.0
        },
    ));
    out.push_str(&format!(
        "│ rows redrawn: {rows_redrawn}   skipped: {rows_skipped}  ({skip_pct:.0}% skipped but still read)\n"
    ));
    out.push_str(&format!(
        "│ status draws: {status_draws}   skipped: {status_skipped}\n"
    ));
    out.push_str("│ ── stage time (active processing) ─────────────────────────\n");
    out.push_str(&format!(
        "│   escape scan : {scan_ms:8.1} ms  {:4.1}%\n",
        pct(scan_ms)
    ));
    out.push_str(&format!(
        "│   vt_write    : {vt_ms:8.1} ms  {:4.1}%\n",
        pct(vt_ms)
    ));
    out.push_str(&format!(
        "│   render      : {render_ms:8.1} ms  {:4.1}%\n",
        pct(render_ms)
    ));
    out.push_str(&format!(
        "│   status line : {status_ms:8.1} ms  {:4.1}%\n",
        pct(status_ms)
    ));
    out.push_str(&format!(
        "│ session wall: {session_ms:.0} ms   input throughput: {in_throughput:.1} MiB/s\n"
    ));
    out.push_str("└────────────────────────────────────────────────────────────\n");

    let _ = io::stderr().write_all(out.as_bytes());
}
