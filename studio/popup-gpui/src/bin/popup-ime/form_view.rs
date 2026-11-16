//! The form window: espanso's `form` and `choice` variables, shown near the
//! caret. See `studio/EXPANDER.md`, "Forms and choice variables".
//!
//! The window takes the keyboard focus, so an input method works in it. It
//! shows the layout as a sentence with an input box for each field, and a
//! preview of the result. The engine thread waits for the answer on `Reply`.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};

use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::*;
use gpui_kit::*;
use popup_core::caret::Caret;
use popup_core::focus::{self, Handle};
use popup_core::form::{self, Form, Kind, Part};
use popup_core::{place, Rect};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// The typed values by field name, or `None` for Cancel.
pub type Reply = Sender<Option<HashMap<String, String>>>;

pub const WIDTH: f32 = 560.0;
const PAD: f32 = 10.0;
const LINE_H: f32 = 36.0;
const AREA_H: f32 = 90.0;
const ROW_H: f32 = 28.0;
const LIST_ROWS: usize = 6;
const PREVIEW_LINES: usize = 4;
const TEXT_H: f32 = 18.0;
const STATUS_H: f32 = 22.0;

enum Editor {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
}

struct FieldView {
    editor: Editor,
    /// The values of a `choice` or `list` field. Its input box filters them.
    values: Option<Vec<String>>,
    /// Indexes of the values that match the filter.
    matches: Vec<usize>,
    /// The selected entry of `matches`.
    selected: usize,
    width: f32,
}

pub struct FormView {
    form: Form,
    fields: Vec<FieldView>,
    current: usize,
    reply: Option<Reply>,
    was_active: bool,
    list: ScrollHandle,
    _subs: Vec<Subscription>,
}

/// Opens the form window below the caret line, or above it.
pub fn open(form: Form, caret: Caret, reply: Reply, cx: &mut App) {
    let height = height(&form);
    let p = place::below_caret(&caret, WIDTH, height);
    let rect = Rect {
        left: p.x,
        top: p.y,
        right: p.x + (WIDTH * p.scale).round() as i32,
        bottom: p.y + (height * p.scale).round() as i32,
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(WIDTH), px(height)),
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
    let opened = cx.open_window(options, move |window, cx| {
        cx.new(|cx| FormView::new(form, reply.clone(), rect, window, cx))
    });
    if let Err(e) = opened {
        // `reply` moved into the closure and is dropped with it, which
        // the engine reads as Cancel.
        popup_core::log::line(&format!("form: cannot open the window: {e:#}"));
    }
}

