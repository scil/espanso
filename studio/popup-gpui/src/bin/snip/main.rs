//! Resident input-method style popup built with gpui. See `studio/POPUP.md`.
//!
//! The process starts once and stays in the background. A keyboard hook
//! (`popup_core::hook`) watches for the trigger. The search keyword stays in
//! the target application, where the user types it. The popup shows only the
//! candidates and never takes focus.
//!
//! The process is also the text expander: an engine thread
//! (`popup_core::expand`) gets every key press from the hook and expands
//! espanso triggers, so espanso itself does not need to run. See
//! `studio/EXPANDER.md`.
//!
//! Forms and `choice` variables open a window of their own near the caret
//! ([`form_view`]). That window takes the focus while the user types.
//!
//! A tray icon ([`tray`]) offers pause, reload and other common actions. See
//! `studio/EDITOR.md`.
#![windows_subsystem = "windows"]

mod editor;
mod form_view;
mod tray;

use std::cell::RefCell;
use std::collections::HashMap;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use gpui_kit::component::{ActiveTheme, Theme};
use gpui_kit::prelude::*;
use gpui_kit::*;
use popup_core::args::Args;
use popup_core::caret::{self, Caret};
use popup_core::data::{self, Entry};
use popup_core::expand::{Ask, Engine, Expansion, Input, Method};
use popup_core::form::Form;
use popup_core::focus::{self, Handle};
use popup_core::hook::{self, HookEvent, Shared};
use popup_core::{place, timing, Rect};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

const WIDTH: f32 = 420.0;
const ROW_H: f32 = 40.0;
const MAX_ROWS: usize = 8;
const PAD: f32 = 6.0;
const STATUS_H: f32 = 22.0;

/// The snippets for the popup, or the load error. The engine thread sets it
/// after each load. `None` until the first load ends.
type Entries = Result<Arc<Vec<Entry>>, String>;
static ENTRIES: RwLock<Option<Entries>> = RwLock::new(None);

fn entries() -> Option<Entries> {
    ENTRIES.read().unwrap().clone()
}

fn set_entries(e: Entries) {
    *ENTRIES.write().unwrap() = Some(e);
}

/// Messages to the engine thread.
enum ToEngine {
    Key(Input),
    /// The user picked a snippet in the popup. `erase` is the number of
    /// characters of the popup trigger and keyword before the caret.
    Insert { id: i32, erase: usize },
    /// Load the configuration again.
    Reload,
    /// Ctrl+E or Ctrl+N in the popup: erase the popup trigger and the
    /// keyword (`erase` characters), then open the editor. The erase comes
    /// first, while the target application still has the focus.
    Editor { erase: usize, request: editor::Request },
}

/// Loads the engine and publishes its snippets. On failure, a loaded list
/// stays, so a bad edit does not empty the popup.
fn load_engine(base: &std::path::Path, ui: &Sender<Ui>, shared: &Arc<Shared>) -> Option<Engine> {
    let ask = UiAsk { ui: ui.clone(), shared: shared.clone() };
    match Engine::load_with(base, Box::new(ask)) {
        Ok(engine) => {
            let entries = engine.entries();
            let n = entries.len();
            popup_core::log::line(&format!("engine: {n} matches loaded"));
            set_entries(Ok(Arc::new(entries)));
            let _ = ui.send(Ui::Loaded(Ok(n)));
            Some(engine)
        }
        Err(e) => {
            let e = format!("{e:#}");
            popup_core::log::line(&format!("engine: cannot load the configuration: {e}"));
            if !matches!(entries(), Some(Ok(_))) {
                set_entries(Err(e.clone()));
            }
            let _ = ui.send(Ui::Loaded(Err(e)));
            None
        }
    }
}

/// Owns the expansion engine. espanso numbers its matches with a counter per
/// thread, so the engine is loaded and used on this one thread.
fn engine_thread(
    base: Option<PathBuf>,
    prefix_wait: Duration,
    rx: Receiver<ToEngine>,
    shared: Arc<Shared>,
    ui: Sender<Ui>,
) {
    let base = match snip_core::Workspace::open(base) {
        Ok(ws) => ws.base().to_path_buf(),
        Err(e) => {
            let e = e.to_string();
            popup_core::log::line(&format!("engine: cannot open the configuration folder: {e}"));
            set_entries(Err(e.clone()));
            let _ = ui.send(Ui::Loaded(Err(e)));
            return;
        }
    };
    let _ = ui.send(Ui::Base(base.clone()));
    let mut loaded = load_engine(&base, &ui, &shared);
    // If the first load fails, wait for a reload: the user can fix the file
    // and pick Reload in the tray menu.
    let engine = loop {
        if let Some(engine) = loaded.take() {
            break engine;
        }
        match rx.recv() {
            Ok(ToEngine::Reload) => loaded = load_engine(&base, &ui, &shared),
            Ok(_) => {}
            Err(_) => return,
        }
    };
    timing::mark("data-loaded");
    run_engine(engine, &base, prefix_wait, rx, shared, ui)
}

