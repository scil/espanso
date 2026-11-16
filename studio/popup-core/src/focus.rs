//! Focus handling. espanso starts the popup from a background process, and
//! Windows does not let such a process take the foreground by default. The
//! popup remembers the window that had focus, takes focus for its own window,
//! and gives focus back before it types or exits.

/// An opaque window handle (`HWND` on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle(pub isize);

/// The window that has keyboard focus now.
pub fn foreground() -> Option<Handle> {
    #[cfg(windows)]
    {
        crate::win::foreground()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The first visible top-level window of this process. Call it on the UI
/// thread after the framework shows its window.
pub fn own_window() -> Option<Handle> {
    #[cfg(windows)]
    {
        crate::win::own_window()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Brings a window to the foreground even when this process does not own the
/// foreground.
pub fn bring_to_front(h: Handle) {
    #[cfg(windows)]
    crate::win::bring_to_front(h);
    #[cfg(not(windows))]
    let _ = h;
}

/// Gives focus back to `h`, then waits until it has it (at most 300 ms).
pub fn restore(h: Handle) {
    #[cfg(windows)]
    crate::win::restore(h);
    #[cfg(not(windows))]
    let _ = h;
}

/// Asks Windows 11 to draw rounded corners and a border for a frameless
/// window. Older Windows versions ignore it.
pub fn round_corners(h: Handle) {
    #[cfg(windows)]
    crate::win::round_corners(h);
    #[cfg(not(windows))]
    let _ = h;
}

/// How fast `type_text` types: `chunk` characters per `SendInput` call, with
/// `gap` between calls.
///
/// The default is one character every 5 ms. In the Windows 11 Notepad, two
/// characters per call (5 ms gap) or a 2 ms gap still dropped and repeated
/// characters, also for a 10-character date.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    pub chunk: usize,
    pub gap: std::time::Duration,
}

impl Default for Pace {
    fn default() -> Self {
        Pace { chunk: 1, gap: std::time::Duration::from_millis(5) }
    }
}

/// How `type_text` inserts long text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongText {
    /// Paste through the clipboard, marked so that clipboard managers and
    /// the Windows clipboard history skip it, then restore the old text.
    Paste,
    /// Type it as key events, like short text. The clipboard stays untouched.
    Type,
}

/// How `type_text` inserts text. Text is short if it has at most
/// `short_limit` UTF-16 units and no line break. Short text is always typed.
/// Long text follows `long`.
#[derive(Debug, Clone, Copy)]
pub struct Insert {
    pub short_limit: usize,
    pub long: LongText,
    pub pace: Pace,
}

impl Default for Insert {
    fn default() -> Self {
        Insert { short_limit: 32, long: LongText::Paste, pace: Pace::default() }
    }
}

static INSERT: std::sync::OnceLock<Insert> = std::sync::OnceLock::new();

/// Sets how `type_text` inserts text. The first call wins. `Args::parse`
/// calls it with the command line settings.
pub fn set_insert(how: Insert) {
    let _ = INSERT.set(how);
}

/// The settings that `type_text` uses.
pub fn insert_settings() -> Insert {
    INSERT.get().copied().unwrap_or_default()
}

/// Inserts text into the focused window, by the rules of `Insert`.
pub fn type_text(text: &str) {
    let how = insert_settings();
    let short = text.encode_utf16().count() <= how.short_limit && !text.contains(['\n', '\r']);
    if short || how.long == LongText::Type {
        type_keys(text, how.pace);
    } else {
        paste(text);
    }
}

/// Types text as key events at the given pace. It never uses the clipboard.
pub fn type_keys(text: &str, pace: Pace) {
    #[cfg(windows)]
    crate::win::type_text(text, pace);
    #[cfg(not(windows))]
    let _ = (text, pace);
}

/// Pastes text through the clipboard, hidden from clipboard history, then
/// restores the old clipboard text.
pub fn paste(text: &str) {
    #[cfg(windows)]
    crate::win::paste(text);
    #[cfg(not(windows))]
    let _ = text;
}

/// The clipboard text, if the clipboard holds text.
pub fn clipboard_text() -> Option<String> {
    #[cfg(windows)]
    {
        crate::win::clipboard_get()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Moves `h` to the screen rectangle `r` (physical pixels), puts it on top
/// and shows it without taking focus.
pub fn show_at(h: Handle, r: crate::Rect) {
    #[cfg(windows)]
    crate::win::show_at(h, r);
    #[cfg(not(windows))]
    let _ = (h, r);
}

/// Hides `h`.
pub fn hide(h: Handle) {
    #[cfg(windows)]
    crate::win::hide(h);
    #[cfg(not(windows))]
    let _ = h;
}

/// Marks `h` so that Windows never gives it focus, also when the user clicks
/// it. The popup of the resident mode uses it, like an input method window.
pub fn set_no_activate(h: Handle) {
    #[cfg(windows)]
    crate::win::set_no_activate(h);
    #[cfg(not(windows))]
    let _ = h;
}

/// Sends `n` Backspace key presses to the focused window.
pub fn send_backspaces(n: usize) {
    #[cfg(windows)]
    crate::win::send_backspaces(n);
    #[cfg(not(windows))]
    let _ = n;
}

/// Sends `n` Left arrow key presses to the focused window.
pub fn send_lefts(n: usize) {
    #[cfg(windows)]
    crate::win::send_lefts(n);
    #[cfg(not(windows))]
    let _ = n;
}

/// The foreground application: full program path and window title, for
/// espanso's per-application configurations.
pub fn foreground_app() -> crate::expand::App {
    #[cfg(windows)]
    {
        crate::expand::App {
            exec: crate::win::foreground_path(),
            title: crate::win::foreground_title(),
        }
    }
    #[cfg(not(windows))]
    {
        crate::expand::App::default()
    }
}

/// Whether a process with this file name runs, such as `espanso.exe`.
pub fn process_running(name: &str) -> bool {
    #[cfg(windows)]
    {
        crate::win::process_running(name)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

/// File name of the program that owns the foreground window, for the log.
pub fn foreground_exe() -> Option<String> {
    #[cfg(windows)]
    {
        crate::win::foreground_exe()
    }
    #[cfg(not(windows))]
    {
        None
    }
}
