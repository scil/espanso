//! Trigger expansion, the work of the espanso daemon, built on espanso's own
//! library crates: `espanso-config` reads the configuration, `espanso-match`
//! finds triggers in the typed keys, and `espanso-render` fills variables.
//! The glue between them follows espanso's worker and `espanso-engine`. See
//! `studio/EXPANDER.md`.
//!
//! The engine has no Windows code, so its tests run anywhere. The caller
//! feeds it key presses ([`Engine::key`]) and does what the returned
//! [`Expansion`] says: erase, insert, move the caret.
//!
//! The loaded configuration lives until the process ends (`Box::leak`),
//! because the matcher state borrows the matcher and the match set borrows
//! the store. A reload loads it again and leaks the old one. espanso numbers
//! matches with a thread-local counter, so one thread must load and use the
//! engine.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use espanso_config::config::{AppProperties, Config, ConfigStore};
use espanso_config::matches::store::{MatchSet, MatchStore};
use espanso_config::matches::{
    Match, MatchCause, MatchEffect, TextInjectMode, UpperCasingStyle,
};
use espanso_match::event::{Event, Key as MatchKey};
use espanso_match::regex::{RegexMatch, RegexMatcher, RegexMatcherOptions, RegexMatcherState};
use espanso_match::rolling::matcher::{
    RollingMatcher, RollingMatcherOptions, RollingMatcherState,
};
use espanso_match::rolling::{RollingMatch, StringMatchOptions};
use espanso_match::{MatchResult, Matcher};
use espanso_render::extension::choice::{Choice, ChoiceSelector, ChoiceSelectorResult};
use espanso_render::extension::clipboard::ClipboardProvider;
use espanso_render::extension::form::{FormProvider, FormProviderResult};
use espanso_render::{
    CasingStyle, Context, Params, RenderOptions, RenderResult, Renderer, Template, Value,
    Variable,
};
use unicode_segmentation::UnicodeSegmentation;

use snip_core::pieces::{self, Pick};
use snip_core::priority::{self, Origins};

use crate::data::Entry;
use crate::form::{Form, PickItem, Preview};

/// Shows a form to the user and waits for the answer. The engine calls it
/// on its own thread, while it renders a snippet.
pub trait Ask {
    /// The typed values by field name, or `None` if the user canceled.
    fn ask(&self, form: Form) -> Option<HashMap<String, String>>;

    /// Shows a list and returns the index of the picked item, or `None` if
    /// the user canceled. `snip` shows it in the popup. The default shows
    /// a choice form with the titles.
    fn pick(&self, items: Vec<PickItem>) -> Option<usize> {
        let titles: Vec<String> = items.iter().map(|i| i.title.clone()).collect();
        let mut answer = self.ask(Form::choice(titles.clone()))?;
        let title = answer.remove(crate::form::CHOICE_FIELD)?;
        titles.iter().position(|t| *t == title)
    }

    /// Shows a list with check boxes and returns the indexes of the checked
    /// items, or `None` if the user canceled. `checked` holds the start
    /// state of each item. The default cancels, for callers without such a
    /// list.
    fn pick_many(&self, items: Vec<PickItem>, checked: Vec<bool>) -> Option<Vec<usize>> {
        let _ = (items, checked);
        None
    }
}

/// Cancels every form, for tests and for callers without a UI.
pub struct NoAsk;

impl Ask for NoAsk {
    fn ask(&self, _: Form) -> Option<HashMap<String, String>> {
        None
    }
}

/// A key press as the engine sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A typed character. Enter is `'\n'` and Tab is `'\t'`.
    Char(char),
    Backspace,
    /// A key or click that moves the caret or ends the typing: an arrow key,
    /// Home, End, Escape, a key with Ctrl, Alt or Win, a mouse click. The
    /// matcher forgets the keys it saw.
    Reset,
}

/// The foreground application, for per-application configurations
/// (`filter_exec`, `filter_title`).
#[derive(Debug, Clone, Default)]
pub struct App {
    /// Full path of the program, as espanso uses it.
    pub exec: Option<String>,
    pub title: Option<String>,
}

/// How to insert the text of an expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The `--short-limit` and `--long-text` settings decide.
    Auto,
    /// The snippet has `force_mode: keys`.
    Keys,
    /// The snippet has `force_mode: clipboard`.
    Paste,
}

/// What the caller must do for one expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    /// Backspaces that erase the trigger.
    pub erase: usize,
    pub text: String,
    /// Left arrow presses after the text, for the cursor hint `$|$`.
    pub lefts: usize,
    pub method: Method,
    /// Match id, for the log.
    pub id: i32,
}

/// The last expansion, for `undo_backspace`: a Backspace right after it
/// erases the text and types the trigger again.
struct Undo {
    trigger: String,
    len: usize,
    enabled: bool,
}

