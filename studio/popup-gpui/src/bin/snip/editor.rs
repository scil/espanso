//! The snippet editor form. See `studio/EDITOR.md`, step 3.
//!
//! It lives in the popup window, to the right of the popup list, which picks
//! the snippet (the edit session). It shows one match as a form and as YAML
//! text. The YAML text is the source of truth. The form reads it with
//! `snip_core::matchedit` when the Form tab opens, and writes it back as the
//! smallest change when the YAML tab opens or on Save.
//!
//! Saves go through `snip_core::Workspace`, which checks the new file with
//! espanso's own loader and refuses a write after a change on disk. After a
//! save, `reload` makes the engine load the configuration again.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{self as input, Editor, EditorState, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use popup_core::focus;
use serde_norway::Value;
use snip_core::matchedit::{self, MatchForm, Param, OUTPUT_KEYS, VAR_TYPES};
use snip_core::workspace::Snippet;
use snip_core::{SnipError, Workspace};

/// The height of the tab row above the form. The popup's list header in the
/// edit session has the same height, so the two halves line up.
pub const TAB_ROW_H: f32 = 34.0;
/// The width of the form beside the popup list, in logical pixels.
pub const FORM_W: f32 = 700.0;
const DEFAULT_FILE: &str = "match/base.yml";
/// The label that marks the copy of the snippet that the Test button renders.
const TEST_LABEL: &str = "__snip_editor_test__";

/// What the editor shows when it opens.
#[derive(Debug, Clone)]
pub enum Request {
    /// The snippet with these triggers (joined with two spaces, or the
    /// regex) and label, as the popup shows it.
    Find { trigger: String, label: String },
    /// A new snippet. `trigger` fills the first trigger.
    New { trigger: Option<String> },
    /// The holder wants to show its settings panel instead of the form.
    /// The editor only asks about unsaved edits and then sends
    /// [`EditorEvent::Settings`].
    Settings,
}

/// What the editor tells the popup that holds it.
pub enum EditorEvent {
    /// Esc: the holder removes the form and keeps its list.
    Close,
    /// Ctrl+Enter after a save: the holder ends the edit.
    Done,
    /// Ctrl+F: the holder focuses its list.
    FocusList,
    /// A [`Request::Settings`] passed the unsaved edits check: the holder
    /// shows its settings panel.
    Settings,
}

impl EventEmitter<EditorEvent> for EditorView {}

/// The key context of the editor. The Tab interceptor looks for it.
const KEY_CONTEXT: &str = "SnipEditor";

thread_local! {
    /// The editors that exist, for the Tab interceptor.
    static EDITORS: RefCell<Vec<WeakEntity<EditorView>>> = const { RefCell::new(Vec::new()) };
}

