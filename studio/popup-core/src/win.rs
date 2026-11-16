//! Windows implementation of caret lookup, monitors, focus and typing.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayUnaccessData,
};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, GetProcessTimes,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomationTextPattern, IUIAutomationTextPattern2,
    IUIAutomationTextRange, TextPatternRangeEndpoint_Start, TextUnit_Character,
    UIA_TextPattern2Id, UIA_TextPatternId,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumThreadWindows, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo,
    GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow, ShowWindow,
    GUITHREADINFO, SW_SHOW,
};

use crate::caret::{Caret, Source};
use crate::focus::Handle;
use crate::Rect;

pub fn init_dpi() {
    // Fails when the manifest or a framework set it already. Both are fine.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn since_process_start() -> Duration {
    unsafe {
        let (mut created, mut a, mut b, mut c) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if GetProcessTimes(GetCurrentProcess(), &mut created, &mut a, &mut b, &mut c).is_err() {
            return Duration::ZERO;
        }
        let now = GetSystemTimeAsFileTime();
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        // FILETIME counts 100 ns steps.
        Duration::from_nanos(t(now).saturating_sub(t(created)) * 100)
    }
}

// ---------------------------------------------------------------- caret

/// Result of the UI Automation thread: a precise caret, and the focused
/// element's box as a weaker hint.
#[derive(Default)]
struct UiaResult {
    caret: Option<(Rect, Source)>,
    focus: Option<Rect>,
}

pub fn locate_caret(uia_timeout: Duration) -> Caret {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(uia_lookup());
    });
    let uia = rx.recv_timeout(uia_timeout).unwrap_or_default();

    if let Some((rect, source)) = uia.caret {
        return Caret { rect, source };
    }
    if let Some(rect) = win32_caret() {
        return Caret { rect, source: Source::Win32Caret };
    }
    if let Some(r) = uia.focus {
        // Use the element's bottom-left corner as a 1 px wide "caret".
        let rect = Rect { left: r.left, top: r.bottom - 1, right: r.left + 1, bottom: r.bottom };
        return Caret { rect, source: Source::UiaFocus };
    }
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    Caret {
        rect: Rect { left: p.x, top: p.y, right: p.x + 1, bottom: p.y + 20 },
        source: Source::Mouse,
    }
}

fn uia_lookup() -> UiaResult {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            crate::timing::mark(&format!("uia-error CoInitializeEx {e}"));
            return UiaResult::default();
        }
        let out = uia_lookup_inner().unwrap_or_else(|e| {
            crate::timing::mark(&format!("uia-error {e}"));
            UiaResult::default()
        });
        CoUninitialize();
        out
    }
}

unsafe fn uia_lookup_inner() -> windows::core::Result<UiaResult> {
    let uia: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
    let el = uia.GetFocusedElement()?;
    let mut out = UiaResult::default();

    if let Ok(p2) = el.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) {
        let mut active = BOOL(0);
        if let Ok(range) = p2.GetCaretRange(&mut active) {
            if let Some(r) = caret_rect_of(&range) {
                out.caret = Some((r, Source::UiaCaret));
                return Ok(out);
            }
        }
    }
    if let Ok(p) = el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
        if let Ok(sel) = p.GetSelection() {
            if sel.Length().unwrap_or(0) > 0 {
                if let Ok(range) = sel.GetElement(0) {
                    if let Some(r) = caret_rect_of(&range) {
                        out.caret = Some((r, Source::UiaSelection));
                        return Ok(out);
                    }
                }
            }
        }
    }
    if let Ok(r) = el.CurrentBoundingRectangle() {
        if r.right > r.left && r.bottom > r.top {
            out.focus = Some(from_rect(r));
        }
    }
    Ok(out)
}