type States = (RollingMatcherState<'static, i32>, Option<RegexMatcherState>);

/// A plain trigger, for the wait on a trigger that starts a longer one.
struct Trigger {
    id: i32,
    /// In lower case when `case_insensitive`.
    text: String,
    case_insensitive: bool,
}

impl Trigger {
    fn key(&self, typed: &str) -> String {
        if self.case_insensitive { typed.to_lowercase() } else { typed.to_string() }
    }

    fn equals(&self, typed: &str) -> bool {
        self.text == self.key(typed)
    }

    fn longer_than(&self, typed: &str) -> bool {
        let typed = self.key(typed);
        self.text.len() > typed.len() && self.text.starts_with(&typed)
    }
}

/// A typed trigger that is also the start of a longer trigger. It waits for
/// the next keys: they either continue a longer trigger or expand this one.
struct Pending {
    chosen: MatchResult<i32>,
    /// Other matches with the same trigger and the same priority as
    /// `chosen`. If there are any, the user picks one from a list.
    ties: Vec<MatchResult<i32>>,
    app: App,
    /// Keys typed after the trigger that still fit a longer trigger.
    extra: String,
}

pub struct Engine {
    configs: &'static dyn ConfigStore,
    store: &'static dyn MatchStore,
    /// All matches of all configurations, in load order.
    all: Vec<&'static Match>,
    by_id: HashMap<i32, &'static Match>,
    rolling: &'static RollingMatcher<i32>,
    regex: &'static RegexMatcher<i32>,
    /// One matcher state per key press, newest last, so that Backspace can
    /// go back one key.
    history: VecDeque<States>,
    limit: usize,
    templates: &'static HashMap<i32, Template>,
    global_vars: &'static HashMap<i32, Variable>,
    contexts: HashMap<i32, Context<'static>>,
    renderer: &'static dyn Renderer,
    forms: &'static Forms,
    undo: Option<Undo>,
    /// Every plain trigger with its match id, in load order.
    triggers: Vec<Trigger>,
    pending: Option<Pending>,
    /// Source file and priority of each match. When matches share a
    /// trigger, the lowest priority wins.
    origins: Origins,
}

impl Engine {
    /// Loads the espanso configuration in `base` (the folder that holds
    /// `config` and `match`). Forms are canceled.
    pub fn load(base: &Path) -> Result<Engine> {
        Engine::load_with(base, Box::new(NoAsk))
    }

    /// Loads the configuration. `ask` shows forms and choice lists.
    pub fn load_with(base: &Path, ask: Box<dyn Ask>) -> Result<Engine> {
        let (configs, store, errors) = espanso_config::load(base)?;
        for set in &errors {
            for e in &set.errors {
                crate::log::line(&format!("config problem in {}: {}", set.file.display(), e.error));
            }
        }
        let configs: &'static dyn ConfigStore = Box::leak(configs);
        let store: &'static dyn MatchStore = Box::leak(store);

        let origins = Origins::load(base, store);
        for p in &origins.problems {
            crate::log::line(&format!("config problem in {}: {}", p.file, p.message));
        }

        let paths: Vec<String> = configs.get_all_match_paths().into_iter().collect();
        let all: Vec<&'static Match> = store.query(&paths).matches;
        let by_id = all.iter().map(|m| (m.id, *m)).collect();

        let default = configs.default();
        let mut rolling = Vec::new();
        let mut regex = Vec::new();
        for m in &all {
            match &m.cause {
                MatchCause::Trigger(cause) => {
                    for trigger in &cause.triggers {
                        rolling.push(RollingMatch::from_string(
                            m.id,
                            trigger,
                            &StringMatchOptions {
                                case_insensitive: cause.propagate_case,
                                left_word: cause.left_word,
                                right_word: cause.right_word,
                            },
                        ));
                    }
                }
                MatchCause::Regex(cause) => regex.push(RegexMatch::new(m.id, &cause.regex)),
                MatchCause::None => {}
            }
        }
        let rolling = Box::leak(Box::new(RollingMatcher::new(
            &rolling,
            RollingMatcherOptions {
                char_word_separators: default.word_separators(),
                key_word_separators: vec![],
            },
        )));
        let regex = Box::leak(Box::new(RegexMatcher::new(
            &regex,
            RegexMatcherOptions { max_buffer_size: default.max_regex_buffer_size() },
        )));

        let templates: HashMap<i32, Template> =
            all.iter().filter_map(|m| Some((m.id, to_template(m)?))).collect();
        let mut global_vars = HashMap::new();
        for config in configs.configs() {
            for var in store.query(config.match_paths()).global_vars {
                global_vars.entry(var.id).or_insert_with(|| to_variable(var.clone()));
            }
        }

        let forms: &'static Forms = Box::leak(Box::new(Forms { ask, preview: Mutex::new(None) }));
        let triggers = all
            .iter()
            .filter_map(|m| match &m.cause {
                MatchCause::Trigger(c) => Some(c.triggers.iter().map(|t| Trigger {
                    id: m.id,
                    text: if c.propagate_case { t.to_lowercase() } else { t.clone() },
                    case_insensitive: c.propagate_case,
                })),
                _ => None,
            })
            .flatten()
            .collect();
        Ok(Engine {
            configs,
            store,
            all,
            by_id,
            rolling,
            regex,
            history: VecDeque::new(),
            limit: default.backspace_limit().max(1),
            templates: Box::leak(Box::new(templates)),
            global_vars: Box::leak(Box::new(global_vars)),
            contexts: HashMap::new(),
            renderer: renderer(base, forms),
            forms,
            undo: None,
            triggers,
            pending: None,
            origins,
        })
    }

    /// Feeds one key press. `app` is called only when a trigger matched.
    pub fn key(&mut self, input: Input, app: impl FnOnce() -> App) -> Option<Expansion> {
        let undo = self.undo.take();
        match input {
            Input::Backspace => {
                if let Some(mut p) = self.pending.take() {
                    self.history.pop_back();
                    // Erasing a key after the trigger keeps it waiting.
                    // Erasing a trigger character ends the wait.
                    if p.extra.pop().is_some() {
                        self.pending = Some(p);
                    }
                    return None;
                }
                if let Some(u) = undo.filter(|u| u.enabled) {
                    self.history.clear();
                    // The user's Backspace already erased one character.
                    return Some(Expansion {
                        erase: u.len.saturating_sub(1),
                        text: u.trigger,
                        lefts: 0,
                        method: Method::Keys,
                        id: -1,
                    });
                }
                self.history.pop_back();
                None
            }
            Input::Reset => {
                // The caret moved, so the trigger may no longer be before
                // it. A waiting trigger does not expand.
                self.history.clear();
                self.pending = None;
                None
            }
            Input::Char(c) => {
                let key = match c {
                    '\n' => MatchKey::Enter,
                    '\t' => MatchKey::Tab,
                    ' ' => MatchKey::Space,
                    _ => MatchKey::Other,
                };
                let results = self.feed(Event::Key { key, chars: Some(c.to_string()) });
                let Some(mut p) = self.pending.take() else {
                    return if results.is_empty() { None } else { self.expand(results, &app()) };
                };
                // The matcher dropped its state when the waiting trigger
                // matched, so the keys after it are compared with the
                // trigger list here.
                drop(results);
                p.extra.push(c);
                let typed = format!("{}{}", typed_trigger(&p.chosen), p.extra);
                let active = self.active_ids(&p.app);
                let left = p.chosen.left_separator.clone();
                let same: Vec<MatchResult<i32>> = self
                    .triggers
                    .iter()
                    .filter(|t| t.equals(&typed) && active.contains(&t.id))
                    .map(|t| MatchResult {
                        id: t.id,
                        trigger: format!("{}{typed}", left.as_deref().unwrap_or("")),
                        left_separator: left.clone(),
                        right_separator: None,
                        vars: HashMap::new(),
                    })
                    .collect();
                if let Some((chosen, ties)) = self.rank(same) {
                    if self.extends(&typed, &p.app) {
                        self.pending = Some(Pending { chosen, ties, app: p.app, extra: String::new() });
                        return None;
                    }
                    return self.finish(chosen, ties, &p.app, "");
                }
                if self.extends(&typed, &p.app) {
                    self.pending = Some(p);
                    return None;
                }
                self.finish(p.chosen, p.ties, &p.app, &p.extra)
            }
        }
    }

    /// Tells the matcher that text went in by other means (the popup). The
    /// next trigger can follow it, as after a word separator.
    pub fn inserted(&mut self) {
        self.pending = None;
        self.history.clear();
        self.undo = None;
        self.feed(Event::VirtualSeparator);
    }

    fn feed(&mut self, event: Event) -> Vec<MatchResult<i32>> {
        let prev = self.history.back();
        let (rolling_state, mut results) = self.rolling.process(prev.map(|s| &s.0), event.clone());
        let (regex_state, regex_results) =
            self.regex.process(prev.and_then(|s| s.1.as_ref()), event);
        results.extend(regex_results);
        self.history.push_back((rolling_state, Some(regex_state)));
        while self.history.len() > self.limit {
            self.history.pop_front();
        }
        results
    }

    fn expand(&mut self, results: Vec<MatchResult<i32>>, app: &App) -> Option<Expansion> {
        let props = AppProperties {
            title: app.title.as_deref(),
            class: None,
            exec: app.exec.as_deref(),
        };
        let config = self.configs.active(&props);
        let set = self.store.query(config.match_paths());
        let active: HashSet<i32> = set.matches.iter().map(|m| m.id).collect();
        let valid: Vec<MatchResult<i32>> = results.into_iter().filter(|r| active.contains(&r.id)).collect();
        let count = valid.len();
        let (chosen, ties) = self.rank(valid)?;
        if count > 1 {
            crate::log::line(&format!(
                "expand: {count} matches for the trigger, best match {} with priority {}, {} ties",
                chosen.id,
                self.origins.priority(chosen.id),
                ties.len()
            ));
        }
        if self.extends(typed_trigger(&chosen), app) {
            crate::log::line(&format!("expand: match {} waits, a longer trigger starts the same", chosen.id));
            self.pending = Some(Pending { chosen, ties, app: app.clone(), extra: String::new() });
            return None;
        }
        self.finish(chosen, ties, app, "")
    }

    /// Splits valid results into the strongest one and the others with the
    /// same priority, both in load order. Weaker results are dropped.
    fn rank(&self, results: Vec<MatchResult<i32>>) -> Option<(MatchResult<i32>, Vec<MatchResult<i32>>)> {
        let best = priority::best(results.iter().map(|r| r.id), &self.origins)?;
        let level = self.origins.priority(best);
        let mut seen = HashSet::new();
        let mut equal = results
            .into_iter()
            .filter(|r| self.origins.priority(r.id) == level && seen.insert(r.id));
        let chosen = equal.next()?;
        Some((chosen, equal.collect()))
    }

    /// With ties, asks the user to pick one match from a list, as espanso
    /// does. `None` if the user canceled.
    fn pick(&self, chosen: MatchResult<i32>, ties: Vec<MatchResult<i32>>) -> Option<MatchResult<i32>> {
        if ties.is_empty() {
            return Some(chosen);
        }
        let mut all = vec![chosen];
        all.extend(ties);
        let labels = choice_labels(all.iter().map(|r| self.by_id.get(&r.id).copied()));
        let items = all
            .iter()
            .zip(labels)
            .map(|(r, title)| PickItem {
                title,
                detail: match self.by_id.get(&r.id).map(|m| &m.effect) {
                    Some(MatchEffect::Text(t)) => one_line(&t.replace, 120),
                    _ => String::new(),
                },
                badge: self.origins.by_id.get(&r.id).map_or_else(String::new, |o| {
                    let name = o.file.rsplit('/').next().unwrap_or(&o.file);
                    match o.index {
                        Some(i) => format!("{name}:{}", i + 1),
                        None => name.to_string(),
                    }
                }),
            })
            .collect();
        let index = self.forms.ask.pick(items)?;
        (index < all.len()).then(|| all.swap_remove(index))
    }

    /// Whether a trigger waits for the next key. If no key comes in time,
    /// the caller calls [`Engine::flush`].
    pub fn waiting(&self) -> bool {
        self.pending.is_some()
    }

    /// Expands a waiting trigger, with the keys typed after it.
    pub fn flush(&mut self) -> Option<Expansion> {
        let p = self.pending.take()?;
        self.finish(p.chosen, p.ties, &p.app, &p.extra)
    }

    /// Whether `typed` is the start of a longer trigger of the active
    /// configuration. Regex triggers do not count.
    fn extends(&self, typed: &str, app: &App) -> bool {
        if !self.triggers.iter().any(|t| t.longer_than(typed)) {
            return false;
        }
        let active = self.active_ids(app);
        self.triggers.iter().any(|t| t.longer_than(typed) && active.contains(&t.id))
    }

    /// The ids of the matches of the configuration active in `app`.
    fn active_ids(&self, app: &App) -> HashSet<i32> {
        let props = AppProperties { title: app.title.as_deref(), class: None, exec: app.exec.as_deref() };
        let config = self.configs.active(&props);
        self.store.query(config.match_paths()).matches.iter().map(|m| m.id).collect()
    }

    /// Expands a chosen match. `extra` holds keys typed after the trigger:
    /// they are erased with it and typed again after the text.
    fn finish(
        &mut self,
        chosen: MatchResult<i32>,
        ties: Vec<MatchResult<i32>>,
        app: &App,
        extra: &str,
    ) -> Option<Expansion> {
        let props = AppProperties { title: app.title.as_deref(), class: None, exec: app.exec.as_deref() };
        let config = self.configs.active(&props);
        let set = self.store.query(config.match_paths());
        self.history.clear();
        let Some(chosen) = self.pick(chosen, ties) else {
            crate::log::line("expand: the list of matches was canceled");
            return None;
        };
        let m = *self.by_id.get(&chosen.id)?;
        let typed = typed_trigger(&chosen).to_string();
        let body = match self.render(m, Some(&chosen.trigger), chosen.vars, config.as_ref(), &set) {
            Ok(body) => body,
            Err(e) => {
                crate::log::line(&format!("expand: match {} not rendered: {e}", m.id));
                return None;
            }
        };
        let left = chosen.left_separator.as_deref().map_or(0, |s| s.chars().count());
        let erase = chosen.trigger.chars().count().saturating_sub(left) + extra.chars().count();
        let mut body = body;
        if let Some(right) = &chosen.right_separator {
            body.push_str(right);
        }
        body.push_str(extra);
        let (text, lefts) = cursor_hint(&body);
        self.undo = (lefts == 0).then(|| Undo {
            trigger: format!("{typed}{extra}"),
            len: text.chars().count(),
            enabled: config.undo_backspace(),
        });
        self.feed(Event::VirtualSeparator);
        Some(Expansion { erase, text, lefts, method: method_of(m), id: m.id })
    }

    /// Renders a match for the popup: no trigger, so no case change. The
    /// popup erases its own trigger and keyword.
    pub fn render_by_id(&mut self, id: i32, app: &App) -> Result<Expansion> {
        let m = *self.by_id.get(&id).ok_or_else(|| anyhow!("no match with id {id}"))?;
        let props = AppProperties {
            title: app.title.as_deref(),
            class: None,
            exec: app.exec.as_deref(),
        };
        // The popup lists the matches of every configuration. A match that is
        // not active here renders with the first configuration that holds
        // it, so it sees the global variables of its own files.
        let holds = |c: &std::sync::Arc<dyn Config>| self.store.query(c.match_paths()).matches.iter().any(|x| x.id == id);
        let active = self.configs.active(&props);
        let config = if holds(&active) {
            active
        } else {
            self.configs.configs().into_iter().find(holds).unwrap_or(active)
        };
        let set = self.store.query(config.match_paths());
        let body = self.render(m, None, HashMap::new(), config.as_ref(), &set)?;
        let (text, lefts) = cursor_hint(&body);
        Ok(Expansion { erase: 0, text, lefts, method: method_of(m), id })
    }

    /// Renders the first match with this label, as the popup would. The
    /// editor's Test button uses it.
    pub fn render_label(&mut self, label: &str) -> Result<String> {
        let id = self
            .all
            .iter()
            .find(|m| m.label.as_deref() == Some(label))
            .map(|m| m.id)
            .ok_or_else(|| anyhow!("the test snippet did not load"))?;
        let app = App { exec: None, title: None };
        Ok(self.render_by_id(id, &app)?.text)
    }

    fn render(
        &mut self,
        m: &Match,
        trigger: Option<&str>,
        trigger_vars: HashMap<String, String>,
        config: &dyn Config,
        set: &MatchSet<'static>,
    ) -> Result<String> {
        let template =
            self.templates.get(&m.id).ok_or_else(|| anyhow!("only text snippets can expand"))?;
        // The user checks paragraphs first. Only the checked ones render, so
        // a variable that only an unchecked paragraph uses does not run.
        let picked = match self.origins.picks.get(&m.id) {
            Some(pick) => Some(self.pick_pieces(&template.body, *pick)?),
            None => None,
        };
        let (templates, global_vars) = (self.templates, self.global_vars);
        let context = self
            .contexts
            .entry(config.id())
            .or_insert_with(|| context_for(set, templates, global_vars));
        let options = RenderOptions { casing_style: casing_of(m, trigger) };
        let mut augmented;
        let template = if trigger_vars.is_empty() && picked.is_none() {
            template
        } else {
            augmented = template.clone();
            if let Some(body) = picked {
                augmented.body = body;
                keep_used_vars(&mut augmented);
            }
            for (name, value) in trigger_vars {
                let mut params = Params::new();
                params.insert("echo".to_string(), Value::String(value));
                augmented.vars.insert(
                    0,
                    Variable {
                        name,
                        var_type: "echo".to_string(),
                        params,
                        inject_vars: false,
                        ..Default::default()
                    },
                );
            }
            &augmented
        };
        let form_vars: Vec<String> =
            template.vars.iter().filter(|v| v.var_type == "form").map(|v| v.name.clone()).collect();
        *self.forms.preview.lock().unwrap() =
            Some(Preview { body: template.body.clone(), vars: form_vars });
        match self.renderer.render(template, context, &options) {
            RenderResult::Success(body) => Ok(body),
            RenderResult::Aborted => Err(anyhow!("aborted")),
            RenderResult::Error(e) => Err(e),
        }
    }

    /// Asks which paragraphs of `body` to keep and returns the new body. A
    /// body with one paragraph needs no list.
    fn pick_pieces(&self, body: &str, pick: Pick) -> Result<String> {
        let pieces = pieces::split(body);
        if pieces.parts.len() < 2 {
            return Ok(body.to_string());
        }
        let items = pieces
            .parts
            .iter()
            .map(|p| {
                let mut lines = p.lines();
                let first = lines.next().unwrap_or_default();
                let more = lines.count();
                PickItem {
                    title: one_line(first, 120),
                    detail: p.clone(),
                    badge: if more > 0 { format!("+{more} lines") } else { String::new() },
                }
            })
            .collect();
        let checked = vec![pick.all_checked; pieces.parts.len()];
        match self.forms.ask.pick_many(items, checked) {
            Some(chosen) if !chosen.is_empty() => Ok(pieces::join(&pieces, &chosen)),
            _ => Err(anyhow!("aborted")),
        }
    }

    /// The snippets for the popup list, in load order.
    pub fn entries(&self) -> Vec<Entry> {
        self.all
            .iter()
            .filter_map(|m| {
                let trigger = match &m.cause {
                    MatchCause::Trigger(c) => c.triggers.join("  "),
                    MatchCause::Regex(c) => c.regex.clone(),
                    MatchCause::None => return None,
                };
                let (text, insertable) = match &m.effect {
                    MatchEffect::Text(t) => (t.replace.clone(), true),
                    MatchEffect::Image(i) => (i.path.clone(), false),
                    MatchEffect::None => return None,
                };
                Some(Entry::new(
                    m.id,
                    trigger,
                    m.label.clone().unwrap_or_default(),
                    &text,
                    &m.search_terms,
                    insertable,
                ))
            })
            .collect()
    }
}

/// One unique line per match for the list that picks a match: its label,
/// or the first line of its text. A repeated line gets ` (2)`, ` (3)`.
fn choice_labels<'a>(matches: impl Iterator<Item = Option<&'a Match>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in matches {
        let base = m
            .and_then(|m| {
                m.label.clone().filter(|l| !l.trim().is_empty()).or_else(|| match &m.effect {
                    MatchEffect::Text(t) => {
                        t.replace.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
                    }
                    MatchEffect::Image(i) => Some(i.path.clone()),
                    MatchEffect::None => None,
                })
            })
            .unwrap_or_else(|| "(empty)".to_string());
        let base: String = if base.chars().count() > 80 {
            base.chars().take(79).chain(std::iter::once('\u{2026}')).collect()
        } else {
            base
        };
        let mut label = base.clone();
        let mut n = 1;
        while out.contains(&label) {
            n += 1;
            label = format!("{base} ({n})");
        }
        out.push(label);
    }
    out
}

