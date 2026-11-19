//! The settings panel. It shows in the right half of the popup edit
//! session, where the edit form shows otherwise. The Settings button under
//! the snippet list and "Settings..." in the tray menu open it.
//!
//! It shows the settings file (`popup_core::settings`). Save writes the
//! file, and the popup then restarts snip, because the hook and the engine
//! read the settings only at the start. A setting that a command line flag
//! sets shows its value as text: the flag wins over the file.
//!
//! Pause, Start at sign-in, Restart as administrator and Open log act at
//! once, as the tray menu items do. The popup that holds the tray gets them
//! as [`SettingsEvent`]s.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use popup_core::args::Args;
use popup_core::focus::LongText;
use popup_core::hook::TabKey;
use popup_core::settings::{self, Settings};

const LABEL_W: f32 = 170.0;

const TABS: [(&str, &str); 3] = [("accept", "Insert the row"), ("move", "Next row"), ("ignore", "Leave to the app")];
const LONG: [(&str, &str); 2] = [("paste", "Paste"), ("type", "Type")];

/// What the panel asks the popup to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsEvent {
    Pause(bool),
    SignIn(bool),
    RestartAsAdmin,
    OpenLog,
    /// The settings file changed: start snip again.
    Restart,
    /// Esc or Close: the popup shows only its list again.
    Close,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

/// The state that the popup knows and the window shows.
pub struct Status {
    pub paused: bool,
    pub sign_in: bool,
    pub admin: bool,
    pub can_restart_as_admin: bool,
}

pub struct SettingsView {
    /// The settings file as it was when the window opened.
    saved: Settings,
    /// The values that snip runs with now.
    running: Settings,
    /// The keys that a command line flag sets.
    fixed: Vec<&'static str>,
    tab: String,
    long_text: String,
    log: bool,
    trigger: Entity<InputState>,
    short_limit: Entity<InputState>,
    type_chunk: Entity<InputState>,
    type_gap_ms: Entity<InputState>,
    prefix_wait_ms: Entity<InputState>,
    status: Status,
    /// The last message under the form, and whether it is an error.
    note: Option<(bool, String)>,
    /// The panel takes keys (Esc) with this focus, when no box has it.
    pub focus: FocusHandle,
}

/// The settings that the running process uses, from its arguments.
pub fn running_settings(a: &Args) -> Settings {
    Settings {
        trigger: a.trigger.clone(),
        tab: match a.tab {
            TabKey::Accept => "accept",
            TabKey::Move => "move",
            TabKey::Ignore => "ignore",
        }
        .into(),
        long_text: if a.insert.long == LongText::Type { "type" } else { "paste" }.into(),
        short_limit: a.insert.short_limit,
        type_chunk: a.insert.pace.chunk,
        type_gap_ms: a.insert.pace.gap.as_millis() as u64,
        prefix_wait_ms: a.prefix_wait.as_millis() as u64,
        log: a.log.is_some(),
    }
}

impl SettingsView {
    pub fn new(running: Settings, fixed: Vec<&'static str>, status: Status, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = Settings::load();
        let mut input = |text: String| cx.new(|cx| InputState::new(window, cx).default_value(text));
        SettingsView {
            trigger: input(saved.trigger.clone()),
            short_limit: input(saved.short_limit.to_string()),
            type_chunk: input(saved.type_chunk.to_string()),
            type_gap_ms: input(saved.type_gap_ms.to_string()),
            prefix_wait_ms: input(saved.prefix_wait_ms.to_string()),
            tab: saved.tab.clone(),
            long_text: saved.long_text.clone(),
            log: saved.log,
            saved,
            running,
            fixed,
            status,
            note: None,
            focus: cx.focus_handle(),
        }
    }

