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
//! Forms open a window of their own near the caret ([`form_view`]). That
//! window takes the focus while the user types. A pick list (a `choice`
//! variable, or a trigger that several matches share) opens in the popup
//! instead, and the hook swallows the keys that filter it.
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

use gpui_kit::component::scroll::ScrollbarMode;
use gpui_kit::component::{ActiveTheme, Theme};
use gpui_kit::prelude::*;
use gpui_kit::*;
use popup_core::args::Args;
use popup_core::caret::{self, Caret};
use popup_core::data::{self, Entry};
use popup_core::expand::{Ask, Engine, Expansion, Input, Method};
use popup_core::form::{self as pform, Form, PickItem};
use popup_core::focus::{self, Handle};
use popup_core::hook::{self, HookEvent, Shared};
use popup_core::{place, timing, Rect};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

const WIDTH: f32 = 420.0;
const ROW_H: f32 = 28.0;
const MAX_ROWS: usize = 8;
const PAD: f32 = 6.0;
const STATUS_H: f32 = 22.0;
/// The width of the edit form beside the list, and the window height while
/// the popup shows it, in logical pixels.
const FORM_W: f32 = 700.0;
const EDIT_H: f32 = 760.0;

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
    // The `;;` popup was open during the current wait.
    let mut popup_in_wait = false;
    loop {
        // A trigger that starts a longer trigger waits for the next key. If
        // none comes in time, it expands. While the popup is open, the user
        // picks there, so the wait does not end by time: the next key
        // decides. If the popup closes without a key, nothing expands.
        let msg = if engine.waiting() {
            match rx.recv_timeout(prefix_wait) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if shared.is_active() {
                        popup_in_wait = true;
                    } else if popup_in_wait {
                        popup_core::log::line("engine: the popup closed, so the waiting trigger does not expand");
                        let _ = engine.key(Input::Reset, focus::foreground_app);
                    } else if let Some(x) = engine.flush() {
                        expanded(x);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            popup_in_wait = false;
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

    fn pick(&self, items: Vec<PickItem>) -> Option<usize> {
        // From here on the hook swallows typed keys, so the trigger stays
        // as typed. It replaces an open `;;` session.
        self.shared.start_pick();
        // Give the application a moment to draw the last trigger character.
        std::thread::sleep(Duration::from_millis(10));
        let caret = caret::locate(Duration::from_millis(150));
        let (reply, answer) = mpsc::channel();
        popup_core::log::line(&format!("pick: open, {} items", items.len()));
        if self.ui.send(Ui::Pick { items, caret, reply }).is_err() {
            self.shared.deactivate();
            return None;
        }
        let answer = answer.recv().ok().flatten();
        self.shared.deactivate();
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
    /// Show a pick list in the popup and send the picked index on `reply`.
    Pick { items: Vec<PickItem>, caret: Caret, reply: Sender<Option<usize>> },
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
            // `;;.`: the hook swallowed the `.`. Type the last keyword into
            // the application, so the erase after a pick removes it too.
            // The hook ignores this input, because it is our own.
            HookEvent::Repeat(keyword) => {
                focus::type_text(&keyword);
                Ui::Hook(HookEvent::Query(keyword))
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
    /// Let the popup take focus, show it at the rectangle and focus it: the
    /// edit session.
    Edit(Rect),
    /// The edit session ended: the popup never takes focus again, hides,
    /// and gives focus back to the window, if any. Then it runs the step,
    /// if any.
    EndEdit(Option<Handle>, Option<AfterEdit>),
}

/// A step after the edit session, once the focus is back: an insert.
type AfterEdit = Box<dyn FnOnce() + Send>;

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
                WinOp::Edit(r) => {
                    focus::set_no_activate(h, false);
                    focus::show_at(h, r);
                    focus::bring_to_front(h);
                }
                WinOp::EndEdit(prev, then) => {
                    focus::set_no_activate(h, true);
                    focus::hide(h);
                    if let Some(p) = prev {
                        focus::restore(p);
                    }
                    if let Some(then) = then {
                        then();
                    }
                }
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
    /// The open pick list, if the session is one. `results` then points
    /// into its items.
    pick: Option<Pick>,
    /// The edit form beside the list, while the popup shows one.
    edit: Option<EditSession>,
    /// The focus of the list while the popup has focus: keys then search,
    /// move and insert, as in the popup.
    list_focus: FocusHandle,
}

/// The popup after a right click, Ctrl+E or Ctrl+N: it takes focus and shows
/// an edit form beside its list. Esc in the form removes the form, and the
/// popup shows only its list, still with focus.
struct EditSession {
    /// The form and its subscription. `None` in the list-only mode.
    form: Option<(Entity<editor::EditorView>, Subscription)>,
    /// Where the popup was. The window's top left corner stays there.
    near: Rect,
    /// The window that had focus before. It gets focus back when the edit
    /// ends with Esc in the list, a save, or an insert.
    prev: Option<Handle>,
    /// The popup window had focus once. A later loss of focus ends the edit,
    /// unless it has unsaved changes.
    was_active: bool,
}

/// A pick list from the expander: the items and where the answer goes.
struct Pick {
    items: Vec<PickItem>,
    reply: Sender<Option<usize>>,
}

impl Pick {
    fn answer(self, index: Option<usize>) {
        popup_core::log::line(&format!("ui pick: {index:?}"));
        let _ = self.reply.send(index);
    }
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
            focus::set_no_activate(h, true);
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
            pick: None,
            edit: None,
            list_focus: cx.focus_handle(),
        }
    }

    /// Starts the edit session at the popup rectangle `near`, or shows the
    /// request in the open session. The window grows to the right (or moves
    /// left at the screen edge) and takes focus.
    fn edit_in_popup(&mut self, request: editor::Request, near: Rect, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.is_none() {
            let prev = focus::foreground().filter(|h| !focus::is_own(*h));
            self.edit = Some(EditSession { form: None, near, prev, was_active: false });
            self.shared.set_popup_rect(None);
            popup_core::log::line("ui edit session: start");
        }
        self.show_form(request, window, cx);
    }

    /// Shows `request` in the form, and opens the form if the popup shows
    /// only its list. A request while the form is open replaces its snippet,
    /// after it asks about unsaved changes.
    fn show_form(&mut self, request: editor::Request, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.form_view() {
            view.update(cx, |v, cx| {
                v.request(request, window, cx);
                v.focus_form(window, cx);
            });
            return;
        }
        if self.edit.is_none() {
            return;
        }
        let Some(base) = self.base.clone() else {
            popup_core::log::line("editor: the configuration folder is not known yet");
            self.end_edit(true, None, cx);
            return;
        };
        let engine = self.engine.clone();
        let reload: Rc<dyn Fn()> = Rc::new(move || {
            let _ = engine.send(ToEngine::Reload);
        });
        let view = cx.new(|cx| editor::EditorView::embedded(base, reload, request, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, ev: &editor::EditorEvent, window, cx| match ev {
            editor::EditorEvent::Close => this.hide_form(window, cx),
            editor::EditorEvent::Done => this.end_edit(true, None, cx),
            editor::EditorEvent::FocusList => this.focus_list(window, cx),
        });
        if let Some(edit) = &mut self.edit {
            edit.form = Some((view, sub));
        }
        self.place_edit();
        cx.notify();
    }

    /// The editor of the open form.
    fn form_view(&self) -> Option<Entity<editor::EditorView>> {
        self.edit.as_ref()?.form.as_ref().map(|(view, _)| view.clone())
    }

    /// Esc in the form: the form goes, and the popup shows only its list,
    /// with focus. Unsaved changes are gone: the form asked first.
    fn hide_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.edit else { return };
        edit.form = None;
        popup_core::log::line("ui edit session: list only");
        self.place_edit();
        self.focus_list(window, cx);
    }

    fn focus_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.list_focus, cx);
        cx.notify();
    }

    /// Moves and sizes the window for the edit session: the list where the
    /// popup was, and the form to its right, if it shows.
    fn place_edit(&mut self) {
        let Some(edit) = &self.edit else { return };
        let r = match edit.form {
            Some(_) => rect_at(edit.near, WIDTH + FORM_W, EDIT_H),
            None => rect_at(edit.near, WIDTH, self.height()),
        };
        if self.shown != Some(r) {
            self.shown = Some(r);
            if let Some(win) = &self.win {
                let _ = win.send(WinOp::Edit(r));
            }
        }
    }

    /// Ends the edit session and hides the popup. With `give_back`, the
    /// window that had focus before gets it again. `then` runs after that,
    /// on the window thread: an insert must wait until the focus is back.
    fn end_edit(&mut self, give_back: bool, then: Option<AfterEdit>, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.take() else { return };
        popup_core::log::line(&format!("ui edit session: end, give back focus {give_back}"));
        self.open = false;
        self.shown = None;
        self.hovered = None;
        if let Some(win) = &self.win {
            let _ = win.send(WinOp::EndEdit(edit.prev.filter(|_| give_back), then));
        }
        cx.notify();
    }

    /// Whether the edit form is open with unsaved changes.
    fn edit_dirty(&self, cx: &App) -> bool {
        self.form_view().is_some_and(|view| view.read(cx).dirty(cx))
    }

    /// The request that opens the snippet of `row` in the form.
    fn find_request(&self, row: usize) -> Option<editor::Request> {
        let e = &self.snap.as_ref()?[*self.results.get(row)?];
        Some(editor::Request::Find {
            trigger: e.trigger.clone(),
            label: e.label.clone(),
            keyword: self.keyword.clone(),
            prefix: self.trigger.clone(),
        })
    }

    /// Selects `row` in the edit session. An open form shows its snippet,
    /// but the focus stays where it is.
    fn select_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if row >= self.results.len() {
            return;
        }
        self.selected = row;
        self.scroll.scroll_to_item(row);
        if let (Some(view), Some(request)) = (self.form_view(), self.find_request(row)) {
            view.update(cx, |v, cx| v.preview(request, window, cx));
        }
        cx.notify();
    }

    /// Puts the cursor in the open form, or opens the form with the
    /// selected row.
    fn enter_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.form_view() {
            Some(view) => view.update(cx, |v, cx| v.focus_form(window, cx)),
            None => self.edit_row(self.selected, window, cx),
        }
    }

    /// Opens the form with the snippet of `row`.
    fn edit_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.find_request(row) else { return };
        self.selected = row;
        self.scroll.scroll_to_item(row);
        self.show_form(request, window, cx);
        cx.notify();
    }

    /// Inserts the snippet of `row` from the list-only mode: the window that
    /// had focus gets it back, and then the engine types the text. The typed
    /// trigger is already gone, so it erases nothing.
    fn insert_from_list(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(snips) = self.snap.clone() else { return };
        let Some(e) = self.results.get(row).map(|&i| &snips[i]) else { return };
        if !e.insertable {
            self.note = Some("Image snippets come later.".into());
            cx.notify();
            return;
        }
        let id = e.id;
        let engine = self.engine.clone();
        let then: AfterEdit = Box::new(move || {
            let _ = engine.send(ToEngine::Insert { id, erase: 0 });
        });
        self.end_edit(true, Some(then), cx);
    }

    /// The search changed in the list: the rows follow, as in the popup.
    fn search_changed(&mut self, cx: &mut Context<Self>) {
        self.note = None;
        self.refresh(cx);
        self.place_edit();
    }

    /// Keys while the list has focus in the edit session. They work as in
    /// the popup: type to search, Up and Down move, Enter inserts, Ctrl+E or
    /// Right edits, Ctrl+N makes a new snippet, Esc closes the popup. With
    /// the form open, Up and Down also show the row in the form while the
    /// list keeps the focus. Right, Ctrl+E or Enter then puts the cursor in
    /// the form, and Esc closes the form.
    fn list_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if m.alt || m.platform {
            return;
        }
        let form = self.form_view();
        match (m.control, k.key.as_str()) {
            (false, "up") => self.select_row(self.selected.saturating_sub(1), window, cx),
            (false, "down") => self.select_row(self.selected + 1, window, cx),
            // The form asks about unsaved changes first, as for Esc there.
            (false, "escape") => match form {
                Some(view) => view.update(cx, |v, cx| v.try_close(window, cx)),
                None => self.end_edit(true, None, cx),
            },
            (false, "enter") if form.is_some() => self.enter_form(window, cx),
            (false, "enter") => self.insert_from_list(self.selected, cx),
            (true, d) if form.is_none() && d.len() == 1 && ("1"..="9").contains(&d) => {
                let row = d.parse::<usize>().unwrap_or(1) - 1;
                self.insert_from_list(row, cx);
            }
            // Up and Down already show the row in the form.
            (false, "right") | (true, "e") => self.enter_form(window, cx),
            (true, "n") => {
                let trigger = Some(format!("{}{}", self.trigger, self.keyword));
                self.show_form(editor::Request::New { trigger }, window, cx);
            }
            (false, "backspace") => {
                self.keyword.pop();
                self.search_changed(cx);
            }
            (false, _) => match &k.key_char {
                Some(c) if !c.chars().any(char::is_control) => {
                    self.keyword.push_str(c);
                    self.search_changed(cx);
                }
                _ => return,
            },
            _ => return,
        }
        cx.stop_propagation();
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
        // A click into another window ends the edit, unless that drops
        // unsaved changes: then the form stays, like a normal window.
        let active = window.is_window_active();
        let dirty = self.edit_dirty(cx);
        let lost = match &mut self.edit {
            Some(edit) if active => {
                edit.was_active = true;
                false
            }
            Some(edit) => edit.was_active && !dirty,
            None => false,
        };
        if lost {
            self.end_edit(false, None, cx);
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
            // A pick list replaced the `;;` session before its caret lookup
            // ended.
            Ui::Open { .. } if self.shared.is_picking() => {
                popup_core::log::line("ui ignored open: a pick list is open");
            }
            // The popup window shows a form with unsaved changes. The new
            // session gets no popup; the trigger stays typed.
            Ui::Open { .. } if self.edit_dirty(cx) => {
                popup_core::log::line("ui ignored open: the edit form has unsaved changes");
                self.shared.deactivate();
            }
            Ui::Pick { reply, .. } if self.edit_dirty(cx) => {
                popup_core::log::line("ui ignored pick list: the edit form has unsaved changes");
                let _ = reply.send(None);
            }
            Ui::Open { caret, pressed } => {
                self.end_edit(false, None, cx);
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
            Ui::Pick { items, caret, reply } => {
                if let Some(old) = self.pick.take() {
                    old.answer(None);
                }
                // The hook may have ended the session before the list came.
                if !self.shared.is_picking() {
                    let _ = reply.send(None);
                    return;
                }
                self.end_edit(false, None, cx);
                self.pick = Some(Pick { items, reply });
                self.open = true;
                self.caret = Some(caret);
                self.keyword.clear();
                self.note = None;
                self.hovered = None;
                self.refresh(cx);
                self.place_and_show();
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
            // The controller turns `Repeat` into `Query` after it types.
            Ui::Hook(HookEvent::Query(k) | HookEvent::Repeat(k)) => {
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
            Ui::Hook(HookEvent::AcceptRow(row)) => self.accept(row, cx),
            Ui::Hook(HookEvent::Edit) => self.open_from_popup(true),
            Ui::Hook(HookEvent::New { .. }) => self.open_from_popup(false),
            // From the popup: the popup itself shows the form.
            Ui::Editor(request) => match self.editor_near.take() {
                Some(near) => self.edit_in_popup(request, near, window, cx),
                None => {
                    if let Err(e) = self.open_editor(request, None, cx) {
                        popup_core::log::line(&format!("editor: {e}"));
                    }
                }
            },
            Ui::Hook(HookEvent::Click) => {
                if let Some(row) = self.hovered {
                    self.accept(row, cx);
                }
            }
            // A right click edits the row under the mouse. A pick list has
            // nothing to edit, and a click beside the rows does nothing.
            Ui::Hook(HookEvent::EditClick) => match self.hovered {
                Some(row) if self.pick.is_none() && row < self.results.len() => {
                    self.selected = row;
                    self.open_from_popup(true);
                }
                _ => popup_core::log::line("ui ignored edit click: no snippet row under the mouse"),
            },
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
        if let Some(pick) = &self.pick {
            self.filled = true;
            let texts: Vec<String> = pick.items.iter().map(PickItem::search_text).collect();
            self.results = pform::filter(&texts, &self.keyword);
        } else {
            self.refresh_snippets();
        }
        self.selected = 0;
        self.scroll.scroll_to_item(0);
        self.shared.set_results(self.results.len());
        cx.notify();
    }

    fn refresh_snippets(&mut self) {
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
    }

    fn height(&self) -> f32 {
        let rows = self.results.len().clamp(1, MAX_ROWS);
        let status = if self.status().is_some() { STATUS_H } else { 0.0 };
        PAD * 2.0 + rows as f32 * ROW_H + status
    }

    /// The line under the rows. It shows no key help, only a note, the load
    /// state, an empty result, or the filter of a pick list (its typed keys
    /// never reach the application, so they show here).
    fn status(&self) -> Option<SharedString> {
        // In the edit session the keys go to the list, not to an
        // application, so the search shows here too.
        let filter = if self.pick.is_some() || self.edit.is_some() { self.keyword.as_str() } else { "" };
        if let Some(n) = &self.note {
            Some(n.clone())
        } else if self.pick.is_none() && self.snap.is_none() {
            Some("Loading snippets…".into())
        } else if self.results.is_empty() {
            Some(format!("{filter}   no match").trim_start().to_string().into())
        } else if !filter.is_empty() {
            Some(filter.to_string().into())
        } else {
            None
        }
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
        // A pick list that closes without a pick is canceled.
        if let Some(pick) = self.pick.take() {
            pick.answer(None);
        }
        self.open = false;
        self.shown = None;
        self.hovered = None;
        if let Some(win) = &self.win {
            let _ = win.send(WinOp::Hide);
        }
    }

    /// Ctrl+E, Right or a right click (`edit`), or Ctrl+N, in the popup.
    /// The engine erases the typed text and then asks the UI to show the
    /// edit form. The popup stays on screen meanwhile; the form opens in it.
    fn open_from_popup(&mut self, edit: bool) {
        let request = if edit {
            let entry = self.snap.as_ref().zip(self.results.get(self.selected)).map(|(s, &i)| &s[i]);
            match entry {
                Some(e) => editor::Request::Find {
                    trigger: e.trigger.clone(),
                    label: e.label.clone(),
                    keyword: self.keyword.clone(),
                    prefix: self.trigger.clone(),
                },
                None => editor::Request::Browse,
            }
        } else {
            editor::Request::New { trigger: Some(format!("{}{}", self.trigger, self.keyword)) }
        };
        popup_core::log::line(&format!("ui editor: edit {edit}"));
        let erase = self.trigger.chars().count() + self.keyword.chars().count();
        self.editor_near = self.shown;
        // Not `hide`: the window stays until the form opens in it. A pick
        // list never gets here, but would be canceled.
        if let Some(pick) = self.pick.take() {
            pick.answer(None);
        }
        self.open = false;
        self.hovered = None;
        let _ = self.engine.send(ToEngine::Editor { erase, request });
    }

    fn accept(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.pick.is_some() {
            let Some(&index) = self.results.get(row) else { return };
            if let Some(pick) = self.pick.take() {
                pick.answer(Some(index));
            }
            // The engine thread ends the hook session after the answer.
            self.hide();
            return;
        }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let snips = self.snap.clone();
        let snips = snips.as_deref();
        // Each row as (text, badge, exact): the label if there is one, else
        // the start of the text. `exact` is the trigger that is the typed
        // text in full; the row shows it. Other rows show no trigger.
        let lines: Vec<(String, String, Option<String>)> = match &self.pick {
            Some(pick) => self
                .results
                .iter()
                .map(|&i| {
                    let it = &pick.items[i];
                    (it.title.clone(), it.badge.clone(), None)
                })
                .collect(),
            None => self
                .results
                .iter()
                .filter_map(|&i| {
                    let e = &snips?[i];
                    let text = if e.label.is_empty() { e.preview.to_string() } else { e.label.clone() };
                    let badge = if e.insertable { String::new() } else { "image".to_string() };
                    let exact = data::exact_trigger(e, &self.keyword, &self.trigger).map(str::to_string);
                    Some((text, badge, exact))
                })
                .collect(),
        };
        let count = lines.len();
        let editing = self.edit.is_some();
        let rows = lines.into_iter().enumerate().map(|(row, (title, badge, exact))| {
            // Rows 1 to 9 have a number for Ctrl+1 to Ctrl+9.
            let number = if row < 9 { (row + 1).to_string() } else { String::new() };
            let selected = row == self.selected;
            let is_exact = exact.is_some();
            div()
                    .id(row)
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(ROW_H))
                    .pl_1()
                    .pr_2()
                    // A line between rows keeps them apart.
                    .when(row + 1 < count, |d| d.border_b_1().border_color(theme.border))
                    // The theme's `list_active` is too light to find at a glance.
                    .when(selected, |d| d.bg(theme.primary.opacity(0.3)))
                    .when(!selected, |d| d.hover(|s| s.bg(theme.list_hover)))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
                        if *hovered {
                            this.hovered = Some(row);
                        } else if this.hovered == Some(row) {
                            this.hovered = None;
                        }
                    }))
                    // In the edit session, the window has focus and gets the
                    // clicks itself. A click shows the row in the form, or
                    // inserts it when only the list shows. A right click
                    // edits it. Else the hook handles clicks.
                    .when(editing, |d| {
                        d.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            if this.form_view().is_some() {
                                this.select_row(row, window, cx);
                            } else {
                                this.insert_from_list(row, cx);
                            }
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| this.edit_row(row, window, cx)),
                        )
                    })
                    // An exact match has a colored bar on its left edge.
                    .child(
                        div()
                            .w(px(3.0))
                            .h(px(ROW_H - 8.0))
                            .flex_none()
                            .rounded_full()
                            .when(is_exact, |d| d.bg(theme.primary)),
                    )
                    .child(
                        div()
                            .w(px(14.0))
                            .flex_none()
                            .text_xs()
                            .font_family("Consolas")
                            .text_color(theme.muted_foreground)
                            .child(number),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_sm()
                            .truncate()
                            .when(is_exact, |d| d.font_weight(FontWeight::SEMIBOLD))
                            .child(title),
                    )
                    // It also shows its trigger, in the accent color.
                    .when_some(exact, |d, trigger| {
                        d.child(
                            div()
                                .flex_none()
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .bg(theme.primary.opacity(0.15))
                                .text_xs()
                                .font_family("Consolas")
                                .text_color(theme.primary)
                                .child(trigger),
                        )
                    })
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
                    })
        });

        let status = self.status();

        let list = div()
            .flex()
            .flex_col()
            .p(px(PAD))
            .child(
                div()
                    .id("results")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows),
            )
            .children(status.map(|status| {
                div()
                    .h(px(STATUS_H))
                    .flex()
                    .items_center()
                    .px_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(status)
            }));

        let root = div().size_full().flex().bg(theme.background).text_color(theme.foreground);
        let Some(edit) = &self.edit else { return root.child(list.size_full()) };
        // In the edit session the list takes keys when it has focus. A click
        // beside the rows gives it focus. A colored edge shows the focus.
        let list_focused = self.list_focus.is_focused(window);
        let list = list
            .track_focus(&self.list_focus)
            .on_key_down(cx.listener(Self::list_key))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, window, cx| this.focus_list(window, cx)));
        let root = root
            // Alt+Up and Alt+Down select the row above or below, from the
            // form too. An open form shows it.
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    let k = &ev.keystroke;
                    let m = k.modifiers;
                    if !m.alt || m.control || m.shift || m.platform {
                        return;
                    }
                    let row = match k.key.as_str() {
                        "up" => this.selected.checked_sub(1),
                        "down" => Some(this.selected + 1).filter(|r| *r < this.results.len()),
                        _ => return,
                    };
                    cx.stop_propagation();
                    if let Some(row) = row {
                        // From the form, the cursor goes to the new form:
                        // the box that had it is gone.
                        let in_form = !this.list_focus.is_focused(window);
                        this.select_row(row, window, cx);
                        if in_form {
                            this.enter_form(window, cx);
                        }
                    }
                }));
        match &edit.form {
            None => root.child(list.size_full()),
            // The list keeps its width on the left, and the form fills the
            // rest.
            Some((view, _)) => root
                .child(
                    list.w(px(WIDTH))
                        .flex_none()
                        .h_full()
                        .border_r_2()
                        .border_color(if list_focused { theme.primary } else { theme.border }),
                )
                .child(div().flex_1().min_w_0().h_full().child(view.clone())),
        }
    }
}