/// Removes the local variables that the body does not use, directly or
/// through another variable. espanso runs every local variable, so without
/// this a form or a shell command of an unchecked paragraph would run.
fn keep_used_vars(template: &mut Template) {
    fn param_texts<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::String(s) => out.push(s),
            Value::Array(a) => a.iter().for_each(|v| param_texts(v, out)),
            Value::Object(o) => o.values().for_each(|v| param_texts(v, out)),
            _ => {}
        }
    }
    let mut used: HashSet<String> = pieces::var_names(&template.body).into_iter().map(str::to_string).collect();
    // A variable that a used variable needs is used too. Repeat until no
    // name is added.
    loop {
        let before = used.len();
        let mut needs: Vec<String> = Vec::new();
        for var in template.vars.iter().filter(|v| used.contains(&v.name)) {
            needs.extend(var.depends_on.iter().cloned());
            if var.inject_vars {
                let mut texts = Vec::new();
                var.params.values().for_each(|v| param_texts(v, &mut texts));
                needs.extend(texts.iter().flat_map(|t| pieces::var_names(t)).map(str::to_string));
            }
        }
        used.extend(needs);
        if used.len() == before {
            break;
        }
    }
    template.vars.retain(|v| used.contains(&v.name));
}

/// Text on one line: line breaks become spaces, runs of spaces shrink, and
/// text longer than `max` characters ends with `…`.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        flat.chars().take(max - 1).chain(std::iter::once('\u{2026}')).collect()
    } else {
        flat
    }
}

