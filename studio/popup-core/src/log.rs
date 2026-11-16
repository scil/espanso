//! Event log for the resident popup (`--log FILE`).
//!
//! A program with the Windows GUI subsystem has no console, so standard
//! error is lost when a shell starts it. The log goes to a file instead.
//! It records event names, reasons and counts. It never records typed text,
//! keywords or snippet text.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Opens the log file. Later calls do nothing.
pub fn open(path: &Path) -> std::io::Result<()> {
    let f = File::create(path)?;
    let _ = FILE.set(Mutex::new(f));
    Ok(())
}

pub fn is_on() -> bool {
    FILE.get().is_some()
}

/// Writes one line with the time since the process started, in ms.
pub fn line(text: &str) {
    if let Some(f) = FILE.get() {
        let ms = crate::timing::since_process_start().as_secs_f64() * 1000.0;
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{ms:10.1} {text}");
        }
    }
}
