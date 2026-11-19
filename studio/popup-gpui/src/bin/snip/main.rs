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
//! `studio/EDITOR.md`. The edit session can show the settings panel
//! ([`settings_view`]) in place of the edit form.
#![windows_subsystem = "windows"]

mod editor;
mod form_view;
mod settings_view;
mod tray;

use std::collections::HashMap;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use gpui_kit::component::scroll::ScrollbarMode;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::base::Selectable;
use gpui_kit::component::{self as component, ActiveTheme, IconName, Sizable, Theme};
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
/// The header above the list in the edit session. The form's tab row has
/// the same height, so the two halves line up.
const HEAD_H: f32 = editor::TAB_ROW_H;
/// The form fades and slides in over this time when it opens.
const FORM_IN: Duration = Duration::from_millis(120);
/// The width of the edit form beside the list, and the window height while
/// the popup shows it, in logical pixels.
const FORM_W: f32 = editor::FORM_W;
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
    /// With `paragraphs` (Ctrl+P), a list first asks which paragraphs of
    /// the snippet to insert.
    Insert { id: i32, erase: usize, paragraphs: bool },
    /// Load the configuration again.
    Reload,
    /// Ctrl+E or Ctrl+N in the popup: erase the popup trigger and the
    /// keyword (`erase` characters), then show the edit form. The erase comes
    /// first, while the target application still has the focus.
    Editor { erase: usize, request: editor::Request },
}