/// The typed trigger without the word separator before it.
fn typed_trigger(r: &MatchResult<i32>) -> &str {
    match r.left_separator.as_deref() {
        Some(s) => r.trigger.strip_prefix(s).unwrap_or(&r.trigger),
        None => &r.trigger,
    }
}

fn method_of(m: &Match) -> Method {
    match &m.effect {
        MatchEffect::Text(t) => match t.force_mode {
            Some(TextInjectMode::Keys) => Method::Keys,
            Some(TextInjectMode::Clipboard) => Method::Paste,
            None => Method::Auto,
        },
        _ => Method::Auto,
    }
}

/// Removes the first `$|$` and counts the Left presses that put the caret
/// there, in graphemes, as espanso does.
pub fn cursor_hint(body: &str) -> (String, usize) {
    match body.find("$|$") {
        Some(index) => {
            let before = body[..index].graphemes(true).count();
            let total = body.graphemes(true).count();
            (body.replacen("$|$", "", 1), total - before - 3)
        }
        None => (body.to_string(), 0),
    }
}

fn casing_of(m: &Match, trigger: Option<&str>) -> CasingStyle {
    match (&m.cause, trigger) {
        (MatchCause::Trigger(cause), Some(trigger)) if cause.propagate_case => {
            casing_style(trigger, &cause.uppercase_style)
        }
        _ => CasingStyle::None,
    }
}