fn run_engine(
    mut engine: Engine,
    base: &std::path::Path,
    prefix_wait: Duration,
    rx: Receiver<ToEngine>,
    shared: Arc<Shared>,
    ui: Sender<Ui>,
) {
    if focus::process_running("espanso.exe") {
        popup_core::log::line("engine: espanso.exe runs too, so triggers may expand twice");
    }
    let expanded = |x: Expansion| {
        if shared.is_active() {
            shared.deactivate();
            let _ = ui.send(Ui::Hook(HookEvent::Cancel));
        }
        // The hook sees the key before the application does. Wait until
        // the application has the last trigger character.
        std::thread::sleep(Duration::from_millis(10));
        perform(&x);
    };
    loop {
        // A trigger that starts a longer trigger waits for the next key. If
        // none comes in time, it expands.
        let msg = if engine.waiting() {
            match rx.recv_timeout(prefix_wait) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(x) = engine.flush() {
                        expanded(x);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            }
        };
        match msg {
            // While paused, the engine forgets what it saw, so a trigger
            // typed half before the pause never expands after it.
            ToEngine::Key(_) if shared.is_paused() => {
                let _ = engine.key(Input::Reset, focus::foreground_app);
            }
            ToEngine::Key(input) => {
                if let Some(x) = engine.key(input, focus::foreground_app) {
                    expanded(x);
                }
            }
            ToEngine::Reload => {
                if let Some(e) = load_engine(base, &ui, &shared) {
                    engine = e;
                }
            }
            ToEngine::Insert { id, erase } => {
                match engine.render_by_id(id, &focus::foreground_app()) {
                    Ok(x) => {
                        perform(&Expansion { erase, ..x });
                        engine.inserted();
                    }
                    Err(e) => popup_core::log::line(&format!("engine: match {id} not rendered: {e}")),
                }
                // The hook stays active until the text is in, so keys typed
                // meanwhile do not open a new session halfway.
                shared.deactivate();
            }
            ToEngine::Editor { erase, request } => {
                focus::send_backspaces(erase);
                engine.inserted();
                shared.deactivate();
                let _ = ui.send(Ui::Editor(request));
            }
        }
    }
}

/// Shows forms in the UI. It runs on the engine thread, which waits for the
/// answer, so the trigger stays in the text until the user submits.
struct UiAsk {
    ui: Sender<Ui>,
    shared: Arc<Shared>,
}

impl Ask for UiAsk {
    fn ask(&self, form: Form) -> Option<HashMap<String, String>> {
        // The popup may be open: the trigger can start with the popup
        // trigger.
        if self.shared.is_active() {
            self.shared.deactivate();
            let _ = self.ui.send(Ui::Hook(HookEvent::Cancel));
        }
        // Give the application a moment to draw the last trigger character.
        std::thread::sleep(Duration::from_millis(10));
        let target = focus::foreground();
        let caret = caret::locate(Duration::from_millis(150));
        let (reply, answer) = form_view::reply_channel();
        popup_core::log::line(&format!("form: open, {} fields", form.fields.len()));
        if self.ui.send(Ui::Form { form, caret, reply }).is_err() {
            return None;
        }
        // A closed channel without an answer counts as Cancel.
        let answer = answer.recv().ok().flatten();
        // Give the focus back only if a window of this process still has
        // it. If the user clicked another window, that window keeps it.
        if let Some(h) = target {
            if focus::foreground().is_some_and(focus::is_own) {
                focus::restore(h);
            }
        }
        answer
    }
}