/// Loads the engine and publishes its snippets. On failure, a loaded list
/// stays, so a bad edit does not empty the popup.
fn load_engine(base: &std::path::Path, ui: &Sender<Ui>, shared: &Arc<Shared>) -> Option<Engine> {
    let ask = UiAsk { ui: ui.clone(), shared: shared.clone() };
    match Engine::load_with(base, Box::new(ask)) {
        Ok(engine) => {
            let mut entries = engine.entries();
            // The engine does not know the file of a match. The file filter
            // of the edit session needs it, so take it from the match files.
            match snip_core::Workspace::open(Some(base.to_path_buf())).and_then(|ws| ws.list_snippets(None, None)) {
                Ok(list) => data::assign_files(&mut entries, &list.snippets),
                Err(e) => popup_core::log::line(&format!("engine: cannot read the files of the matches: {e}")),
            }
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
    // The `,,` popup was open during the current wait.
    let mut popup_in_wait = false;
    loop {
        // A trigger that starts a longer trigger waits for the next key. A
        // confirm key (Space, Tab, Enter) expands it, and the hook swallows
        // that key. With `--prefix-wait-ms N` above 0, the trigger also
        // expands when no key comes in N ms. While the popup is open, the
        // user picks there, so the wait does not end by time. If the popup
        // closes without a key, nothing expands.
        shared.set_waiting(engine.waiting());
        let msg = if engine.waiting() && !prefix_wait.is_zero() {
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
            ToEngine::Insert { id, erase, paragraphs } => {
                let app = focus::foreground_app();
                let rendered =
                    if paragraphs { engine.render_paragraphs_by_id(id, &app) } else { engine.render_by_id(id, &app) };
                match rendered {
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
        self.show_pick(items, None)?.first().copied()
    }

    fn pick_many(&self, items: Vec<PickItem>, checked: Vec<bool>) -> Option<Vec<usize>> {
        self.show_pick(items, Some(checked))
    }
}

impl UiAsk {
    /// Shows a pick list in the popup and waits for the picked indexes.
    /// `checked` gives the list check boxes and their start state.
    fn show_pick(&self, items: Vec<PickItem>, checked: Option<Vec<bool>>) -> Option<Vec<usize>> {
        // From here on the hook swallows typed keys, so the trigger stays
        // as typed. It replaces an open `,,` session.
        if checked.is_some() {
            self.shared.start_pick_many();
        } else {
            self.shared.start_pick();
        }
        // Give the application a moment to draw the last trigger character.
        std::thread::sleep(Duration::from_millis(10));
        let caret = caret::locate(Duration::from_millis(150));
        let (reply, answer) = mpsc::channel();
        popup_core::log::line(&format!("pick: open, {} items, check boxes {}", items.len(), checked.is_some()));
        if self.ui.send(Ui::Pick { items, checked, caret, reply }).is_err() {
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
    /// Show a pick list in the popup and send the picked indexes on
    /// `reply`. `checked` gives the list check boxes.
    Pick { items: Vec<PickItem>, checked: Option<Vec<bool>>, caret: Caret, reply: Sender<Option<Vec<usize>>> },
    /// The espanso configuration folder.
    Base(PathBuf),
    /// A load ended: the number of snippets, or the error.
    Loaded(Result<usize, String>),
    /// Show the edit form in the popup. The engine sends it after it erased
    /// the typed text.
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
            // `,,.`: the hook swallowed the `.`. Type the last keyword into
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
    /// Where the popup was when Ctrl+E or Ctrl+N asked for the form.
    editor_near: Option<Rect>,
    /// The open pick list, if the session is one. `results` then points
    /// into its items.
    pick: Option<Pick>,
    /// The edit form beside the list, while the popup shows one.
    edit: Option<EditSession>,
    /// The focus of the list while the popup has focus: keys then search,
    /// move and insert, as in the popup.
    list_focus: FocusHandle,
    /// The command line and settings file this process started with.
    args: Args,
}

/// The popup after a right click, Ctrl+E or Ctrl+N: it takes focus and shows
/// an edit form beside its list. Esc in the form removes the form, and the
/// popup shows only its list, still with focus.
struct EditSession {
    /// The form and its subscription. `None` in the list-only mode.
    form: Option<(Entity<editor::EditorView>, Subscription)>,
    /// The settings panel, which shows in place of the form. At most one
    /// of `form` and `settings` is set.
    settings: Option<(Entity<settings_view::SettingsView>, Subscription)>,
    /// Where the popup was. The window's top left corner stays there.
    near: Rect,
    /// The window that had focus before. It gets focus back when the edit
    /// ends with Esc in the list, a save, or an insert.
    prev: Option<Handle>,
    /// The popup window had focus once. A later loss of focus ends the edit,
    /// unless it has unsaved changes.
    was_active: bool,
    /// The list shows only the snippets of this match file. It ends with
    /// the session, so the next `,,` popup searches all files.
    file: Option<String>,
    /// The list shows the match files instead of snippets (Ctrl+O, or a
    /// click on the file button).
    files: Option<FilePick>,
    /// Counts the times the form opened. Each opening gets a new animation.
    form_gen: usize,
    /// The tray started the session. No window waits for text, so a pick
    /// in the list-only mode opens the form instead of inserting.
    from_tray: bool,
}

/// The list of match files in the edit session.
#[derive(Default)]
struct FilePick {
    query: String,
    selected: usize,
}

/// A pick list from the expander: the items and where the answer goes.
struct Pick {
    items: Vec<PickItem>,
    /// The state of each check box, if the list has them.
    checked: Option<Vec<bool>>,
    reply: Sender<Option<Vec<usize>>>,
}

impl Pick {
    fn answer(self, indexes: Option<Vec<usize>>) {
        popup_core::log::line(&format!("ui pick: {indexes:?}"));
        let _ = self.reply.send(indexes);
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
            editor_near: None,
            pick: None,
            edit: None,
            list_focus: cx.focus_handle(),
            args: args.clone(),
        }
    }

    /// Starts the edit session at the popup rectangle `near`, or shows the
    /// request in the open session. The window grows to the right (or moves
    /// left at the screen edge) and takes focus.
    fn edit_in_popup(&mut self, request: editor::Request, near: Rect, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.is_none() {
            let prev = focus::foreground().filter(|h| !focus::is_own(*h));
            self.edit = Some(EditSession {
                form: None,
                settings: None,
                near,
                prev,
                was_active: false,
                file: None,
                files: None,
                form_gen: 0,
                from_tray: false,
            });
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
        let view = cx.new(|cx| editor::EditorView::new(base, reload, request, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, ev: &editor::EditorEvent, window, cx| match ev {
            editor::EditorEvent::Close => this.hide_form(window, cx),
            editor::EditorEvent::Done => this.end_edit(true, None, cx),
            editor::EditorEvent::FocusList => this.focus_list(window, cx),
            editor::EditorEvent::Settings => this.mount_settings(window, cx),
        });
        if let Some(edit) = &mut self.edit {
            edit.settings = None;
            edit.form = Some((view, sub));
            edit.form_gen += 1;
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
        let r = if edit.form.is_some() || edit.settings.is_some() {
            rect_at(edit.near, WIDTH + FORM_W, EDIT_H)
        } else {
            rect_at(edit.near, WIDTH, self.height())
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
        Some(editor::Request::Find { trigger: e.trigger.clone(), label: e.label.clone() })
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
    fn insert_from_list(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.as_ref().is_some_and(|e| e.from_tray) {
            self.edit_row(row, window, cx);
            return;
        }
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
            let _ = engine.send(ToEngine::Insert { id, erase: 0, paragraphs: false });
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
        let key_char = k.key_char.clone();
        if self.files_key(m.control, k.key.as_str(), key_char.as_deref(), window, cx) {
            cx.stop_propagation();
            return;
        }
        match (m.control, k.key.as_str()) {
            (true, "o") => self.open_files(window, cx),
            (false, "up") => self.select_row(self.selected.saturating_sub(1), window, cx),
            (false, "down") => self.select_row(self.selected + 1, window, cx),
            // The form asks about unsaved changes first, as for Esc there.
            (false, "escape") => match form {
                Some(view) => view.update(cx, |v, cx| v.try_close(window, cx)),
                None => self.end_edit(true, None, cx),
            },
            (false, "enter") if form.is_some() => self.enter_form(window, cx),
            (false, "enter") => self.insert_from_list(self.selected, window, cx),
            (true, d) if form.is_none() && d.len() == 1 && ("1"..="9").contains(&d) => {
                let row = d.parse::<usize>().unwrap_or(1) - 1;
                self.insert_from_list(row, window, cx);
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
            self.tray_action(action, window, cx);
        }
    }

    /// New snippet or Edit snippets in the tray: the popup opens its edit
    /// session in the middle of the monitor with the mouse. `None` shows the
    /// first snippet of the list, with the focus in the list for a search.
    fn edit_from_tray(&mut self, request: Option<editor::Request>, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.is_none() {
            // A `,,` popup or a pick list that is open now ends.
            if self.open {
                self.shared.deactivate();
                self.hide();
            }
            let (x, y) = focus::cursor_pos();
            let (work, scale) = place::monitor_at(x, y);
            let w = ((WIDTH + FORM_W) * scale).round() as i32;
            let h = (EDIT_H * scale).round() as i32;
            let left = work.left + (work.width() - w).max(0) / 2;
            let top = work.top + (work.height() - h).max(0) / 2;
            let near = Rect { left, top, right: left + 1, bottom: top + 1 };
            self.keyword.clear();
            self.note = None;
            self.snap = None;
            self.selected = 0;
            self.refresh(cx);
            self.edit = Some(EditSession {
                form: None,
                settings: None,
                near,
                prev: None,
                was_active: false,
                file: None,
                files: None,
                form_gen: 0,
                from_tray: true,
            });
            self.shared.set_popup_rect(None);
            popup_core::log::line("ui edit session: start from the tray");
        }
        // The window comes to the front again, also when the session was
        // already open behind another window.
        self.shown = None;
        let browse = request.is_none();
        match request.or_else(|| self.find_request(self.selected)) {
            Some(r) => self.show_form(r, window, cx),
            None => self.place_edit(),
        }
        if browse {
            self.focus_list(window, cx);
        }
        cx.notify();
    }

    fn tray_action(&mut self, action: tray::Action, window: &mut Window, cx: &mut Context<Self>) {
        popup_core::log::line(&format!("tray: {action:?}"));
        let result = match action {
            tray::Action::NewSnippet => {
                self.edit_from_tray(Some(editor::Request::New { trigger: None }), window, cx);
                Ok(())
            }
            tray::Action::EditSnippets => {
                self.edit_from_tray(None, window, cx);
                Ok(())
            }
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
            tray::Action::Settings => {
                self.settings_from_tray(window, cx);
                Ok(())
            }
            tray::Action::Quit => {
                cx.quit();
                Ok(())
            }
        };
        if let Err(e) = result {
            popup_core::log::line(&format!("tray: {action:?} failed: {e}"));
        }
        if matches!(action, tray::Action::Pause(_) | tray::Action::StartAtSignIn(_)) {
            self.update_settings_status(cx);
        }
    }

    /// The Settings button under the list: the settings panel takes the
    /// place of the form. An open form with unsaved edits asks first.
    fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.is_none() {
            return;
        }
        match self.form_view() {
            // The editor sends `EditorEvent::Settings` once the unsaved
            // edits are saved or discarded, or at once without any.
            Some(view) => view.update(cx, |v, cx| v.request(editor::Request::Settings, window, cx)),
            None => self.mount_settings(window, cx),
        }
    }

    /// Shows the settings panel in the right half, in place of the form.
    fn mount_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &self.edit else { return };
        if let Some((view, _)) = &edit.settings {
            let focus = view.read(cx).focus.clone();
            window.focus(&focus, cx);
            return;
        }
        let (running, fixed, status) =
            (settings_view::running_settings(&self.args), self.args.from_cli.clone(), self.settings_status());
        let view = cx.new(|cx| settings_view::SettingsView::new(running, fixed, status, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, ev: &settings_view::SettingsEvent, window, cx| {
            this.settings_event(*ev, window, cx)
        });
        let focus = view.read(cx).focus.clone();
        if let Some(edit) = &mut self.edit {
            edit.form = None;
            edit.settings = Some((view, sub));
            edit.form_gen += 1;
        }
        popup_core::log::line("ui edit session: settings");
        self.place_edit();
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Esc or Close in the settings panel: the popup shows only its list.
    fn hide_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.edit else { return };
        edit.settings = None;
        popup_core::log::line("ui edit session: list only");
        self.place_edit();
        self.focus_list(window, cx);
    }

    /// "Settings..." in the tray: the edit session opens, as for Edit
    /// snippets, and shows the settings panel.
    fn settings_from_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.is_none() {
            self.edit_from_tray(None, window, cx);
        }
        // The window comes to the front again, also when the session was
        // already open behind another window.
        self.shown = None;
        self.show_settings(window, cx);
        self.place_edit();
    }

    fn settings_status(&self) -> settings_view::Status {
        settings_view::Status {
            paused: self.shared.is_paused(),
            sign_in: tray::start_at_sign_in(),
            admin: focus::is_elevated(),
            can_restart_as_admin: tray::can_restart_as_admin(),
        }
    }

    fn update_settings_status(&self, cx: &mut Context<Self>) {
        if let Some((view, _)) = self.edit.as_ref().and_then(|e| e.settings.as_ref()) {
            let status = self.settings_status();
            view.update(cx, |v, cx| v.set_status(status, cx));
        }
    }

    fn settings_event(&mut self, ev: settings_view::SettingsEvent, window: &mut Window, cx: &mut Context<Self>) {
        use settings_view::SettingsEvent as E;
        let action = match ev {
            E::Pause(on) => {
                if let Some(t) = &self.tray {
                    t.set_pause_checked(on);
                }
                tray::Action::Pause(on)
            }
            E::SignIn(on) => {
                if let Some(t) = &self.tray {
                    t.set_sign_in_checked(on);
                }
                tray::Action::StartAtSignIn(on)
            }
            E::RestartAsAdmin => tray::Action::RestartAsAdmin,
            E::OpenLog => tray::Action::OpenLog,
            E::Close => {
                self.hide_settings(window, cx);
                return;
            }
            E::Restart => {
                match tray::restart() {
                    Ok(()) => cx.quit(),
                    Err(e) => popup_core::log::line(&format!("settings: cannot restart: {e}")),
                }
                return;
            }
        };
        self.tray_action(action, window, cx);
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
            // A pick list replaced the `,,` session before its caret lookup
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
            Ui::Pick { items, checked, caret, reply } => {
                if let Some(old) = self.pick.take() {
                    old.answer(None);
                }
                // The hook may have ended the session before the list came.
                if !self.shared.is_picking() {
                    let _ = reply.send(None);
                    return;
                }
                self.end_edit(false, None, cx);
                self.pick = Some(Pick { items, checked, reply });
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
            Ui::Hook(HookEvent::AcceptParagraphs) => self.accept_as(self.selected, true, cx),
            Ui::Hook(HookEvent::Edit) => self.open_from_popup(true),
            Ui::Hook(HookEvent::New { .. }) => self.open_from_popup(false),
            // From the popup: the popup itself shows the form.
            Ui::Editor(request) => match self.editor_near.take() {
                Some(near) => self.edit_in_popup(request, near, window, cx),
                None => popup_core::log::line("ui ignored editor request: the popup is not on screen"),
            },
            Ui::Hook(HookEvent::Toggle(row)) => self.toggle(row.unwrap_or(self.selected), cx),
            Ui::Hook(HookEvent::ToggleAll) => self.toggle_all(cx),
            // A click on a row with a check box checks it; Enter inserts.
            Ui::Hook(HookEvent::Click) => {
                if let Some(row) = self.hovered {
                    if self.pick.as_ref().is_some_and(|p| p.checked.is_some()) {
                        self.selected = row;
                        self.toggle(row, cx);
                    } else {
                        self.accept(row, cx);
                    }
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
                let file = self.edit.as_ref().and_then(|e| e.file.as_deref());
                self.results = data::filter_in(&entries, &self.keyword, &self.trigger, file);
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
        // The edit session always shows its header and status line: the
        // status line holds the Esc hint.
        let (rows, head, status) = match &self.edit {
            Some(_) => (self.list_rows().clamp(1, MAX_ROWS), HEAD_H, STATUS_H),
            None => (self.results.len().clamp(1, MAX_ROWS), 0.0, if self.status().is_some() { STATUS_H } else { 0.0 }),
        };
        PAD * 2.0 + head + rows as f32 * ROW_H + status
    }

    /// The number of rows the list shows: files or snippets.
    fn list_rows(&self) -> usize {
        match self.file_rows() {
            Some(rows) => rows.len(),
            None => self.results.len(),
        }
    }

    /// The rows of the file list while it shows: the file (`None` for all
    /// files) and its text. The query keeps the files whose name has it.
    fn file_rows(&self) -> Option<Vec<(Option<String>, String)>> {
        let pick = self.edit.as_ref()?.files.as_ref()?;
        let entries = self.snap.clone().or_else(|| entries().and_then(Result::ok)).unwrap_or_default();
        let q = pick.query.to_lowercase();
        let all = std::iter::once((None, format!("All files ({})", entries.len())));
        let files = data::file_counts(&entries).into_iter().map(|(f, n)| {
            let text = format!("{f} ({n})");
            (Some(f), text)
        });
        Some(all.chain(files).filter(|(f, text)| (f.is_none() && q.is_empty()) || (f.is_some() && text.to_lowercase().contains(&q))).collect())
    }

    /// Shows the file list in place of the snippets, with focus. The row of
    /// the current filter is selected.
    fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.edit else { return };
        if edit.files.is_none() {
            edit.files = Some(FilePick::default());
            let current = edit.file.clone();
            let row = self.file_rows().and_then(|rows| rows.iter().position(|(f, _)| *f == current)).unwrap_or(0);
            if let Some(pick) = self.edit.as_mut().and_then(|e| e.files.as_mut()) {
                pick.selected = row;
            }
            self.scroll.scroll_to_item(row);
        }
        self.place_edit();
        self.focus_list(window, cx);
    }

    /// Leaves the file list. With `file`, the snippets of that file show
    /// (`Some(None)` means all files). Without, the filter stays as it was.
    fn close_files(&mut self, file: Option<Option<String>>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.edit else { return };
        edit.files = None;
        // Only a new filter changes the rows, so the selected row stays.
        if let Some(file) = file.filter(|f| *f != edit.file) {
            popup_core::log::line(&format!("ui file filter: {}", if file.is_some() { "one file" } else { "all files" }));
            edit.file = file;
            self.refresh(cx);
        } else {
            self.scroll.scroll_to_item(self.selected);
        }
        self.place_edit();
        self.focus_list(window, cx);
    }

    /// Keys while the file list shows. Returns false when the file list
    /// does not show. It takes every other key, so a key for the hidden
    /// snippet rows, such as Ctrl+1, does nothing.
    fn files_key(&mut self, ctrl: bool, key: &str, key_char: Option<&str>, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let n = self.file_rows().map_or(0, |r| r.len());
        let Some(pick) = self.edit.as_mut().and_then(|e| e.files.as_mut()) else { return false };
        match (ctrl, key) {
            (false, "up") => pick.selected = pick.selected.saturating_sub(1),
            (false, "down") => pick.selected = (pick.selected + 1).min(n.saturating_sub(1)),
            (false, "enter") => {
                let row = pick.selected;
                self.pick_file(row, window, cx);
                return true;
            }
            (false, "escape") | (true, "o") => {
                self.close_files(None, window, cx);
                return true;
            }
            (false, "backspace") => {
                pick.query.pop();
                pick.selected = 0;
            }
            (false, _) => match key_char {
                Some(c) if !c.chars().any(char::is_control) => {
                    pick.query.push_str(c);
                    pick.selected = 0;
                }
                _ => return true,
            },
            _ => return true,
        }
        let row = pick.selected;
        self.scroll.scroll_to_item(row);
        self.place_edit();
        cx.notify();
        true
    }

    /// Picks the file of `row` in the file list.
    fn pick_file(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((file, _)) = self.file_rows().and_then(|rows| rows.into_iter().nth(row)) {
            self.close_files(Some(file), window, cx);
        }
    }

    /// What Esc does next in the edit session, for the status line.
    fn esc_hint(&self) -> &'static str {
        match &self.edit {
            Some(e) if e.files.is_some() => "Esc: back to snippets",
            Some(e) if e.form.is_some() || e.settings.is_some() => "Esc: list only",
            Some(_) => "Esc: close",
            None => "",
        }
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
        } else if let Some(c) = self.pick.as_ref().and_then(|p| p.checked.as_ref()) {
            // A list with check boxes always shows its keys and count.
            let n = c.iter().filter(|&&on| on).count();
            let shown = if self.results.is_empty() { "no match   " } else { "" };
            Some(format!("{filter}   {shown}{n} of {} checked · Space checks · Enter inserts", c.len()).trim_start().to_string().into())
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
            match self.find_request(self.selected) {
                Some(r) => r,
                None => {
                    popup_core::log::line("ui ignored edit: no snippet row");
                    return;
                }
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

    /// Checks or unchecks the item of a shown row in a list with check
    /// boxes.
    fn toggle(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(&index) = self.results.get(row) else { return };
        if let Some(Pick { items, checked: Some(checked), .. }) = self.pick.as_mut() {
            popup_core::form::toggle(items, checked, index);
            cx.notify();
        }
    }

    /// Checks every shown row, or unchecks them if all are checked.
    fn toggle_all(&mut self, cx: &mut Context<Self>) {
        let Some(checked) = self.pick.as_mut().and_then(|p| p.checked.as_mut()) else { return };
        let all = self.results.iter().all(|&i| checked[i]);
        for &i in &self.results {
            checked[i] = !all;
        }
        cx.notify();
    }

    fn accept(&mut self, row: usize, cx: &mut Context<Self>) {
        self.accept_as(row, false, cx);
    }

    /// Inserts a row. With `paragraphs` (Ctrl+P), the snippet opens a list
    /// that asks which of its paragraphs to insert.
    fn accept_as(&mut self, row: usize, paragraphs: bool, cx: &mut Context<Self>) {
        if let Some(pick) = &self.pick {
            // With check boxes, the checked items go back. With none
            // checked, the selected row does.
            let shown = self.results.get(row).copied();
            let chosen: Vec<usize> = match &pick.checked {
                Some(c) if c.contains(&true) => (0..c.len()).filter(|&i| c[i]).collect(),
                _ => shown.into_iter().collect(),
            };
            if chosen.is_empty() {
                return;
            }
            if let Some(pick) = self.pick.take() {
                pick.answer(Some(chosen));
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
            "ui accept: row {row} of {}, insertable {}, paragraphs {paragraphs}",
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
        let _ = self.engine.send(ToEngine::Insert { id: e.id, erase, paragraphs });
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
        // The check box of each row, in a list that has them.
        let checks: Vec<Option<bool>> = match self.pick.as_ref().and_then(|p| p.checked.as_ref()) {
            Some(c) => self.results.iter().map(|&i| Some(c[i])).collect(),
            None => vec![None; count],
        };
        let editing = self.edit.is_some();
        let rows = lines.into_iter().enumerate().map(|(row, (title, badge, exact))| {
            // Rows 1 to 9 have a number for Ctrl+1 to Ctrl+9.
            let number = if row < 9 { (row + 1).to_string() } else { String::new() };
            let selected = row == self.selected;
            let is_exact = exact.is_some();
            let check = checks.get(row).copied().flatten();
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
                                this.insert_from_list(row, window, cx);
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
                    // A check box: a frame, filled when checked.
                    .when_some(check, |d, on| {
                        d.child(
                            div()
                                .size(px(12.0))
                                .flex_none()
                                .rounded_sm()
                                .border_1()
                                .border_color(if on { theme.primary } else { theme.muted_foreground })
                                .when(on, |d| d.bg(theme.primary)),
                        )
                    })
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
        // In the file list, its rows replace the snippets.
        let rows: Vec<AnyElement> = match self.file_rows() {
            Some(files) => self.render_file_rows(files, &theme, cx),
            None => rows.map(IntoElement::into_any_element).collect(),
        };
        let body = div().flex_1().min_h_0().flex().flex_col().p(px(PAD)).child(
            div().id("results").flex_1().overflow_y_scroll().track_scroll(&self.scroll).children(rows),
        );

        let root = div().size_full().flex().bg(theme.background).text_color(theme.foreground);
        let Some(edit) = &self.edit else {
            let list = body.children(status.map(|status| {
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
            return root.child(list.size_full());
        };
        // In the edit session the list has a header with the file filter,
        // and its status line always shows what Esc does next.
        let file_label = match (&edit.files, &edit.file) {
            (Some(_), _) => "Pick a file".to_string(),
            (None, Some(f)) => f.clone(),
            (None, None) => "All files".to_string(),
        };
        let header = div()
            .h(px(HEAD_H))
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("file-filter")
                    .label(file_label)
                    .icon(IconName::ChevronDown)
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.edit.as_ref().is_some_and(|e| e.files.is_some()) {
                            this.close_files(None, window, cx);
                        } else {
                            this.open_files(window, cx);
                        }
                    })),
            )
            .child(div().flex_1())
            .child(div().text_xs().text_color(theme.muted_foreground).child("Ctrl+O"));
        let status_line = div()
            .h(px(STATUS_H))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .mx(px(PAD))
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                Button::new("open-settings")
                    .icon(IconName::Settings)
                    .label("Settings")
                    .ghost()
                    .xsmall()
                    .selected(edit.settings.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.show_settings(window, cx))),
            )
            .child(div().flex_1().min_w_0().truncate().child(match &edit.files {
                Some(pick) => SharedString::from(pick.query.clone()),
                None => status.unwrap_or_default(),
            }))
            .child(div().flex_none().child(self.esc_hint()));
        // The list takes keys when it has focus. A click beside the rows
        // gives it focus. A colored edge shows the focus.
        let list_focused = self.list_focus.is_focused(window);
        let list = div()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .child(status_line)
            .track_focus(&self.list_focus)
            .on_key_down(cx.listener(Self::list_key))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, window, cx| this.focus_list(window, cx)));
        let root = root
            // Alt+Up and Alt+Down select the row above or below, from the
            // form too. An open form shows it. Ctrl+O opens or closes the
            // file list from anywhere in the session.
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    let k = &ev.keystroke;
                    let m = k.modifiers;
                    if m.control && !m.alt && !m.shift && !m.platform && k.key == "o" {
                        cx.stop_propagation();
                        if this.edit.as_ref().is_some_and(|e| e.files.is_some()) {
                            this.close_files(None, window, cx);
                        } else {
                            this.open_files(window, cx);
                        }
                        return;
                    }
                    if !m.alt || m.control || m.shift || m.platform {
                        return;
                    }
                    if this.edit.as_ref().is_some_and(|e| e.files.is_some()) {
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
        let side: Option<AnyView> = match (&edit.form, &edit.settings) {
            (Some((view, _)), _) => Some(view.clone().into()),
            (None, Some((view, _))) => Some(view.clone().into()),
            (None, None) => None,
        };
        match side {
            None => root.child(list.size_full()),
            // The list keeps its width on the left, and the form fills the
            // rest. The form fades in and slides in a little from the list,
            // so the eye sees where it came from. The window itself does not
            // animate: a stepped resize flickers.
            Some(view) => root
                .child(
                    list.w(px(WIDTH))
                        .flex_none()
                        .h_full()
                        .border_r_2()
                        .border_color(if list_focused { theme.primary } else { theme.border }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .relative()
                        .child(view)
                        .with_animation(
                            ("form-in", edit.form_gen),
                            Animation::new(FORM_IN).with_easing(ease_out_quint()),
                            |d, t| d.opacity(t).left(px(16.0 * (1.0 - t))),
                        ),
                ),
        }
    }
}

impl Popup {
    /// The rows of the file list. The current filter has the colored bar.
    fn render_file_rows(&self, files: Vec<(Option<String>, String)>, theme: &component::Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(edit) = &self.edit else { return Vec::new() };
        let selected = edit.files.as_ref().map_or(0, |p| p.selected);
        let count = files.len();
        files
            .into_iter()
            .enumerate()
            .map(|(row, (file, text))| {
                let is_selected = row == selected;
                let is_current = file == edit.file;
                div()
                    .id(("file", row))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(ROW_H))
                    .pl_1()
                    .pr_2()
                    .when(row + 1 < count, |d| d.border_b_1().border_color(theme.border))
                    .when(is_selected, |d| d.bg(theme.primary.opacity(0.3)))
                    .when(!is_selected, |d| d.hover(|s| s.bg(theme.list_hover)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.pick_file(row, window, cx)))
                    .child(div().w(px(3.0)).h(px(ROW_H - 8.0)).flex_none().rounded_full().when(is_current, |d| d.bg(theme.primary)))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_sm()
                            .truncate()
                            .when(file.is_none(), |d| d.text_color(theme.muted_foreground))
                            .when(is_current, |d| d.font_weight(FontWeight::SEMIBOLD))
                            .child(text),
                    )
                    .into_any_element()
            })
            .collect()
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
        HookEvent::AcceptParagraphs => "accept as paragraphs",
        HookEvent::Edit => "edit",
        HookEvent::New { .. } => "new",
        HookEvent::Toggle(_) => "toggle",
        HookEvent::ToggleAll => "toggle all",
        HookEvent::Cancel => "cancel",
    }
}

fn main() {
    popup_core::init_dpi();
    let args = Args::parse_with(&popup_core::settings::Settings::load());
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
