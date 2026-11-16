//! Shared logic for the caret popup prototypes (`popup-slint`, `popup-gpui`).
//!
//! - [`caret`] finds the text caret on screen: UI Automation first, then the
//!   Win32 caret, then the focused element, then the mouse.
//! - [`place`] picks the popup position next to the caret, inside the
//!   monitor work area.
//! - [`focus`] remembers the window that had focus, takes focus for the
//!   popup, gives it back, and can type text into it.
//! - [`data`] loads snippets through `snip-core` and filters them in memory.
//! - [`hook`] watches the keyboard for the trigger and the keyword after it,
//!   for the resident input-method style popup.
//! - [`expand`] expands full triggers with espanso's matcher and renderer,
//!   as the espanso daemon does.
//! - [`log`] writes the resident popup's event log to a file.
//! - [`args`] and [`timing`] are the shared command line and timing log.

pub mod args;
pub mod caret;
pub mod data;
pub mod expand;
pub mod focus;
pub mod form;
pub mod hook;
pub mod log;
pub mod place;
pub mod timing;

#[cfg(windows)]
mod win;

/// A screen rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Ends the popup. It gives focus back to `previous`, then sends `text` to
/// standard output or types it (`type_text`), then exits the process. `None`
/// means the user cancelled, so nothing is sent.
pub fn finish(previous: Option<focus::Handle>, text: Option<String>, type_text: bool) -> ! {
    use std::io::Write;
    if let Some(h) = previous {
        focus::restore(h);
    }
    if let Some(t) = text {
        if type_text {
            focus::type_text(&t);
        } else {
            let mut out = std::io::stdout();
            let _ = out.write_all(t.as_bytes());
            let _ = out.flush();
        }
    }
    std::process::exit(0)
}

/// Call this first in `main`, before the UI framework creates a window, so
/// that caret and window coordinates are both physical pixels.
pub fn init_dpi() {
    #[cfg(windows)]
    win::init_dpi();
}
