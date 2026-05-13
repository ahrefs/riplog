//! SIGINT handling so long pipelines exit cleanly and still emit any
//! pending summary output (e.g. `--count`, `--list-keys`, `--count-by`).

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the SIGINT handler; checked in tight loops so we can exit cleanly
/// and still emit `--count` / `--list-keys` / `--count-by` summaries.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn install_signal_handler() {
    // Idempotent — `set_handler` errors if called twice. Ignore that path so
    // the binary stays usable when run as a library.
    let _ = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst));
}

#[inline]
pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}