/// The caret box of a (usually empty) text range. An empty range often has
/// no box, so this also tries the next character and the previous one.
unsafe fn caret_rect_of(range: &IUIAutomationTextRange) -> Option<Rect> {
    if let Some(r) = first_rect(range) {
        return Some(Rect { right: r.left + 1, ..r });
    }
    if let Ok(next) = range.Clone() {
        if next.ExpandToEnclosingUnit(TextUnit_Character).is_ok() {
            if let Some(r) = first_rect(&next) {
                return Some(Rect { right: r.left + 1, ..r });
            }
        }
    }
    if let Ok(prev) = range.Clone() {
        let mut moved = 0;
        if prev
            .MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -1)
            .map(|m| moved = m)
            .is_ok()
            && moved != 0
        {
            if let Some(r) = first_rect(&prev) {
                // The caret sits after the previous character.
                return Some(Rect { left: r.right - 1, ..r });
            }
        }
    }
    None
}

/// First rectangle of a text range, from the flat array of x, y, w, h.
unsafe fn first_rect(range: &IUIAutomationTextRange) -> Option<Rect> {
    let psa: *mut SAFEARRAY = range.GetBoundingRectangles().ok()?;
    if psa.is_null() {
        return None;
    }
    let mut out = None;
    let lo = SafeArrayGetLBound(psa, 1).unwrap_or(0);
    let hi = SafeArrayGetUBound(psa, 1).unwrap_or(-1);
    if hi - lo + 1 >= 4 {
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        if SafeArrayAccessData(psa, &mut data).is_ok() {
            let v = std::slice::from_raw_parts(data as *const f64, 4);
            let (x, y, w, h) = (v[0], v[1], v[2], v[3]);
            if h > 0.0 {
                out = Some(Rect {
                    left: x.round() as i32,
                    top: y.round() as i32,
                    right: (x + w).round() as i32,
                    bottom: (y + h).round() as i32,
                });
            }
            let _ = SafeArrayUnaccessData(psa);
        }
    }
    let _ = SafeArrayDestroy(psa);
    out
}

fn win32_caret() -> Option<Rect> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return None;
        }
        let tid = GetWindowThreadProcessId(fg, None);
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(tid, &mut gti).ok()?;
        if gti.hwndCaret.is_invalid() {
            return None;
        }
        let rc = gti.rcCaret;
        if rc.bottom <= rc.top {
            return None;
        }
        let mut a = POINT { x: rc.left, y: rc.top };
        let mut b = POINT { x: rc.right, y: rc.bottom };
        if !ClientToScreen(gti.hwndCaret, &mut a).as_bool()
            || !ClientToScreen(gti.hwndCaret, &mut b).as_bool()
        {
            return None;
        }
        Some(Rect { left: a.x, top: a.y, right: b.x.max(a.x + 1), bottom: b.y })
    }
}

fn from_rect(r: RECT) -> Rect {
    Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

// ---------------------------------------------------------------- monitors

pub fn monitor_at(x: i32, y: i32) -> (Rect, f32) {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work = if GetMonitorInfoW(mon, &mut info).as_bool() {
            from_rect(info.rcWork)
        } else {
            Rect { left: 0, top: 0, right: 1920, bottom: 1080 }
        };
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (work, dx as f32 / 96.0)
    }
}

// ---------------------------------------------------------------- focus

fn hwnd(h: Handle) -> HWND {
    HWND(h.0 as *mut std::ffi::c_void)
}

pub fn foreground() -> Option<Handle> {
    let h = unsafe { GetForegroundWindow() };
    (!h.is_invalid()).then_some(Handle(h.0 as isize))
}

pub fn is_own(h: Handle) -> bool {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd(h), Some(&mut pid));
    }
    pid == unsafe { GetCurrentProcessId() }
}

pub fn own_window() -> Option<Handle> {
    unsafe extern "system" fn each(h: HWND, out: LPARAM) -> BOOL {
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == GetCurrentProcessId() && IsWindowVisible(h).as_bool() {
            *(out.0 as *mut isize) = h.0 as isize;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut found: isize = 0;
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(each),
            LPARAM(&mut found as *mut isize as isize),
        );
    }
    (found != 0).then_some(Handle(found))
}