    /// Reads the form. An error names the box with a wrong value.
    fn draft(&self, cx: &App) -> Result<Settings, String> {
        let text = |s: &Entity<InputState>| s.read(cx).value().trim().to_string();
        fn number<T: std::str::FromStr>(v: String, what: &str) -> Result<T, String> {
            v.parse().map_err(|_| format!("{what} must be a whole number, not \"{v}\"."))
        }
        let trigger = text(&self.trigger);
        if trigger.is_empty() {
            return Err("The trigger must not be empty.".into());
        }
        Ok(Settings {
            trigger,
            tab: self.tab.clone(),
            long_text: self.long_text.clone(),
            short_limit: number(text(&self.short_limit), "Short text limit")?,
            type_chunk: number::<usize>(text(&self.type_chunk), "Characters per call")?.max(1),
            type_gap_ms: number(text(&self.type_gap_ms), "Gap between calls")?,
            prefix_wait_ms: number(text(&self.prefix_wait_ms), "Wait time")?,
            log: self.log,
        })
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let result = self.draft(cx).and_then(|d| {
            if d == self.saved {
                return Err("Nothing changed.".into());
            }
            d.save().map(|_| d)
        });
        match result {
            Ok(d) => {
                popup_core::log::line("settings: saved, restart");
                self.saved = d;
                self.note = Some((false, "Saved. snip starts again.".into()));
                cx.emit(SettingsEvent::Restart);
            }
            Err(e) => self.note = Some((true, e)),
        }
        cx.notify();
    }

    /// Updates the state after the popup acted on an event.
    pub fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    /// The value as text, when a command line flag sets the key.
    fn fixed_value(&self, key: &str) -> Option<String> {
        if !self.fixed.contains(&key) {
            return None;
        }
        let r = &self.running;
        let v = match key {
            "trigger" => r.trigger.clone(),
            "tab" => r.tab.clone(),
            "long-text" => r.long_text.clone(),
            "short-limit" => r.short_limit.to_string(),
            "type-chunk" => r.type_chunk.to_string(),
            "type-gap-ms" => r.type_gap_ms.to_string(),
            "prefix-wait-ms" => r.prefix_wait_ms.to_string(),
            "log" => if r.log { "on" } else { "off" }.into(),
            _ => String::new(),
        };
        Some(format!("{v}  (set by --{key} on the command line)"))
    }

    fn row(&self, label: &'static str, key: &'static str, control: AnyElement, hint: Option<&'static str>, cx: &App) -> Div {
        let theme = cx.theme();
        let control = match self.fixed_value(key) {
            Some(v) => div().text_sm().text_color(theme.muted_foreground).child(v).into_any_element(),
            None => control,
        };
        div()
            .flex()
            .items_start()
            .gap_3()
            .child(div().w(px(LABEL_W)).flex_none().pt_1().text_sm().child(label))
            .child(
                // Without a zero minimum width, a long hint widens the
                // column past the window instead of wrapping.
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(control)
                    .when_some(hint, |d, h| d.child(div().text_xs().text_color(theme.muted_foreground).child(h))),
            )
    }

    fn input(&self, state: &Entity<InputState>, wide: bool) -> AnyElement {
        div().w(px(if wide { 200.0 } else { 90.0 })).child(Input::new(state).small()).into_any_element()
    }

    fn radios(
        &self,
        id: &'static str,
        options: &'static [(&'static str, &'static str)],
        current: &str,
        set: fn(&mut SettingsView, &str),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        RadioGroup::horizontal(id)
            .children(options.iter().map(|(_, label)| *label))
            .selected_index(options.iter().position(|(v, _)| *v == current))
            .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                if let Some((v, _)) = options.get(*ix) {
                    set(this, v);
                    this.note = None;
                    cx.notify();
                }
            }))
            .into_any_element()
    }
}

