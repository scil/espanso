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
//! - Ctrl+E or Right (with at least one result) and Ctrl+N are swallowed and
//!   ask to edit the selected snippet ([`HookEvent::Edit`]) or to create one
//!   ([`HookEvent::New`]). The hook stays active until the UI calls
//!   [`Shared::deactivate`]. The Ctrl key alone changes nothing.
//! - Any other key, a key with Ctrl, Alt or Win, or a mouse click outside the
//!   popup cancels and goes through.
//! - A mouse click inside the popup is swallowed ([`HookEvent::Click`]). A
//!   right click asks to edit the row under the mouse
//!   ([`HookEvent::EditClick`]).
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
//! A `.` as the first keyword character repeats the last search: the hook
//! swallows the `.` and sends [`HookEvent::Repeat`] with the keyword of the
//! last session. The UI types that keyword into the application, so the
//! text before the caret is the trigger and the keyword, as usual.
//!
//! The expander can also open the popup as a pick list, for a trigger that
//! several matches share or for a `choice` variable ([`Shared::start_pick`]).
//! Then the hook swallows every typed character and Backspace, so the text
//! before the caret stays the trigger that the expander will erase. The
//! characters still filter the list ([`HookEvent::Query`]). Up, Down, Enter,
//! Tab, Escape and clicks work as in the popup. Backspace on an empty
//! keyword cancels. Any other key cancels and goes through.
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
    /// `.` as the first keyword character: the keyword of the last search
    /// is now the keyword. The UI types it into the application.
    Repeat(String),
    /// Up (-1) or Down (+1).
    Move(i32),
    /// The left or middle mouse button went down inside the popup. The hook
    /// swallows the click, so the target window keeps focus.
    Click,
    /// The right mouse button went down inside the popup: edit the row
    /// under the mouse. The hook swallows the click. The hook stays active
    /// until the UI calls [`Shared::deactivate`].
    EditClick,
    /// Enter or Tab with at least one result. The hook stays active until the
    /// UI calls [`Shared::deactivate`].
    Accept { keyword: String },
    /// Ctrl+1 to Ctrl+9: insert the row with that number (0-based here).
    /// The hook stays active until the UI calls [`Shared::deactivate`].
    AcceptRow(usize),
    /// Ctrl+E or Right with at least one result: edit the selected snippet. The hook
    /// stays active until the UI calls [`Shared::deactivate`].
    Edit,
    /// Ctrl+N: create a snippet. The hook stays active until the UI calls
    /// [`Shared::deactivate`].
    New { keyword: String },
    /// The popup must close.
    Cancel,
}

/// State that the hook thread and the UI share.
#[derive(Default)]
pub struct Shared {
    active: AtomicBool,
    /// The session is a pick list from the expander, not a `;;` search.
    pick: AtomicBool,
    paused: AtomicBool,
    results: AtomicUsize,
    popup: Mutex<Option<Rect>>,
}

impl Shared {
    /// While paused, the trigger does not open the popup. The expander
    /// checks the same flag.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
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
        self.pick.store(false, Ordering::Relaxed);
        self.active.store(false, Ordering::Relaxed);
        self.set_popup_rect(None);
    }
    /// Starts a pick-list session. It replaces an open `;;` session.
    pub fn start_pick(&self) {
        self.set_results(0);
        self.pick.store(true, Ordering::Relaxed);
        self.active.store(true, Ordering::Relaxed);
    }
    /// Whether the session is a pick list.
    pub fn is_picking(&self) -> bool {
        self.pick.load(Ordering::Relaxed)
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
    tab: TabKey,
    on_input: Option<OnInput>,
) -> Receiver<(Instant, HookEvent)> {
    let (tx, rx) = std::sync::mpsc::channel();
    ALLOW_INJECTED.store(allow_injected, Ordering::Relaxed);
    #[cfg(windows)]
    {
        let trigger: Vec<char> = trigger.chars().collect();
        std::thread::spawn(move || win::run(Machine::new(trigger, shared, tx, on_input).with_tab(tab)));
    }
    #[cfg(not(windows))]
    {
        let _ = (trigger, shared, tx, on_input, tab);
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
    Right,
    Enter,
    Tab,
    /// Tab with Shift.
    ShiftTab,
    Escape,
    /// Shift or Caps Lock alone. They never change the state.
    Modifier,
    /// The Ctrl key alone. It starts a shortcut such as Ctrl+E.
    Ctrl,
    /// Ctrl with a letter key, and no Alt or Win. The letter is lowercase.
    CtrlLetter(char),
    /// Ctrl with a digit key 1 to 9 on the main keyboard, and no Alt or Win.
    CtrlDigit(u8),
    /// Any other key, or any key with Ctrl, Alt or Win.
    Other,
}

/// What Tab does while the popup is open (`--tab`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TabKey {
    /// Tab inserts the selected row, as Enter does. Shift+Tab does the
    /// same. This is the default.
    #[default]
    Accept,
    /// Tab selects the next row and Shift+Tab the previous one, as Down
    /// and Up do.
    Move,
    /// The popup does not use Tab. Tab goes to the application and closes
    /// the popup, as other keys do.
    Ignore,
}

