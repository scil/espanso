//! Caret popup prototype built with gpui and gpui-kit. See `studio/POPUP.md`.
#![windows_subsystem = "windows"]

use std::sync::OnceLock;
use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Theme};
use gpui_kit::prelude::*;
use gpui_kit::*;
use popup_core::args::Args;
use popup_core::data::Snippets;
use popup_core::focus::Handle;
use popup_core::{caret, focus, place, timing};

const WIDTH: f32 = 480.0;
const HEIGHT: f32 = 380.0;
const ROW_H: f32 = 46.0;

static SNIPPETS: OnceLock<Result<Snippets, String>> = OnceLock::new();

struct Popup {
    input: Entity<InputState>,
    results: Vec<usize>,
    selected: usize,
    status: SharedString,
    scroll: ScrollHandle,
    previous: Option<Handle>,
    type_text: bool,
    filled: bool,
    had_focus: bool,
    keep_open: bool,
    _subs: Vec<Subscription>,
}

impl Popup {
    fn new(args: &Args, previous: Option<Handle>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search snippets")
                .default_value(args.query.clone())
        });
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::Change = ev {
                this.refresh(cx);
            }
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        Popup {
            input,
            results: Vec::new(),
            selected: 0,
            status: "Loading snippets…".into(),
            scroll: ScrollHandle::new(),
            previous,
            type_text: args.type_text,
            filled: false,
            had_focus: false,
            keep_open: args.keep_open,
            _subs: vec![sub],
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(snips)) = SNIPPETS.get() else { return };
        let q = self.input.read(cx).value();
        self.results = snips.filter(&q, "");
        self.selected = 0;
        self.scroll.scroll_to_item(0);
        self.status = format!(
            "{} of {} snippets   ↑↓ select   Enter insert   Esc close",
            self.results.len(),
            snips.entries.len()
        )
        .into();
        cx.notify();
    }

    fn choose(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(snips)) = SNIPPETS.get() else { return };
        let Some(&i) = self.results.get(row) else { return };
        match snips.text_for(&snips.entries[i]) {
            Ok(text) => {
                window.minimize_window();
                popup_core::finish(self.previous, Some(text), self.type_text);
            }
            Err(e) => {
                self.status = format!("Cannot insert: {e}").into();
                cx.notify();
            }
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "up" => {
                self.selected = self.selected.saturating_sub(1);
                self.scroll.scroll_to_item(self.selected);
            }
            "down" => {
                if self.selected + 1 < self.results.len() {
                    self.selected += 1;
                }
                self.scroll.scroll_to_item(self.selected);
            }
            "enter" => self.choose(self.selected, window, cx),
            "escape" => popup_core::finish(self.previous, None, false),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Runs every 15 ms: fills the list when the data thread finishes, and
    /// closes the popup when it loses focus, like an input method window.
    fn tick(&mut self, cx: &mut Context<Self>) {
        if !self.filled {
            match SNIPPETS.get() {
                Some(Ok(_)) => {
                    self.filled = true;
                    self.refresh(cx);
                    timing::mark("list-filled");
                }
                Some(Err(e)) => {
                    self.filled = true;
                    self.status = format!("Cannot load snippets: {e}").into();
                    cx.notify();
                }
                None => {}
            }
        }
        let own = focus::own_window();
        if own.is_some() && own == focus::foreground() {
            self.had_focus = true;
        } else if self.had_focus && !self.keep_open {
            popup_core::finish(None, None, false);
        }
    }
}

impl Render for Popup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let snips = match SNIPPETS.get() {
            Some(Ok(s)) => Some(s),
            _ => None,
        };
        let rows = self.results.iter().enumerate().filter_map(|(row, &i)| {
            let e = &snips?.entries[i];
            let title = if e.label.is_empty() { e.trigger.clone() } else { e.label.clone() };
            let badge = if !e.label.is_empty() {
                e.trigger.clone()
            } else if !e.insertable {
                "espanso".to_string()
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
                    .px_3()
                    .rounded_md()
                    .when(selected, |d| d.bg(theme.list_active))
                    .when(!selected, |d| d.hover(|s| s.bg(theme.list_hover)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = row;
                        this.choose(row, window, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(title))
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

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_2p5()
            .bg(theme.background)
            .text_color(theme.foreground)
            .capture_key_down(cx.listener(Self::on_key))
            .child(Input::new(&self.input))
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
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(self.status.clone()),
            )
    }
}

fn main() {
    popup_core::init_dpi();
    let args = Args::parse();
    timing::enable(args.timing);
    timing::mark("main");

    let previous = focus::foreground();
    let base = args.base.clone();
    std::thread::spawn(move || {
        let loaded = Snippets::load(base).map_err(|e| e.to_string());
        let _ = SNIPPETS.set(loaded);
        timing::mark("data-loaded");
    });

    let caret = caret::locate(Duration::from_millis(250));
    timing::mark(&format!("caret-{:?}", caret.source));
    let p = place::below_caret(&caret, WIDTH, HEIGHT);

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        Theme::sync_system_appearance(None, cx);
        timing::mark("app-init");

        // gpui takes logical screen coordinates.
        let origin = point(px(p.x as f32 / p.scale), px(p.y as f32 / p.scale));
        // gpui checks the bounds against one display (the primary one when
        // none is given) and falls back to default bounds when they do not
        // fit, so pass the display that holds the caret.
        let display_id = cx
            .displays()
            .into_iter()
            .find(|d| d.bounds().contains(&origin))
            .map(|d| d.id());
        let options = WindowOptions {
            display_id,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: size(px(WIDTH), px(HEIGHT)),
            })),
            titlebar: None,
            focus: true,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        };
        let args2 = args.clone();
        let (_, view) = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| Popup::new(&args2, previous, window, cx))
        })
        .expect("cannot open the popup window");
        timing::mark("window-opened");

        let exit_after = args.exit_after_ms;
        view.update(cx, |_, cx| {
            cx.on_next_frame_hook(exit_after);
            cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_millis(15)).await;
                if this.update(cx, |p, cx| p.tick(cx)).is_err() {
                    break;
                }
            })
            .detach();
        });
    });
}

/// First frame: take focus, round the corners, start the benchmark timer.
trait FirstFrame {
    fn on_next_frame_hook(&mut self, exit_after: Option<u64>);
}

impl FirstFrame for Context<'_, Popup> {
    fn on_next_frame_hook(&mut self, exit_after: Option<u64>) {
        self.defer(move |cx| {
            let Some(window) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
                return;
            };
            let _ = window.update(cx, move |_, window, _| {
                window.on_next_frame(move |_, cx| {
                    timing::mark("first-frame");
                    if let Some(h) = focus::own_window() {
                        focus::round_corners(h);
                        focus::bring_to_front(h);
                    }
                    if let Some(ms) = exit_after {
                        cx.spawn(async move |cx| {
                            cx.background_executor().timer(Duration::from_millis(ms)).await;
                            timing::mark("exit");
                            std::process::exit(0)
                        })
                        .detach();
                    }
                });
            });
        });
    }
}