pub fn bring_to_front(h: Handle) {
    unsafe {
        let w = hwnd(h);
        if GetForegroundWindow() == w {
            return;
        }
        // An input event from this process lifts the foreground lock. An
        // empty mouse event (no move, no button) has no other effect. An Alt
        // tap would also work, but it opens the menu access keys of the
        // previous window (seen in Notepad).
        let mi = MOUSEINPUT { dwExtraInfo: crate::hook::OWN_INPUT, ..Default::default() };
        let nudge = INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi } };
        send(&[nudge]);
        let _ = ShowWindow(w, SW_SHOW);
        let _ = BringWindowToTop(w);
        let _ = SetForegroundWindow(w);
    }
}

pub fn restore(h: Handle) {
    unsafe {
        let w = hwnd(h);
        let _ = SetForegroundWindow(w);
        let start = Instant::now();
        while GetForegroundWindow() != w && start.elapsed() < Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub fn round_corners(h: Handle) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    unsafe {
        let pref = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd(h),
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&pref) as u32,
        );
    }
}

/// Inserts text into the focused window by typing it. It never uses the
/// clipboard: a paste would put the snippet into clipboard managers and the
/// Windows clipboard history, and a restore after it brings back text only.
///
/// Characters go as Unicode key events and line breaks as Enter key presses.
/// The text goes in chunks with a pause between them: one burst of a long
/// snippet came out garbled in the Windows 11 Notepad (characters dropped
/// and repeated).
pub fn type_text(text: &str, pace: crate::focus::Pace) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_UNICODE, VK_RETURN};
    let unicode = |unit: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | flags,
                time: 0,
                dwExtraInfo: crate::hook::OWN_INPUT,
            },
        },
    };
    let mut chunks: Vec<Vec<INPUT>> = vec![Vec::new()];
    let mut in_chunk = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let inputs = chunks.last_mut().unwrap();
        match c {
            // "\r\n" and a lone "\r" are one line break.
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                inputs.push(key(VK_RETURN, KEYBD_EVENT_FLAGS(0)));
                inputs.push(key(VK_RETURN, KEYEVENTF_KEYUP));
            }
            _ => {
                let mut buf = [0u16; 2];
                for &unit in c.encode_utf16(&mut buf).iter() {
                    inputs.push(unicode(unit, KEYBD_EVENT_FLAGS(0)));
                    inputs.push(unicode(unit, KEYEVENTF_KEYUP));
                }
            }
        }
        in_chunk += 1;
        if in_chunk == pace.chunk.max(1) {
            chunks.push(Vec::new());
            in_chunk = 0;
        }
    }
    let (mut sent, mut total) = (0, 0);
    for (i, inputs) in chunks.iter().enumerate() {
        if i > 0 && !pace.gap.is_zero() {
            std::thread::sleep(pace.gap);
        }
        sent += send(inputs);
        total += inputs.len();
    }
    crate::log::line(&format!(
        "type: {} chars in {} chunks, sent {sent} of {total} events",
        text.chars().count(),
        chunks.iter().filter(|c| !c.is_empty()).count()
    ));
}

/// Pastes text with Ctrl+V. The clipboard data carries four marker
/// formats, so clipboard managers skip it (`Clipboard Viewer Ignore` for
/// Ditto, `ExcludeClipboardContentFromMonitorProcessing` for others), and the Windows
/// clipboard history and cloud clipboard do not keep it
/// (`CanIncludeInClipboardHistory` and `CanUploadToCloudClipboard` set to
/// 0). After 300 ms the old clipboard text comes back, with the same
/// markers. Only text comes back: an old image or file list is lost.
pub fn paste(text: &str) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_V};
    let text = text.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n");
    let old = clipboard_get();
    if !clipboard_set(Some(&text)) {
        crate::log::line("paste: cannot open the clipboard");
        return;
    }
    // Ctrl goes down in a call of its own. In one burst, Notepad got the V
    // without the Ctrl on the test machine.
    let mut sent = send(&[key(VK_CONTROL, KEYBD_EVENT_FLAGS(0))]);
    std::thread::sleep(Duration::from_millis(30));
    sent += send(&[key(VK_V, KEYBD_EVENT_FLAGS(0)), key(VK_V, KEYEVENTF_KEYUP)]);
    std::thread::sleep(Duration::from_millis(30));
    sent += send(&[key(VK_CONTROL, KEYEVENTF_KEYUP)]);
    crate::log::line(&format!(
        "paste: {} chars, sent {sent} of 4 events, old clipboard text {}",
        text.chars().count(),
        old.is_some()
    ));
    // The target reads the clipboard when it handles the Ctrl+V.
    std::thread::sleep(Duration::from_millis(300));
    clipboard_set(old.as_deref());
}