/// Erases the trigger, inserts the text and moves the caret.
fn perform(x: &Expansion) {
    let started = Instant::now();
    focus::send_backspaces(x.erase);
    match x.method {
        Method::Auto => focus::type_text(&x.text),
        Method::Keys => focus::type_keys(&x.text, focus::insert_settings().pace),
        Method::Paste => focus::paste(&x.text),
    }
    focus::send_lefts(x.lefts);
    popup_core::log::line(&format!(
        "engine: match {} expanded, {} back, {} chars {:?}, {} left, in {} ms",
        x.id,
        x.erase,
        x.text.chars().count(),
        x.method,
        x.lefts,
        started.elapsed().as_millis()
    ));
}

/// Messages from the controller thread to the UI.
enum Ui {
    Open { caret: Caret, pressed: Instant },
    Hook(HookEvent),
    /// Show a form near the caret and send the answer on `reply`.
    Form { form: Form, caret: Caret, reply: form_view::Reply },
    /// The espanso configuration folder.
    Base(PathBuf),
    /// A load ended: the number of snippets, or the error.
    Loaded(Result<usize, String>),
    /// Open the editor. The engine sends it after it erased the typed text.
    Editor(editor::Request),
}

/// Turns hook events into UI messages. It looks up the caret for `Open`, so
/// the slow UI Automation call never blocks the hook or the UI thread.
fn controller(rx: Receiver<(Instant, HookEvent)>, tx: Sender<Ui>) {
    for (pressed, ev) in rx {
        let msg = match ev {
            HookEvent::Open => {
                // The hook sees the key before the application does. Give the
                // application a moment to draw the last trigger character, so
                // the caret is after it.
                std::thread::sleep(Duration::from_millis(10));
                popup_core::log::line(&format!(
                    "open in {}",
                    focus::foreground_exe().unwrap_or_else(|| "unknown program".into())
                ));
                let caret = caret::locate(Duration::from_millis(150));
                timing::since(&format!("caret-{:?}", caret.source), pressed);
                Ui::Open { caret, pressed }
            }
            other => Ui::Hook(other),
        };
        if tx.send(msg).is_err() {
            break;
        }
    }
}

/// Window operations for the window thread.
enum WinOp {
    Show(Rect),
    Hide,
}

/// Runs window operations on a thread of its own. `SetWindowPos` and
/// `ShowWindow` send messages such as `WM_SIZE` to the window procedure at
/// once. gpui handles them with its app state, which the UI thread holds
/// while it runs our code, so a direct call panics with "RefCell already
/// borrowed". From another thread, the messages wait until the UI thread
/// reads its message queue.
fn window_thread(h: Handle) -> Sender<WinOp> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for op in rx {
            match op {
                WinOp::Show(r) => focus::show_at(h, r),
                WinOp::Hide => focus::hide(h),
            }
        }
    });
    tx
}

struct Popup {
    win: Option<Sender<WinOp>>,
    shared: Arc<Shared>,
    engine: Sender<ToEngine>,
    ui_rx: Receiver<Ui>,
    trigger: String,
    open: bool,
    caret: Option<Caret>,
    keyword: String,
    results: Vec<usize>,
    selected: usize,
    hovered: Option<usize>,
    note: Option<SharedString>,
    scroll: ScrollHandle,
    shown: Option<Rect>,
    filled: bool,
    /// The snippet list that `results` points into. A reload does not
    /// change it while the popup is open.
    snap: Option<Arc<Vec<Entry>>>,
    tray: Option<tray::Tray>,
    /// The tray tooltip text without the pause state.
    loaded: String,
    base: Option<PathBuf>,
    log: Option<PathBuf>,
    /// The editor window, if it is open. Opened in `cx.defer`, so it lives
    /// in a cell.
    editor: Rc<RefCell<Option<WindowHandle<editor::EditorView>>>>,
    /// Where the popup was when Ctrl+E or Ctrl+N opened the editor.
    editor_near: Option<Rect>,
}