/// The popup window in the edit session, `width` by `height` logical
/// pixels: its top left corner is where the popup was, `near`. The
/// rectangle moves inside the work area of that monitor.
fn rect_at(near: Rect, width: f32, height: f32) -> Rect {
    let (work, scale) = place::monitor_at(near.left, near.top);
    let w = ((width * scale).round() as i32).min(work.width());
    let h = ((height * scale).round() as i32).min(work.height());
    let left = near.left.min(work.right - w).max(work.left);
    let top = near.top.min(work.bottom - h).max(work.top);
    Rect { left, top, right: left + w, bottom: top + h }
}

/// The name of a hook event, without the keyword, for the log.
fn kind(ev: &HookEvent) -> &'static str {
    match ev {
        HookEvent::Open => "open",
        HookEvent::Query(_) => "query",
        HookEvent::Repeat(_) => "repeat",
        HookEvent::Move(_) => "move",
        HookEvent::Click => "click",
        HookEvent::EditClick => "edit click",
        HookEvent::Accept { .. } => "accept",
        HookEvent::AcceptRow(_) => "accept row",
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
    let hook_rx = hook::start(&args.trigger, shared.clone(), args.allow_injected, args.tab, Some(on_input));
    std::thread::spawn(move || controller(hook_rx, ui_tx));

    // The assets hold the icon images of the editor buttons.
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        editor::init(cx);
        Theme::sync_system_appearance(None, cx);
        // A text area that holds more text than fits shows its scrollbar
        // all the time, not only while it scrolls.
        Theme::set_scrollbar_mode(ScrollbarMode::Always, cx);

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