const CF_UNICODETEXT: u32 = 13;

/// The clipboard text, if the clipboard holds text.
pub fn clipboard_get() -> Option<String> {
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData};
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    if !open_clipboard() {
        return None;
    }
    let out = unsafe {
        GetClipboardData(CF_UNICODETEXT).ok().and_then(|h| {
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if p.is_null() {
                return None;
            }
            let units = GlobalSize(g) / 2;
            let slice = std::slice::from_raw_parts(p, units);
            let len = slice.iter().position(|&u| u == 0).unwrap_or(units);
            let s = String::from_utf16_lossy(&slice[..len]);
            let _ = GlobalUnlock(g);
            Some(s)
        })
    };
    unsafe {
        let _ = CloseClipboard();
    }
    out
}

/// Replaces the clipboard with `text` (or empties it for `None`), marked so
/// that clipboard history skips it. Returns false if the clipboard stays
/// locked by another program.
fn clipboard_set(text: Option<&str>) -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    if !open_clipboard() {
        return false;
    }
    unsafe {
        let _ = EmptyClipboard();
        if let Some(text) = text {
            let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            if let Some(g) = global_copy(bytes_of(&units)) {
                let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(g.0)));
            }
            let zero = 0u32.to_ne_bytes();
            for name in [
                // Ditto skips clipboard data that has this format. It
                // recorded pastes that had only the formats below.
                w!("Clipboard Viewer Ignore"),
                w!("ExcludeClipboardContentFromMonitorProcessing"),
                w!("CanIncludeInClipboardHistory"),
                w!("CanUploadToCloudClipboard"),
            ] {
                let format = RegisterClipboardFormatW(name);
                if let (true, Some(g)) = (format != 0, global_copy(&zero)) {
                    let _ = SetClipboardData(format, Some(HANDLE(g.0)));
                }
            }
        }
        let _ = CloseClipboard();
    }
    true
}

fn bytes_of(units: &[u16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(units.as_ptr() as *const u8, units.len() * 2) }
}

/// Copies bytes into a movable global memory block, as the clipboard needs.
fn global_copy(bytes: &[u8]) -> Option<windows::Win32::Foundation::HGLOBAL> {
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    unsafe {
        let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
        let p = GlobalLock(g) as *mut u8;
        if p.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(g);
        Some(g)
    }
}

/// Opens the clipboard. Another program can hold it for a moment, so it
/// tries 10 times, 10 ms apart.
fn open_clipboard() -> bool {
    use windows::Win32::System::DataExchange::OpenClipboard;
    for _ in 0..10 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn key(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: crate::hook::OWN_INPUT },
        },
    }
}

/// Sends input events. Returns how many Windows accepted. Fewer than sent
/// means another program blocked them (for example a window with higher
/// integrity, such as an administrator window, has focus).
fn send(inputs: &[INPUT]) -> u32 {
    if inputs.is_empty() {
        return 0;
    }
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) }
}

pub fn show_at(h: Handle, r: Rect) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SW_SHOWNOACTIVATE,
    };
    unsafe {
        let w = hwnd(h);
        let _ = SetWindowPos(
            w,
            Some(HWND_TOPMOST),
            r.left,
            r.top,
            r.width(),
            r.height(),
            SWP_NOACTIVATE,
        );
        // Show with `ShowWindow`, not with `SWP_SHOWWINDOW`: only
        // `ShowWindow` sends `WM_SHOWWINDOW`, and gpui draws a window that
        // becomes visible when it gets that message.
        let _ = ShowWindow(w, SW_SHOWNOACTIVATE);
    }
}

