//! The snippet editor window. See `studio/EDITOR.md`, step 3.
//!
//! Left: a search box and the snippet list. Right: one match, as a form or
//! as YAML text. The YAML text is the source of truth. The form reads it with
//! `snip_core::matchedit` when the Form tab opens, and writes it back as the
//! smallest change when the YAML tab opens or on Save.
//!
//! Saves go through `snip_core::Workspace`, which checks the new file with
//! espanso's own loader and refuses a write after a change on disk. After a
//! save, `reload` makes the engine load the configuration again.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme, IconName, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use popup_core::focus::{self, Handle};
use popup_core::{place, Rect};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_norway::Value;
use snip_core::matchedit::{self, MatchForm, Param, OUTPUT_KEYS, VAR_TYPES};
use snip_core::workspace::Snippet;
use snip_core::yamlfile::MatchFile;
use snip_core::{SnipError, Workspace};

const LIST_W: f32 = 320.0;
/// The window size in logical pixels. A compact window has no list.
const FULL_W: f32 = 1080.0;
const COMPACT_W: f32 = FULL_W - LIST_W;
const WINDOW_H: f32 = 780.0;
const MAX_ROWS: usize = 300;
const DEFAULT_FILE: &str = "match/base.yml";
/// The label that marks the copy of the snippet that the Test button renders.
const TEST_LABEL: &str = "__snip_editor_test__";

/// What the editor shows when it opens.
#[derive(Debug, Clone)]
pub enum Request {
    /// The list, with the first snippet selected.
    Browse,
    /// The snippet with these triggers (joined with spaces, or the regex)
    /// and label, as the popup shows it.
    Find { trigger: String, label: String },
    /// A new snippet. `trigger` fills the first trigger.
    New { trigger: Option<String> },
}

/// A step that would drop unsaved edits. It waits until the user saves or
/// discards them.
enum Pending {
    /// Open this snippet (file, index) from the list.
    Open(String, usize),
    /// Show a request from the popup, the tray or the New button.
    Request(Request),
}

/// Opens the editor window, or brings the open one to the front and shows
/// the request there.
///
/// `near` is the screen rectangle of the popup, when the request comes from
/// the popup. The editor then opens compact, without the list, with its top
/// left corner where the popup was, on the same monitor. It stays hidden
/// until it is in place, so it never shows up somewhere else first.
pub fn open(
    handle: &mut Option<WindowHandle<EditorView>>,
    base: PathBuf,
    reload: Rc<dyn Fn()>,
    request: Request,
    near: Option<Rect>,
    cx: &mut App,
) {
    let place = near.map(place_near);
    if let Some(h) = handle {
        let req = request.clone();
        let shown = h.update(cx, |view, window, cx| {
            if near.is_some() {
                view.compact = true;
            } else if view.compact {
                view.toggle_list(window, cx);
            }
            view.request(req, window, cx);
            view.front(place);
        });
        if shown.is_ok() {
            return;
        }
    }
    let compact = near.is_some();
    let width = if compact { COMPACT_W } else { FULL_W };
    // The window opens hidden and shows up only in its place: next to the
    // popup, where it was last time, or in the middle of the monitor with
    // the mouse.
    let place = place.or_else(|| Some(full_place()));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(width), px(WINDOW_H)), cx)),
        titlebar: Some(TitlebarOptions { title: Some("snip editor".into()), ..Default::default() }),
        focus: false,
        show: false,
        kind: WindowKind::Normal,
        ..Default::default()
    };
    let opened = cx.open_window(options, move |window, cx| {
        cx.new(|cx| EditorView::new(base, reload, request, compact, place, window, cx))
    });
    match opened {
        Ok(h) => *handle = Some(h),
        Err(e) => popup_core::log::line(&format!("editor: cannot open the window: {e:#}")),
    }
}

/// The match on the right side.
struct Current {
    /// The file of the match, or the target file of a new match.
    file: String,
    /// `None` for a new match.
    index: Option<usize>,
    hash: String,
    read_only: bool,
    /// The line of the match in its file, 0 for a new match.
    line: usize,
}

/// The inputs of one variable parameter.
enum ParamInput {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
    /// The value is a list or mapping that the form cannot show.
    YamlOnly,
}

