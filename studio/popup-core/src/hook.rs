//! Input-method style trigger detection with low-level keyboard and mouse
//! hooks.
//!
//! The user types in the target application as usual. When the last typed
//! characters equal the trigger (for example `;;`), the hook becomes active
//! and reports [`HookEvent::Open`]. While it is active:
//!
//! - Printable characters go to the application and extend the keyword
//!   ([`HookEvent::Query`]).
//! - Backspace goes to the application and shortens the keyword. Backspace on
//!   an empty keyword cancels.
//! - Up and Down are swallowed and move the selection ([`HookEvent::Move`]).
//! - Enter and Tab are swallowed and choose the selection
//!   ([`HookEvent::Accept`]). With no results they cancel and go through.
//! - Escape is swallowed and cancels.
//! - Any other key, a key with Ctrl, Alt or Win, or a mouse click outside the
//!   popup cancels and goes through.
//! - A mouse click inside the popup is swallowed ([`HookEvent::Click`]).
//!
//! The hook ignores injected input, so the popup's own Backspace and paste
//! events and espanso's expansions never extend the keyword. Injected input
//! from another program closes an open popup: espanso expanded a trigger
//! that the user typed in full, so the keyword at the caret is gone. For tests with a
//! tool that types through `SendInput`, `allow_injected` accepts injected
//! input, except the popup's own, which carries [`OWN_INPUT`].
//!
//! The hook also passes every key press it handles to the expander
//! ([`crate::expand`]) as an [`Input`], through the `on_input` callback of
//! [`start`]. A key that the popup swallows reaches the expander as
//! [`Input::Reset`], because the application never got it.
//!
//! While a window of this process has the focus (a form), the hook ignores
//! all input, because the user types into that window.
//!
//! The hook sees every key press. It keeps only the last few characters and
//! the current keyword in memory, and it logs nothing.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::expand::Input;
use crate::Rect;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    /// The trigger was typed.
    Open,
    /// The keyword after the trigger changed.
    Query(String),
    /// Up (-1) or Down (+1).
    Move(i32),
    /// A mouse button went down inside the popup. The hook swallows the
    /// click, so the target window keeps focus.
    Click,
    /// Enter or Tab with at least one result. The hook stays active until the
    /// UI calls [`Shared::deactivate`].
    Accept { keyword: String },
    /// The popup must close.
    Cancel,
}

/// State that the hook thread and the UI share.
#[derive(Default)]
pub struct Shared {
    active: AtomicBool,
    results: AtomicUsize,
    popup: Mutex<Option<Rect>>,
}

impl Shared {
    /// Number of results the popup shows now.
    pub fn set_results(&self, n: usize) {
        self.results.store(n, Ordering::Relaxed);
    }
    /// Screen rectangle of the popup, so that clicks inside it do not cancel.
    pub fn set_popup_rect(&self, r: Option<Rect>) {
        *self.popup.lock().unwrap() = r;
    }
    /// Whether a popup session is open.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    /// Ends the current session. The hook goes back to watching for the
    /// trigger.
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.set_popup_rect(None);
    }
}

/// `dwExtraInfo` of every input event that this crate sends, so that the
/// hook can tell them apart from other injected input.
pub const OWN_INPUT: usize = 0x5350_4F50;

static ALLOW_INJECTED: AtomicBool = AtomicBool::new(false);

/// Receives every key press for the expander.
pub type OnInput = Box<dyn Fn(Input) + Send>;

/// Starts the hook thread. Each event carries the time of the key press, so
/// the UI can measure its latency.
pub fn start(
    trigger: &str,
    shared: Arc<Shared>,
    allow_injected: bool,
    on_input: Option<OnInput>,
) -> Receiver<(Instant, HookEvent)> {
    let (tx, rx) = std::sync::mpsc::channel();
    ALLOW_INJECTED.store(allow_injected, Ordering::Relaxed);
    #[cfg(windows)]
    {
        let trigger: Vec<char> = trigger.chars().collect();
        std::thread::spawn(move || win::run(Machine::new(trigger, shared, tx, on_input)));
    }
    #[cfg(not(windows))]
    {
        let _ = (trigger, shared, tx, on_input);
    }
    rx
}