impl FormView {
    fn new(form: Form, reply: Reply, rect: Rect, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut fields = Vec::new();
        let mut subs = Vec::new();
        for (i, f) in form.fields.iter().enumerate() {
            let on_event = move |this: &mut Self, ev: &InputEvent, cx: &mut Context<Self>| this.on_input(i, ev, cx);
            let view = match &f.kind {
                Kind::Text { multiline: true } => {
                    let s = cx.new(|cx| TextareaState::new(window, cx).default_value(f.default.clone()));
                    subs.push(cx.subscribe_in(&s, window, move |this, _, ev, _, cx| on_event(this, ev, cx)));
                    FieldView { editor: Editor::Area(s), values: None, matches: vec![], selected: 0, width: WIDTH - 2.0 * PAD }
                }
                Kind::Text { multiline: false } => {
                    let s = cx.new(|cx| InputState::new(window, cx).default_value(f.default.clone()));
                    subs.push(cx.subscribe_in(&s, window, move |this, _, ev, _, cx| on_event(this, ev, cx)));
                    FieldView { editor: Editor::Line(s), values: None, matches: vec![], selected: 0, width: box_width(&f.default) }
                }
                Kind::Choice { values } => {
                    let s = cx.new(|cx| InputState::new(window, cx).placeholder("filter"));
                    subs.push(cx.subscribe_in(&s, window, move |this, _, ev, _, cx| on_event(this, ev, cx)));
                    let width = values.iter().map(|v| box_width(v)).fold(box_width(""), f32::max);
                    FieldView {
                        editor: Editor::Line(s),
                        values: Some(values.clone()),
                        matches: (0..values.len()).collect(),
                        selected: values.iter().position(|v| *v == f.default).unwrap_or(0),
                        width,
                    }
                }
            };
            fields.push(view);
        }
        // Clicking another window cancels the form, as it would close an
        // input method window.
        subs.push(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.was_active = true;
            } else if this.was_active {
                this.finish(false, window, cx);
            }
        }));
        let hwnd = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(Handle(h.hwnd.get())),
            _ => None,
        };
        if let Some(h) = hwnd {
            focus::round_corners(h);
            // gpui handles the messages of `SetWindowPos` and
            // `SetForegroundWindow` with its app state, which this thread
            // holds now. Another thread makes them wait in the queue.
            std::thread::spawn(move || {
                focus::show_at(h, rect);
                focus::bring_to_front(h);
            });
        }
        let mut view = FormView {
            form,
            fields,
            current: 0,
            reply: Some(reply),
            was_active: false,
            list: ScrollHandle::new(),
            _subs: subs,
        };
        view.focus_field(0, window, cx);
        view
    }

    fn on_input(&mut self, i: usize, ev: &InputEvent, cx: &mut Context<Self>) {
        match ev {
            InputEvent::Focus => self.current = i,
            InputEvent::Change => {
                let f = &mut self.fields[i];
                if let (Some(values), Editor::Line(s)) = (&f.values, &f.editor) {
                    f.matches = form::filter(values, &s.read(cx).value());
                    f.selected = 0;
                    self.list.scroll_to_item(0);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn focus_field(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.fields.get(i) else { return };
        self.current = i;
        match &f.editor {
            Editor::Line(s) => s.update(cx, |s, cx| s.focus(window, cx)),
            Editor::Area(s) => s.update(cx, |s, cx| s.focus(window, cx)),
        }
        if f.values.is_some() {
            self.list.scroll_to_item(f.selected);
        }
        cx.notify();
    }

    fn value(&self, i: usize, cx: &App) -> String {
        let f = &self.fields[i];
        match (&f.values, &f.editor) {
            (Some(values), _) => f.matches.get(f.selected).map(|&j| values[j].clone()).unwrap_or_default(),
            (None, Editor::Line(s)) => s.read(cx).value().to_string(),
            (None, Editor::Area(s)) => s.read(cx).value().to_string(),
        }
    }

    fn values(&self, cx: &App) -> HashMap<String, String> {
        self.form.fields.iter().enumerate().map(|(i, f)| (f.name.clone(), self.value(i, cx))).collect()
    }

    /// Sends the answer once and closes the window. The engine thread gives
    /// the focus back to the application.
    fn finish(&mut self, submit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(reply) = self.reply.take() else { return };
        let answer = submit.then(|| self.values(cx));
        popup_core::log::line(&format!("form: {}", if submit { "submit" } else { "cancel" }));
        window.remove_window();
        let _ = reply.send(answer);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        let field = self.fields.get(self.current);
        let multiline = matches!(field.map(|f| &f.editor), Some(Editor::Area(_)));
        let choice = field.is_some_and(|f| f.values.is_some());
        match ev.keystroke.key.as_str() {
            "escape" => self.finish(false, window, cx),
            "enter" if multiline && !m.control => return,
            "enter" => self.finish(true, window, cx),
            "tab" => {
                let n = self.fields.len().max(1);
                let next = if m.shift { (self.current + n - 1) % n } else { (self.current + 1) % n };
                self.focus_field(next, window, cx);
            }
            "up" | "down" if choice => {
                let f = &mut self.fields[self.current];
                let n = f.matches.len();
                if n > 0 {
                    f.selected = if ev.keystroke.key == "up" {
                        f.selected.saturating_sub(1)
                    } else {
                        (f.selected + 1).min(n - 1)
                    };
                    self.list.scroll_to_item(f.selected);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn field_element(&self, i: usize, theme: &component::Theme, cx: &mut Context<Self>) -> AnyElement {
        let f = &self.fields[i];
        match (&f.editor, &f.values) {
            (Editor::Area(s), _) => Textarea::new(s).w_full().h(px(AREA_H - 6.0)).into_any_element(),
            // A list field shows its value until it gets the focus.
            (Editor::Line(_), Some(_)) if i != self.current => div()
                .id(("choice", i))
                .w(px(f.width))
                .h(px(30.0))
                .px_2()
                .flex()
                .items_center()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .truncate()
                .child(self.value(i, cx))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| this.focus_field(i, window, cx)))
                .into_any_element(),
            (Editor::Line(s), _) => Input::new(s).w(px(f.width)).into_any_element(),
        }
    }
}

impl Drop for FormView {
    fn drop(&mut self) {
        // A window closed in any other way counts as Cancel.
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(None);
        }
    }
}

impl Render for FormView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut lines: Vec<Vec<AnyElement>> = vec![Vec::new()];
        let mut field_index = 0;
        for part in self.form.parts.clone() {
            match part {
                Part::Text(t) => {
                    for (n, seg) in t.split('\n').enumerate() {
                        if n > 0 {
                            lines.push(Vec::new());
                        }
                        if !seg.is_empty() {
                            lines.last_mut().unwrap().push(div().child(seg.to_string()).into_any_element());
                        }
                    }
                }
                Part::Field(name) => {
                    let Some(i) = self.form.fields.iter().position(|f| f.name == name) else { continue };
                    // A repeated field shows its box at the first place only.
                    if i >= field_index {
                        field_index = i + 1;
                        let el = self.field_element(i, &theme, cx);
                        lines.last_mut().unwrap().push(el);
                    } else {
                        lines.last_mut().unwrap().push(div().text_color(theme.muted_foreground).child(self.value(i, cx)).into_any_element());
                    }
                }
            }
        }
        let layout = div().flex().flex_col().gap_1().children(
            lines.into_iter().map(|items| div().flex().flex_wrap().items_center().gap_1().min_h(px(LINE_H - 4.0)).children(items)),
        );

        let current = self.fields.get(self.current);
        let has_list = self.fields.iter().any(|f| f.values.is_some());
        let list = current.and_then(|f| Some((f, f.values.as_ref()?))).map(|(f, values)| {
            let rows = f.matches.iter().enumerate().map(|(row, &j)| {
                let selected = row == f.selected;
                let current = self.current;
                div()
                    .id(row)
                    .h(px(ROW_H))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .truncate()
                    .when(selected, |d| d.bg(theme.list_active))
                    .when(!selected, |d| d.hover(|s| s.bg(theme.list_hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.fields[current].selected = row;
                            cx.notify();
                        }),
                    )
                    .child(values[j].clone())
            });
            div().id("list").flex_1().overflow_y_scroll().track_scroll(&self.list).children(rows)
        });

        let preview = self.form.has_preview().then(|| {
            // The cursor hint is not part of the text.
            let text = self.form.fill(&self.values(cx)).replace("$|$", "");
            let shown: Vec<&str> = text.lines().take(PREVIEW_LINES).collect();
            div()
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.muted)
                .text_sm()
                .text_color(theme.muted_foreground)
                .children(shown.into_iter().map(|l| div().truncate().child(l.to_string())))
        });

        let multiline = matches!(current.map(|f| &f.editor), Some(Editor::Area(_)));
        let status = match (multiline, current.is_some_and(|f| f.values.is_some())) {
            (true, _) => "Tab next field   Ctrl+Enter insert   Esc cancel",
            (_, true) => "↑↓ choose   Tab next field   Enter insert   Esc cancel",
            _ => "Tab next field   Enter insert   Esc cancel",
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .p(px(PAD))
            .bg(theme.background)
            .text_color(theme.foreground)
            .capture_key_down(cx.listener(Self::on_key))
            .child(layout)
            .children(preview)
            .when(has_list, |d| match list {
                Some(list) => d.child(list),
                None => d.child(div().flex_1()),
            })
            .child(div().h(px(STATUS_H)).flex().items_center().text_xs().text_color(theme.muted_foreground).child(status))
    }
}

/// Width estimate of text in logical pixels: wide characters, such as
/// Chinese, take about twice the width of ASCII.
fn text_width(s: &str) -> f32 {
    s.chars().map(|c| if c.is_ascii() { 7.5 } else { 14.0 }).sum()
}

fn box_width(default: &str) -> f32 {
    (text_width(default) + 32.0).clamp(140.0, WIDTH - 2.0 * PAD)
}

/// The window height for a form: the layout rows, the preview, the list and
/// the status line.
pub fn height(form: &Form) -> f32 {
    let inner = WIDTH - 2.0 * PAD;
    let mut rows = 1.0;
    let mut extra = 0.0;
    let mut used = 0.0;
    let add = |w: f32, rows: &mut f32, used: &mut f32| {
        if *used + w > inner && *used > 0.0 {
            *rows += 1.0;
            *used = 0.0;
        }
        *used += w;
    };
    for part in &form.parts {
        match part {
            Part::Text(t) => {
                for (n, seg) in t.split('\n').enumerate() {
                    if n > 0 {
                        rows += 1.0;
                        used = 0.0;
                    }
                    add(text_width(seg), &mut rows, &mut used);
                }
            }
            Part::Field(name) => match form.fields.iter().find(|f| &f.name == name).map(|f| (&f.kind, f)) {
                Some((Kind::Text { multiline: true }, _)) => extra += AREA_H,
                Some((Kind::Choice { values }, _)) => {
                    let w = values.iter().map(|v| box_width(v)).fold(box_width(""), f32::max);
                    add(w + 4.0, &mut rows, &mut used);
                }
                Some((_, f)) => add(box_width(&f.default) + 4.0, &mut rows, &mut used),
                None => {}
            },
        }
    }
    let mut h = PAD * 2.0 + STATUS_H + rows * LINE_H + extra + 8.0;
    if form.has_preview() {
        let defaults: HashMap<String, String> =
            form.fields.iter().map(|f| (f.name.clone(), f.default.clone())).collect();
        let lines = form.fill(&defaults).lines().count().clamp(1, PREVIEW_LINES);
        h += lines as f32 * TEXT_H + 12.0;
    }
    let list_len = form
        .fields
        .iter()
        .filter_map(|f| match &f.kind {
            Kind::Choice { values } => Some(values.len()),
            _ => None,
        })
        .max();
    if let Some(n) = list_len {
        h += n.clamp(1, LIST_ROWS) as f32 * ROW_H + 8.0;
    }
    h
}

/// A channel for one answer.
pub fn reply_channel() -> (Reply, mpsc::Receiver<Option<HashMap<String, String>>>) {
    mpsc::channel()
}