struct VarInputs {
    name: Entity<InputState>,
    /// (path, kind, input) for each parameter of the type, then
    /// `depends_on`.
    params: Vec<(Vec<&'static str>, Param, ParamInput)>,
}

/// The text inputs of the form. Flags and choices live in `form`.
struct FormInputs {
    comments: Entity<TextareaState>,
    triggers: Vec<Entity<InputState>>,
    regex: Entity<InputState>,
    label: Entity<InputState>,
    search_terms: Entity<TextareaState>,
    body: Entity<TextareaState>,
    vars: Vec<VarInputs>,
    _subs: Vec<Subscription>,
}

pub struct EditorView {
    ws: Option<Workspace>,
    reload: Rc<dyn Fn()>,
    hwnd: Option<Handle>,
    /// Without the list, for a quick edit from the popup.
    compact: bool,
    search: Entity<InputState>,
    all: Vec<Snippet>,
    shown: Vec<usize>,
    selected: Option<usize>,
    files: Vec<String>,
    /// Every match file, packages included. The Test button imports them.
    match_files: Vec<String>,
    /// The names of the global variables in all match files.
    global_names: HashSet<String>,
    /// The list shows only this file.
    file_filter: Option<String>,
    target_file: String,
    file_menu: bool,
    current: Option<Current>,
    yaml: Entity<EditorState>,
    /// 0 is "Snippet", 1 is "More options".
    tab: usize,
    /// The YAML text that the form shows. A YAML change event with this
    /// text came from the form itself.
    last_yaml: String,
    form: Option<MatchForm>,
    inputs: Option<FormInputs>,
    /// The parse error of the YAML text. The form keeps the last valid
    /// state and does not write while it is set.
    form_error: Option<String>,
    status: String,
    status_error: bool,
    confirm_delete: bool,
    /// The YAML text as it is in the file, to tell unsaved edits.
    saved_yaml: String,
    /// Esc was pressed once with unsaved edits.
    confirm_close: bool,
    /// The step that waits for Save or Discard of unsaved edits.
    pending: Option<Pending>,
    /// The result of the Test button.
    test_output: Option<Result<String, String>>,
    testing: bool,
    /// The window title now, to set it only when it changes.
    title: String,
    list_scroll: ScrollHandle,
    form_scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EditorView {
    fn new(
        base: PathBuf,
        reload: Rc<dyn Fn()>,
        request: Request,
        compact: bool,
        place: Option<Rect>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search triggers, labels, text, files"));
        let yaml = cx.new(|cx| EditorState::new(window, cx).language("yaml"));
        let subs = vec![
            cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.filter(cx);
                }
            }),
            cx.subscribe_in(&yaml, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::Change) && this.yaml_text(cx) != this.last_yaml {
                    this.read_form(window, cx);
                }
            }),
        ];
        let hwnd = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(Handle(h.hwnd.get())),
            _ => None,
        };
        let (ws, status, status_error) = match Workspace::open(Some(base)) {
            Ok(ws) => (Some(ws), String::new(), false),
            Err(e) => (None, format!("Cannot open the espanso folder: {e}"), true),
        };
        let mut view = EditorView {
            ws,
            reload,
            hwnd,
            compact,
            search,
            all: Vec::new(),
            shown: Vec::new(),
            selected: None,
            files: Vec::new(),
            match_files: Vec::new(),
            global_names: HashSet::new(),
            file_filter: None,
            target_file: last_file().unwrap_or_else(|| DEFAULT_FILE.into()),
            file_menu: false,
            current: None,
            yaml,
            tab: 0,
            last_yaml: String::new(),
            form: None,
            inputs: None,
            form_error: None,
            status,
            status_error,
            confirm_delete: false,
            saved_yaml: String::new(),
            confirm_close: false,
            pending: None,
            test_output: None,
            testing: false,
            title: String::new(),
            list_scroll: ScrollHandle::new(),
            form_scroll: ScrollHandle::new(),
            _subs: subs,
        };
        // The close button of the title bar asks first, like Esc.
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| weak.update(cx, |view, cx| view.may_close(cx)).unwrap_or(true));
        view.refresh_list(cx);
        view.load(request, window, cx);
        view.front(place);
        view
    }

    /// Whether the YAML differs from the file.
    fn dirty(&self, cx: &App) -> bool {
        self.current.is_some() && self.yaml_text(cx).trim_end() != self.saved_yaml.trim_end()
    }

    /// Shows a request, but asks first if that drops unsaved edits.
    pub fn request(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        self.go(Pending::Request(request), window, cx);
    }

    /// Runs `step` now, or keeps it for the Save / Discard bar when there
    /// are unsaved edits.
    fn go(&mut self, step: Pending, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty(cx) {
            self.pending = Some(step);
            cx.notify();
            return;
        }
        self.run(step, window, cx);
    }

    fn run(&mut self, step: Pending, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = None;
        match step {
            Pending::Open(file, index) => self.open_snippet(&file, index, window, cx),
            Pending::Request(r) => self.load(r, window, cx),
        }
    }

    /// Before the window closes: the first try with unsaved edits only
    /// warns. Returns whether the window may close.
    fn may_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dirty(cx) && !self.confirm_close {
            self.confirm_close = true;
            self.set_status("Unsaved changes. Close again to drop them. Ctrl+S saves.", true);
            cx.notify();
            return false;
        }
        self.remember_rect();
        true
    }

    /// Stores the window rectangle for the next start. A compact window
    /// is a quick edit next to the popup, so it does not count.
    fn remember_rect(&self) {
        let (Some(h), false) = (self.hwnd, self.compact) else { return };
        if let Some(r) = focus::window_rect(h) {
            // A minimized window sits at -32000.
            if r.left > -30000 && r.width() > 200 && r.height() > 200 {
                write_setting(RECT_FILE, &format!("{} {} {} {}", r.left, r.top, r.right, r.bottom));
            }
        }
    }

    /// Moves the window to `place` (if any) and brings it to the front.
    /// Windows lets a background process do this right after a tray click or
    /// a popup key. gpui handles the messages of these calls with its app
    /// state, which this thread holds now, so another thread makes them.
    fn front(&self, place: Option<Rect>) {
        if let Some(h) = self.hwnd {
            std::thread::spawn(move || {
                if let Some(r) = place {
                    focus::move_window(h, r);
                }
                focus::bring_to_front(h);
            });
        }
    }

    /// Shows or hides the snippet list. The window grows or shrinks by the
    /// width of the list.
    fn toggle_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compact = !self.compact;
        let mut s = window.bounds().size;
        s.width = if self.compact { s.width - px(LIST_W) } else { s.width + px(LIST_W) };
        window.resize(s);
        cx.notify();
    }

    fn refresh_list(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = &self.ws else { return };
        let listed = ws.list_snippets(None, None);
        let files = ws.list_files();
        let base = ws.base().to_path_buf();
        match listed {
            Ok(list) => {
                self.all = list.snippets;
                if let Some(p) = list.file_errors.first() {
                    self.set_status(format!("{}: {}", p.file, p.message), true);
                }
            }
            Err(e) => self.set_status(format!("Cannot list snippets: {e}"), true),
        }
        if let Ok(files) = files {
            let matches: Vec<_> = files.into_iter().filter(|f| f.kind == "match").collect();
            self.files = matches.iter().filter(|f| !f.read_only).map(|f| f.path.clone()).collect();
            self.match_files = matches.iter().map(|f| f.path.clone()).collect();
            self.global_names = self
                .match_files
                .iter()
                .filter_map(|f| std::fs::read_to_string(base.join(f)).ok())
                .filter_map(|text| MatchFile::parse(&text).ok())
                .flat_map(|m| m.global_vars.into_iter().map(|(name, _)| name))
                .collect();
        }
        if self.file_filter.as_ref().is_some_and(|f| !self.all.iter().any(|s| &s.file == f)) {
            self.file_filter = None;
        }
        self.filter(cx);
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let q = self.search.read(cx).value().to_lowercase();
        self.shown = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, s)| self.file_filter.as_ref().is_none_or(|f| &s.file == f))
            .filter(|(_, s)| q.is_empty() || matches_query(s, &q))
            .map(|(i, _)| i)
            .collect();
        cx.notify();
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.status_error = error;
    }

    /// Shows a request on the right side.
    fn load(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_delete = false;
        self.pending = None;
        self.test_output = None;
        match request {
            Request::Browse => {
                if self.current.is_none() {
                    if let Some(&i) = self.shown.first() {
                        let (file, index) = (self.all[i].file.clone(), self.all[i].index);
                        self.open_snippet(&file, index, window, cx);
                    }
                }
            }
            Request::Find { trigger, label } => {
                // The list may be older than the engine's snippets.
                self.refresh_list(cx);
                let same = |s: &Snippet| s.triggers.join(" ") == trigger || s.regex.as_deref() == Some(trigger.as_str());
                let found = self
                    .all
                    .iter()
                    .find(|s| same(s) && s.label.as_deref().unwrap_or("") == label)
                    .or_else(|| self.all.iter().find(|s| same(s)))
                    .map(|s| (s.file.clone(), s.index));
                match found {
                    Some((file, index)) => self.open_snippet(&file, index, window, cx),
                    None => self.set_status(format!("No snippet in the files has the trigger {trigger}."), true),
                }
            }
            Request::New { trigger } => {
                let trigger = trigger.unwrap_or_default();
                let text = format!("trigger: {}\nreplace: \"\"", quote(&trigger));
                self.current = Some(Current {
                    file: self.target_file.clone(),
                    index: None,
                    hash: String::new(),
                    read_only: false,
                    line: 0,
                });
                self.selected = None;
                self.set_yaml(&text, window, cx);
                self.tab = 0;
                self.form = None;
                self.inputs = None;
                self.read_form(window, cx);
                self.saved_yaml = text;
                self.set_status("New snippet. Ctrl+S saves it.", false);
            }
        }
        // The cursor goes to the replacement text, or to the search box when
        // no snippet is open.
        self.tab = 0;
        self.confirm_close = false;
        match &self.inputs {
            Some(inputs) => inputs.body.update(cx, |s, cx| s.focus(window, cx)),
            None => self.search.update(cx, |s, cx| s.focus(window, cx)),
        }
        cx.notify();
    }

    fn open_snippet(&mut self, file: &str, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = &self.ws else { return };
        match ws.get_snippet(file, index) {
            Ok(d) => {
                self.selected = self.all.iter().position(|s| s.file == file && s.index == index);
                if let Some(pos) = self.selected.and_then(|i| self.shown.iter().position(|&s| s == i)) {
                    self.list_scroll.scroll_to_item(pos);
                }
                self.current = Some(Current {
                    file: d.snippet.file.clone(),
                    index: Some(index),
                    hash: d.snippet.hash.clone(),
                    read_only: d.snippet.read_only,
                    line: d.snippet.line,
                });
                self.test_output = None;
                self.confirm_close = false;
                self.set_yaml(&d.yaml, window, cx);
                self.saved_yaml = d.yaml.clone();
                self.confirm_delete = false;
                self.form = None;
                self.inputs = None;
                self.read_form(window, cx);
                let ro = if d.snippet.read_only { "  (read-only package file)" } else { "" };
                self.set_status(format!("{}, line {}{ro}", d.snippet.file, d.snippet.line), false);
            }
            Err(e) => self.set_status(format!("Cannot read {file} #{index}: {e}"), true),
        }
        cx.notify();
    }

    fn set_yaml(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_string();
        self.yaml.update(cx, |s, cx| s.set_value(text, window, cx));
    }

    fn yaml_text(&self, cx: &App) -> String {
        self.yaml.read(cx).value().to_string()
    }

    /// YAML to form: parses the text and rebuilds the inputs. If the text
    /// is not valid, the form keeps its last state and shows the error.
    fn read_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.yaml_text(cx);
        self.last_yaml = text.clone();
        match MatchForm::from_yaml(&text) {
            Ok(form) => {
                self.form_error = None;
                self.build_inputs(form, window, cx);
            }
            Err(e) => self.form_error = Some(e.to_string()),
        }
        cx.notify();
    }

    /// Form to YAML: writes the changed keys into the text after each form
    /// edit.
    fn sync_to_yaml(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.form_error.is_some() {
            self.set_status("The YAML is not valid. Fix it first, the form does not write until then.", true);
            cx.notify();
            return;
        }
        let Some(form) = self.collect(cx) else { return };
        let text = self.yaml_text(cx);
        match form.apply(&text) {
            Ok(new) => {
                if new.trim_end() != text.trim_end() {
                    self.set_yaml(&new, window, cx);
                    // Read the text back, so the change event of the YAML
                    // box sees exactly what it holds.
                    self.last_yaml = self.yaml_text(cx);
                }
                if self.status_error {
                    self.set_status("", false);
                }
            }
            Err(e) => self.set_status(format!("The form cannot write this change: {e}"), true),
        }
        cx.notify();
    }

    /// The form with the text of every input.
    fn collect(&self, cx: &App) -> Option<MatchForm> {
        let mut form = self.form.clone()?;
        let inputs = self.inputs.as_ref()?;
        let line = |s: &Entity<InputState>| s.read(cx).value().to_string();
        let area = |s: &Entity<TextareaState>| s.read(cx).value().to_string();
        form.comments = lines(&area(&inputs.comments));
        form.triggers = inputs.triggers.iter().map(line).filter(|t| !t.is_empty()).collect();
        form.regex = line(&inputs.regex);
        form.label = line(&inputs.label);
        form.search_terms = lines(&area(&inputs.search_terms));
        form.body = area(&inputs.body);
        for (var, vi) in form.vars.iter_mut().zip(&inputs.vars) {
            matchedit::var_set(var, &["name"], &line(&vi.name), Param::Text);
            for (path, kind, input) in &vi.params {
                let text = match input {
                    ParamInput::Line(s) => line(s),
                    ParamInput::Area(s) => area(s),
                    ParamInput::YamlOnly => continue,
                };
                matchedit::var_set(var, path, &text, *kind);
            }
        }
        Some(form)
    }

    /// Rebuilds the inputs from a form. Structural edits (add a trigger,
    /// change a variable type) collect the form, change it and call this.
    fn build_inputs(&mut self, form: MatchForm, window: &mut Window, cx: &mut Context<Self>) {
        let subs = std::cell::RefCell::new(Vec::new());
        let on_change = |this: &mut Self, ev: &InputEvent, window: &mut Window, cx: &mut Context<Self>| {
            if matches!(ev, InputEvent::Change) {
                this.sync_to_yaml(window, cx);
            }
        };
        let line = |text: &str, ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (text, ph) = (text.to_string(), ph.to_string());
            let e = cx.new(|cx| InputState::new(window, cx).default_value(text).placeholder(ph));
            subs.borrow_mut().push(cx.subscribe_in(&e, window, move |this, _, ev, window, cx| on_change(this, ev, window, cx)));
            e
        };
        let area = |text: &str, ph: &str, rows: usize, window: &mut Window, cx: &mut Context<Self>| {
            let (text, ph) = (text.to_string(), ph.to_string());
            let e = cx.new(|cx| TextareaState::new(window, cx).rows(rows).default_value(text).placeholder(ph));
            subs.borrow_mut().push(cx.subscribe_in(&e, window, move |this, _, ev, window, cx| on_change(this, ev, window, cx)));
            e
        };
        let mut triggers: Vec<_> =
            form.triggers.iter().map(|t| line(t, ";;sig", window, cx)).collect();
        if triggers.is_empty() {
            triggers.push(line("", ";;sig", window, cx));
        }
        let vars = form
            .vars
            .iter()
            .map(|var| {
                let kind = matchedit::var_get(var, &["type"]);
                let spec = VAR_TYPES.iter().find(|(t, _)| *t == kind).map(|(_, p)| *p).unwrap_or(&[]);
                let mut params: Vec<(Vec<&'static str>, Param, ParamInput)> = spec
                    .iter()
                    .map(|(key, p)| (vec!["params", *key], *p))
                    .chain(std::iter::once((vec!["depends_on"], Param::Lines)))
                    .filter(|(_, p)| *p != Param::Bool)
                    .map(|(path, p)| {
                        let input = if !form_can_show(var, &path, p) {
                            ParamInput::YamlOnly
                        } else {
                            let text = matchedit::var_get(var, &path);
                            match p {
                                Param::Multiline | Param::Lines => {
                                    ParamInput::Area(area(&text, path.last().unwrap(), 3, window, cx))
                                }
                                _ => ParamInput::Line(line(&text, path.last().unwrap(), window, cx)),
                            }
                        };
                        (path, p, input)
                    })
                    .collect();
                params.shrink_to_fit();
                VarInputs { name: line(&matchedit::var_get(var, &["name"]), "name", window, cx), params }
            })
            .collect();
        self.inputs = Some(FormInputs {
            comments: area(&form.comments.join("\n"), "# lines above the match", 2, window, cx),
            triggers,
            regex: line(&form.regex, "A regex trigger replaces the triggers", window, cx),
            label: line(&form.label, "Name shown in the ;; popup (optional)", window, cx),
            search_terms: area(&form.search_terms.join("\n"), "search terms, one per line", 2, window, cx),
            body: area(&form.body, "Text to insert. {{name}} inserts a variable.", 8, window, cx),
            vars,
            _subs: Vec::new(),
        });
        if let Some(i) = &mut self.inputs {
            i._subs = subs.into_inner();
        }
        self.form = Some(form);
        cx.notify();
    }

    /// Collects the form, lets `f` change it, and rebuilds the inputs.
    fn edit_form(&mut self, window: &mut Window, cx: &mut Context<Self>, f: impl FnOnce(&mut MatchForm)) {
        if self.form_error.is_some() {
            self.sync_to_yaml(window, cx);
            return;
        }
        if let Some(mut form) = self.collect(cx) {
            f(&mut form);
            self.build_inputs(form, window, cx);
            self.sync_to_yaml(window, cx);
        }
    }

    /// Changes the type of a variable. The parameters of the old type go.
    fn set_var_type(&mut self, vi: usize, kind: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_form(window, cx, |f| {
            if let Some(v) = f.vars.get_mut(vi) {
                if matchedit::var_get(v, &["type"]) != kind {
                    let name = matchedit::var_get(v, &["name"]);
                    *v = matchedit::new_var(&name, kind);
                }
            }
        });
    }

    /// Saves the match. Returns whether it is saved.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (Some(ws), Some(cur)) = (&self.ws, &self.current) else { return false };
        if cur.read_only {
            self.set_status("Package files are read-only.", true);
            cx.notify();
            return false;
        }
        let text = self.yaml_text(cx);
        let result = match cur.index {
            Some(i) => ws.update_snippet(&cur.file, i, &cur.hash, &text),
            None => ws.create_snippet(&self.target_file, &text),
        };
        match result {
            Ok(d) => {
                if cur.index.is_none() {
                    remember_file(&self.target_file);
                }
                let (file, index) = (d.snippet.file.clone(), d.snippet.index);
                (self.reload)();
                self.refresh_list(cx);
                self.open_snippet(&file, index, window, cx);
                self.set_status(format!("Saved to {file}. snip reloads the snippets."), false);
                cx.notify();
                true
            }
            Err(e) => {
                self.set_status(save_error(&e), true);
                cx.notify();
                false
            }
        }
    }

    /// Renders the YAML as it is now, without a save and without an
    /// insert. A copy of the configuration with this snippet loads on a
    /// background thread, because shell and script variables can be slow.
    fn run_test(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = &self.ws else { return };
        if self.testing {
            return;
        }
        let text = self.yaml_text(cx);
        let yaml = MatchForm::from_yaml(&text).and_then(|mut f| {
            f.label = TEST_LABEL.into();
            f.apply(&text)
        });
        let yaml = match yaml {
            Ok(y) => y,
            Err(e) => {
                self.test_output = Some(Err(format!("The YAML is not valid: {e}")));
                cx.notify();
                return;
            }
        };
        let base = ws.base().to_path_buf();
        let files = self.match_files.clone();
        self.testing = true;
        self.test_output = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { test_render(&base, &files, &yaml) }).await;
            let _ = this.update(cx, |view, cx| {
                view.testing = false;
                view.test_output = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the file of the match at its line: in VS Code when it is on the
    /// PATH, otherwise in the default program for the file.
    fn open_in_file(&mut self, cx: &mut Context<Self>) {
        let (Some(ws), Some(cur)) = (&self.ws, &self.current) else { return };
        let path = ws.base().join(&cur.file);
        let file = cur.file.clone();
        match open_at_line(&path, cur.line) {
            Ok(how) => self.set_status(format!("Opened {file} in {how}."), false),
            Err(e) => self.set_status(format!("Cannot open {file}: {e}"), true),
        }
        cx.notify();
    }

    /// Up or Down in the search box: opens the next or previous snippet of
    /// the list.
    fn list_step(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.shown.is_empty() {
            return;
        }
        let pos = self.selected.and_then(|i| self.shown.iter().position(|&s| s == i));
        let next = match pos {
            None => 0,
            Some(p) if down => (p + 1).min(self.shown.len() - 1),
            Some(p) => p.saturating_sub(1),
        };
        if Some(next) == pos {
            return;
        }
        let s = &self.all[self.shown[next]];
        let step = Pending::Open(s.file.clone(), s.index);
        self.go(step, window, cx);
    }

    fn search_focused(&self, window: &Window, cx: &App) -> bool {
        self.search.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Warnings for the triggers: a trigger that another snippet uses too.
    fn trigger_hints(&self, cx: &App) -> Vec<String> {
        let Some(inputs) = &self.inputs else { return Vec::new() };
        let here = self.current.as_ref().and_then(|c| c.index.map(|i| (c.file.as_str(), i)));
        let mut out = Vec::new();
        for t in &inputs.triggers {
            let t = t.read(cx).value().to_string();
            if t.is_empty() {
                continue;
            }
            let other = self
                .all
                .iter()
                .find(|s| Some((s.file.as_str(), s.index)) != here && s.triggers.contains(&t));
            if let Some(s) = other {
                let name = s.label.clone().unwrap_or_else(|| s.preview.lines().next().unwrap_or("").to_string());
                out.push(format!("{t} is also the trigger of \"{name}\" in {}, line {}.", s.file, s.line));
            }
        }
        out
    }

    /// Warnings for the variables: a `{{name}}` in the text that no variable
    /// defines, and a variable that nothing uses.
    fn var_hints(&self, cx: &App) -> Vec<String> {
        let (Some(form), Some(inputs)) = (&self.form, &self.inputs) else { return Vec::new() };
        // A form body uses [[field]], not {{name}}.
        if form.output == "form" {
            return Vec::new();
        }
        let body = braces(&inputs.body.read(cx).value());
        let names: Vec<String> = inputs.vars.iter().map(|v| v.name.read(cx).value().trim().to_string()).collect();
        let params: Vec<String> = inputs
            .vars
            .iter()
            .flat_map(|v| v.params.iter())
            .filter_map(|(_, _, input)| match input {
                ParamInput::Line(s) => Some(s.read(cx).value().to_string()),
                ParamInput::Area(s) => Some(s.read(cx).value().to_string()),
                ParamInput::YamlOnly => None,
            })
            .collect();
        let in_params = braces(&params.join("\n"));
        let mut out = Vec::new();
        for u in &body {
            if !names.contains(u) && !self.global_names.contains(u) {
                out.push(format!("{{{{{u}}}}} in the text has no variable here and no global variable."));
            }
        }
        for n in names.iter().filter(|n| !n.is_empty()) {
            if !body.contains(n) && !in_params.contains(n) {
                out.push(format!("The variable {n} is not used in the text."));
            }
        }
        out
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(ws), Some(cur)) = (&self.ws, &self.current) else { return };
        let Some(index) = cur.index else {
            // A new match that was never saved: just drop it.
            self.current = None;
            self.form = None;
            self.inputs = None;
            self.set_yaml("", window, cx);
            cx.notify();
            return;
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
            self.set_status("Click Delete again to delete this snippet from the file.", true);
            cx.notify();
            return;
        }
        self.confirm_delete = false;
        match ws.delete_snippet(&cur.file, index, &cur.hash) {
            Ok(()) => {
                let file = cur.file.clone();
                (self.reload)();
                self.current = None;
                self.form = None;
                self.inputs = None;
                self.set_yaml("", window, cx);
                self.refresh_list(cx);
                self.set_status(format!("Deleted from {file}."), false);
            }
            Err(e) => self.set_status(save_error(&e), true),
        }
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        match (m.control, ev.keystroke.key.as_str()) {
            (true, "s") => {
                self.save(window, cx);
            }
            (true, "enter") => {
                if !self.dirty(cx) || self.save(window, cx) {
                    self.remember_rect();
                    window.remove_window();
                }
            }
            (true, "n") => self.request(Request::New { trigger: None }, window, cx),
            (false, k @ ("up" | "down")) if self.search_focused(window, cx) => self.list_step(k == "down", window, cx),
            (false, "enter") if self.search_focused(window, cx) => match &self.inputs {
                Some(inputs) => inputs.body.update(cx, |s, cx| s.focus(window, cx)),
                None => self.list_step(true, window, cx),
            },
            (true, "f") => {
                if self.compact {
                    self.toggle_list(window, cx);
                }
                self.search.update(cx, |s, cx| s.focus(window, cx));
            }
            (false, "tab") => {
                if !self.tab_move(m.shift, window, cx) {
                    return;
                }
            }
            (_, "escape") => return,
            _ => {
                self.confirm_close = false;
                return;
            }
        }
        cx.stop_propagation();
    }

    /// The text boxes in Tab order: the search box (if the list shows),
    /// then the boxes of the open form tab, top to bottom. The YAML box is
    /// not in the order, so Tab indents there.
    fn tab_order(&self, cx: &App) -> Vec<FocusHandle> {
        let line = |s: &Entity<InputState>| s.read(cx).focus_handle(cx);
        let area = |s: &Entity<TextareaState>| s.read(cx).focus_handle(cx);
        let mut order = Vec::new();
        if !self.compact {
            order.push(line(&self.search));
        }
        let (Some(form), Some(inputs)) = (&self.form, &self.inputs) else { return order };
        if self.tab == 0 {
            if form.regex.is_empty() {
                order.extend(inputs.triggers.iter().map(line));
            } else {
                order.push(line(&inputs.regex));
            }
            order.push(area(&inputs.body));
            order.push(line(&inputs.label));
            for v in &inputs.vars {
                order.push(line(&v.name));
                for (_, _, input) in &v.params {
                    match input {
                        ParamInput::Line(s) => order.push(line(s)),
                        ParamInput::Area(s) => order.push(area(s)),
                        ParamInput::YamlOnly => {}
                    }
                }
            }
        } else {
            if form.regex.is_empty() {
                order.push(line(&inputs.regex));
            }
            order.push(area(&inputs.search_terms));
            order.push(area(&inputs.comments));
        }
        order
    }

    /// Tab or Shift+Tab (`back`): moves the focus to the next or previous
    /// box. Returns false when the focus is not in a box of the order, so
    /// the key goes on (to the YAML box, for example).
    fn tab_move(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let order = self.tab_order(cx);
        let Some(pos) = order.iter().position(|h| h.is_focused(window)) else { return false };
        let n = order.len();
        let next = if back { (pos + n - 1) % n } else { (pos + 1) % n };
        window.focus(&order[next], cx);
        true
    }

    /// Esc closes the window. With unsaved edits, the first Esc only warns.
    /// It runs after the inputs, so a menu that Esc closes keeps the window.
    fn on_escape(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key != "escape" || ev.keystroke.modifiers.modified() {
            return;
        }
        cx.stop_propagation();
        if self.may_close(cx) {
            window.remove_window();
        }
    }

    // Rendering.

    fn render_list(&self, theme: &component::Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_button = self.file_filter_button(cx);
        let rows = self.shown.iter().take(MAX_ROWS).enumerate().map(|(row, &i)| {
            let s = &self.all[i];
            let title = s.label.clone().or_else(|| s.triggers.first().cloned()).or_else(|| s.regex.clone()).unwrap_or_default();
            let sub = if s.label.is_some() { s.triggers.join("  ") } else { s.preview.lines().next().unwrap_or("").to_string() };
            let selected = self.selected == Some(i);
            let (file, index) = (s.file.clone(), s.index);
            div()
                .id(("snip", row))
                .px_2()
                .py_1()
                .rounded_md()
                .when(selected, |d| d.bg(theme.list_active))
                .when(!selected, |d| d.hover(|st| st.bg(theme.list_hover)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| this.go(Pending::Open(file.clone(), index), window, cx)),
                )
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
                .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(sub))
        });
        let more = (self.shown.len() > MAX_ROWS).then(|| {
            div().p_2().text_xs().text_color(theme.muted_foreground).child(format!(
                "{} more. Type more letters to narrow the list.",
                self.shown.len() - MAX_ROWS
            ))
        });
        div()
            .w(px(LIST_W))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_r_1()
            .border_color(theme.border)
            .child(Input::new(&self.search))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{} of {} snippets", self.shown.len(), self.all.len())),
                    )
                    .child(filter_button),
            )
            .child(
                div()
                    .id("list")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .children(rows)
                    .children(more),
            )
    }

    /// The menu that limits the list to one file. Each item shows the
    /// number of snippets in the file.
    fn file_filter_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for s in &self.all {
            *counts.entry(s.file.as_str()).or_default() += 1;
        }
        let items: Vec<(Option<String>, String)> = std::iter::once((None, format!("All files ({})", self.all.len())))
            .chain(counts.iter().map(|(f, n)| (Some(f.to_string()), format!("{f} ({n})"))))
            .collect();
        let label = self.file_filter.clone().unwrap_or_else(|| "All files".into());
        let current = self.file_filter.clone();
        let weak = cx.entity().downgrade();
        Button::new("file-filter").label(label).icon(IconName::ChevronDown).ghost().xsmall().dropdown_menu(move |mut menu, _, _| {
            for (file, text) in &items {
                let (view, file) = (weak.clone(), file.clone());
                menu = menu.item(PopupMenuItem::new(text.clone()).checked(file == current).on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.file_filter = file.clone();
                        this.filter(cx);
                    });
                }));
            }
            menu
        })
    }

    /// The bar that waits for Save or Discard before a step drops unsaved
    /// edits.
    fn render_pending(&self, theme: &component::Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let what = match self.pending.as_ref()? {
            Pending::Open(..) => "open the other snippet",
            Pending::Request(Request::New { .. }) => "start a new snippet",
            Pending::Request(_) => "open the snippet",
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .mx_2()
                .mt_1()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(theme.warning)
                .child(div().flex_1().text_sm().child(format!("This snippet has unsaved changes. Save them before you {what}?")))
                .child(Button::new("p-save").label("Save").primary().xsmall().on_click(cx.listener(|this, _, window, cx| {
                    if this.save(window, cx) {
                        if let Some(step) = this.pending.take() {
                            this.run(step, window, cx);
                        }
                    }
                })))
                .child(Button::new("p-drop").label("Discard").danger().xsmall().on_click(cx.listener(|this, _, window, cx| {
                    if let Some(step) = this.pending.take() {
                        this.run(step, window, cx);
                    }
                })))
                .child(Button::new("p-keep").label("Keep editing").ghost().xsmall().on_click(cx.listener(|this, _, _, cx| {
                    this.pending = None;
                    cx.notify();
                })))
                .into_any_element(),
        )
    }

    /// The output of the Test button, between the form and the YAML.
    fn render_test(&self, theme: &component::Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.testing && self.test_output.is_none() {
            return None;
        }
        let (text, color) = match &self.test_output {
            None => ("Running the snippet...".to_string(), theme.muted_foreground),
            Some(Ok(t)) => (t.clone(), theme.foreground),
            Some(Err(e)) => (format!("The test failed: {e}"), theme.danger),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(theme.border)
                .px_3()
                .py_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(div().flex_1().child("Test output. Nothing was inserted."))
                        .child(Button::new("close-test").icon(IconName::Close).ghost().xsmall().on_click(cx.listener(|this, _, _, cx| {
                            this.test_output = None;
                            cx.notify();
                        }))),
                )
                .child(div().id("test-out").max_h(px(120.0)).overflow_y_scroll().text_sm().text_color(color).child(text))
                .into_any_element(),
        )
    }

    /// Warning lines under a field.
    fn hints(theme: &component::Theme, hints: Vec<String>) -> impl Iterator<Item = Div> {
        let color = theme.warning;
        hints.into_iter().take(4).map(move |h| div().text_xs().text_color(color).child(h))
    }

    /// A form row: a label on the left and the field on the right. Every
    /// input is the small size, 24 px high, with its first text line
    /// centered in the top 24 px. The label sits in a 24 px line too, so
    /// both line up. `center` centers the whole row, for radios.
    fn row(muted: Hsla, name: &str, center: bool, el: AnyElement) -> Div {
        div()
            .flex()
            .gap_2()
            .when(center, |d| d.items_center())
            .when(!center, |d| d.items_start())
            .child(
                div()
                    .w(px(84.0))
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(muted)
                    .child(name.to_string()),
            )
            .child(div().flex_1().min_w_0().child(el))
    }

    fn render_empty(&self, theme: &component::Theme) -> Option<AnyElement> {
        if self.inputs.is_some() {
            return None;
        }
        let text = match &self.form_error {
            Some(e) => format!("The form cannot show this match. {e}"),
            None => "Pick a snippet on the left, or press Ctrl+N for a new one.".into(),
        };
        let color = if self.form_error.is_some() { theme.danger } else { theme.muted_foreground };
        Some(div().p_4().text_color(color).child(text).into_any_element())
    }

    /// The "Snippet" tab: triggers, text, label and variables.
    fn render_main(&self, theme: &component::Theme, cx: &mut Context<Self>) -> AnyElement {
        if let Some(e) = self.render_empty(theme) {
            return e;
        }
        let (Some(form), Some(inputs)) = (&self.form, &self.inputs) else { unreachable!() };
        let muted = theme.muted_foreground;
        let row = |name: &str, el: AnyElement| Self::row(muted, name, false, el);
        let icon = |id: SharedString, name: IconName, tip: &'static str| Button::new(id).icon(name).ghost().xsmall().tooltip(tip);

        // Triggers are short, so they sit side by side and wrap. Each extra
        // trigger has a remove button inside its box. The plus button
        // follows the last one.
        let trigger_boxes = inputs.triggers.iter().enumerate().map(|(i, t)| {
            let remove = (i > 0).then(|| {
                icon(format!("rm-trigger-{i}").into(), IconName::Close, "Remove this trigger").on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.edit_form(window, cx, |f| {
                            if i < f.triggers.len() {
                                f.triggers.remove(i);
                            }
                        })
                    },
                ))
            });
            let input = Input::new(t).small();
            let input = match remove {
                Some(b) => input.suffix(b),
                None => input,
            };
            div().w(px(130.0)).child(input)
        });
        let add_trigger = icon("add-trigger".into(), IconName::Plus, "Add a trigger").on_click(cx.listener(|this, _, window, cx| {
            this.edit_form(window, cx, |f| {
                if f.triggers.is_empty() {
                    f.triggers.push(String::new());
                }
                f.triggers.push(String::new());
            });
            // The cursor goes to the new box.
            if let Some(last) = this.inputs.as_ref().and_then(|i| i.triggers.last().cloned()) {
                last.update(cx, |s, cx| s.focus(window, cx));
            }
        }));
        let triggers = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().flex().flex_wrap().items_center().gap_1().children(trigger_boxes).child(add_trigger))
            .children(Self::hints(theme, self.trigger_hints(cx)));

        // Variables: one compact card each.
        let weak = cx.entity().downgrade();
        let vars = form.vars.iter().zip(&inputs.vars).enumerate().map(|(vi, (var, inp))| {
            let kind = matchedit::var_get(var, &["type"]);
            let menu_kind = kind.clone();
            let menu_view = weak.clone();
            let type_button = Button::new(SharedString::from(format!("vtype-{vi}")))
                .label(if kind.is_empty() { "type".to_string() } else { kind.clone() })
                .icon(IconName::ChevronDown)
                .outline()
                .xsmall()
                .dropdown_menu(move |mut menu, _, _| {
                    for (t, _) in VAR_TYPES {
                        let t: &'static str = t;
                        let view = menu_view.clone();
                        menu = menu.item(PopupMenuItem::new(t).checked(t == menu_kind).on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.set_var_type(vi, t, window, cx));
                        }));
                    }
                    menu
                });
            let bools: Vec<&'static str> = VAR_TYPES
                .iter()
                .find(|(t, _)| *t == kind)
                .map(|(_, ps)| ps.iter().filter(|(_, p)| *p == Param::Bool).map(|(k, _)| *k).collect())
                .unwrap_or_default();
            let bool_boxes = bools.into_iter().map(|key| {
                let on = matchedit::var_get(var, &["params", key]) == "true";
                Checkbox::new(SharedString::from(format!("vb-{vi}-{key}"))).label(key).checked(on).small().on_click(cx.listener(
                    move |this, v: &bool, window, cx| {
                        if let Some(var) = this.form.as_mut().and_then(|f| f.vars.get_mut(vi)) {
                            matchedit::var_set(var, &["params", key], if *v { "true" } else { "" }, Param::Bool);
                        }
                        this.sync_to_yaml(window, cx);
                    },
                ))
            });
            let params = inp.params.iter().map(|(path, _, input)| {
                let el = match input {
                    ParamInput::Line(s) => Input::new(s).small().into_any_element(),
                    ParamInput::Area(s) => Textarea::new(s).small().h(px(64.0)).into_any_element(),
                    ParamInput::YamlOnly => {
                        div().h(px(24.0)).flex().items_center().text_sm().text_color(muted).child("Edit this value in the YAML below.").into_any_element()
                    }
                };
                row(path.last().unwrap(), el)
            });
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .child(div().w(px(150.0)).child(Input::new(&inp.name).small()))
                        .child(type_button)
                        .children(bool_boxes)
                        .child(div().flex_1())
                        .child(icon(format!("rm-var-{vi}").into(), IconName::Close, "Remove this variable").on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.edit_form(window, cx, |f| {
                                    if vi < f.vars.len() {
                                        f.vars.remove(vi);
                                    }
                                })
                            }),
                        )),
                )
                .children(params)
                .into_any_element()
        });
        let add_var = icon("add-var".into(), IconName::Plus, "Add a variable").on_click(cx.listener(|this, _, window, cx| {
            this.edit_form(window, cx, |f| {
                let name = format!("var{}", f.vars.len() + 1);
                f.vars.push(matchedit::new_var(&name, "shell"));
            });
        }));
        let var_hints = self.var_hints(cx);
        let variables = div()
            .flex()
            .flex_col()
            .gap_1()
            .children(vars)
            .children(Self::hints(theme, var_hints))
            .child(div().flex().items_center().gap_1().child(add_var).when(form.vars.is_empty(), |d| {
                d.child(div().text_sm().text_color(muted).child("Use a variable in the text as {{name}}."))
            }));

        let others = (!form.other_keys.is_empty()).then(|| {
            div()
                .text_xs()
                .text_color(muted)
                .child(format!("Other keys: {}. Edit them in the YAML below.", form.other_keys.join(", ")))
        });
        let has_regex = !form.regex.is_empty();

        div()
            .flex()
            .flex_col()
            .gap_2()
            .when(!has_regex, |d| d.child(row("Triggers", triggers.into_any_element())))
            .when(has_regex, |d| d.child(row("Regex", Input::new(&inputs.regex).small().into_any_element())))
            .child(row(output_title(&form.output), Textarea::new(&inputs.body).small().h(px(140.0)).into_any_element()))
            .child(row("Label", Input::new(&inputs.label).small().into_any_element()))
            .child(row("Variables", variables.into_any_element()))
            .children(others)
            .into_any_element()
    }

    /// The "More options" tab: the fields that few snippets use.
    fn render_more(&self, theme: &component::Theme, cx: &mut Context<Self>) -> AnyElement {
        if let Some(e) = self.render_empty(theme) {
            return e;
        }
        let (Some(form), Some(inputs)) = (&self.form, &self.inputs) else { unreachable!() };
        let muted = theme.muted_foreground;
        let row = |name: &str, el: AnyElement| Self::row(muted, name, false, el);
        let line_row = |name: &str, el: AnyElement| Self::row(muted, name, true, el);
        let flag = |id: &'static str, on: bool, set: fn(&mut MatchForm, bool)| {
            Checkbox::new(id).label(id).checked(on).small().on_click(cx.listener(move |this, v: &bool, window, cx| {
                if let Some(f) = &mut this.form {
                    set(f, *v);
                }
                this.sync_to_yaml(window, cx);
            }))
        };
        let flags = div()
            .flex()
            .flex_wrap()
            .gap_4()
            .child(flag("word", form.word, |f, v| f.word = v))
            .child(flag("left_word", form.left_word, |f, v| f.left_word = v))
            .child(flag("right_word", form.right_word, |f, v| f.right_word = v))
            .child(flag("propagate_case", form.propagate_case, |f, v| f.propagate_case = v));
        let output = radios("out", OUTPUT_KEYS, &form.output, cx, |f, v| f.output = v.to_string());
        let upper = radios("upper", &["", "uppercase", "capitalize", "capitalize_words"], &form.uppercase_style, cx, |f, v| {
            f.uppercase_style = v.to_string()
        });
        let force = radios("force", &["", "keys", "clipboard"], &form.force_mode, cx, |f, v| f.force_mode = v.to_string());
        div()
            .flex()
            .flex_col()
            .gap_3()
            .when(form.regex.is_empty(), |d| d.child(row("Regex", Input::new(&inputs.regex).small().into_any_element())))
            .child(row("Search terms", Textarea::new(&inputs.search_terms).small().h(px(48.0)).into_any_element()))
            .child(line_row("Output key", output.into_any_element()))
            .child(line_row("Word", flags.into_any_element()))
            .when(form.propagate_case, |d| d.child(line_row("Case style", upper.into_any_element())))
            .child(line_row("Insert by", force.into_any_element()))
            .child(row("Comments", Textarea::new(&inputs.comments).small().h(px(48.0)).into_any_element()))
            .into_any_element()
    }

    fn render_bottom(&self, theme: &component::Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let new_match = self.current.as_ref().is_some_and(|c| c.index.is_none());
        let file_label = match &self.current {
            Some(c) if c.index.is_some() => c.file.clone(),
            _ => format!("New snippet goes to: {}", self.target_file),
        };
        let file_button = Button::new("file").label(file_label).ghost().compact().on_click(cx.listener(|this, _, _, cx| {
            this.file_menu = !this.file_menu;
            cx.notify();
        }));
        let menu = (self.file_menu && new_match).then(|| {
            div()
                .id("files")
                .max_h(px(240.0))
                .overflow_y_scroll()
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .children(self.files.iter().map(|f| {
                    let file = f.clone();
                    div()
                        .id(SharedString::from(format!("f-{f}")))
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .text_sm()
                        .hover(|s| s.bg(theme.list_hover))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                            this.target_file = file.clone();
                            if let Some(c) = &mut this.current {
                                c.file = file.clone();
                            }
                            this.file_menu = false;
                            cx.notify();
                        }))
                        .child(f.clone())
                }))
        });
        let has = self.current.is_some();
        let saved = self.current.as_ref().is_some_and(|c| c.index.is_some());
        let save_label = if self.dirty(cx) { "\u{2022} Save  Ctrl+S" } else { "Save  Ctrl+S" };
        let delete_label = if self.confirm_delete { "Delete: click again" } else { "Delete" };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(theme.border)
            .children(menu)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().flex().overflow_hidden().child(file_button))
                    .when(saved, |d| {
                        d.child(Button::new("open-file").label("Open file").ghost().compact().tooltip("Open the file at this snippet").on_click(
                            cx.listener(|this, _, _, cx| this.open_in_file(cx)),
                        ))
                    })
                    .child(Button::new("new").label("New").ghost().compact().on_click(cx.listener(|this, _, window, cx| {
                        this.request(Request::New { trigger: None }, window, cx)
                    })))
                    .when(has, |d| {
                        d.child(
                            Button::new("test")
                                .label(if self.testing { "Testing..." } else { "Test" })
                                .outline()
                                .compact()
                                .tooltip("Run the snippet and show its text. Shell and script variables run too.")
                                .on_click(cx.listener(|this, _, _, cx| this.run_test(cx))),
                        )
                        .child(Button::new("delete").label(delete_label).danger().compact().on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))))
                        .child(
                            Button::new("save")
                                .label(save_label)
                                .primary()
                                .compact()
                                .tooltip("Ctrl+Enter saves and closes")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save(window, cx);
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(if self.status_error { theme.danger } else { theme.muted_foreground })
                    .child(self.status.clone()),
            )
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // The title names the snippet, with a dot while it has unsaved edits.
        let name = self
            .inputs
            .as_ref()
            .and_then(|i| i.triggers.first())
            .map(|t| t.read(cx).value().to_string())
            .filter(|t| !t.is_empty())
            .or_else(|| self.form.as_ref().map(|f| f.regex.clone()).filter(|r| !r.is_empty()));
        let title = match (&self.current, name) {
            (None, _) => "snip editor".to_string(),
            (Some(_), name) => format!(
                "{}{} - snip editor",
                if self.dirty(cx) { "\u{2022} " } else { "" },
                name.unwrap_or_else(|| "new snippet".into())
            ),
        };
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
        let more_label = if self.form.as_ref().is_some_and(needs_more) { "More options \u{2022}" } else { "More options" };
        let tabs = TabBar::new("tabs")
            .underline()
            .small()
            .selected_index(self.tab)
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.tab = *ix;
                cx.notify();
            }))
            .child(Tab::new().label("Snippet"))
            .child(Tab::new().label(more_label));
        let body = if self.tab == 0 { self.render_main(&theme, cx) } else { self.render_more(&theme, cx) };
        let yaml_head = div()
            .flex()
            .gap_2()
            .items_center()
            .px_3()
            .pt_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child("YAML")
            .children(self.form_error.as_ref().map(|e| {
                div().text_color(theme.danger).truncate().child(format!("not valid, the form is on hold: {e}"))
            }));
        let right = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .pt_1()
                    .child(div().flex_1().child(tabs))
                    .child(
                        Button::new("list")
                            .label(if self.compact { "Show list" } else { "Hide list" })
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_list(window, cx))),
                    ),
            )
            .children(self.render_pending(&theme, cx))
            .child(
                div()
                    .id("form")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.form_scroll)
                    .p_3()
                    .child(body),
            )
            .children(self.render_test(&theme, cx))
            .child(div().border_t_1().border_color(theme.border).child(yaml_head))
            .child(div().h(px(230.0)).px_2().pb_1().child(Editor::new(&self.yaml).h_full()))
            .child(self.render_bottom(&theme, cx));
        div()
            .size_full()
            .flex()
            .bg(theme.background)
            .text_color(theme.foreground)
            .capture_key_down(cx.listener(Self::on_key))
            .on_key_down(cx.listener(Self::on_escape))
            .when(!self.compact, |d| d.child(self.render_list(&theme, cx)))
            .child(right)
    }
}