impl Popup {
    fn new(
        args: &Args,
        shared: Arc<Shared>,
        engine: Sender<ToEngine>,
        ui_rx: Receiver<Ui>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let hwnd = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(Handle(h.hwnd.get())),
            _ => None,
        };
        if let Some(h) = hwnd {
            focus::set_no_activate(h);
            focus::round_corners(h);
        }
        let win = hwnd.map(window_thread);
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(8)).await;
            if this.update_in(cx, |p, window, cx| p.tick(window, cx)).is_err() {
                break;
            }
        })
        .detach();
        Popup {
            win,
            shared,
            engine,
            ui_rx,
            trigger: args.trigger.clone(),
            open: false,
            caret: None,
            keyword: String::new(),
            results: Vec::new(),
            selected: 0,
            hovered: None,
            note: None,
            scroll: ScrollHandle::new(),
            shown: None,
            filled: false,
            snap: None,
            tray: match tray::Tray::new(focus::is_elevated()) {
                Ok(t) => Some(t),
                Err(e) => {
                    popup_core::log::line(&format!("tray: cannot create the icon: {e}"));
                    None
                }
            },
            loaded: "loading snippets".into(),
            base: None,
            log: args.log.clone(),
            editor: Rc::new(RefCell::new(None)),
            editor_near: None,
        }
    }

    /// Opens the editor window after this update.
    /// Opens the editor after this update. `near` is the popup rectangle
    /// for a request from the popup.
    fn open_editor(&mut self, request: editor::Request, near: Option<Rect>, cx: &mut Context<Self>) -> Result<(), String> {
        let base = self.base.clone().ok_or("the configuration folder is not known yet")?;
        let engine = self.engine.clone();
        let reload: Rc<dyn Fn()> = Rc::new(move || {
            let _ = engine.send(ToEngine::Reload);
        });
        let handle = self.editor.clone();
        cx.defer(move |cx| editor::open(&mut handle.borrow_mut(), base, reload, request, near, cx));
        Ok(())
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Ok(msg) = self.ui_rx.try_recv() {
            self.handle(msg, window, cx);
        }
        if self.open && !self.filled && entries().is_some() {
            self.refresh(cx);
            self.place_and_show();
        }
        let actions = self.tray.as_ref().map(|t| t.poll()).unwrap_or_default();
        for action in actions {
            self.tray_action(action, cx);
        }
    }

    fn tray_action(&mut self, action: tray::Action, cx: &mut Context<Self>) {
        popup_core::log::line(&format!("tray: {action:?}"));
        let result = match action {
            tray::Action::NewSnippet => self.open_editor(editor::Request::New { trigger: None }, None, cx),
            tray::Action::EditSnippets => self.open_editor(editor::Request::Browse, None, cx),
            tray::Action::Pause(on) => {
                self.shared.set_paused(on);
                if on && self.open {
                    self.shared.deactivate();
                    self.hide();
                }
                self.show_status();
                Ok(())
            }
            tray::Action::Reload => {
                let _ = self.engine.send(ToEngine::Reload);
                Ok(())
            }
            tray::Action::OpenFolder => match &self.base {
                Some(b) => tray::open(b),
                None => Err("the configuration folder is not known yet".into()),
            },
            tray::Action::OpenLog => match &self.log {
                Some(l) => tray::open(l),
                None => Err("snip runs without --log".into()),
            },
            tray::Action::StartAtSignIn(on) => {
                let r = tray::set_start_at_sign_in(on);
                if r.is_err() {
                    if let Some(t) = &self.tray {
                        t.set_sign_in_checked(!on);
                    }
                }
                r
            }
            tray::Action::RestartAsAdmin => tray::restart_as_admin(),
            tray::Action::Quit => {
                cx.quit();
                Ok(())
            }
        };
        if let Err(e) = result {
            popup_core::log::line(&format!("tray: {action:?} failed: {e}"));
        }
    }

    fn show_status(&mut self) {
        let paused = self.shared.is_paused();
        let text = if paused { format!("paused, {}", self.loaded) } else { self.loaded.clone() };
        if let Some(t) = &mut self.tray {
            t.set_status(paused, &text);
        }
    }

    fn handle(&mut self, msg: Ui, window: &mut Window, cx: &mut Context<Self>) {
        match msg {
            // A form or an expansion can end the session before the caret
            // lookup for `Open` is done.
            Ui::Open { .. } if !self.shared.is_active() => {
                popup_core::log::line("ui ignored open: the session already ended");
            }
            Ui::Open { caret, pressed } => {
                self.open = true;
                self.caret = Some(caret);
                self.keyword.clear();
                self.note = None;
                self.hovered = None;
                self.snap = None;
                self.refresh(cx);
                self.place_and_show();
                window.on_next_frame(move |_, _| timing::since("open-to-frame", pressed));
            }
            Ui::Form { form, caret, reply } => {
                // Open the window after this update: gpui does not open a
                // window while it updates another one.
                cx.defer(move |cx| form_view::open(form, caret, reply, cx));
            }
            Ui::Base(b) => self.base = Some(b),
            Ui::Loaded(r) => {
                self.loaded = match r {
                    Ok(n) => format!("{n} snippets"),
                    Err(e) => format!("load error: {e}"),
                };
                self.show_status();
            }
            Ui::Hook(ev) if !self.open => {
                popup_core::log::line(&format!("ui ignored {:?} while closed", kind(&ev)));
            }
            Ui::Hook(HookEvent::Query(k)) => {
                self.keyword = k;
                self.note = None;
                self.refresh(cx);
                self.place_and_show();
            }
            Ui::Hook(HookEvent::Move(d)) => {
                let n = self.results.len();
                if n > 0 {
                    self.selected = (self.selected as i32 + d).clamp(0, n as i32 - 1) as usize;
                    self.scroll.scroll_to_item(self.selected);
                    cx.notify();
                }
            }
            Ui::Hook(HookEvent::Accept { .. }) => self.accept(self.selected, cx),
            Ui::Hook(HookEvent::Edit) => self.open_from_popup(true),
            Ui::Hook(HookEvent::New { .. }) => self.open_from_popup(false),
            Ui::Editor(request) => {
                let near = self.editor_near.take();
                if let Err(e) = self.open_editor(request, near, cx) {
                    popup_core::log::line(&format!("editor: {e}"));
                }
            }
            Ui::Hook(HookEvent::Click) => {
                if let Some(row) = self.hovered {
                    self.accept(row, cx);
                }
            }
            // The hook already went back to idle, so do not deactivate it:
            // the user may have typed the trigger again since.
            Ui::Hook(HookEvent::Cancel) => self.hide(),
            Ui::Hook(HookEvent::Open) => {}
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // A keyword change keeps the list of the session, so the rows stay
        // valid. `Ui::Open` clears `snap`, so a new session takes the newest
        // list.
        match self.snap.clone().map(Ok).or_else(entries) {
            Some(Ok(entries)) => {
                self.filled = true;
                self.results = data::filter(&entries, &self.keyword, &self.trigger);
                self.snap = Some(entries);
            }
            Some(Err(e)) => {
                self.filled = true;
                self.results.clear();
                self.note = Some(format!("Cannot load snippets: {e}").into());
            }
            None => self.results.clear(),
        }
        self.selected = 0;
        self.scroll.scroll_to_item(0);
        self.shared.set_results(self.results.len());
        cx.notify();
    }

    fn height(&self) -> f32 {
        let rows = self.results.len().clamp(1, MAX_ROWS);
        PAD * 2.0 + rows as f32 * ROW_H + STATUS_H
    }

    fn place_and_show(&mut self) {
        let (Some(win), Some(caret)) = (&self.win, &self.caret) else { return };
        let height = self.height();
        let p = place::below_caret(caret, WIDTH, height);
        let r = Rect {
            left: p.x,
            top: p.y,
            right: p.x + (WIDTH * p.scale).round() as i32,
            bottom: p.y + (height * p.scale).round() as i32,
        };
        if self.shown != Some(r) {
            let _ = win.send(WinOp::Show(r));
            self.shown = Some(r);
            self.shared.set_popup_rect(Some(r));
        }
    }

    fn hide(&mut self) {
        self.open = false;
        self.shown = None;
        self.hovered = None;
        if let Some(win) = &self.win {
            let _ = win.send(WinOp::Hide);
        }
    }

    /// Ctrl+E or Right (`edit`), or Ctrl+N, in the popup. The engine erases the typed
    /// text and then asks the UI to open the editor.
    fn open_from_popup(&mut self, edit: bool) {
        let request = if edit {
            let entry = self.snap.as_ref().zip(self.results.get(self.selected)).map(|(s, &i)| &s[i]);
            match entry {
                Some(e) => editor::Request::Find { trigger: e.trigger.clone(), label: e.label.clone() },
                None => editor::Request::Browse,
            }
        } else {
            editor::Request::New { trigger: Some(format!("{}{}", self.trigger, self.keyword)) }
        };
        popup_core::log::line(&format!("ui editor: edit {edit}"));
        let erase = self.trigger.chars().count() + self.keyword.chars().count();
        self.editor_near = self.shown;
        self.hide();
        let _ = self.engine.send(ToEngine::Editor { erase, request });
    }

    fn accept(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(entries) = self.snap.clone() else {
            popup_core::log::line("ui accept: snippets not loaded");
            return;
        };
        let Some(e) = self.results.get(row).map(|&i| &entries[i]) else {
            popup_core::log::line(&format!("ui accept: no row {row} of {}", self.results.len()));
            return;
        };
        popup_core::log::line(&format!(
            "ui accept: row {row} of {}, insertable {}",
            self.results.len(),
            e.insertable
        ));
        if !e.insertable {
            self.note = Some("Image snippets come later. Esc closes.".into());
            cx.notify();
            return;
        }
        let erase = self.trigger.chars().count() + self.keyword.chars().count();
        self.hide();
        let _ = self.engine.send(ToEngine::Insert { id: e.id, erase });
    }
}

