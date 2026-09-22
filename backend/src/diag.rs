//! One-line stderr diagnostics for the dev server.
//!
//! Tabs render their empty state while loading, so a slow request looks
//! exactly like a broken one. These lines show what the backend actually
//! does while someone clicks: which endpoint runs, in which reference
//! currency, how many lookups it needs, whether the range prewarm fired,
//! what Frankfurter answered, and how long it took.
//!
//! Privacy: amounts, names, descriptions, category names, and dates never
//! reach these lines. Counts, currency codes, durations, HTTP statuses,
//! and file paths only. `FINGUARD_FX_QUIET=1` silences them.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OP: AtomicU64 = AtomicU64::new(1);

/// Begin a logged operation, returning its id for correlating lines.
pub fn begin() -> u64 {
    NEXT_OP.fetch_add(1, Ordering::Relaxed)
}

fn quiet() -> bool {
    std::env::var_os("FINGUARD_FX_QUIET").is_some()
}

/// Emit one timestamped line to stderr.
pub fn event(op: u64, tag: &str, msg: impl std::fmt::Display) {
    if quiet() {
        return;
    }
    eprintln!(
        "[{}][op={op}][{tag}] {msg}",
        chrono::Local::now().format("%H:%M:%S"),
    );
}