/// A key press as the state machine sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    Up,
    Down,
    Enter,
    Tab,
    Escape,
    /// Shift or Caps Lock alone. They never change the state.
    Modifier,
    /// Any other key, or any key with Ctrl, Alt or Win.
    Other,
}

/// What the hook does with a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Swallow,
}

/// The trigger and keyword state machine. It has no Windows code, so the
/// tests run on any platform.
pub struct Machine {
    trigger: Vec<char>,
    recent: Vec<char>,
    active: bool,
    keyword: String,
    shared: Arc<Shared>,
    tx: Sender<(Instant, HookEvent)>,
    on_input: Option<OnInput>,
}

impl Machine {
    pub fn new(
        trigger: Vec<char>,
        shared: Arc<Shared>,
        tx: Sender<(Instant, HookEvent)>,
        on_input: Option<OnInput>,
    ) -> Self {
        Machine {
            trigger,
            recent: Vec::new(),
            active: false,
            keyword: String::new(),
            shared,
            tx,
            on_input,
        }
    }

    fn pass_on(&self, input: Input) {
        if let Some(f) = &self.on_input {
            f(input);
        }
    }

    fn emit(&self, ev: HookEvent) {
        let _ = self.tx.send((Instant::now(), ev));
    }

    fn cancel(&mut self, reason: &str) {
        crate::log::line(&format!("hook cancel: {reason}"));
        self.active = false;
        self.keyword.clear();
        self.recent.clear();
        self.shared.deactivate();
        self.emit(HookEvent::Cancel);
    }

    /// Whether a session is open.
    pub fn is_active(&self) -> bool {
        self.active && self.shared.active.load(Ordering::Relaxed)
    }

    pub fn key(&mut self, key: Key) -> Verdict {
        let verdict = self.popup_key(key);
        let input = match (verdict, key) {
            (Verdict::Swallow, _) => Some(Input::Reset),
            (_, Key::Char(c)) => Some(Input::Char(c)),
            (_, Key::Backspace) => Some(Input::Backspace),
            (_, Key::Enter) => Some(Input::Char('\n')),
            (_, Key::Tab) => Some(Input::Char('\t')),
            (_, Key::Modifier) => None,
            (_, Key::Up | Key::Down | Key::Escape | Key::Other) => Some(Input::Reset),
        };
        if let Some(input) = input {
            self.pass_on(input);
        }
        verdict
    }

    fn popup_key(&mut self, key: Key) -> Verdict {
        // The UI may end the session (after an insert, or with no results).
        if self.active && !self.shared.active.load(Ordering::Relaxed) {
            self.active = false;
            self.keyword.clear();
            self.recent.clear();
        }
        if self.active {
            self.active_key(key)
        } else {
            self.idle_key(key);
            Verdict::Pass
        }
    }

    fn idle_key(&mut self, key: Key) {
        match key {
            Key::Char(c) => {
                self.recent.push(c);
                let n = self.trigger.len();
                if self.recent.len() > n {
                    self.recent.drain(..self.recent.len() - n);
                }
                if n > 0 && self.recent == self.trigger {
                    self.recent.clear();
                    self.keyword.clear();
                    self.active = true;
                    self.shared.set_results(0);
                    self.shared.active.store(true, Ordering::Relaxed);
                    crate::log::line("hook open");
                    self.emit(HookEvent::Open);
                }
            }
            Key::Backspace => {
                self.recent.pop();
            }
            Key::Modifier => {}
            _ => self.recent.clear(),
        }
    }

