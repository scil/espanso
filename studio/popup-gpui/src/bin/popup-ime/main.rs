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
#![windows_subsystem = "windows"]

mod form_view;

use std::collections::HashMap;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
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

/// The snippets for the popup. The engine thread sets it after it loads the
/// configuration.
static ENTRIES: OnceLock<Result<Vec<Entry>, String>> = OnceLock::new();

/// Messages to the engine thread.
enum ToEngine {
    Key(Input),
    /// The user picked a snippet in the popup. `erase` is the number of
    /// characters of the popup trigger and keyword before the caret.
    Insert { id: i32, erase: usize },
}

/// Owns the expansion engine. espanso numbers its matches with a counter per
/// thread, so the engine is loaded and used on this one thread.
fn engine_thread(base: Option<PathBuf>, rx: Receiver<ToEngine>, shared: Arc<Shared>, ui: Sender<Ui>) {
    let loaded = snip_core::Workspace::open(base)
        .map_err(|e| e.to_string())
        .and_then(|ws| {
            let ask = UiAsk { ui: ui.clone(), shared: shared.clone() };
            Engine::load_with(ws.base(), Box::new(ask)).map_err(|e| format!("{e:#}"))
        });
    let mut engine = match loaded {
        Ok(engine) => {
            let entries = engine.entries();
            popup_core::log::line(&format!("engine: {} matches loaded", entries.len()));
            let _ = ENTRIES.set(Ok(entries));
            engine
        }
        Err(e) => {
            popup_core::log::line(&format!("engine: cannot load the configuration: {e}"));
            let _ = ENTRIES.set(Err(e));
            return;
        }
    };
    timing::mark("data-loaded");
    if focus::process_running("espanso.exe") {
        popup_core::log::line("engine: espanso.exe runs too, so triggers may expand twice");
    }
    for msg in rx {
        match msg {
            ToEngine::Key(input) => {
                let Some(x) = engine.key(input, focus::foreground_app) else { continue };
                if shared.is_active() {
                    shared.deactivate();
                    let _ = ui.send(Ui::Hook(HookEvent::Cancel));
                }
                // The hook sees the key before the application does. Wait
                // until the application has the last trigger character.
                std::thread::sleep(Duration::from_millis(10));
                perform(&x);
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
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Ok(msg) = self.ui_rx.try_recv() {
            self.handle(msg, window, cx);
        }
        if self.open && !self.filled && ENTRIES.get().is_some() {
            self.refresh(cx);
            self.place_and_show();
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
                self.refresh(cx);
                self.place_and_show();
                window.on_next_frame(move |_, _| timing::since("open-to-frame", pressed));
            }
            Ui::Form { form, caret, reply } => {
                // Open the window after this update: gpui does not open a
                // window while it updates another one.
                cx.defer(move |cx| form_view::open(form, caret, reply, cx));
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
        match ENTRIES.get() {
            Some(Ok(entries)) => {
                self.filled = true;
                self.results = data::filter(entries, &self.keyword, &self.trigger);
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

    fn accept(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(Ok(entries)) = ENTRIES.get() else {
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
        let snips = match ENTRIES.get() {
            Some(Ok(s)) => Some(s),
            _ => None,
        };
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
                format!("{}{}   no match   Esc close", self.trigger, self.keyword).into()
            }
            None => format!(
                "{}{}   {} matches   ↑↓ select   Enter insert   Esc close",
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
        let (base, shared, ui_tx) = (args.base.clone(), shared.clone(), ui_tx.clone());
        std::thread::spawn(move || engine_thread(base, engine_rx, shared, ui_tx));
    }
    let keys = engine_tx.clone();
    let on_input: hook::OnInput = Box::new(move |input| {
        let _ = keys.send(ToEngine::Key(input));
    });
    let hook_rx = hook::start(&args.trigger, shared.clone(), args.allow_injected, Some(on_input));
    std::thread::spawn(move || controller(hook_rx, ui_tx));

    gpui_kit::application().run(move |cx| {
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