/// Makes Tab and Shift+Tab move between the boxes of the form. Call it once
/// at the start.
///
/// It is a keystroke interceptor, which gpui runs before all key bindings.
/// A binding cannot do it: a multi-line text box, such as the replacement
/// text, binds Tab to indent, and that binding is the deepest one. The
/// popup window's gpui-kit `Root` also binds Tab, to its own focus order.
/// Outside the boxes of the order (the YAML box, for example), Tab works as
/// before.
pub fn init(cx: &mut App) {
    cx.intercept_keystrokes(|ev, window, cx| {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if k.key != "tab" || m.control || m.alt || m.platform {
            return;
        }
        if !ev.context_stack.iter().any(|c| c.contains(KEY_CONTEXT)) {
            return;
        }
        let editors: Vec<_> = EDITORS.with(|e| {
            let mut e = e.borrow_mut();
            e.retain(|w| w.upgrade().is_some());
            e.iter().filter_map(WeakEntity::upgrade).collect()
        });
        for view in editors {
            if view.update(cx, |v, cx| v.tab_move(m.shift, window, cx)) {
                cx.stop_propagation();
                return;
            }
        }
    })
    .detach();
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
/// The drag of a text area's resize handle, with the key of the area.
struct AreaResize(SharedString);

/// The drag shows nothing under the mouse: the area itself changes height.
struct NoPreview;

impl Render for NoPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

enum ParamInput {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
    /// The value is a list or mapping that the form cannot show.
    YamlOnly,
}

/// The open `{{` picker: the byte offset of `{{` in the text, the name
/// typed after it, and the highlighted row.
struct Picker {
    start: usize,
    typed: String,
    selected: usize,
}

/// One row of the variable chips or of the `{{` picker.
#[derive(Debug, Clone, PartialEq)]
enum Pick {
    /// A variable of this snippet, by name.
    Own(String),
    /// A global variable: name and type.
    Global(String, String),
    /// A new variable of this type.
    New(&'static str),
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
    /// Every snippet in the match files, to find a requested snippet and to
    /// warn about a trigger that another snippet uses.
    all: Vec<Snippet>,
    files: Vec<String>,
    /// Every match file, packages included. The Test button imports them.
    match_files: Vec<String>,
    /// The global variables of all match files: (name, type), sorted by
    /// name, each name once.
    globals: Vec<(String, String)>,
    /// The `{{` picker under the text box (see [`matchedit::open_braces`]).
    picker: Option<Picker>,
    /// The `{{` offset whose picker Esc closed. It stays closed until the
    /// caret leaves that `{{`.
    picker_closed: Option<usize>,
    target_file: String,
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
    /// A request that would drop unsaved edits. It waits until the user
    /// saves or discards them.
    pending: Option<Request>,
    /// The result of the Test button.
    test_output: Option<Result<String, String>>,
    testing: bool,
    /// The width of the space left of the New button, measured at each paint.
    /// The file button and its menu take this width.
    file_w: Rc<std::cell::Cell<f32>>,
    form_scroll: ScrollHandle,
    /// The heights the user gave the text areas by dragging their handles,
    /// by area key. They last while the form is open.
    area_heights: HashMap<SharedString, Pixels>,
    /// The focus of the window itself, for keys when no box has the focus.
    focus: FocusHandle,
    /// A load does not move the focus: the holder's list keeps it. See
    /// [`EditorView::preview`].
    keep_focus: bool,
    _subs: Vec<Subscription>,
}

impl EditorView {
    /// An editor inside the popup window. The popup shows its own list
    /// beside it. It never moves or closes the window. To close, it emits
    /// [`EditorEvent::Close`].
    pub fn new(base: PathBuf, reload: Rc<dyn Fn()>, request: Request, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let yaml = cx.new(|cx| EditorState::new(window, cx).language("yaml"));
        let subs = vec![cx.subscribe_in(&yaml, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) && this.yaml_text(cx) != this.last_yaml {
                this.read_form(window, cx);
            }
        })];
        let (ws, status, status_error) = match Workspace::open(Some(base)) {
            Ok(ws) => (Some(ws), String::new(), false),
            Err(e) => (None, format!("Cannot open the espanso folder: {e}"), true),
        };
        let mut view = EditorView {
            ws,
            reload,
            all: Vec::new(),
            files: Vec::new(),
            match_files: Vec::new(),
            globals: Vec::new(),
            picker: None,
            picker_closed: None,
            target_file: last_file().unwrap_or_else(|| DEFAULT_FILE.into()),
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
            file_w: Rc::new(std::cell::Cell::new(0.0)),
            form_scroll: ScrollHandle::new(),
            area_heights: HashMap::new(),
            focus: cx.focus_handle(),
            keep_focus: false,
            _subs: subs,
        };
        let weak = cx.entity().downgrade();
        EDITORS.with(|e| e.borrow_mut().push(weak));
        view.refresh_list(cx);
        view.load(request, window, cx);
        view
    }

    /// Whether the YAML differs from the file.
    pub fn dirty(&self, cx: &App) -> bool {
        self.current.is_some() && self.yaml_text(cx).trim_end() != self.saved_yaml.trim_end()
    }

    /// Shows a request, but asks first if that drops unsaved edits.
    pub fn request(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        self.go(request, window, cx);
    }

    /// Like [`EditorView::request`], but the focus stays where it is: Up and
    /// Down in the holder's list show each snippet while the list keeps the
    /// keys. With unsaved edits, the Save / Discard bar asks first, and its
    /// answer then loads as usual.
    pub fn preview(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        self.keep_focus = true;
        self.go(request, window, cx);
        self.keep_focus = false;
    }

    /// Runs `step` now, or keeps it for the Save / Discard bar when there
    /// are unsaved edits.
    fn go(&mut self, step: Request, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty(cx) {
            self.pending = Some(step);
            cx.notify();
            return;
        }
        self.run(step, window, cx);
    }

    fn run(&mut self, step: Request, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = None;
        if matches!(step, Request::Settings) {
            cx.emit(EditorEvent::Settings);
            return;
        }
        self.load(step, window, cx);
    }

    /// Before the form closes: the first try with unsaved edits only warns.
    /// Returns whether the form may close.
    fn may_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dirty(cx) && !self.confirm_close {
            self.confirm_close = true;
            self.set_status("Unsaved changes. Close again to drop them. Ctrl+S saves.", true);
            cx.notify();
            return false;
        }
        true
    }

    fn refresh_list(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = &self.ws else { return };
        let listed = ws.list_snippets(None, None);
        let files = ws.list_files();
        let globals = ws.global_vars();
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
        }
        match globals {
            Ok(g) => self.globals = g,
            Err(e) => popup_core::log::line(&format!("editor: cannot load the global variables: {e}")),
        }
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
            Request::Find { trigger, label } => {
                // The list may be older than the engine's snippets.
                self.refresh_list(cx);
                // The popup joins several triggers with two spaces.
                let same = |s: &Snippet| s.triggers.join("  ") == trigger || s.regex.as_deref() == Some(trigger.as_str());
                let found = self
                    .all
                    .iter()
                    .find(|s| same(s) && s.label.as_deref().unwrap_or("") == label)
                    .or_else(|| self.all.iter().find(|s| same(s)))
                    .map(|s| (s.file.clone(), s.index));
                match found {
                    Some((file, index)) => self.open_snippet(&file, index, window, cx),
                    None => {
                        self.current = None;
                        self.form = None;
                        self.inputs = None;
                        self.set_yaml("", window, cx);
                        self.set_status(format!("No snippet in the files has the trigger {trigger}. Reload, then pick it again."), true);
                    }
                }
            }
            // `run` sends it to the holder and never loads it.
            Request::Settings => {}
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
                self.set_yaml(&text, window, cx);
                self.tab = 0;
                self.form = None;
                self.inputs = None;
                self.read_form(window, cx);
                self.saved_yaml = text;
                self.set_status("New snippet. Ctrl+S saves it.", false);
            }
        }
        // The cursor goes to the replacement text.
        self.tab = 0;
        self.confirm_close = false;
        if self.keep_focus {
            cx.notify();
            return;
        }
        match &self.inputs {
            Some(inputs) => inputs.body.update(cx, |s, cx| s.focus(window, cx)),
            // No form: the editor itself takes the keys, so Esc still closes
            // it.
            None => window.focus(&self.focus, cx),
        }
        cx.notify();
    }

    fn open_snippet(&mut self, file: &str, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = &self.ws else { return };
        match ws.get_snippet(file, index) {
            Ok(d) => {
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
            form.triggers.iter().map(|t| line(t, ",,sig", window, cx)).collect();
        if triggers.is_empty() {
            triggers.push(line("", ",,sig", window, cx));
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
            label: line(&form.label, "Name shown in the ,, popup (optional)", window, cx),
            search_terms: area(&form.search_terms.join("\n"), "search terms, one per line", 2, window, cx),
            body: area(&form.body, "Text to insert. {{name}} inserts a variable.", 8, window, cx),
            vars,
            _subs: Vec::new(),
        });
        if let Some(i) = &mut self.inputs {
            let mut subs = subs.into_inner();
            // Caret moves notify the box, not the editor: the picker
            // follows them here.
            subs.push(cx.observe_in(&i.body, window, |this, _, window, cx| this.update_picker(window, cx)));
            i._subs = subs;
        }
        self.picker = None;
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

    /// Moves variable `vi` into the file that holds the global variables,
    /// and removes it from the match. `{{name}}` in the text then takes the
    /// global value. The match file changes only when the user saves.
    fn make_global(&mut self, vi: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(ws), Some(cur)) = (&self.ws, &self.current) else { return };
        let Some(var) = self.collect(cx).and_then(|f| f.vars.get(vi).cloned()) else { return };
        let name = matchedit::var_get(&var, &["name"]);
        match ws.add_global_var(&var, &cur.file) {
            Ok(file) => {
                self.edit_form(window, cx, |f| {
                    if vi < f.vars.len() {
                        f.vars.remove(vi);
                    }
                });
                self.refresh_list(cx);
                self.set_status(format!("Moved {name} to the global variables in {file}. Save to remove it from this match."), false);
            }
            Err(e) => self.set_status(format!("Cannot make {name} global: {e}"), true),
        }
        cx.notify();
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
            if !names.contains(u) && !self.globals.iter().any(|(g, _)| g == u) {
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

    /// The names of this snippet's variables, as the boxes hold them, each
    /// once.
    fn own_var_names(&self, cx: &App) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for v in self.inputs.iter().flat_map(|i| &i.vars) {
            let n = v.name.read(cx).value().trim().to_string();
            if !n.is_empty() && !out.contains(&n) {
                out.push(n);
            }
        }
        out
    }

    /// Whether the text box of this output takes `{{name}}`. A form uses
    /// `[[field]]`, and an image path takes no variables.
    fn takes_vars(&self) -> bool {
        self.form.as_ref().is_some_and(|f| matches!(f.output.as_str(), "replace" | "markdown" | "html"))
    }

    /// Opens, updates or closes the `{{` picker after the text or the caret
    /// of the text box changed.
    fn update_picker(&mut self, window: &Window, cx: &mut Context<Self>) {
        let mut found = None;
        if let Some(inputs) = self.inputs.as_ref().filter(|_| self.takes_vars()) {
            let s = inputs.body.read(cx);
            let range = s.selected_range();
            if s.focus_handle(cx).is_focused(window) && range.is_empty() {
                let text = s.value();
                found = matchedit::open_braces(&text, range.end).map(|(start, typed)| (start, typed.to_string()));
            }
        }
        if found.is_none() {
            self.picker_closed = None;
        }
        let next = found.filter(|(start, _)| self.picker_closed != Some(*start));
        let now = self.picker.as_ref().map(|p| (p.start, p.typed.clone()));
        if next != now {
            self.picker = next.map(|(start, typed)| Picker { start, typed, selected: 0 });
            cx.notify();
        }
    }

    /// The rows of the open picker.
    fn picker_rows(&self, cx: &App) -> Vec<Pick> {
        let Some(p) = &self.picker else { return Vec::new() };
        pick_rows(&self.own_var_names(cx), &self.globals, &p.typed)
    }

    /// Moves the highlighted row of the picker by `step`, around the ends.
    fn picker_step(&mut self, step: isize, cx: &mut Context<Self>) -> bool {
        let n = self.picker_rows(cx).len() as isize;
        let Some(p) = self.picker.as_mut().filter(|_| n > 0) else { return false };
        p.selected = (p.selected as isize + step).rem_euclid(n) as usize;
        cx.notify();
        true
    }

    /// Inserts the highlighted row of the picker in place of the typed
    /// `{{name`. False when the picker has no rows.
    fn accept_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(p) = &self.picker else { return false };
        let Some(pick) = self.picker_rows(cx).get(p.selected).cloned() else { return false };
        let start = p.start;
        let Some(body) = self.inputs.as_ref().map(|i| i.body.clone()) else { return false };
        let (text, caret) = {
            let s = body.read(cx);
            (s.value().to_string(), s.selected_range().end)
        };
        self.picker = None;
        self.insert_pick(pick, matchedit::braces_span(&text, start, caret), window, cx);
        true
    }

    /// Writes `{{name}}` over the bytes `span` of the text box, and puts
    /// the caret after it. A new variable is added to the form first, with
    /// a free name.
    fn insert_pick(&mut self, pick: Pick, span: std::ops::Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(body) = self.inputs.as_ref().map(|i| i.body.clone()) else { return };
        let name = match &pick {
            Pick::Own(n) | Pick::Global(n, _) => n.clone(),
            Pick::New(kind) => {
                let mut taken = self.own_var_names(cx);
                taken.extend(self.globals.iter().map(|(g, _)| g.clone()));
                matchedit::free_var_name(kind, &taken)
            }
        };
        let token = format!("{{{{{name}}}}}");
        let caret = span.start + token.len();
        match pick {
            Pick::New(kind) => {
                let text = body.read(cx).value().to_string();
                let Some(new_text) = text.get(..span.start).zip(text.get(span.end..)).map(|(a, b)| format!("{a}{token}{b}"))
                else {
                    return;
                };
                // The form rebuilds every box: the text goes in with the
                // new variable.
                self.edit_form(window, cx, |f| {
                    f.body = new_text;
                    f.vars.push(matchedit::new_var(&name, kind));
                });
                if let Some(body) = self.inputs.as_ref().map(|i| i.body.clone()) {
                    body.update(cx, |s, cx| {
                        s.focus(window, cx);
                        s.set_selected_range(caret..caret, cx);
                    });
                }
            }
            _ => body.update(cx, |s, cx| {
                s.focus(window, cx);
                s.set_selected_range(span, cx);
                s.replace(token, window, cx);
            }),
        }
        cx.notify();
    }

    /// Inserts a chip or a menu row over the selection of the text box.
    fn insert_at_caret(&mut self, pick: Pick, window: &mut Window, cx: &mut Context<Self>) {
        let Some(span) = self.inputs.as_ref().map(|i| i.body.read(cx).selected_range()) else { return };
        self.insert_pick(pick, span, window, cx);
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
                    cx.emit(EditorEvent::Done);
                }
            }
            (true, "f") => cx.emit(EditorEvent::FocusList),
            // Left at the start of a box goes back to the list.
            (false, "left") if !m.modified() && self.caret_at_start(window, cx) => {
                cx.emit(EditorEvent::FocusList)
            }
            (true, "n") => self.request(Request::New { trigger: None }, window, cx),
            (_, "escape") => return,
            _ => {
                self.confirm_close = false;
                return;
            }
        }
        cx.stop_propagation();
    }

    /// Whether the focused text box has its caret at the very start, with
    /// no text selected. False when no box of the form has the focus.
    fn caret_at_start(&self, window: &Window, cx: &App) -> bool {
        let line = |s: &Entity<InputState>| {
            let s = s.read(cx);
            s.focus_handle(cx).is_focused(window).then(|| s.selected_range() == (0..0))
        };
        let area = |s: &Entity<TextareaState>| {
            let s = s.read(cx);
            s.focus_handle(cx).is_focused(window).then(|| s.selected_range() == (0..0))
        };
        let yaml = self.yaml.read(cx);
        if yaml.focus_handle(cx).is_focused(window) {
            return yaml.selected_range() == (0..0);
        }
        let Some(inputs) = &self.inputs else { return false };
        let mut found = [&inputs.comments, &inputs.search_terms, &inputs.body].into_iter().filter_map(area);
        if let Some(at) = found.next() {
            return at;
        }
        let mut found = inputs.triggers.iter().chain([&inputs.regex, &inputs.label]).filter_map(line);
        if let Some(at) = found.next() {
            return at;
        }
        for v in &inputs.vars {
            if let Some(at) = line(&v.name) {
                return at;
            }
            for (_, _, input) in &v.params {
                let at = match input {
                    ParamInput::Line(s) => line(s),
                    ParamInput::Area(s) => area(s),
                    ParamInput::YamlOnly => None,
                };
                if let Some(at) = at {
                    return at;
                }
            }
        }
        false
    }

    /// The text boxes in Tab order: the boxes of the open form tab, top to
    /// bottom. The YAML box is
    /// not in the order, so Tab indents there.
    fn tab_order(&self, cx: &App) -> Vec<FocusHandle> {
        let line = |s: &Entity<InputState>| s.read(cx).focus_handle(cx);
        let area = |s: &Entity<TextareaState>| s.read(cx).focus_handle(cx);
        let mut order = Vec::new();
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

    /// Puts the cursor in the form: in the replacement text, or on the
    /// editor itself when no snippet is open.
    pub fn focus_form(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.inputs {
            Some(inputs) => inputs.body.update(cx, |s, cx| s.focus(window, cx)),
            None => window.focus(&self.focus, cx),
        }
    }

    /// Tab or Shift+Tab (`back`): moves the focus to the next or previous
    /// box. Returns false when the focus is not in a box of the order, so
    /// the key goes on (to the YAML box, for example).
    fn tab_move(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !back && self.picker.is_some() && self.accept_pick(window, cx) {
            return true;
        }
        let order = self.tab_order(cx);
        let Some(pos) = order.iter().position(|h| h.is_focused(window)) else { return false };
        let n = order.len();
        let next = if back { (pos + n - 1) % n } else { (pos + 1) % n };
        window.focus(&order[next], cx);
        true
    }

    /// Esc from the popup list: closes like Esc in the form. With unsaved edits, the first call only warns.
    pub fn try_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.may_close(cx) {
            self.close(window, cx);
        } else {
            self.focus_form(window, cx);
        }
    }

    /// Esc closes the form. With unsaved edits, the first Esc only warns.
    /// It runs after the inputs, so a menu that Esc closes keeps the form.
    fn on_escape(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key != "escape" || ev.keystroke.modifiers.modified() {
            return;
        }
        cx.stop_propagation();
        if self.may_close(cx) {
            self.close(window, cx);
        }
    }

    /// Asks the popup to remove the form.
    fn close(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Close);
    }

    // Rendering.

    /// The bar that waits for Save or Discard before a step drops unsaved
    /// edits.
    fn render_pending(&self, theme: &component::Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let what = match self.pending.as_ref()? {
            Request::New { .. } => "start a new snippet",
            Request::Find { .. } => "open the snippet",
            Request::Settings => "open the settings",
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

    /// A text area with a handle below it. Dragging the handle sets the
    /// height of the area, which scrolls when its text is taller.
    fn resizable_area(
        &self,
        key: impl Into<SharedString>,
        default_h: f32,
        state: &Entity<TextareaState>,
        theme: &component::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const HANDLE_H: f32 = 8.0;
        let key: SharedString = key.into();
        let h = self.area_heights.get(&key).copied().unwrap_or(px(default_h));
        let drag_key = key.clone();
        let grip = theme.muted_foreground.opacity(0.5);
        let grip_hover = theme.muted_foreground;
        div()
            .flex()
            .flex_col()
            .on_drag_move(cx.listener(move |this, e: &DragMoveEvent<AreaResize>, _, cx| {
                if e.drag(cx).0 != drag_key {
                    return;
                }
                let h = (e.event.position.y - e.bounds.top() - px(HANDLE_H)).clamp(px(40.0), px(2000.0));
                this.area_heights.insert(drag_key.clone(), h);
                cx.notify();
            }))
            // The text area paints lines past its bottom edge when its text
            // is taller than the area. The clip keeps them off the controls
            // below.
            .child(div().h(h).overflow_hidden().child(Textarea::new(state).small().h_full()))
            .child(
                div()
                    .id(SharedString::from(format!("resize-{key}")))
                    .h(px(HANDLE_H))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_row_resize()
                    .group("grip")
                    .child(
                        div()
                            .w(px(36.0))
                            .h(px(3.0))
                            .rounded_full()
                            .bg(grip)
                            .group_hover("grip", move |s| s.bg(grip_hover)),
                    )
                    .on_drag(AreaResize(key), |_, _, _, cx| cx.new(|_| NoPreview)),
            )
            .into_any_element()
    }

    fn render_empty(&self, theme: &component::Theme) -> Option<AnyElement> {
        if self.inputs.is_some() {
            return None;
        }
        let text = match &self.form_error {
            Some(e) => format!("The form cannot show this match. {e}"),
            None => "Pick a snippet in the list, or press Ctrl+N for a new one.".into(),
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
            let bool_boxes: Vec<_> = bool_boxes.collect();
            // A `global` variable only points at one, so it cannot become one.
            let global_box = (kind != "global").then(|| {
                Checkbox::new(SharedString::from(format!("vg-{vi}")))
                    .label("Global")
                    .checked(false)
                    .small()
                    .tooltip("Move this variable to the global variables, so every match can use it")
                    .on_click(cx.listener(move |this, _: &bool, window, cx| this.make_global(vi, window, cx)))
            });
            let params: Vec<_> = inp.params.iter().map(|(path, _, input)| {
                let el = match input {
                    ParamInput::Line(s) => Input::new(s).small().into_any_element(),
                    ParamInput::Area(s) => {
                        self.resizable_area(format!("var-{vi}-{}", path.join(".")), 64.0, s, theme, cx)
                    }
                    ParamInput::YamlOnly => {
                        div().h(px(24.0)).flex().items_center().text_sm().text_color(muted).child("Edit this value in the YAML below.").into_any_element()
                    }
                };
                row(path.last().unwrap(), el)
            }).collect();
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
                        .children(global_box)
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
        let vars: Vec<_> = vars.collect();
        let var_hints = self.var_hints(cx);
        let variables = div()
            .flex()
            .flex_col()
            .gap_1()
            .children(self.render_var_bar(theme, cx))
            .children(vars)
            .children(Self::hints(theme, var_hints));

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
            .child(row(output_title(&form.output), self.render_body(theme, cx)))
            .child(row("Label", Input::new(&inputs.label).small().into_any_element()))
            .child(row("Variables", variables.into_any_element()))
            .children(others)
            .into_any_element()
    }

    /// The text box of the output, with the `{{` picker below it.
    fn render_body(&self, theme: &component::Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(inputs) = &self.inputs else { return div().into_any_element() };
        let area = div()
            .capture_action(cx.listener(|this, _: &input::MoveUp, _, cx| {
                if this.picker_step(-1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &input::MoveDown, _, cx| {
                if this.picker_step(1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &input::Enter, window, cx| {
                if this.accept_pick(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &input::Escape, _, cx| {
                if let Some(p) = this.picker.take() {
                    this.picker_closed = Some(p.start);
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(self.resizable_area("body", 140.0, &inputs.body, theme, cx));
        div().flex().flex_col().gap_1().child(area).children(self.render_picker(theme, cx)).into_any_element()
    }

    /// The top line of the Variables row: the variables that the text can
    /// use, one chip each, or one menu when there are many. A click inserts
    /// `{{name}}` at the caret of the text box. The New menu makes a
    /// variable and inserts it. An output without `{{name}}` (a form, an
    /// image) shows only New, which then only adds the variable.
    fn render_var_bar(&self, theme: &component::Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        const MAX_CHIPS: usize = 6;
        let (Some(inputs), Some(form)) = (self.inputs.as_ref(), self.form.as_ref()) else { return None };
        if !self.takes_vars() {
            let view = cx.entity().downgrade();
            let add = Button::new("vars-add")
                .label("New")
                .icon(IconName::Plus)
                .ghost()
                .xsmall()
                .tooltip("Add a variable")
                .dropdown_menu(move |menu, _, _| type_menu(menu, view.clone(), false));
            return Some(div().flex().child(add).into_any_element());
        }
        let muted = theme.muted_foreground;
        let own = self.own_var_names(cx);
        let kind_of = |name: &str| {
            inputs
                .vars
                .iter()
                .position(|v| v.name.read(cx).value().trim() == name)
                .and_then(|i| form.vars.get(i))
                .map(|v| matchedit::var_get(v, &["type"]))
                .unwrap_or_default()
        };
        let used = braces(&inputs.body.read(cx).value());
        let picks: Vec<(Pick, String)> = own
            .iter()
            .map(|n| {
                let kind = kind_of(n);
                (Pick::Own(n.clone()), format!("{kind}: {}. Click to insert.", matchedit::var_help(&kind)))
            })
            .chain(self.globals.iter().filter(|(g, _)| !own.contains(g)).map(|(g, t)| {
                (Pick::Global(g.clone(), t.clone()), format!("Global {t} variable: {}. Click to insert.", matchedit::var_help(t)))
            }))
            .collect();
        let weak = cx.entity().downgrade();
        let mut bar = div().flex().flex_wrap().items_center().gap_1();
        if picks.len() <= MAX_CHIPS {
            let mut global_seen = false;
            for (i, (pick, tip)) in picks.into_iter().enumerate() {
                let (name, global) = match &pick {
                    Pick::Own(n) => (n.clone(), false),
                    Pick::Global(n, _) => (n.clone(), true),
                    Pick::New(_) => continue,
                };
                if global && !global_seen {
                    global_seen = true;
                    bar = bar.child(div().text_xs().text_color(muted).child("global:"));
                }
                let view = weak.clone();
                let chip = Button::new(SharedString::from(format!("vchip-{i}")))
                    .label(format!("{{{{{name}}}}}"))
                    .xsmall()
                    .tooltip(tip)
                    .on_click(move |_, window, cx| {
                        let pick = pick.clone();
                        let _ = view.update(cx, |this, cx| this.insert_at_caret(pick, window, cx));
                    });
                // A variable of this snippet that the text does not use yet
                // is dimmed.
                bar = bar.child(if global || used.contains(&name) { chip.outline() } else { chip.ghost() });
            }
        } else {
            let view = weak.clone();
            let count = picks.len();
            let menu = Button::new("vars-menu")
                .label(format!("Variables ({count})"))
                .icon(IconName::ChevronDown)
                .outline()
                .xsmall()
                .dropdown_menu(move |mut menu, _, _| {
                    let mut section = None;
                    for (pick, _) in &picks {
                        let (title, label) = match pick {
                            Pick::Own(n) => ("This snippet", format!("{{{{{n}}}}}")),
                            Pick::Global(n, t) => ("Global", format!("{{{{{n}}}}}    {t}")),
                            Pick::New(_) => continue,
                        };
                        if section != Some(title) {
                            if section.is_some() {
                                menu = menu.separator();
                            }
                            menu = menu.label(title);
                            section = Some(title);
                        }
                        let (view, pick) = (view.clone(), pick.clone());
                        menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                            let pick = pick.clone();
                            let _ = view.update(cx, |this, cx| this.insert_at_caret(pick, window, cx));
                        }));
                    }
                    menu.max_h(px(320.0)).scrollable(true)
                });
            bar = bar.child(menu);
        }
        let add_view = weak.clone();
        let add = Button::new("vars-add")
            .label("New")
            .icon(IconName::Plus)
            .ghost()
            .xsmall()
            .tooltip("Make a new variable and insert it at the caret")
            .dropdown_menu(move |menu, _, _| type_menu(menu, add_view.clone(), true));
        Some(bar.child(add).child(div().text_xs().text_color(muted).child("Click to insert, or type {{ in the text")).into_any_element())
    }

    /// The `{{` picker: the variables that match the typed name, and new
    /// variables by type. Up and Down move, Enter or Tab inserts, Esc closes.
    fn render_picker(&self, theme: &component::Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.picker.as_ref()?;
        let rows = self.picker_rows(cx);
        if rows.is_empty() {
            return None;
        }
        let muted = theme.muted_foreground;
        let selected = p.selected.min(rows.len() - 1);
        let items = rows.into_iter().enumerate().map(|(i, pick)| {
            let (label, note) = match &pick {
                Pick::Own(n) => (format!("{{{{{n}}}}}"), "this snippet".to_string()),
                Pick::Global(n, t) => (format!("{{{{{n}}}}}"), format!("global {t}")),
                Pick::New(t) => (format!("+ new {t}"), matchedit::var_help(t).to_string()),
            };
            div()
                .id(SharedString::from(format!("pick-{i}")))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .rounded_sm()
                .text_sm()
                .cursor_pointer()
                .when(i == selected, |d| d.bg(theme.accent).text_color(theme.accent_foreground))
                .child(div().flex_shrink_0().child(label))
                .child(div().min_w_0().truncate().text_xs().text_color(muted).child(note))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        if let Some(p) = this.picker.as_mut() {
                            p.selected = i;
                        }
                        this.accept_pick(window, cx);
                        cx.stop_propagation();
                    }),
                )
        });
        Some(
            div()
                .flex()
                .flex_col()
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .children(items)
                .child(div().px_2().text_xs().text_color(muted).child("Up/Down move, Enter or Tab inserts, Esc closes"))
                .into_any_element(),
        )
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
            .child(row("Search terms", self.resizable_area("search_terms", 48.0, &inputs.search_terms, theme, cx)))
            .child(line_row("Output key", output.into_any_element()))
            .child(line_row("Word", flags.into_any_element()))
            .when(form.propagate_case, |d| d.child(line_row("Case style", upper.into_any_element())))
            .child(line_row("Insert by", force.into_any_element()))
            .child(row("Comments", self.resizable_area("comments", 48.0, &inputs.comments, theme, cx)))
            .into_any_element()
    }

    fn render_bottom(&self, theme: &component::Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let new_match = self.current.as_ref().is_some_and(|c| c.index.is_none());
        let file_label = match &self.current {
            Some(c) if c.index.is_some() => c.file.clone(),
            _ => format!("New snippet goes to: {}", self.target_file),
        };
        // A new snippet picks its file in a drop-down menu. The menu closes
        // itself on a click outside it and on Esc. It opens upward, because
        // the button sits at the bottom of the form.
        // The button and the menu reach the New button. The width comes from
        // the last paint. Until the first paint the button keeps its own width.
        let width = self.file_w.get();
        // The button centers its label and icon. A child that fills the
        // button keeps them on the left instead.
        let content = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .when(new_match, |d| d.child(Icon::new(IconName::ChevronUp).small()))
            .child(div().min_w_0().whitespace_nowrap().text_ellipsis().child(file_label));
        let file_button = Button::new("file").child(content).ghost().compact();
        let file_button = if width > 0.0 { file_button.w(px(width)) } else { file_button };
        let file_button = if new_match {
            let files = self.files.clone();
            let current = self.target_file.clone();
            let weak = cx.entity().downgrade();
            let menu_w = self.file_w.clone();
            file_button
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
                    let w = px(menu_w.get().max(200.0));
                    menu = menu.scrollable(true).max_h(px(320.0)).min_w(w).max_w(w);
                    for f in &files {
                        let (view, file) = (weak.clone(), f.clone());
                        menu = menu.item(PopupMenuItem::new(f.clone()).checked(*f == current).on_click(move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.target_file = file.clone();
                                if let Some(c) = &mut this.current {
                                    c.file = file.clone();
                                }
                                cx.notify();
                            });
                        }));
                    }
                    menu
                })
                .into_any_element()
        } else {
            file_button.into_any_element()
        };
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child({
                        let cell = self.file_w.clone();
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .overflow_hidden()
                            .child(file_button)
                            // Measures the space. A change paints once more.
                            .child(
                                canvas(
                                    move |bounds, window, _| {
                                        let w = f32::from(bounds.size.width).floor();
                                        if (cell.get() - w).abs() >= 1.0 {
                                            cell.set(w);
                                            window.refresh();
                                        }
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .inset_0()
                                .size_full(),
                            )
                    })
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
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
                    .h(px(TAB_ROW_H))
                    .flex_none()
                    .flex()
                    .items_end()
                    .px_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().flex_1().child(tabs)),
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
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .capture_key_down(cx.listener(Self::on_key))
            .on_key_down(cx.listener(Self::on_escape))
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

/// The settings of the editor are small text files in `%LOCALAPPDATA%\snip`.
const LAST_FILE: &str = "last-file.txt";

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

/// The menu of variable types, each with its help line. A pick adds a
/// variable of that type; with `insert`, it also inserts it at the caret.
fn type_menu(mut menu: PopupMenu, view: WeakEntity<EditorView>, insert: bool) -> PopupMenu {
    for (t, _) in VAR_TYPES {
        let t: &'static str = t;
        let view = view.clone();
        let label = format!("{t}: {}", matchedit::var_help(t));
        menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
            let _ = view.update(cx, |this, cx| {
                if insert {
                    this.insert_at_caret(Pick::New(t), window, cx);
                } else {
                    this.edit_form(window, cx, |f| {
                        let taken: Vec<String> = f.vars.iter().map(|v| matchedit::var_get(v, &["name"])).collect();
                        f.vars.push(matchedit::new_var(&matchedit::free_var_name(t, &taken), t));
                    });
                }
            });
        }));
    }
    menu
}

/// The rows of the `{{` picker for the name `typed` so far: this snippet's
/// variables, then the global ones, then a new variable of each type. A
/// name matches when it contains `typed`, ignoring case; names that start
/// with it come first. A type matches when it starts with `typed`.
fn pick_rows(own: &[String], globals: &[(String, String)], typed: &str) -> Vec<Pick> {
    const MAX: usize = 12;
    let typed = typed.to_lowercase();
    let rank = |name: &str| {
        let n = name.to_lowercase();
        if n.starts_with(&typed) { Some(0) } else if n.contains(&typed) { Some(1) } else { None }
    };
    let mut vars: Vec<(u8, usize, Pick)> = own
        .iter()
        .map(|n| (rank(n), Pick::Own(n.clone())))
        .chain(globals.iter().filter(|(g, _)| !own.contains(g)).map(|(g, t)| (rank(g), Pick::Global(g.clone(), t.clone()))))
        .enumerate()
        .filter_map(|(i, (r, p))| r.map(|r| (r, i, p)))
        .collect();
    vars.sort_by_key(|(r, i, _)| (*r, *i));
    let new = VAR_TYPES.iter().map(|(t, _)| *t).filter(|t| *t != "global" && t.starts_with(&typed)).map(Pick::New);
    vars.into_iter().map(|(_, _, p)| p).chain(new).take(MAX).collect()
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

#[cfg(test)]
mod tests {
    use super::{pick_rows, Pick};

    #[test]
    fn picker_rows_put_own_and_prefix_matches_first() {
        let own = vec!["name".to_string(), "surname".to_string()];
        let globals = vec![("myname".to_string(), "echo".to_string()), ("name".to_string(), "echo".to_string())];
        let rows = pick_rows(&own, &globals, "Na");
        // Prefix matches first, a global with an own name is left out, and
        // no type starts with "na".
        assert_eq!(
            rows,
            vec![
                Pick::Own("name".into()),
                Pick::Own("surname".into()),
                Pick::Global("myname".into(), "echo".into()),
            ]
        );
        // "d" offers the date type as a new variable.
        assert_eq!(pick_rows(&own, &globals, "d"), vec![Pick::New("date")]);
        // An empty name lists everything, and never the global type.
        let all = pick_rows(&[], &[], "");
        assert!(all.contains(&Pick::New("shell")) && !all.contains(&Pick::New("global")));
    }
}