impl TabKey {
    /// `accept`, `move` or `ignore`.
    pub fn parse(s: &str) -> Option<TabKey> {
        match s {
            "accept" => Some(TabKey::Accept),
            "move" => Some(TabKey::Move),
            "ignore" => Some(TabKey::Ignore),
            _ => None,
        }
    }
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
    /// The keyword of the last search that had one. `.` repeats it.
    last: String,
    /// The machine handles a pick-list session.
    in_pick: bool,
    shared: Arc<Shared>,
    tx: Sender<(Instant, HookEvent)>,
    on_input: Option<OnInput>,
    tab: TabKey,
}

impl Machine {
    /// Sets what Tab does in the popup.
    pub fn with_tab(mut self, tab: TabKey) -> Self {
        self.tab = tab;
        self
    }

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
            last: String::new(),
            in_pick: false,
            shared,
            tx,
            on_input,
            tab: TabKey::default(),
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

    /// Keeps the keyword of a `;;` session that ends, for `.`.
    fn remember(&mut self) {
        if !self.in_pick && !self.keyword.is_empty() {
            self.last = self.keyword.clone();
        }
    }

    fn cancel(&mut self, reason: &str) {
        crate::log::line(&format!("hook cancel: {reason}"));
        self.remember();
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
            (_, Key::Modifier | Key::Ctrl) => None,
            (
                _,
                Key::Up
                | Key::Down
                | Key::Right
                | Key::ShiftTab
                | Key::Escape
                | Key::Other
                | Key::CtrlLetter(_)
                | Key::CtrlDigit(_),
            ) => {
                Some(Input::Reset)
            }
        };
        if let Some(input) = input {
            self.pass_on(input);
        }
        verdict
    }

    fn popup_key(&mut self, key: Key) -> Verdict {
        if self.shared.is_picking() {
            if !self.in_pick {
                self.in_pick = true;
                self.active = false;
                self.keyword.clear();
                self.recent.clear();
            }
            return self.pick_key(key);
        }
        if self.in_pick {
            self.in_pick = false;
            self.keyword.clear();
            self.recent.clear();
        }
        // The UI may end the session (after an insert, or with no results).
        if self.active && !self.shared.active.load(Ordering::Relaxed) {
            self.remember();
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
                if n > 0 && self.recent == self.trigger && !self.shared.is_paused() {
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
            Key::Modifier | Key::Ctrl => {}
            _ => self.recent.clear(),
        }
    }

    /// Ctrl+1 to Ctrl+9 picks a numbered row, if the list has it.
    fn digit_row(&mut self, n: u8) -> bool {
        let row = usize::from(n.max(1)) - 1;
        if row < self.shared.results.load(Ordering::Relaxed) {
            crate::log::line(&format!("hook accept row {n}"));
            self.emit(HookEvent::AcceptRow(row));
            true
        } else {
            false
        }
    }

    /// Tab and Shift+Tab as the key they act as in the popup, by `--tab`.
    fn tab_as(&self, key: Key) -> Key {
        match (key, self.tab) {
            (Key::Tab | Key::ShiftTab, TabKey::Accept) => Key::Enter,
            (Key::Tab, TabKey::Move) => Key::Down,
            (Key::ShiftTab, TabKey::Move) => Key::Up,
            (Key::Tab | Key::ShiftTab, TabKey::Ignore) => Key::Other,
            _ => key,
        }
    }

    fn active_key(&mut self, key: Key) -> Verdict {
        match self.tab_as(key) {
            Key::Char('.') if self.keyword.is_empty() && !self.last.is_empty() => {
                crate::log::line("hook repeat the last search");
                self.keyword = self.last.clone();
                self.emit(HookEvent::Repeat(self.keyword.clone()));
                Verdict::Swallow
            }
            Key::CtrlDigit(n) if self.digit_row(n) => Verdict::Swallow,
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
            // `tab_as` already turned Tab into another key.
            Key::Enter | Key::Tab | Key::ShiftTab => {
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
            Key::Modifier | Key::Ctrl => Verdict::Pass,
            Key::CtrlLetter('e') | Key::Right if self.shared.results.load(Ordering::Relaxed) > 0 => {
                crate::log::line("hook edit");
                self.emit(HookEvent::Edit);
                Verdict::Swallow
            }
            Key::CtrlLetter('n') => {
                crate::log::line("hook new");
                self.emit(HookEvent::New { keyword: self.keyword.clone() });
                Verdict::Swallow
            }
            Key::CtrlLetter(_) | Key::CtrlDigit(_) => {
                self.cancel("ctrl shortcut");
                Verdict::Pass
            }
            Key::Right => {
                self.cancel("right with no results");
                Verdict::Pass
            }
            Key::Other => {
                self.cancel("other key or chord");
                Verdict::Pass
            }
        }
    }

    /// A key in a pick-list session. Typing filters the list but never
    /// reaches the application.
    fn pick_key(&mut self, key: Key) -> Verdict {
        match self.tab_as(key) {
            Key::Char(c) => {
                self.keyword.push(c);
                self.emit(HookEvent::Query(self.keyword.clone()));
                Verdict::Swallow
            }
            Key::Backspace => {
                if self.keyword.pop().is_some() {
                    self.emit(HookEvent::Query(self.keyword.clone()));
                } else {
                    self.cancel("backspace on empty pick keyword");
                }
                Verdict::Swallow
            }
            Key::Up => {
                self.emit(HookEvent::Move(-1));
                Verdict::Swallow
            }
            Key::Down => {
                self.emit(HookEvent::Move(1));
                Verdict::Swallow
            }
            // `tab_as` already turned Tab into another key.
            Key::Enter | Key::Tab | Key::ShiftTab => {
                if self.shared.results.load(Ordering::Relaxed) > 0 {
                    crate::log::line("hook pick accept");
                    self.emit(HookEvent::Accept { keyword: self.keyword.clone() });
                }
                Verdict::Swallow
            }
            Key::Escape => {
                self.cancel("escape in pick list");
                Verdict::Swallow
            }
            Key::Modifier | Key::Ctrl => Verdict::Pass,
            Key::CtrlDigit(n) if self.digit_row(n) => Verdict::Swallow,
            Key::Right | Key::CtrlLetter(_) | Key::CtrlDigit(_) | Key::Other => {
                self.cancel("other key in pick list");
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
        if (self.active || self.shared.is_picking()) && self.shared.active.load(Ordering::Relaxed) {
            self.cancel("injected input from another program");
        }
    }

    /// A mouse button went down at a screen point. `right` is the right
    /// button.
    pub fn click(&mut self, x: i32, y: i32, right: bool) -> Verdict {
        self.pass_on(Input::Reset);
        if self.shared.is_picking() {
            self.in_pick = true;
            self.active = true;
        }
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
            self.emit(if right { HookEvent::EditClick } else { HookEvent::Click });
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
        VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_TAB, VK_UP,
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
                                .map(|m| m.click(ms.pt.x, ms.pt.y, msg == WM_RBUTTONDOWN))
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
        let (ctrl, alt) = (down(VK_CONTROL.0 as i32), down(VK_MENU.0 as i32));
        let win = down(VK_LWIN.0 as i32) || down(VK_RWIN.0 as i32);
        if [VK_CONTROL, VK_LCONTROL, VK_RCONTROL].iter().any(|k| is(*k)) && !alt && !win {
            return Key::Ctrl;
        }
        if [VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|k| is(*k))
        {
            return Key::Other;
        }
        // Ctrl with a letter: the virtual key codes of A to Z are their
        // ASCII codes.
        if ctrl && !alt && !win && (0x41..=0x5A).contains(&vk) {
            return Key::CtrlLetter((vk as u8 + 32) as char);
        }
        // Ctrl with a digit: the virtual key codes of 1 to 9 are their
        // ASCII codes.
        if ctrl && !alt && !win && (0x31..=0x39).contains(&vk) {
            return Key::CtrlDigit(vk as u8 - 0x30);
        }
        if ctrl || alt || win {
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
        if is(VK_RIGHT) {
            return Key::Right;
        }
        if is(VK_RETURN) {
            return Key::Enter;
        }
        if is(VK_TAB) {
            return if down(VK_SHIFT.0 as i32) { Key::ShiftTab } else { Key::Tab };
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
    fn tab_follows_its_setting() {
        let open = |tab: TabKey| {
            let (m, rx, shared) = machine();
            let mut m = m.with_tab(tab);
            typed(&mut m, "//a");
            shared.set_results(3);
            let _ = events(&rx);
            (m, rx, shared)
        };

        let (mut m, rx, _) = open(TabKey::Accept);
        assert_eq!(m.key(Key::Tab), Verdict::Swallow);
        assert_eq!(events(&rx), vec![HookEvent::Accept { keyword: "a".into() }]);

        let (mut m, rx, _) = open(TabKey::Move);
        assert_eq!(m.key(Key::Tab), Verdict::Swallow);
        assert_eq!(m.key(Key::ShiftTab), Verdict::Swallow);
        assert_eq!(events(&rx), vec![HookEvent::Move(1), HookEvent::Move(-1)]);

        let (mut m, rx, shared) = open(TabKey::Ignore);
        assert_eq!(m.key(Key::Tab), Verdict::Pass);
        assert_eq!(events(&rx), vec![HookEvent::Cancel]);
        assert!(!shared.is_active());
    }

    #[test]
    fn pick_list_swallows_typing() {
        let (mut m, rx, shared) = machine();
        typed(&mut m, "ab");
        shared.start_pick();
        assert_eq!(m.key(Key::Char('x')), Verdict::Swallow);
        assert_eq!(m.key(Key::Char('y')), Verdict::Swallow);
        assert_eq!(m.key(Key::Backspace), Verdict::Swallow);
        assert_eq!(m.key(Key::Down), Verdict::Swallow);
        // No results: Enter does nothing, and the application never gets it.
        assert_eq!(m.key(Key::Enter), Verdict::Swallow);
        shared.set_results(2);
        assert_eq!(m.key(Key::Enter), Verdict::Swallow);
        assert_eq!(
            events(&rx),
            vec![
                HookEvent::Query("x".into()),
                HookEvent::Query("xy".into()),
                HookEvent::Query("x".into()),
                HookEvent::Move(1),
                HookEvent::Accept { keyword: "x".into() },
            ]
        );
        // The UI ends the session. The trigger opens the popup again.
        shared.deactivate();
        typed(&mut m, "//");
        assert_eq!(events(&rx), vec![HookEvent::Open]);
    }

    #[test]
    fn dot_repeats_the_last_search() {
        let (mut m, rx, shared) = machine();
        // No search yet: `.` is a keyword character.
        typed(&mut m, "//.");
        assert_eq!(events(&rx), vec![HookEvent::Open, HookEvent::Query(".".into())]);
        m.key(Key::Escape);
        typed(&mut m, "//exp");
        shared.set_results(1);
        m.key(Key::Enter);
        // The UI ends the session after the insert.
        shared.deactivate();
        events(&rx);
        typed(&mut m, "//");
        assert_eq!(m.key(Key::Char('.')), Verdict::Swallow);
        assert_eq!(events(&rx), vec![HookEvent::Open, HookEvent::Repeat("exp".into())]);
        // Typing goes on from the repeated keyword.
        typed(&mut m, "l");
        assert_eq!(events(&rx), vec![HookEvent::Query("expl".into())]);
        // A canceled search counts too. `.` later in a keyword is a character.
        m.key(Key::Escape);
        typed(&mut m, "//a.");
        assert_eq!(events(&rx).last(), Some(&HookEvent::Query("a.".into())));
        m.key(Key::Escape);
        typed(&mut m, "//.");
        assert_eq!(events(&rx).last(), Some(&HookEvent::Repeat("a.".into())));
    }

    #[test]
    fn ctrl_digit_picks_a_numbered_row() {
        let (mut m, rx, shared) = machine();
        typed(&mut m, "//a");
        shared.set_results(3);
        assert_eq!(m.key(Key::CtrlDigit(2)), Verdict::Swallow);
        assert_eq!(events(&rx).last(), Some(&HookEvent::AcceptRow(1)));
        // No row 5: the shortcut goes to the application.
        assert_eq!(m.key(Key::CtrlDigit(5)), Verdict::Pass);
        assert_eq!(events(&rx), vec![HookEvent::Cancel]);
        // The same in a pick list.
        shared.start_pick();
        shared.set_results(2);
        assert_eq!(m.key(Key::CtrlDigit(1)), Verdict::Swallow);
        assert_eq!(events(&rx), vec![HookEvent::AcceptRow(0)]);
    }

    #[test]
    fn pick_list_cancels() {
        let (mut m, rx, shared) = machine();
        shared.start_pick();
        assert_eq!(m.key(Key::Backspace), Verdict::Swallow);
        assert!(!shared.is_active() && !shared.is_picking());
        shared.start_pick();
        assert_eq!(m.key(Key::Escape), Verdict::Swallow);
        shared.start_pick();
        assert_eq!(m.key(Key::Other), Verdict::Pass);
        assert_eq!(events(&rx), vec![HookEvent::Cancel, HookEvent::Cancel, HookEvent::Cancel]);
    }

    #[test]
    fn pick_list_replaces_a_popup_session() {
        let (mut m, rx, shared) = machine();
        typed(&mut m, "//ab");
        shared.start_pick();
        assert_eq!(m.key(Key::Char('z')), Verdict::Swallow);
        assert_eq!(
            events(&rx),
            vec![
                HookEvent::Open,
                HookEvent::Query("a".into()),
                HookEvent::Query("ab".into()),
                HookEvent::Query("z".into()),
            ]
        );
    }

    #[test]
    fn paused_trigger_does_not_open() {
        let (mut m, rx, shared) = machine();
        shared.set_paused(true);
        typed(&mut m, "//si");
        assert_eq!(events(&rx), vec![]);
        shared.set_paused(false);
        typed(&mut m, "//");
        assert_eq!(events(&rx), vec![HookEvent::Open]);
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
    fn ctrl_e_and_ctrl_n_are_swallowed() {
        let (mut m, rx, shared) = machine();
        typed(&mut m, "//si");
        assert_eq!(m.key(Key::Ctrl), Verdict::Pass);
        // Ctrl+E needs a result.
        assert_eq!(m.key(Key::CtrlLetter('e')), Verdict::Pass);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Cancel));
        typed(&mut m, "//si");
        shared.set_results(2);
        assert_eq!(m.key(Key::Ctrl), Verdict::Pass);
        assert_eq!(m.key(Key::CtrlLetter('e')), Verdict::Swallow);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Edit));
        assert_eq!(m.key(Key::CtrlLetter('n')), Verdict::Swallow);
        assert_eq!(events(&rx).last(), Some(&HookEvent::New { keyword: "si".into() }));
        assert_eq!(m.key(Key::CtrlLetter('c')), Verdict::Pass);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Cancel));
    }

    #[test]
    fn right_edits_with_results_and_passes_without() {
        let (mut m, rx, shared) = machine();
        typed(&mut m, "//si");
        shared.set_results(1);
        assert_eq!(m.key(Key::Right), Verdict::Swallow);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Edit));
        let (mut m, rx, _) = machine();
        typed(&mut m, "//zz");
        assert_eq!(m.key(Key::Right), Verdict::Pass);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Cancel));
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
        assert_eq!(m.click(5, 5, false), Verdict::Swallow);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Click));
        assert_eq!(m.click(5, 5, true), Verdict::Swallow);
        assert_eq!(events(&rx).last(), Some(&HookEvent::EditClick));
        assert_eq!(m.click(50, 5, true), Verdict::Pass);
        assert_eq!(events(&rx).last(), Some(&HookEvent::Cancel));
    }
}
