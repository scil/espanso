//! Caret popup prototype built with Slint. See `studio/POPUP.md`.
#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use popup_core::args::Args;
use popup_core::data::Snippets;
use popup_core::{caret, focus, place, timing};
use slint::{ModelRc, SharedString, VecModel};

const WIDTH: f32 = 480.0;
const HEIGHT: f32 = 380.0;

static SNIPPETS: OnceLock<Result<Snippets, String>> = OnceLock::new();

slint::slint! {
    import { Palette } from "std-widgets.slint";

    export struct Row {
        trigger: string,
        label: string,
        preview: string,
        insertable: bool,
    }

    export component Popup inherits Window {
        in property <[Row]> rows;
        in-out property <int> selected: 0;
        in property <string> status;
        in-out property <string> query;
        callback query-edited(string);
        callback choose(int);
        callback cancel();

        no-frame: true;
        always-on-top: true;
        width: 480px;
        height: 380px;
        background: Palette.background;
        title: "Snippets";
        forward-focus: input;

        property <length> row-h: 46px;

        changed selected => {
            if (selected * row-h < -list.content-y) {
                list.content-y = -selected * row-h;
            } else if ((selected + 1) * row-h > -list.content-y + list.height) {
                list.content-y = -((selected + 1) * row-h - list.height);
            }
        }

        Rectangle {
            border-width: 1px;
            border-color: Palette.border;
        }

        FocusScope {
            capture-key-pressed(event) => {
                if (event.text == Key.UpArrow) {
                    if (selected > 0) { selected -= 1; }
                    return accept;
                }
                if (event.text == Key.DownArrow) {
                    if (selected < rows.length - 1) { selected += 1; }
                    return accept;
                }
                if (event.text == Key.Return) {
                    root.choose(selected);
                    return accept;
                }
                if (event.text == Key.Escape) {
                    root.cancel();
                    return accept;
                }
                return reject;
            }

            VerticalLayout {
                padding: 10px;
                spacing: 8px;

                Rectangle {
                    height: 40px;
                    border-radius: 8px;
                    background: Palette.control-background;
                    border-width: 1px;
                    border-color: input.has-focus ? Palette.accent-background : Palette.border;

                    HorizontalLayout {
                        padding-left: 12px;
                        padding-right: 12px;
                        input := TextInput {
                            text <=> root.query;
                            horizontal-stretch: 1;
                            horizontal-alignment: left;
                            font-size: 15px;
                            vertical-alignment: center;
                            single-line: true;
                            color: Palette.foreground;
                            edited => { root.query-edited(self.text); }
                        }
                    }
                }

                list := Flickable {
                    content-height: rows.length * row-h;
                    for row[i] in rows : Rectangle {
                        x: 0;
                        y: i * row-h;
                        width: parent.width;
                        height: row-h;
                        border-radius: 6px;
                        background: i == selected ? Palette.accent-background.transparentize(0.82)
                                  : touch.has-hover ? Palette.foreground.transparentize(0.95)
                                  : transparent;

                        touch := TouchArea {
                            clicked => { root.selected = i; root.choose(i); }
                        }

                        HorizontalLayout {
                            padding-left: 10px;
                            padding-right: 10px;
                            spacing: 10px;
                            VerticalLayout {
                                alignment: center;
                                Text {
                                    text: row.label != "" ? row.label : row.trigger;
                                    font-size: 14px;
                                    font-weight: 600;
                                    overflow: elide;
                                    color: Palette.foreground;
                                }
                                Text {
                                    text: row.preview;
                                    font-size: 12px;
                                    overflow: elide;
                                    color: Palette.foreground.transparentize(0.45);
                                }
                            }
                            if row.label != "" || !row.insertable : Rectangle {
                                horizontal-stretch: 0;
                                width: badge.preferred-width + 12px;
                                height: 22px;
                                y: (parent.height - self.height) / 2;
                                border-radius: 4px;
                                background: Palette.foreground.transparentize(0.92);
                                badge := Text {
                                    text: row.label != "" ? row.trigger : "espanso";
                                    font-size: 11px;
                                    font-family: "Consolas";
                                    color: Palette.foreground.transparentize(0.3);
                                }
                            }
                        }
                    }
                }

                Text {
                    text: status;
                    font-size: 11px;
                    color: Palette.foreground.transparentize(0.5);
                }
            }
        }
    }
}