    fn active_key(&mut self, key: Key) -> Verdict {
        match key {
            Key::Char(c) => {
                self.keyword.push(c);
                self.emit(HookEvent::Query(self.keyword.clone()));
                Verdict::Pass
            }
            Key::Backspace => {
                if self.keyword.pop().is_some() {
                    self.emit(HookEvent::Query(self.keyword.clone()));
                } else {
                    self.cancel("backspace on empty keyword");
                }
                Verdict::Pass
            }
            Key::Up => {
                crate::log::line("hook move up");
                self.emit(HookEvent::Move(-1));
                Verdict::Swallow
            }
            Key::Down => {
                crate::log::line("hook move down");
                self.emit(HookEvent::Move(1));
                Verdict::Swallow
            }
            Key::Enter | Key::Tab => {
                if self.shared.results.load(Ordering::Relaxed) > 0 {
                    crate::log::line("hook accept");
                    self.emit(HookEvent::Accept { keyword: self.keyword.clone() });
                    Verdict::Swallow
                } else {
                    self.cancel("enter with no results");
                    Verdict::Pass
                }
            }
            Key::Escape => {
                self.cancel("escape");
                Verdict::Swallow
            }
            Key::Modifier => Verdict::Pass,
            Key::Other => {
                self.cancel("other key or chord");
                Verdict::Pass
            }
        }
    }

    /// Another program injected a key, for example espanso while it expands
    /// a trigger that the user typed in full. The text at the caret is no
    /// longer what the hook counted, so an open popup closes.
    pub fn foreign_input(&mut self) {
        // The text before the caret changed in a way that the expander cannot
        // see.
        self.pass_on(Input::Reset);
        if self.active && self.shared.active.load(Ordering::Relaxed) {
            self.cancel("injected input from another program");
        }
    }