impl Render for Popup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let snips = self.snap.clone();
        let snips = snips.as_deref();
        let rows = self.results.iter().enumerate().filter_map(|(row, &i)| {
            let e = &snips?[i];
            let title = if e.label.is_empty() { e.trigger.clone() } else { e.label.clone() };
            let badge = if !e.label.is_empty() {
                e.trigger.clone()
            } else if !e.insertable {
                "image".to_string()
            } else {
                String::new()
            };
            let selected = row == self.selected;
            Some(
                div()
                    .id(row)
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(ROW_H))
                    .px_2()
                    .rounded_md()
                    .when(selected, |d| d.bg(theme.list_active))
                    .when(!selected, |d| d.hover(|s| s.bg(theme.list_hover)))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
                        if *hovered {
                            this.hovered = Some(row);
                        } else if this.hovered == Some(row) {
                            this.hovered = None;
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(e.preview.clone()),
                            ),
                    )
                    .when(!badge.is_empty(), |d| {
                        d.child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .bg(theme.muted)
                                .text_xs()
                                .font_family("Consolas")
                                .text_color(theme.muted_foreground)
                                .child(badge),
                        )
                    }),
            )
        });

        let status: SharedString = match &self.note {
            Some(n) => n.clone(),
            None if snips.is_none() => "Loading snippets…".into(),
            None if self.results.is_empty() => {
                format!("{}{}   no match   Ctrl+N new snippet   Esc close", self.trigger, self.keyword).into()
            }
            None => format!(
                "{}{}   {} matches   Enter insert   → edit   Ctrl+N new   Esc close",
                self.trigger,
                self.keyword,
                self.results.len()
            )
            .into(),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .p(px(PAD))
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .id("results")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows),
            )
            .child(
                div()
                    .h(px(STATUS_H))
                    .flex()
                    .items_center()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(status),
            )
    }
}

