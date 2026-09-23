//! Wall clock since process start, printed only with LAUNCHR_TIMING=1 in the
//! environment. Startup latency is the whole point of a launcher, so the
//! breakdown stays in the binary rather than living in a scratch patch.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static ON: OnceLock<bool> = OnceLock::new();

pub fn init() {
    START.get_or_init(Instant::now);
    ON.get_or_init(|| std::env::var_os("LAUNCHR_TIMING").is_some());
}

/// Whether marks are printed, so a caller can skip setting up one that costs
/// something to observe.
pub fn enabled() -> bool {
    *ON.get_or_init(|| false)
}

pub fn mark(label: &str) {
    if enabled() {
        if let Some(start) = START.get() {
            eprintln!("launchr: {:7.1}ms  {label}", start.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