/// The case of the typed trigger, copied from espanso's worker
/// (`calculate_casing_style`).
fn casing_style(trigger: &str, style: &UpperCasingStyle) -> CasingStyle {
    let mut letters = trigger.chars().filter(|c| c.is_alphabetic());
    let (Some(first), second) = (letters.next(), letters.next()) else {
        return CasingStyle::None;
    };
    if !first.is_uppercase() {
        return CasingStyle::None;
    }
    match (second, style) {
        (Some(s), _) if s.is_uppercase() => CasingStyle::Uppercase,
        (Some(_), UpperCasingStyle::CapitalizeWords) => CasingStyle::CapitalizeWords,
        (Some(_), _) => CasingStyle::Capitalize,
        (None, UpperCasingStyle::Capitalize) => CasingStyle::Capitalize,
        (None, UpperCasingStyle::CapitalizeWords) => CasingStyle::CapitalizeWords,
        (None, UpperCasingStyle::Uppercase) => CasingStyle::Uppercase,
    }
}

fn context_for(
    set: &MatchSet<'static>,
    templates: &'static HashMap<i32, Template>,
    global_vars: &'static HashMap<i32, Variable>,
) -> Context<'static> {
    Context {
        templates: set.matches.iter().filter_map(|m| templates.get(&m.id)).collect(),
        global_vars: set.global_vars.iter().filter_map(|v| global_vars.get(&v.id)).collect(),
    }
}

fn to_template(m: &Match) -> Option<Template> {
    let MatchEffect::Text(effect) = &m.effect else { return None };
    let ids = match &m.cause {
        MatchCause::Trigger(cause) => cause.triggers.clone(),
        _ => Vec::new(),
    };
    Some(Template {
        ids,
        body: effect.replace.clone(),
        vars: effect.vars.iter().cloned().map(to_variable).collect(),
    })
}

fn to_variable(var: espanso_config::matches::Variable) -> Variable {
    Variable {
        name: var.name,
        var_type: var.var_type,
        params: to_params(var.params),
        inject_vars: var.inject_vars,
        depends_on: var.depends_on,
    }
}

fn to_params(params: espanso_config::matches::Params) -> Params {
    params.into_iter().map(|(k, v)| (k, to_value(v))).collect()
}

fn to_value(value: espanso_config::matches::Value) -> Value {
    use espanso_config::matches::{Number, Value as V};
    match value {
        V::Null => Value::Null,
        V::Bool(b) => Value::Bool(b),
        V::Number(Number::Integer(i)) => Value::Number(espanso_render::Number::Integer(i)),
        V::Number(Number::Float(f)) => Value::Number(espanso_render::Number::Float(f.into_inner())),
        V::String(s) => Value::String(s),
        V::Array(a) => Value::Array(a.into_iter().map(to_value).collect()),
        V::Object(o) => Value::Object(to_params(o)),
    }
}

/// The clipboard text for `{{clipboard}}` variables.
struct ClipboardText;

impl ClipboardProvider for ClipboardText {
    fn get_text(&self) -> Option<String> {
        crate::focus::clipboard_text()
    }
}

/// The form and choice provider of the renderer. It passes each form to
/// [`Ask`].
struct Forms {
    ask: Box<dyn Ask>,
    /// The text of the snippet being rendered, for the form preview.
    preview: Mutex<Option<Preview>>,
}

