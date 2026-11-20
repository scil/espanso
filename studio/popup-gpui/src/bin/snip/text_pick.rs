//! The text pick window: Ctrl+Caps Lock in the popup. It shows the rendered
//! text of a snippet near the caret, and the user selects parts of it with
//! the mouse or with Shift and the arrow keys. Ctrl with a mouse press keeps
//! the selection as a picked part and starts the next one; a press without
//! Ctrl drops the picked parts. Enter inserts the parts in text order, one
//! per line. Without a selection Enter inserts nothing. Esc cancels.
//!
//! The window takes the keyboard focus, as the form window does. The engine
//! thread waits for the answer on `Reply`.

use std::sync::mpsc::Sender;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{self as input, Editor, EditorState, TextDecoration, TextDecorationCollection};
use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::*;
use gpui_kit::*;
use popup_core::caret::Caret;
use popup_core::focus::{self, Handle};
use popup_core::{place, Rect};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// The text to insert, or `None` for Cancel.
pub type Reply = Sender<Option<String>>;

const WIDTH: f32 = 640.0;
const PAD: f32 = 10.0;
const LINE_H: f32 = 20.0;
const STATUS_H: f32 = 30.0;
const MIN_LINES: usize = 4;
const MAX_LINES: usize = 22;

pub struct TextPickView {
    text: Entity<EditorState>,
    /// The parts picked with Ctrl, shown with a background color. Their
    /// ranges follow edits of the text.
    picked: TextDecorationCollection,
    /// Enter came without a selection: the status line says so.
    nothing: bool,
    reply: Option<Reply>,
    was_active: bool,
    _subs: Vec<Subscription>,
}

/// The window height for a text: its lines, wrapped at the window width,
/// between `MIN_LINES` and `MAX_LINES`, and the status line.
fn height(text: &str) -> f32 {
    // About 80 ASCII characters fit in a line; a wide character, such as
    // Chinese, takes two.
    let lines: usize = text
        .split('\n')
        .map(|l| {
            let w: usize = l.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
            w / 80 + 1
        })
        .sum();
    let lines = lines.clamp(MIN_LINES, MAX_LINES);
    PAD * 2.0 + lines as f32 * LINE_H + 24.0 + STATUS_H
}

/// Opens the window below the caret line, or above it.
pub fn open(text: String, caret: Caret, reply: Reply, cx: &mut App) {
    let height = height(&text);
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
        cx.new(|cx| TextPickView::new(text, reply.clone(), rect, window, cx))
    });
    if let Err(e) = opened {
        // `reply` is dropped with the closure, which the engine reads as
        // Cancel.
        popup_core::log::line(&format!("text pick: cannot open the window: {e:#}"));
    }
}

impl TextPickView {
    fn new(text: String, reply: Reply, rect: Rect, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| {
            EditorState::new(window, cx).line_number(false).folding(false).soft_wrap(true).default_value(text)
        });
        let picked = state.update(cx, |s, cx| s.create_decorations_collection(Vec::new(), cx));
        let mut subs = Vec::new();
        // Clicking another window cancels, as in the form window.
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
            // See `form_view::FormView::new`: another thread moves and
            // raises the window, so gpui handles those messages later.
            std::thread::spawn(move || {
                focus::show_at(h, rect);
                focus::bring_to_front(h);
            });
        }
        state.update(cx, |s, cx| {
            s.set_selected_range(0..0, cx);
            s.focus(window, cx);
        });
        TextPickView { text: state, picked, nothing: false, reply: Some(reply), was_active: false, _subs: subs }
    }

    /// The picked parts and the selection, in text order, one per line.
    /// `None` when nothing is selected.
    fn chosen(&self, cx: &App) -> Option<String> {
        let s = self.text.read(cx);
        let value = s.value().to_string();
        let mut ranges = self.picked.get_ranges(cx);
        ranges.push(s.selected_range());
        let parts: Vec<&str> = merge(ranges).into_iter().filter_map(|r| value.get(r)).collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    /// A mouse press in the text. With Ctrl, the selection becomes a picked
    /// part, and the press starts the next selection. Without Ctrl, the
    /// picked parts go.
    fn on_press(&mut self, ev: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if ev.button != MouseButton::Left {
            return;
        }
        if ev.modifiers.control {
            let r = self.text.read(cx).selected_range();
            if !r.is_empty() {
                let style = HighlightStyle { background_color: Some(cx.theme().selection), ..Default::default() };
                self.picked.append(vec![TextDecoration::new(r, style)], cx);
            }
        } else {
            self.picked.clear(cx);
        }
        self.nothing = false;
        cx.notify();
    }

    /// Sends the answer once and closes the window. The engine thread gives
    /// the focus back to the application.
    fn finish(&mut self, insert: bool, window: &mut Window, cx: &mut Context<Self>) {
        let answer = match insert {
            true => match self.chosen(cx) {
                Some(t) => Some(t),
                // Nothing selected: nothing to insert, and the window stays.
                None => {
                    self.nothing = true;
                    cx.notify();
                    return;
                }
            },
            false => None,
        };
        let Some(reply) = self.reply.take() else { return };
        popup_core::log::line(&format!(
            "text pick: {}",
            match &answer {
                Some(t) => format!("insert {} characters", t.chars().count()),
                None => "cancel".into(),
            }
        ));
        window.remove_window();
        let _ = reply.send(answer);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        match ev.keystroke.key.as_str() {
            "escape" => self.finish(false, window, cx),
            // Shift+Enter breaks the line, as in other text boxes.
            "enter" if !m.shift && !m.alt => self.finish(true, window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Drop for TextPickView {
    fn drop(&mut self) {
        // A window closed in any other way counts as Cancel.
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(None);
        }
    }
}

impl Render for TextPickView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status = if self.nothing {
            "Nothing selected. Select text first, or Esc to cancel."
        } else {
            "Drag to select   Ctrl+drag adds a part   Enter insert   Esc cancel"
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
            // The editor binds Enter and Esc to actions, and a key binding
            // runs before any key listener. Catch the actions first.
            // Shift+Enter still breaks the line.
            .capture_action(cx.listener(|this, a: &input::Enter, window, cx| {
                if !a.shift {
                    cx.stop_propagation();
                    this.finish(true, window, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::Escape, window, cx| {
                cx.stop_propagation();
                this.finish(false, window, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .capture_any_mouse_down(cx.listener(Self::on_press))
                    .child(Editor::new(&self.text).h_full()),
            )
            .child(
                div()
                    .h(px(STATUS_H))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_xs()
                            .text_color(if self.nothing { theme.danger } else { theme.muted_foreground })
                            .child(status),
                    )
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .ghost()
                            .compact()
                            .on_click(cx.listener(|this, _, window, cx| this.finish(false, window, cx))),
                    )
                    .child(
                        Button::new("insert")
                            .label("Insert")
                            .primary()
                            .compact()
                            .on_click(cx.listener(|this, _, window, cx| this.finish(true, window, cx))),
                    ),
            )
    }
}

/// Sorts byte ranges and joins the ones that overlap or touch. Empty ranges
/// go.
fn merge(mut ranges: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
    ranges.retain(|r| !r.is_empty());
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::merge;

    #[test]
    fn merge_sorts_and_joins_overlaps() {
        assert_eq!(merge(vec![8..10, 0..3, 2..5, 6..6]), vec![0..5, 8..10]);
        assert!(merge(vec![3..3]).is_empty());
    }
}