/// Moves and sizes `h` (physical pixels) without a change of z-order or
/// focus. A hidden window stays hidden.
pub fn move_window(h: Handle, r: Rect) {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
    unsafe {
        let _ = SetWindowPos(hwnd(h), None, r.left, r.top, r.width(), r.height(), SWP_NOACTIVATE | SWP_NOZORDER);
    }
}

/// The screen rectangle of `h` in physical pixels, frame included.
pub fn window_rect(h: Handle) -> Option<Rect> {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd(h), &mut r).ok().map(|_| from_rect(r)) }
}

/// The mouse position in physical pixels.
pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

pub fn hide(h: Handle) {
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    unsafe {
        let _ = ShowWindow(hwnd(h), SW_HIDE);
    }
}

pub fn set_no_activate(h: Handle) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    };
    unsafe {
        let w = hwnd(h);
        let style = GetWindowLongPtrW(w, GWL_EXSTYLE);
        SetWindowLongPtrW(w, GWL_EXSTYLE, style | WS_EX_NOACTIVATE.0 as isize);
    }
}

pub fn send_backspaces(n: usize) {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_BACK;
    send_taps(VK_BACK, n, "backspace");
}

pub fn send_lefts(n: usize) {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_LEFT;
    send_taps(VK_LEFT, n, "left");
}

/// Presses and releases a key `n` times, at the typing pace of
/// [`crate::focus::insert_settings`]. One burst of 8 Backspaces erased more
/// than 8 characters in the Windows 11 Notepad.
fn send_taps(vk: VIRTUAL_KEY, n: usize, name: &str) {
    if n == 0 {
        return;
    }
    let pace = crate::focus::insert_settings().pace;
    let (mut sent, mut total) = (0, 0);
    let mut done = 0;
    // The pause also comes after the last tap: the first typed character
    // right after a Backspace burst came out wrong in Notepad.
    while done < n {
        let count = pace.chunk.max(1).min(n - done);
        let mut inputs = Vec::with_capacity(count * 2);
        for _ in 0..count {
            inputs.push(key(vk, KEYBD_EVENT_FLAGS(0)));
            inputs.push(key(vk, KEYEVENTF_KEYUP));
        }
        sent += send(&inputs);
        total += inputs.len();
        done += count;
        if !pace.gap.is_zero() {
            std::thread::sleep(pace.gap);
        }
    }
    crate::log::line(&format!("{name}: sent {sent} of {total} events"));
}

/// File name of the program that owns the foreground window.
pub fn foreground_exe() -> Option<String> {
    let path = foreground_path()?;
    path.rsplit(['\\', '/']).next().map(str::to_string)
}

/// Full path of the program that owns the foreground window.
pub fn foreground_path() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n);
        let _ = CloseHandle(h);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// Title of the foreground window.
pub fn foreground_title() -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(GetForegroundWindow(), &mut buf) };
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
}

/// Whether a process with this file name runs, case-insensitive.
pub fn process_running(name: &str) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = false;
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(name) {
                found = true;
                break;
            }
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// Whether this process runs with administrator rights.
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Opens a folder in Explorer, or a file in its default program, through
/// `ShellExecuteW`. The existing Explorer process shows the folder in about
/// 0.25 s. Starting `explorer.exe <folder>` took about 2.4 s, because the new
/// process hands the folder to the running Explorer and exits.
pub fn shell_open(path: &std::path::Path) -> Result<(), String> {
    use windows::core::{w, HSTRING};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        // ShellExecute needs COM on the calling thread.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let r = ShellExecuteW(None, w!("open"), &HSTRING::from(path.as_os_str()), None, None, SW_SHOWNORMAL);
        // A value above 32 means success.
        if r.0 as isize > 32 {
            Ok(())
        } else {
            Err(format!("ShellExecuteW failed with code {}", r.0 as isize))
        }
    }
}