impl FormProvider for Forms {
    fn show(&self, layout: &str, fields: &Params, _: &Params) -> FormProviderResult {
        let mut form = Form::from_espanso(layout, fields);
        form.preview = self.preview.lock().unwrap().clone();
        match self.ask.ask(form) {
            Some(values) => FormProviderResult::Success(values),
            None => FormProviderResult::Aborted,
        }
    }
}

impl ChoiceSelector for Forms {
    fn show(&self, choices: &[Choice]) -> ChoiceSelectorResult {
        let items = choices.iter().map(|c| PickItem::new(c.label)).collect();
        let picked = self.ask.pick(items).and_then(|i| choices.get(i));
        match picked {
            Some(c) => ChoiceSelectorResult::Success(c.id.to_string()),
            None => ChoiceSelectorResult::Aborted,
        }
    }
}

/// The renderer with espanso's extensions. `base` is espanso's config
/// folder: `%CONFIG%` in scripts and the working folder of shell commands.
fn renderer(base: &Path, forms: &'static Forms) -> &'static dyn Renderer {
    use espanso_render::extension::{
        choice::ChoiceExtension, clipboard::ClipboardExtension, date::DateExtension,
        date::DefaultLocaleProvider, echo::EchoExtension, form::FormExtension,
        random::RandomExtension, script::ScriptExtension, shell::ShellExtension,
    };
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| base.to_path_buf());
    let packages = base.join("match").join("packages");
    let locale: &'static DefaultLocaleProvider = Box::leak(Box::new(DefaultLocaleProvider::new()));
    let extensions: Vec<&'static dyn espanso_render::Extension> = vec![
        Box::leak(Box::new(ClipboardExtension::new(&ClipboardText))),
        Box::leak(Box::new(DateExtension::new(locale))),
        Box::leak(Box::new(EchoExtension::new())),
        Box::leak(Box::new(RandomExtension::new())),
        Box::leak(Box::new(ScriptExtension::new(base, &home, &packages))),
        Box::leak(Box::new(ShellExtension::new(base))),
        Box::leak(Box::new(FormExtension::new(forms))),
        Box::leak(Box::new(ChoiceExtension::new(forms))),
    ];
    Box::leak(Box::new(espanso_render::create(extensions)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a configuration folder and loads it. `files` are paths
    /// relative to the base with their text.
    fn engine(files: &[(&str, &str)]) -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::create_dir_all(dir.path().join("match")).unwrap();
        std::fs::write(dir.path().join("config/default.yml"), "").unwrap();
        for (path, text) in files {
            std::fs::write(dir.path().join(path), text).unwrap();
        }
        (Engine::load(dir.path()).unwrap(), dir)
    }

    fn typed(e: &mut Engine, s: &str) -> Option<Expansion> {
        typed_in(e, s, App::default())
    }

    fn typed_in(e: &mut Engine, s: &str, app: App) -> Option<Expansion> {
        let mut last = None;
        for c in s.chars() {
            last = e.key(Input::Char(c), || app.clone());
        }
        last
    }

    fn text(x: Option<Expansion>) -> Option<(usize, String, usize)> {
        x.map(|x| (x.erase, x.text, x.lefts))
    }

    const BASE: &str = r#"
matches:
  - trigger: ":hi"
    replace: "hello"
  - trigger: "alh"
    replace: "although"
    propagate_case: true
  - trigger: "hw"
    replace: "hello world"
    left_word: true
  - trigger: ":cur"
    replace: "a$|$bc"
  - regex: "=(?P<n>\\d+)x"
    replace: "{{n}}!"
  - trigger: ":year"
    replace: "{{y}}"
    vars:
      - name: y
        type: date
        params:
          format: "%Y"
  - trigger: ":form"
    form: "Hi [[name]]"
"#;

    #[test]
    fn plain_trigger_expands() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        assert_eq!(text(typed(&mut e, "say :hi")), Some((3, "hello".into(), 0)));
    }

    #[test]
    fn backspace_inside_a_trigger() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        assert_eq!(typed(&mut e, ":hx"), None);
        assert_eq!(e.key(Input::Backspace, App::default), None);
        assert_eq!(text(typed(&mut e, "i")), Some((3, "hello".into(), 0)));
    }

    #[test]
    fn reset_forgets_typed_keys() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        typed(&mut e, ":h");
        e.key(Input::Reset, App::default);
        assert_eq!(typed(&mut e, "i"), None);
    }

    #[test]
    fn left_word_needs_a_separator() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        assert_eq!(typed(&mut e, "ahw"), None);
        // The separator stays: only the trigger is erased.
        assert_eq!(text(typed(&mut e, " hw")), Some((2, "hello world".into(), 0)));
    }

    #[test]
    fn case_propagates() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        assert_eq!(typed(&mut e, " Alh").map(|x| x.text), Some("Although".into()));
        assert_eq!(typed(&mut e, " ALH").map(|x| x.text), Some("ALTHOUGH".into()));
        assert_eq!(typed(&mut e, " alh").map(|x| x.text), Some("although".into()));
    }

    #[test]
    fn cursor_hint_moves_left() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        assert_eq!(text(typed(&mut e, ":cur")), Some((4, "abc".into(), 2)));
        assert_eq!(cursor_hint("🦊$|$é"), ("🦊é".into(), 1));
    }

    #[test]
    fn regex_captures_become_variables() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        assert_eq!(text(typed(&mut e, "=12x")), Some((4, "12!".into(), 0)));
    }

    #[test]
    fn date_variable_renders() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        let x = typed(&mut e, ":year").unwrap();
        assert!(x.text.len() == 4 && x.text.chars().all(|c| c.is_ascii_digit()), "{}", x.text);
    }

    /// Answers every form with fixed values and records the forms.
    struct Answer(HashMap<String, String>, std::sync::Arc<Mutex<Vec<Form>>>);

    impl Ask for Answer {
        fn ask(&self, form: Form) -> Option<HashMap<String, String>> {
            self.1.lock().unwrap().push(form);
            Some(self.0.clone())
        }
    }

    #[test]
    fn canceled_form_inserts_nothing() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        assert_eq!(typed(&mut e, ":form"), None);
        // The next trigger still works.
        assert_eq!(text(typed(&mut e, " :hi")), Some((3, "hello".into(), 0)));
    }

    #[test]
    fn form_and_choice_use_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::create_dir_all(dir.path().join("match")).unwrap();
        std::fs::write(dir.path().join("config/default.yml"), "").unwrap();
        let choice = r##"
  - trigger: ":pick"
    replace: "{{c}}"
    vars:
      - name: c
        type: choice
        params:
          values:
            - label: "Red"
              id: "#f00"
            - "blue"
