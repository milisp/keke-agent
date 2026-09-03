//! Benchmark-only startup checkpoints, gated on `KEKE_STARTUP_TRACE`.
//!
//! Not a general profiler: it exists to answer one question — where does the
//! time between process start and first frame actually go — without adding
//! permanent overhead to a normal run.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Call once, as early as possible in `main`.
pub(crate) fn record_start() {
    let _ = START.set(Instant::now());
}

/// Print `label` and the elapsed time since [`record_start`] to stderr, if
/// `KEKE_STARTUP_TRACE` is set. A no-op otherwise, so it costs nothing in a
/// normal run beyond one env lookup.
pub(crate) fn mark(label: &str) {
    if std::env::var_os("KEKE_STARTUP_TRACE").is_none() {
        return;
    }
    if let Some(start) = START.get() {
        eprintln!("[startup] {:>7?} {label}", start.elapsed());
    }
}