struct State {
    results: RefCell<Vec<usize>>,
    model: Rc<VecModel<Row>>,
}

impl State {
    fn refresh(&self, ui: &Popup, query: &str) {
        let Some(Ok(snips)) = SNIPPETS.get() else {
            return;
        };
        let hits = snips.filter(query, "");
        let rows: Vec<Row> = hits
            .iter()
            .map(|&i| {
                let e = &snips.entries[i];
                Row {
                    trigger: e.trigger.as_str().into(),
                    label: e.label.as_str().into(),
                    preview: e.preview.as_str().into(),
                    insertable: e.insertable,
                }
            })
            .collect();
        ui.set_status(SharedString::from(format!(
            "{} of {} snippets   ↑↓ select   Enter insert   Esc close",
            hits.len(),
            snips.entries.len()
        )));
        self.model.set_vec(rows);
        *self.results.borrow_mut() = hits;
        ui.set_selected(0);
    }
}

fn main() -> Result<(), slint::PlatformError> {
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

    let ui = Popup::new()?;
    timing::mark("ui-created");
    let p = place::below_caret(&caret, WIDTH, HEIGHT);
    ui.window().set_position(slint::PhysicalPosition::new(p.x, p.y));
    ui.set_query(args.query.as_str().into());
    ui.set_status("Loading snippets…".into());

    let state = Rc::new(State {
        results: RefCell::new(Vec::new()),
        model: Rc::new(VecModel::default()),
    });
    ui.set_rows(ModelRc::from(state.model.clone()));

    {
        let (state, weak) = (state.clone(), ui.as_weak());
        ui.on_query_edited(move |q: SharedString| {
            if let Some(ui) = weak.upgrade() {
                state.refresh(&ui, &q);
            }
        });
    }
    {
        let (state, weak, type_text) = (state.clone(), ui.as_weak(), args.type_text);
        ui.on_choose(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(Ok(snips)) = SNIPPETS.get() else { return };
            let Some(&i) = state.results.borrow().get(row as usize) else { return };
            match snips.text_for(&snips.entries[i]) {
                Ok(text) => {
                    let _ = ui.hide();
                    popup_core::finish(previous, Some(text), type_text);
                }
                Err(e) => ui.set_status(SharedString::from(format!("Cannot insert: {e}"))),
            }
        });
    }
    ui.on_cancel(move || popup_core::finish(previous, None, false));

    // First frame: take focus, round the corners, start the benchmark timer.
    {
        let exit_after = args.exit_after_ms;
        let mut done = false;
        let _ = ui.window().set_rendering_notifier(move |s, _| {
            if done || !matches!(s, slint::RenderingState::AfterRendering) {
                return;
            }
            done = true;
            timing::mark("first-frame");
            if let Some(ms) = exit_after {
                slint::Timer::single_shot(Duration::from_millis(ms), || {
                    timing::mark("exit");
                    std::process::exit(0)
                });
            }
        });
    }
    ui.show()?;
    if let Some(h) = focus::own_window() {
        focus::round_corners(h);
        focus::bring_to_front(h);
    }
    timing::mark("shown");

    // Fill the list when the data thread finishes, and close when the popup
    // loses focus, like an input method window.
    let poll = slint::Timer::default();
    {
        let (state, weak) = (state.clone(), ui.as_weak());
        let mut filled = false;
        let mut had_focus = false;
        let keep_open = args.keep_open;
        poll.start(slint::TimerMode::Repeated, Duration::from_millis(15), move || {
            let Some(ui) = weak.upgrade() else { return };
            if !filled {
                match SNIPPETS.get() {
                    Some(Ok(_)) => {
                        filled = true;
                        state.refresh(&ui, &ui.get_query());
                        timing::mark("list-filled");
                    }
                    Some(Err(e)) => {
                        filled = true;
                        ui.set_status(SharedString::from(format!("Cannot load snippets: {e}")));
                    }
                    None => {}
                }
            }
            let own = focus::own_window();
            let fg = focus::foreground();
            if own.is_some() && own == fg {
                had_focus = true;
            } else if had_focus && !keep_open {
                popup_core::finish(None, None, false);
            }
        });
    }

    slint::run_event_loop()
}