"##;
        std::fs::write(dir.path().join("match/base.yml"), format!("{BASE}{choice}")).unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let answers = HashMap::from([
            ("name".to_string(), "Bob".to_string()),
            ("choice".to_string(), "Red".to_string()),
        ]);
        let mut e = Engine::load_with(dir.path(), Box::new(Answer(answers, seen.clone()))).unwrap();
        assert_eq!(text(typed(&mut e, ":form")), Some((5, "Hi Bob".into(), 0)));
        assert_eq!(text(typed(&mut e, " :pick")), Some((5, "#f00".into(), 0)));
        let seen = seen.lock().unwrap();
        let preview = seen[0].preview.as_ref().unwrap();
        assert_eq!(seen[0].fill(&HashMap::from([("name".into(), "Al".into())])), "Hi Al");
        assert_eq!(preview.vars, ["form1"]);
        assert!(seen[1].is_choice());
    }

    #[test]
    fn backspace_after_expansion_undoes_it() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        typed(&mut e, ":hi");
        assert_eq!(text(e.key(Input::Backspace, App::default)), Some((4, ":hi".into(), 0)));
        // Only right after the expansion.
        typed(&mut e, ":hi");
        typed(&mut e, "x");
        assert_eq!(e.key(Input::Backspace, App::default), None);
    }

    #[test]
    fn per_application_matches() {
        let (mut e, _d) = engine(&[
            ("match/base.yml", BASE),
            ("match/_np.yml", "matches:\n  - trigger: \":np\"\n    replace: \"notepad\"\n"),
            (
                "config/np.yml",
                "filter_exec: \"notepad.exe\"\nextra_includes:\n  - \"../match/_np.yml\"\n",
            ),
        ]);
        let other = App { exec: Some(r"C:\Tools\other.exe".into()), title: None };
        let notepad = App { exec: Some(r"C:\Windows\notepad.exe".into()), title: None };
        assert_eq!(typed_in(&mut e, ":np", other.clone()), None);
        assert_eq!(typed_in(&mut e, " :np", notepad.clone()).map(|x| x.text), Some("notepad".into()));
        // The default matches stay valid in the application.
        assert_eq!(typed_in(&mut e, " :hi", notepad).map(|x| x.text), Some("hello".into()));
    }

    #[test]
    fn popup_renders_a_match_of_another_application() {
        let np = "global_vars:\n  - name: who\n    type: echo\n    params:\n      echo: \"pad\"\nmatches:\n  - trigger: \":np\"\n    replace: \"note{{who}}\"\n";
        let (mut e, _d) = engine(&[
            ("match/base.yml", BASE),
            ("match/_np.yml", np),
            ("config/np.yml", "filter_exec: \"notepad.exe\"\nextra_includes:\n  - \"../match/_np.yml\"\n"),
        ]);
        let other = App { exec: Some(r"C:\Tools\other.exe".into()), title: None };
        let entry = e.entries().into_iter().find(|x| x.trigger == ":np").unwrap();
        // The match is not active in `other`, so its own configuration
        // renders it, with the global variable of its file.
        assert_eq!(e.render_by_id(entry.id, &other).unwrap().text, "notepad");
    }

    const PREFIX: &str = r#"
matches:
  - trigger: ";su"
    replace: "update "
  - trigger: ";sun"
    replace: "uninstall "
  - trigger: ";suxyz"
    replace: "long"