/// A row of radio buttons that picks one value. `""` shows as "default".
fn radios(
    id: &'static str,
    values: &'static [&'static str],
    current: &str,
    cx: &mut Context<EditorView>,
    set: fn(&mut MatchForm, &str),
) -> RadioGroup {
    RadioGroup::horizontal(id)
        .children(values.iter().map(|v| if v.is_empty() { "default" } else { *v }))
        .selected_index(values.iter().position(|v| *v == current))
        .on_click(cx.listener(move |this, ix: &usize, window, cx| {
            if let (Some(f), Some(v)) = (&mut this.form, values.get(*ix)) {
                set(f, v);
            }
            this.sync_to_yaml(window, cx);
        }))
}

/// Whether the form can show a parameter as text: not a nested list or
/// mapping.
fn form_can_show(var: &Value, path: &[&str], p: Param) -> bool {
    let mut v = var;
    for key in path {
        match v.get(*key) {
            Some(next) => v = next,
            None => return true,
        }
    }
    match v {
        Value::Mapping(_) => false,
        Value::Sequence(l) => p == Param::Lines && l.iter().all(|x| !x.is_mapping() && !x.is_sequence()),
        _ => true,
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim_end).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn matches_query(s: &Snippet, q: &str) -> bool {
    let hit = |t: &str| t.to_lowercase().contains(q);
    s.triggers.iter().any(|t| hit(t))
        || s.regex.as_deref().is_some_and(hit)
        || s.label.as_deref().is_some_and(hit)
        || hit(&s.preview)
        || s.search_terms.iter().any(|t| hit(t))
        || hit(&s.file)
}

fn save_error(e: &SnipError) -> String {
    match e {
        SnipError::Conflict { .. } => "The file changed on disk since you opened this snippet. Pick it in the list again to load the new text.".into(),
        SnipError::Validation(problems) => {
            let first: Vec<String> = problems.iter().take(3).map(|p| p.message.clone()).collect();
            format!("espanso rejects this change: {}", first.join("; "))
        }
        other => format!("Not saved: {other}"),
    }
}

/// The compact window rectangle for a popup at `near`: the same top left
/// corner, moved inside the work area of that monitor.
fn place_near(near: Rect) -> Rect {
    let (work, scale) = place::monitor_at(near.left, near.top);
    let w = ((COMPACT_W * scale).round() as i32).min(work.width());
    let h = ((WINDOW_H * scale).round() as i32).min(work.height());
    let left = near.left.min(work.right - w).max(work.left);
    let top = near.top.min(work.bottom - h).max(work.top);
    Rect { left, top, right: left + w, bottom: top + h }
}

/// The window rectangle for a start from the tray: where it was last time,
/// if that is still on a monitor, or else in the middle of the monitor with
/// the mouse.
fn full_place() -> Rect {
    let saved = read_setting(RECT_FILE).and_then(|t| {
        let n: Vec<i32> = t.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        (n.len() == 4).then(|| Rect { left: n[0], top: n[1], right: n[2], bottom: n[3] })
    });
    if let Some(r) = saved {
        let (work, _) = place::monitor_at((r.left + r.right) / 2, r.top + 10);
        // The title bar must be on the monitor, so the user can move it.
        let reachable = r.top >= work.top - 10 && r.top + 30 <= work.bottom && r.right > work.left + 50 && r.left < work.right - 50;
        if reachable {
            return r;
        }
    }
    let (x, y) = focus::cursor_pos();
    let (work, scale) = place::monitor_at(x, y);
    let w = ((FULL_W * scale).round() as i32).min(work.width());
    let h = ((WINDOW_H * scale).round() as i32).min(work.height());
    let left = work.left + (work.width() - w) / 2;
    let top = work.top + (work.height() - h) / 2;
    Rect { left, top, right: left + w, bottom: top + h }
}

/// The settings of the editor are small text files in `%LOCALAPPDATA%\snip`.
const LAST_FILE: &str = "last-file.txt";
const RECT_FILE: &str = "editor-window.txt";

fn read_setting(name: &str) -> Option<String> {
    let dir = std::env::var_os("LOCALAPPDATA")?;
    let text = std::fs::read_to_string(PathBuf::from(dir).join("snip").join(name)).ok()?;
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn write_setting(name: &str, text: &str) {
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        let dir = PathBuf::from(dir).join("snip");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(name), text);
    }
}

