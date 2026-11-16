use std::time::Duration;

use crate::Rect;

/// Where the caret position came from, most precise first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// UI Automation `TextPattern2::GetCaretRange`.
    UiaCaret,
    /// UI Automation `TextPattern::GetSelection`.
    UiaSelection,
    /// Win32 system caret from `GetGUIThreadInfo`.
    Win32Caret,
    /// Bounding box of the focused UI Automation element.
    UiaFocus,
    /// Mouse pointer. Used when nothing else works.
    Mouse,
}

#[derive(Debug, Clone, Copy)]
pub struct Caret {
    pub rect: Rect,
    pub source: Source,
}

impl Caret {
    /// True when the rectangle is the caret itself, not an approximation.
    pub fn is_precise(&self) -> bool {
        matches!(
            self.source,
            Source::UiaCaret | Source::UiaSelection | Source::Win32Caret
        )
    }
}

/// Finds the caret of the foreground window. UI Automation runs on its own
/// thread and gets `uia_timeout`, because some applications answer slowly.
pub fn locate(uia_timeout: Duration) -> Caret {
    #[cfg(windows)]
    {
        crate::win::locate_caret(uia_timeout)
    }
    #[cfg(not(windows))]
    {
        let _ = uia_timeout;
        Caret {
            rect: Rect { left: 100, top: 100, right: 101, bottom: 120 },
            source: Source::Mouse,
        }
    }
}
