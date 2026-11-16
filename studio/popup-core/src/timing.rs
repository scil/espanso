use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Time since the operating system created this process. It includes the
/// loader and runtime start, so it matches what a user waits for.
pub fn since_process_start() -> Duration {
    #[cfg(windows)]
    {
        crate::win::since_process_start()
    }
    #[cfg(not(windows))]
    {
        Duration::ZERO
    }
}

/// Prints `timing <label> <ms>` to standard error when timing is on.
pub fn mark(label: &str) {
    if ENABLED.load(Ordering::Relaxed) {
        let ms = since_process_start().as_secs_f64() * 1000.0;
        eprintln!("timing {label} {ms:.1}");
        crate::log::line(&format!("timing {label}"));
    }
}

/// Prints `timing <label> <ms>` with the time since `from`, when timing is
/// on. The resident popup uses it to measure from the key press.
pub fn since(label: &str, from: std::time::Instant) {
    if ENABLED.load(Ordering::Relaxed) {
        let ms = from.elapsed().as_secs_f64() * 1000.0;
        eprintln!("timing {label} {ms:.1}");
        crate::log::line(&format!("timing {label} {ms:.1} ms"));
    }
}
