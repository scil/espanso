//! Prints where the caret of the foreground window is, and which method
//! found it. Start it, switch to the target application within the delay,
//! and put the caret in a text field.
//!
//!     cargo run -p popup-core --example caret_probe -- 3
use std::time::{Duration, Instant};

fn main() {
    popup_core::init_dpi();
    let delay: u64 = std::env::args().nth(1).and_then(|v| v.parse().ok()).unwrap_or(3);
    std::thread::sleep(Duration::from_secs(delay));
    let start = Instant::now();
    let caret = popup_core::caret::locate(Duration::from_millis(250));
    let took = start.elapsed();
    let (work, scale) = popup_core::place::monitor_at(caret.rect.left, caret.rect.top);
    println!(
        "source={:?} rect={:?} took_ms={:.1} scale={} work={:?}",
        caret.source,
        caret.rect,
        took.as_secs_f64() * 1000.0,
        scale,
        work
    );
}