/// The file that the last new snippet went to.
fn last_file() -> Option<String> {
    read_setting(LAST_FILE)
}

fn remember_file(file: &str) {
    write_setting(LAST_FILE, file);
}

/// The names in `{{name}}` and `{{name.field}}`, each once.
fn braces(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(a) = rest.find("{{") {
        let after = &rest[a + 2..];
        let Some(b) = after.find("}}") else { break };
        let name = after[..b].split('.').next().unwrap_or("").trim();
        if !name.is_empty() && !out.iter().any(|x| x == name) {
            out.push(name.to_string());
        }
        rest = &after[b + 2..];
    }
    out
}

/// Renders the match `yaml` (labeled [`TEST_LABEL`]) with espanso's own
/// renderer. A temporary folder holds a copy of `config/default.yml` and one
/// match file that imports every match file of `base`, so global variables
/// work. Runs on a background thread.
fn test_render(base: &Path, files: &[String], yaml: &str) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!("snip-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let io = |e: std::io::Error| format!("cannot write the test folder: {e}");
    std::fs::create_dir_all(dir.join("config")).map_err(io)?;
    std::fs::create_dir_all(dir.join("match")).map_err(io)?;
    let config = base.join("config").join("default.yml");
    if config.is_file() {
        std::fs::copy(&config, dir.join("config").join("default.yml")).map_err(io)?;
    } else {
        std::fs::write(dir.join("config").join("default.yml"), "# empty\n").map_err(io)?;
    }
    let mut text = String::new();
    if !files.is_empty() {
        text.push_str("imports:\n");
        for f in files {
            text.push_str(&format!("  - {}\n", quote(&base.join(f).to_string_lossy().replace('\\', "/"))));
        }
    }
    text.push_str("matches:\n");
    for (i, line) in yaml.lines().enumerate() {
        text.push_str(if i == 0 { "  - " } else { "    " });
        text.push_str(line);
        text.push('\n');
    }
    // espanso skips match files whose name starts with `_`.
    std::fs::write(dir.join("match").join("snip-test.yml"), text).map_err(io)?;
    let result = popup_core::expand::Engine::load(&dir)
        .map_err(|e| format!("{e:#}"))
        .and_then(|mut engine| engine.render_label(TEST_LABEL).map_err(|e| format!("{e:#}")));
    let _ = std::fs::remove_dir_all(&dir);
    result.map_err(|e| {
        if e == "aborted" {
            "The snippet asks with a form or a choice list. The test does not show those.".into()
        } else {
            e
        }
    })
}

/// Opens `path` at `line` in VS Code when `code` is on the PATH, otherwise in
/// the default program for the file. Returns the program name.
fn open_at_line(path: &Path, line: usize) -> Result<&'static str, String> {
    let has_code =
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("code.cmd").is_file()));
    if has_code {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "code", "-g"]).arg(format!("{}:{}", path.display(), line.max(1)));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console flashes up.
            c.creation_flags(0x0800_0000);
        }
        c.spawn().map(|_| "VS Code").map_err(|e| e.to_string())
    } else {
        focus::shell_open(path).map(|_| "its default program")
    }
}

/// Whether a form uses a field that sits behind "More options".
fn needs_more(f: &MatchForm) -> bool {
    !f.search_terms.is_empty()
        || !f.comments.is_empty()
        || f.output != "replace"
        || f.word
        || f.left_word
        || f.right_word
        || f.propagate_case
        || !f.force_mode.is_empty()
}

/// The row title of the output text.
fn output_title(key: &str) -> &'static str {
    match key {
        "markdown" => "Markdown",
        "html" => "HTML",
        "image_path" => "Image path",
        "form" => "Form",
        _ => "Replace",
    }
}