"#;

    #[test]
    fn longer_trigger_wins_over_its_prefix() {
        let (mut e, _d) = engine(&[("match/base.yml", PREFIX)]);
        assert_eq!(typed(&mut e, ";su"), None);
        assert!(e.waiting());
        assert_eq!(text(typed(&mut e, "n")), Some((4, "uninstall ".into(), 0)));
        assert!(!e.waiting());
    }

    #[test]
    fn other_key_expands_the_prefix_and_stays() {
        let (mut e, _d) = engine(&[("match/base.yml", PREFIX)]);
        assert_eq!(typed(&mut e, ";su"), None);
        // `a` fits no longer trigger: erase the trigger and `a`, type both.
        assert_eq!(text(typed(&mut e, "a")), Some((4, "update a".into(), 0)));
        // `x` and `y` fit `;suxyz`, then `q` does not.
        assert_eq!(typed(&mut e, " ;suxy"), None);
        assert_eq!(text(typed(&mut e, "q")), Some((6, "update xyq".into(), 0)));
    }

    #[test]
    fn pause_expands_the_prefix() {
        let (mut e, _d) = engine(&[("match/base.yml", PREFIX)]);
        typed(&mut e, ";sux");
        assert_eq!(text(e.flush()), Some((4, "update x".into(), 0)));
        assert_eq!(e.flush(), None);
    }

    #[test]
    fn backspace_and_reset_while_waiting() {
        let (mut e, _d) = engine(&[("match/base.yml", PREFIX)]);
        typed(&mut e, ";sux");
        // Erasing the extra key keeps the wait.
        e.key(Input::Backspace, App::default);
        assert!(e.waiting());
        assert_eq!(text(typed(&mut e, "n")), Some((4, "uninstall ".into(), 0)));
        typed(&mut e, " ;su");
        e.key(Input::Reset, App::default);
        assert!(!e.waiting());
        assert_eq!(e.flush(), None);
    }

    #[test]
    fn popup_renders_by_id() {
        let (mut e, _d) = engine(&[("match/base.yml", BASE)]);
        let entry = e.entries().into_iter().find(|x| x.trigger == ":cur").unwrap();
        let x = e.render_by_id(entry.id, &App::default()).unwrap();
        assert_eq!((x.text.as_str(), x.lefts), ("abc", 2));
    }

    /// The lists that [`Check`] saw, with their start states.
    type Seen = std::sync::Arc<Mutex<Vec<(Vec<PickItem>, Vec<bool>)>>>;

    /// Checks fixed paragraphs and records the lists.
    struct Check(Vec<usize>, Seen);

    impl Ask for Check {
        fn ask(&self, _: Form) -> Option<HashMap<String, String>> {
            None
        }
        fn pick_many(&self, items: Vec<PickItem>, checked: Vec<bool>) -> Option<Vec<usize>> {
            self.1.lock().unwrap().push((items, checked));
            Some(self.0.clone())
        }
    }

    #[test]
    fn paragraph_pick_inserts_the_checked_paragraphs() {
        let base = r#"
matches:
  - trigger: ":rep"
    snip: { pick: many, checked: none }
    replace: |
      Hello,

      Line one
      line two

      Today is {{year}}.

      Bye {{who}}
    vars:
      - name: year
        type: date
        params: { format: "%Y" }
      - name: who
        type: shell
        params: { cmd: "exit 1" }
  - trigger: ":one"
    snip: { pick: many }
    replace: "only one"
"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::create_dir_all(dir.path().join("match")).unwrap();
        std::fs::write(dir.path().join("config/default.yml"), "").unwrap();
        std::fs::write(dir.path().join("match/base.yml"), base).unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::load_with(dir.path(), Box::new(Check(vec![2, 0], seen.clone()))).unwrap();
        // The variable of the unchecked last paragraph does not run: its
        // failing shell command would abort the render.
        let x = typed(&mut e, ":rep").unwrap();
        let year = x.text.lines().nth(2).unwrap().to_string();
        assert!(year.starts_with("Today is 20"), "{}", x.text);
        assert_eq!(x.text, format!("Hello,\n\n{year}\n"));
        assert_eq!(x.erase, 4);
        {
            let seen = seen.lock().unwrap();
            let (items, checked) = &seen[0];
            let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
            assert_eq!(titles, ["Hello,", "Line one", "Today is {{year}}.", "Bye {{who}}"]);
            assert_eq!(items[1].badge, "+1 lines");
            assert_eq!(checked, &[false; 4]);
        }
        // One paragraph needs no list.
        assert_eq!(typed(&mut e, " :one").map(|x| x.text), Some("only one".into()));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn paragraph_pick_keeps_variables_that_a_used_variable_needs() {
        let base = r#"
matches:
  - trigger: ":rep"
    snip: { pick: many }
    replace: "x {{b}}\n\ny {{c}}"
    vars:
      - { name: a, type: echo, params: { echo: "A" } }
      - { name: b, type: echo, params: { echo: "{{a}}!" } }
      - { name: c, type: shell, params: { cmd: "exit 1" } }
"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::create_dir_all(dir.path().join("match")).unwrap();
        std::fs::write(dir.path().join("config/default.yml"), "").unwrap();
        std::fs::write(dir.path().join("match/base.yml"), base).unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::load_with(dir.path(), Box::new(Check(vec![0], seen))).unwrap();
        assert_eq!(typed(&mut e, ":rep").map(|x| x.text), Some("x A!".into()));
    }

    #[test]
    fn a_canceled_paragraph_pick_inserts_nothing() {
        let base = "matches:\n  - trigger: \":rep\"\n    snip: { pick: many }\n    replace: \"a\\n\\nb\"\n";
        let (mut e, _d) = engine(&[("match/base.yml", base)]);
        assert_eq!(typed(&mut e, ":rep"), None);
    }

    #[test]
    fn lower_priority_wins_a_shared_trigger() {
        let base = "matches:\n  - trigger: \":x\"\n    replace: \"base\"\n  - trigger: \":y\"\n    replace: \"y1\"\n  - trigger: \":y\"\n    replace: \"y2\"\n";
        let mine = "snip:\n  priority: personal\nmatches:\n  - trigger: \":x\"\n    replace: \"mine\"\n";
        let (mut e, _d) = engine(&[("match/base.yml", base), ("match/mine.yml", mine)]);
        assert_eq!(typed(&mut e, ":x").map(|x| x.text), Some("mine".into()));
        // Equal priorities open a list. The test default cancels it.
        assert_eq!(typed(&mut e, " :y"), None);
        // The next trigger still works.
        assert_eq!(typed(&mut e, " :x").map(|x| x.text), Some("mine".into()));
    }

    #[test]
    fn a_tie_asks_which_match() {
        let base = "matches:\n  - trigger: \":y\"\n    replace: \"one\"\n    label: \"First\"\n  - trigger: \":y\"\n    replace: \"\\n  two\\nmore\"\n  - trigger: \":y\"\n    replace: \"two\"\n";
        // A weaker match in a file with a weaker priority.
        let weak = "snip:\n  priority: package\nmatches:\n  - trigger: \":y\"\n    replace: \"weak\"\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::create_dir_all(dir.path().join("match")).unwrap();
        std::fs::write(dir.path().join("config/default.yml"), "").unwrap();
        std::fs::write(dir.path().join("match/base.yml"), base).unwrap();
        std::fs::write(dir.path().join("match/weak.yml"), weak).unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let answers = HashMap::from([("choice".to_string(), "two (2)".to_string())]);
        let mut e = Engine::load_with(dir.path(), Box::new(Answer(answers, seen.clone()))).unwrap();
        assert_eq!(text(typed(&mut e, ":y")), Some((2, "two".into(), 0)));
        let seen = seen.lock().unwrap();
        assert!(seen[0].is_choice());
        // The weaker match is not in the list.
        let crate::form::Kind::Choice { values } = &seen[0].fields[0].kind else { panic!() };
        assert_eq!(values, &["First", "two", "two (2)"]);
    }

    #[test]
    fn a_match_priority_has_no_effect() {
        // Only the file priority counts, so `mine` wins.
        let base = "matches:\n  - trigger: \":x\"\n    replace: \"base\"\n    snip:\n      priority: -1\n";
        let mine = "snip:\n  priority: personal\nmatches:\n  - trigger: \":x\"\n    replace: \"mine\"\n";
        let (mut e, _d) = engine(&[("match/base.yml", base), ("match/mine.yml", mine)]);
        assert_eq!(typed(&mut e, ":x").map(|x| x.text), Some("mine".into()));
    }

    #[test]
    fn priority_applies_after_a_wait() {
        let base = "matches:\n  - trigger: \";s\"\n    replace: \"short\"\n  - trigger: \";sa\"\n    replace: \"base\"\n";
        let mine = "snip:\n  priority: personal\nmatches:\n  - trigger: \";sa\"\n    replace: \"mine\"\n";
        let (mut e, _d) = engine(&[("match/base.yml", base), ("match/mine.yml", mine)]);
        assert_eq!(typed(&mut e, ";s"), None);
        assert!(e.waiting());
        assert_eq!(typed(&mut e, "a").map(|x| x.text), Some("mine".into()));
    }
}