/// The name of a hook event, without the keyword, for the log.
fn kind(ev: &HookEvent) -> &'static str {
    match ev {
        HookEvent::Open => "open",
        HookEvent::Query(_) => "query",
        HookEvent::Move(_) => "move",
        HookEvent::Click => "click",
        HookEvent::Accept { .. } => "accept",
        HookEvent::Edit => "edit",
        HookEvent::New { .. } => "new",
        HookEvent::Cancel => "cancel",
    }
}

fn main() {
    popup_core::init_dpi();
    let args = Args::parse();
    timing::enable(args.timing);
    if let Some(path) = &args.log {
        if let Err(e) = popup_core::log::open(path) {
            eprintln!("cannot open the log file: {e}");
        }
        popup_core::log::line(&format!("start, trigger of {} chars", args.trigger.chars().count()));
    }

    let shared = Arc::new(Shared::default());
    let (ui_tx, ui_rx) = mpsc::channel();
    let (engine_tx, engine_rx) = mpsc::channel();
    {
        let (base, wait, shared, ui_tx) = (args.base.clone(), args.prefix_wait, shared.clone(), ui_tx.clone());
        std::thread::spawn(move || engine_thread(base, wait, engine_rx, shared, ui_tx));
    }
    let keys = engine_tx.clone();
    let on_input: hook::OnInput = Box::new(move |input| {
        let _ = keys.send(ToEngine::Key(input));
    });
    let hook_rx = hook::start(&args.trigger, shared.clone(), args.allow_injected, Some(on_input));
    std::thread::spawn(move || controller(hook_rx, ui_tx));

    // The assets hold the icon images of the editor buttons.
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        Theme::sync_system_appearance(None, cx);

        // The window is created once, hidden. Each session only moves and
        // shows it, which is much faster than creating a window.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(px(WIDTH), px(200.0)),
            })),
            titlebar: None,
            focus: false,
            show: false,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        };
        let args2 = args.clone();
        gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| Popup::new(&args2, shared, engine_tx, ui_rx, window, cx))
        })
        .expect("cannot open the popup window");
        timing::mark("ready");
    });
}