fn section(title: &'static str, theme: &gpui_kit::component::Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .pt_2()
        .border_t_1()
        .border_color(theme.border)
        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(title))
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status = &self.status;

        let popup = section("Popup", &theme)
            .child(self.row("Trigger", "trigger", self.input(&self.trigger, true), Some("The characters that open the popup."), cx))
            .child(self.row(
                "Tab key",
                "tab",
                self.radios("tab", &TABS, &self.tab.clone(), |t, v| t.tab = v.into(), cx),
                Some("What Tab does while the popup is open."),
                cx,
            ));

        let insert = section("Insert", &theme)
            .child(self.row(
                "Long text",
                "long-text",
                self.radios("long", &LONG, &self.long_text.clone(), |t, v| t.long_text = v.into(), cx),
                Some("Paste keeps the text out of the clipboard history. Type leaves the clipboard alone."),
                cx,
            ))
            .child(self.row(
                "Short text limit",
                "short-limit",
                self.input(&self.short_limit, false),
                Some("Text with at most this many characters and no line break is always typed."),
                cx,
            ))
            .child(self.row("Characters per call", "type-chunk", self.input(&self.type_chunk, false), None, cx))
            .child(self.row(
                "Gap between calls (ms)",
                "type-gap-ms",
                self.input(&self.type_gap_ms, false),
                Some("Raise these if an app drops or repeats typed characters."),
                cx,
            ));

        let prefix = section("Prefix trigger", &theme).child(self.row(
            "Wait time (ms)",
            "prefix-wait-ms",
            self.input(&self.prefix_wait_ms, false),
            Some("A trigger that starts a longer trigger waits for the next key. 0: no time limit."),
            cx,
        ));

        let log_box = Checkbox::new("log")
            .label("Write events to events.log")
            .checked(self.log)
            .small()
            .on_click(cx.listener(|this, v: &bool, _, cx| {
                this.log = *v;
                this.note = None;
                cx.notify();
            }))
            .into_any_element();
        let admin_text = if status.admin { "snip runs as administrator." } else { "snip runs without administrator rights." };
        let startup = section("Startup and rights", &theme)
            .child(
                Checkbox::new("sign-in")
                    .label("Start at sign-in")
                    .checked(status.sign_in)
                    .small()
                    .on_click(cx.listener(|_, v: &bool, _, cx| cx.emit(SettingsEvent::SignIn(*v)))),
            )
            .child(
                Checkbox::new("pause")
                    .label("Pause expansion")
                    .checked(status.paused)
                    .small()
                    .on_click(cx.listener(|_, v: &bool, _, cx| cx.emit(SettingsEvent::Pause(*v)))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().text_sm().child(admin_text))
                    .when(!status.admin && status.can_restart_as_admin, |d| {
                        d.child(
                            Button::new("admin")
                                .label("Restart as administrator")
                                .ghost()
                                .compact()
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::RestartAsAdmin))),
                        )
                    }),
            )
            .child(self.row("Log", "log", log_box, None, cx))
            .child(
                div().child(
                    Button::new("open-log")
                        .label("Open log")
                        .ghost()
                        .compact()
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::OpenLog))),
                ),
            );

        let file = settings::path().map(|p| p.display().to_string()).unwrap_or_default();
        let note = self.note.clone().map(|(error, text)| {
            div().text_sm().text_color(if error { theme.danger } else { theme.primary }).child(text)
        });
        let buttons = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().min_w(px(0.0)).text_xs().text_color(theme.muted_foreground).child(file))
            .child(Button::new("cancel").label("Close").ghost().compact().on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close))))
            .child(
                Button::new("save")
                    .label("Save and restart snip")
                    .primary()
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            );

        // The header has the height of the edit form's tab row, so it lines
        // up with the list header on the left.
        let header = div()
            .h(px(crate::editor::TAB_ROW_H))
            .flex_none()
            .flex()
            .items_center()
            .px_4()
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex_1().text_sm().font_weight(FontWeight::SEMIBOLD).child("Settings"))
            .child(div().text_xs().text_color(theme.muted_foreground).child("Esc: list only"));
        // The groups scroll when the panel is too short for them. The note
        // and the buttons stay at the bottom.
        let body = div()
            .id("settings-body")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .pr_2()
            .child(popup)
            .child(insert)
            .child(prefix)
            .child(startup);
        div()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    cx.stop_propagation();
                    cx.emit(SettingsEvent::Close);
                }
            }))
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(body)
                    .children(note)
                    .child(buttons.pt_3().border_t_1().border_color(theme.border)),
            )
    }
}