    /// A mouse button went down at a screen point.
    pub fn click(&mut self, x: i32, y: i32) -> Verdict {
        self.pass_on(Input::Reset);
        if self.active && !self.shared.active.load(Ordering::Relaxed) {
            self.active = false;
            self.keyword.clear();
        }
        if !self.active {
            self.recent.clear();
            return Verdict::Pass;
        }
        let inside = self.shared.popup.lock().unwrap().is_some_and(|r| {
            x >= r.left && x < r.right && y >= r.top && y < r.bottom
        });
        if inside {
            self.emit(HookEvent::Click);
            Verdict::Swallow
        } else {
            self.cancel("click outside the popup");
            Verdict::Pass
        }
    }
}

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, GetKeyState, GetKeyboardLayout, ToUnicodeEx, VK_BACK, VK_CAPITAL,
        VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_PACKET,
        VK_RCONTROL, VK_RETURN, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_TAB, VK_UP,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW,
        GetWindowThreadProcessId, SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT,
        LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL,
        WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_RBUTTONDOWN,
        WM_RBUTTONUP, WM_SYSKEYDOWN,
    };

    use super::{Key, Machine, Verdict};

    thread_local! {
        static MACHINE: RefCell<Option<Machine>> = const { RefCell::new(None) };
        /// Keys whose key-down was swallowed. Their key-up is swallowed too.
        static SWALLOWED: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
        /// Button-up messages to swallow, because their button-down was.
        static SWALLOWED_BUTTONS: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
    }

    pub fn run(machine: Machine) {
        MACHINE.with(|m| *m.borrow_mut() = Some(machine));
        unsafe {
            let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0);
            let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0);
            if kb.is_err() || ms.is_err() {
                eprintln!("cannot install the keyboard or mouse hook");
                return;
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Whether the hook handles an input event. It ignores input to a window
    /// of this process, such as a form, which the user types into.
    fn wanted(injected: bool, extra: usize) -> bool {
        extra != super::OWN_INPUT
            && (!injected || super::ALLOW_INJECTED.load(std::sync::atomic::Ordering::Relaxed))
            && !own_foreground()
    }

    /// Whether a window of this process has the focus.
    fn own_foreground() -> bool {
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
            pid == windows::Win32::System::Threading::GetCurrentProcessId()
        }
    }

    fn down(vk: i32) -> bool {
        unsafe { GetAsyncKeyState(vk) < 0 }
    }

    unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
            if injected
                && kb.dwExtraInfo != super::OWN_INPUT
                && !super::ALLOW_INJECTED.load(std::sync::atomic::Ordering::Relaxed)
            {
                MACHINE.with(|m| {
                    if let Some(m) = m.borrow_mut().as_mut() {
                        m.foreign_input();
                    }
                });
            }
            if wanted(injected, kb.dwExtraInfo) {
                let is_down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
                let swallow = if is_down {
                    let key = classify(kb);
                    let verdict = MACHINE.with(|m| {
                        let mut m = m.borrow_mut();
                        let m = m.as_mut()?;
                        // Explain an "other key" close: the key code and
                        // the modifiers, never a character.
                        if key == Key::Other && m.is_active() {
                            crate::log::line(&format!(
                                "hook other key: vk 0x{:02X} scan 0x{:02X} flags 0x{:X} \
                                 ctrl {} alt {} lwin {} rwin {}",
                                kb.vkCode,
                                kb.scanCode,
                                kb.flags.0,
                                down(VK_CONTROL.0 as i32),
                                down(VK_MENU.0 as i32),
                                down(VK_LWIN.0 as i32),
                                down(VK_RWIN.0 as i32),
                            ));
                        }
                        Some(m.key(key))
                    })
                    .unwrap_or(Verdict::Pass);
                    if verdict == Verdict::Swallow {
                        SWALLOWED.with(|s| s.borrow_mut().insert(kb.vkCode));
                        true
                    } else {
                        false
                    }
                } else {
                    SWALLOWED.with(|s| s.borrow_mut().remove(&kb.vkCode))
                };
                if swallow {
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let msg = wparam.0 as u32;
            if wanted(ms.flags & LLMHF_INJECTED != 0, ms.dwExtraInfo) {
                let swallow = match msg {
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                        let verdict = MACHINE.with(|m| {
                            m.borrow_mut()
                                .as_mut()
                                .map(|m| m.click(ms.pt.x, ms.pt.y))
                                .unwrap_or(Verdict::Pass)
                        });
                        if verdict == Verdict::Swallow {
                            SWALLOWED_BUTTONS.with(|s| s.borrow_mut().insert(msg + 1));
                        }
                        verdict == Verdict::Swallow
                    }
                    // WM_xBUTTONUP is WM_xBUTTONDOWN + 1.
                    WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
                        SWALLOWED_BUTTONS.with(|s| s.borrow_mut().remove(&msg))
                    }
                    _ => false,
                };
                if swallow {
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    unsafe fn classify(kb: &KBDLLHOOKSTRUCT) -> Key {
        let vk = kb.vkCode as u16;
        let is = |k: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| k.0 == vk;
        if [VK_SHIFT, VK_LSHIFT, VK_RSHIFT, VK_CAPITAL].iter().any(|k| is(*k)) {
            return Key::Modifier;
        }
        if [VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|k| is(*k))
        {
            return Key::Other;
        }
        let chord = down(VK_CONTROL.0 as i32)
            || down(VK_MENU.0 as i32)
            || down(VK_LWIN.0 as i32)
            || down(VK_RWIN.0 as i32);
        if chord {
            return Key::Other;
        }
        if is(VK_BACK) {
            return Key::Backspace;
        }
        if is(VK_UP) {
            return Key::Up;
        }
        if is(VK_DOWN) {
            return Key::Down;
        }
        if is(VK_RETURN) {
            return Key::Enter;
        }
        if is(VK_TAB) {
            return Key::Tab;
        }
        if is(VK_ESCAPE) {
            return Key::Escape;
        }
        // A Unicode event from SendInput carries the character in the scan
        // code.
        if is(VK_PACKET) {
            return match char::from_u32(kb.scanCode) {
                Some(c) if !c.is_control() => Key::Char(c),
                _ => Key::Other,
            };
        }
        match to_char(kb) {
            Some(c) if !c.is_control() => Key::Char(c),
            _ => Key::Other,
        }
    }

    /// The character a key produces in the foreground window's layout.
    unsafe fn to_char(kb: &KBDLLHOOKSTRUCT) -> Option<char> {
        let tid = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let hkl = GetKeyboardLayout(tid);
        let mut state = [0u8; 256];
        if down(VK_SHIFT.0 as i32) {
            state[VK_SHIFT.0 as usize] = 0x80;
        }
        if GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0 {
            state[VK_CAPITAL.0 as usize] = 0x01;
        }
        let mut buf = [0u16; 8];
        // Flag 4: do not change the keyboard state, so dead keys keep
        // working in the target application.
        let n = ToUnicodeEx(kb.vkCode, kb.scanCode, &state, &mut buf, 4, Some(hkl));
        if n == 1 {
            char::from_u32(buf[0] as u32)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> (Machine, Receiver<(Instant, HookEvent)>, Arc<Shared>) {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = std::sync::mpsc::channel();
        (Machine::new("//".chars().collect(), shared.clone(), tx, None), rx, shared)
    }

    fn events(rx: &Receiver<(Instant, HookEvent)>) -> Vec<HookEvent> {
        rx.try_iter().map(|(_, e)| e).collect()
    }

    fn typed(m: &mut Machine, s: &str) {
        for c in s.chars() {
            m.key(Key::Char(c));
        }
    }

    #[test]
    fn trigger_opens_and_keyword_queries() {
        let (mut m, rx, _) = machine();
        typed(&mut m, "a/b//si");
        assert_eq!(
            events(&rx),
            vec![HookEvent::Open, HookEvent::Query("s".into()), HookEvent::Query("si".into())]
        );
    }

    #[test]
    fn navigation_keys_are_swallowed() {
        let (mut m, rx, shared) = machine();
        typed(&mut m, "//x");
        shared.set_results(3);
        assert_eq!(m.key(Key::Down), Verdict::Swallow);
        assert_eq!(m.key(Key::Enter), Verdict::Swallow);
        assert_eq!(m.key(Key::Modifier), Verdict::Pass);
        let ev = events(&rx);
        assert_eq!(ev[ev.len() - 1], HookEvent::Accept { keyword: "x".into() });
        assert_eq!(ev[ev.len() - 2], HookEvent::Move(1));
    }

    #[test]
    fn enter_without_results_cancels_and_passes() {
        let (mut m, rx, _) = machine();
        typed(&mut m, "//zz");
        assert_eq!(m.key(Key::Enter), Verdict::Pass);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Cancel));
    }

    #[test]
    fn backspace_on_empty_keyword_cancels() {
        let (mut m, rx, _) = machine();
        typed(&mut m, "//a");
        m.key(Key::Backspace);
        m.key(Key::Backspace);
        let ev = events(&rx);
        assert_eq!(ev[ev.len() - 2], HookEvent::Query(String::new()));
        assert_eq!(ev[ev.len() - 1], HookEvent::Cancel);
    }

    #[test]
    fn ui_deactivate_returns_to_idle() {
        let (mut m, rx, shared) = machine();
        typed(&mut m, "//a");
        shared.deactivate();
        assert_eq!(m.key(Key::Down), Verdict::Pass);
        typed(&mut m, "//");
        assert_eq!(events(&rx).last(), Some(&HookEvent::Open));
    }

    #[test]
    fn foreign_input_cancels_only_an_open_popup() {
        let (mut m, rx, _) = machine();
        m.foreign_input();
        assert!(events(&rx).is_empty());
        typed(&mut m, "//doc");
        m.foreign_input();
        assert_eq!(events(&rx).last(), Some(&HookEvent::Cancel));
    }

    #[test]
    fn keys_reach_the_expander() {
        let shared = Arc::new(Shared::default());
        let (tx, _rx) = std::sync::mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let on_input: OnInput = Box::new(move |i| sink.lock().unwrap().push(i));
        let mut m = Machine::new("//".chars().collect(), shared.clone(), tx, Some(on_input));
        typed(&mut m, "a//");
        shared.set_results(1);
        m.key(Key::Modifier);
        m.key(Key::Backspace);
        m.key(Key::Down);
        m.key(Key::Enter);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                Input::Char('a'),
                Input::Char('/'),
                Input::Char('/'),
                // Backspace on the empty keyword closes the popup and goes
                // to the application.
                Input::Backspace,
                // The popup is closed now, so Down and Enter go through.
                Input::Reset,
                Input::Char('\n'),
            ]
        );
    }

    #[test]
    fn click_outside_cancels() {
        let (mut m, rx, shared) = machine();
        typed(&mut m, "//");
        shared.set_popup_rect(Some(Rect { left: 0, top: 0, right: 10, bottom: 10 }));
        assert_eq!(m.click(5, 5), Verdict::Swallow);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Click));
        assert_eq!(m.click(50, 5), Verdict::Pass);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Cancel));
    }
}
