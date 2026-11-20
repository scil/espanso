//! Form edits of one match, with the smallest change to its YAML text.
//!
//! The text has the shape of [`crate::yamlfile::Item::editable`]: comment
//! lines, then a block mapping at column 0. A form edit replaces only the
//! top-level keys whose values changed. The text of every other key, its
//! comments and its quoting stay as they were.
//!
//! After each change the new text is parsed again and compared with the
//! expected value, so a wrong splice is an error, never a wrong file.

use serde_norway::{Mapping, Value};

use crate::error::{Result, SnipError};
use crate::yamlfile::normalize_item;

/// Top-level keys that the form edits, in the order a new key goes in.
pub const FORM_KEYS: &[&str] = &[
    "trigger",
    "triggers",
    "regex",
    "replace",
    "markdown",
    "html",
    "image_path",
    "form",
    "label",
    "search_terms",
    "word",
    "left_word",
    "right_word",
    "propagate_case",
    "uppercase_style",
    "force_mode",
    "vars",
];

/// The output keys. A match has one of them.
pub const OUTPUT_KEYS: &[&str] = &["replace", "markdown", "html", "image_path", "form"];

/// Variable types and the parameters that the form shows for each.
pub const VAR_TYPES: &[(&str, &[(&str, Param)])] = &[
    ("date", &[("format", Param::Text), ("offset", Param::Int), ("locale", Param::Text)]),
    ("echo", &[("echo", Param::Text)]),
    ("random", &[("choices", Param::Lines)]),
    (
        "shell",
        &[
            ("cmd", Param::Multiline),
            ("shell", Param::Text),
            ("trim", Param::Bool),
            ("debug", Param::Bool),
        ],
    ),
    ("script", &[("args", Param::Lines)]),
    ("clipboard", &[]),
    ("choice", &[("values", Param::Lines)]),
    ("form", &[("layout", Param::Multiline)]),
    ("match", &[("trigger", Param::Text)]),
    ("global", &[]),
];

/// How the form edits one variable parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    Text,
    Multiline,
    /// A list of strings, one per line.
    Lines,
    Bool,
    Int,
}

/// The fields of a match as the form shows them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MatchForm {
    /// Comment lines above the match, without `#` and one space.
    pub comments: Vec<String>,
    pub triggers: Vec<String>,
    /// Empty when the match has no regex trigger.
    pub regex: String,
    /// One of [`OUTPUT_KEYS`].
    pub output: String,
    /// The value of the output key.
    pub body: String,
    pub label: String,
    pub search_terms: Vec<String>,
    pub word: bool,
    pub left_word: bool,
    pub right_word: bool,
    pub propagate_case: bool,
    /// Empty, `uppercase`, `capitalize` or `capitalize_words`.
    pub uppercase_style: String,
    /// Empty, `keys` or `clipboard`.
    pub force_mode: String,
    /// Each variable as a mapping. [`var_get`] and [`var_set`] edit them.
    pub vars: Vec<Value>,
    /// Top-level keys that only the YAML view edits.
    pub other_keys: Vec<String>,
}

impl MatchForm {
    /// Reads the form fields. Fails when the text is not a block mapping
    /// that this module can edit.
    pub fn from_yaml(yaml: &str) -> Result<MatchForm> {
        let doc = Doc::split(yaml)?;
        let value = doc.value()?;
        let m = value.as_mapping().expect("checked by split");
        let s = |k: &str| m.get(k).map(scalar_text).unwrap_or_default();
        let list = |k: &str| -> Vec<String> {
            match m.get(k) {
                Some(Value::Sequence(l)) => l.iter().map(scalar_text).collect(),
                Some(v) => vec![scalar_text(v)],
                None => Vec::new(),
            }
        };
        let flag = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
        let mut triggers = list("trigger");
        triggers.extend(list("triggers"));
        let output = OUTPUT_KEYS
            .iter()
            .find(|k| m.contains_key(**k))
            .copied()
            .unwrap_or("replace");
        let other_keys = m
            .keys()
            .map(scalar_text)
            .filter(|k| !FORM_KEYS.contains(&k.as_str()))
            .collect();
        Ok(MatchForm {
            comments: doc.comments.iter().map(|c| strip_hash(c)).collect(),
            triggers,
            regex: s("regex"),
            output: output.to_string(),
            body: s(output),
            label: s("label"),
            search_terms: list("search_terms"),
            word: flag("word"),
            left_word: flag("left_word"),
            right_word: flag("right_word"),
            propagate_case: flag("propagate_case"),
            uppercase_style: s("uppercase_style"),
            force_mode: s("force_mode"),
            vars: match m.get("vars") {
                Some(Value::Sequence(l)) => l.clone(),
                _ => Vec::new(),
            },
            other_keys,
        })
    }

    /// Writes the form into `yaml` and returns the new text. Keys whose
    /// value did not change keep their text.
    pub fn apply(&self, yaml: &str) -> Result<String> {
        let mut doc = Doc::split(yaml)?;
        let old = doc.value()?;
        let old = old.as_mapping().expect("checked by split");
        for (key, want) in self.wanted(old) {
            if old.get(key) != want.as_ref() {
                doc.set(key, want.as_ref())?;
            }
        }
        // Rewrite the comments only after a change, so a line such as
        // `## Title` keeps its form.
        let old_texts: Vec<String> = doc.comments.iter().map(|c| strip_hash(c)).collect();
        if self.comments != old_texts {
            doc.comments = self.comments.iter().map(|c| add_hash(c)).collect();
        }
        // `Doc::set` already checked each changed key. The whole text must
        // still read as a form.
        let text = doc.join();
        MatchForm::from_yaml(&text)?;
        Ok(text)
    }

    /// The value each form key must have. `None` removes the key.
    fn wanted(&self, old: &Mapping) -> Vec<(&'static str, Option<Value>)> {
        let mut out: Vec<(&'static str, Option<Value>)> = Vec::new();
        let text = |s: &str| (!s.is_empty()).then(|| Value::String(s.to_string()));
        let strings = |l: &[String]| Value::Sequence(l.iter().cloned().map(Value::String).collect());
        // A flag keeps an explicit `false`, so turning it on and off again
        // gives back the old text.
        let flag = |key: &str, on: bool| -> Option<Value> {
            if on {
                Some(Value::Bool(true))
            } else {
                old.get(key).filter(|v| **v == Value::Bool(false)).cloned()
            }
        };

        if !self.regex.is_empty() {
            out.push(("trigger", None));
            out.push(("triggers", None));
            out.push(("regex", text(&self.regex)));
        } else {
            out.push(("regex", None));
            let keep_list = old.contains_key("triggers") && !old.contains_key("trigger");
            match self.triggers.len() {
                0 => {
                    out.push(("trigger", None));
                    out.push(("triggers", None));
                }
                1 if !keep_list => {
                    out.push(("trigger", Some(Value::String(self.triggers[0].clone()))));
                    out.push(("triggers", None));
                }
                _ => {
                    out.push(("trigger", None));
                    out.push(("triggers", Some(strings(&self.triggers))));
                }
            }
        }
        for key in OUTPUT_KEYS {
            let want = (*key == self.output).then(|| Value::String(self.body.clone()));
            out.push((key, want));
        }
        out.push(("label", text(&self.label)));
        out.push((
            "search_terms",
            (!self.search_terms.is_empty()).then(|| strings(&self.search_terms)),
        ));
        out.push(("word", flag("word", self.word)));
        out.push(("left_word", flag("left_word", self.left_word)));
        out.push(("right_word", flag("right_word", self.right_word)));
        out.push(("propagate_case", flag("propagate_case", self.propagate_case)));
        out.push(("uppercase_style", text(&self.uppercase_style)));
        out.push(("force_mode", text(&self.force_mode)));
        out.push(("vars", (!self.vars.is_empty()).then(|| Value::Sequence(self.vars.clone()))));
        out
    }
}

/// A new variable of type `kind`. A date variable gets the format
/// `%Y-%m-%d`, so it works at once; the other types start empty.
pub fn new_var(name: &str, kind: &str) -> Value {
    let mut m = Mapping::new();
    m.insert("name".into(), name.into());
    m.insert("type".into(), kind.into());
    if VAR_TYPES.iter().any(|(t, p)| *t == kind && !p.is_empty()) {
        let mut params = Mapping::new();
        if kind == "date" {
            params.insert("format".into(), "%Y-%m-%d".into());
        }
        m.insert("params".into(), Value::Mapping(params));
    }
    Value::Mapping(m)
}

/// What each variable type gives, one line each, for the editor's help.
pub const VAR_HELP: &[(&str, &str)] = &[
    ("date", "the date or time, in a strftime format such as %Y-%m-%d"),
    ("echo", "a fixed text"),
    ("random", "one of the choices, picked at random"),
    ("shell", "the output of a shell command"),
    ("script", "the output of a script"),
    ("clipboard", "the text on the clipboard"),
    ("choice", "a value that you pick from a list"),
    ("form", "the fields of a small form that you fill in"),
    ("match", "the text of another snippet, by its trigger"),
    ("global", "a global variable, by its name"),
];

/// The help line of a variable type, or "" for an unknown type.
pub fn var_help(kind: &str) -> &'static str {
    VAR_HELP.iter().find(|(t, _)| *t == kind).map(|(_, h)| *h).unwrap_or("")
}

/// A name for a new variable of type `kind` that no name in `taken` uses:
/// `date1`, `date2` and so on.
pub fn free_var_name(kind: &str, taken: &[String]) -> String {
    (1..).map(|n| format!("{kind}{n}")).find(|n| !taken.contains(n)).unwrap_or_default()
}

/// The `{{` that the caret types a variable name after, in `text` with the
/// caret at byte `caret`: the byte offset of `{{` and the name typed so
/// far. `None` when the text between `{{` and the caret is not a name.
pub fn open_braces(text: &str, caret: usize) -> Option<(usize, &str)> {
    let before = text.get(..caret)?;
    let start = before.rfind("{{")?;
    let typed = &before[start + 2..];
    typed.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-').then_some((start, typed))
}

/// The bytes that a picked name replaces: from the `{{` at `start` to the
/// caret, and a `}}` right after the caret too.
pub fn braces_span(text: &str, start: usize, caret: usize) -> std::ops::Range<usize> {
    let end = if text.get(caret..).is_some_and(|r| r.starts_with("}}")) { caret + 2 } else { caret };
    start..end
}

/// The text of a variable field. `path` is `["name"]`, `["type"]` or
/// `["params", "cmd"]`. A list reads as one item per line.
pub fn var_get(var: &Value, path: &[&str]) -> String {
    let mut v = var;
    for key in path {
        match v.get(*key) {
            Some(next) => v = next,
            None => return String::new(),
        }
    }
    match v {
        Value::Sequence(l) => l.iter().map(scalar_text).collect::<Vec<_>>().join("\n"),
        other => scalar_text(other),
    }
}

/// Sets a variable field from its text. An empty text removes the field.
pub fn var_set(var: &mut Value, path: &[&str], text: &str, kind: Param) {
    let Some((last, parents)) = path.split_last() else { return };
    let mut m = match var {
        Value::Mapping(m) => m,
        _ => return,
    };
    for key in parents {
        let entry = m
            .entry(Value::String(key.to_string()))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        if !entry.is_mapping() {
            *entry = Value::Mapping(Mapping::new());
        }
        m = entry.as_mapping_mut().expect("just made a mapping");
    }
    let value = match kind {
        _ if text.is_empty() => None,
        Param::Text | Param::Multiline => Some(Value::String(text.to_string())),
        Param::Lines => Some(Value::Sequence(
            text.lines().filter(|l| !l.trim().is_empty()).map(|l| l.into()).collect(),
        )),
        Param::Bool => Some(Value::Bool(matches!(text, "true" | "yes" | "on"))),
        Param::Int => Some(match text.trim().parse::<i64>() {
            Ok(n) => Value::Number(n.into()),
            Err(_) => Value::String(text.to_string()),
        }),
    };
    let key = Value::String(last.to_string());
    match value {
        Some(v) => {
            m.insert(key, v);
        }
        None => {
            m.remove(&key);
        }
    }
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => serde_norway::to_string(other).unwrap_or_default().trim_end().to_string(),
    }
}

fn strip_hash(line: &str) -> String {
    let rest = line.trim_start().strip_prefix('#').unwrap_or(line);
    rest.strip_prefix(' ').unwrap_or(rest).to_string()
}

fn add_hash(text: &str) -> String {
    if text.is_empty() {
        "#".into()
    } else {
        format!("# {text}")
    }
}

/// One match text cut into comment lines and top-level entries.
#[derive(Debug, Clone)]
struct Doc {
    comments: Vec<String>,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    key: String,
    /// Comment lines at column 0 directly above the key.
    lead: String,
    /// The key line and the lines that belong to its value, with LF ends.
    body: String,
    /// Blank lines after the value.
    trail: String,
}

impl Doc {
    fn split(yaml: &str) -> Result<Doc> {
        let normalized = normalize_item(yaml)?;
        let mut doc = Doc { comments: Vec::new(), entries: Vec::new() };
        let mut pending = String::new();
        let mut started = false;
        for line in normalized.split('\n') {
            let trimmed = line.trim();
            let at_col0 = !line.starts_with([' ', '\t']);
            if !started {
                if trimmed.starts_with('#') {
                    doc.comments.push(trimmed.to_string());
                    continue;
                }
                if trimmed.is_empty() {
                    continue;
                }
            }
            if at_col0 && trimmed.starts_with('#') {
                pending.push_str(line);
                pending.push('\n');
                continue;
            }
            if at_col0 && !trimmed.is_empty() {
                let key = key_of(line).ok_or_else(|| {
                    SnipError::Unsupported(format!(
                        "the form cannot read the line `{line}`; edit it in the YAML view"
                    ))
                })?;
                started = true;
                doc.entries.push(Entry {
                    key,
                    lead: std::mem::take(&mut pending),
                    body: format!("{line}\n"),
                    trail: String::new(),
                });
                continue;
            }
            let Some(entry) = doc.entries.last_mut() else { continue };
            if trimmed.is_empty() {
                entry.trail.push_str(line);
                entry.trail.push('\n');
            } else {
                // An indented line after blank lines still belongs to the
                // value, such as a block scalar with an empty line.
                entry.body.push_str(&std::mem::take(&mut entry.trail));
                entry.body.push_str(&pending);
                pending.clear();
                entry.body.push_str(line);
                entry.body.push('\n');
            }
        }
        if let Some(last) = doc.entries.last_mut() {
            last.trail.push_str(&pending);
        }
        // The cut must agree with the parser.
        let value = doc.value()?;
        let Some(m) = value.as_mapping() else {
            return Err(SnipError::Unsupported("a match must be a mapping".into()));
        };
        let keys: Vec<String> = m.keys().map(scalar_text).collect();
        let cut: Vec<String> = doc.entries.iter().map(|e| e.key.clone()).collect();
        if keys != cut {
            return Err(SnipError::Unsupported(
                "the form cannot read this match; edit it in the YAML view".into(),
            ));
        }
        Ok(doc)
    }

    fn join(&self) -> String {
        let mut out = String::new();
        for c in &self.comments {
            out.push_str(c);
            out.push('\n');
        }
        for e in &self.entries {
            out.push_str(&e.lead);
            out.push_str(&e.body);
            out.push_str(&e.trail);
        }
        out.trim_end().to_string()
    }

    fn value(&self) -> Result<Value> {
        let text = self.join();
        serde_norway::from_str(&text).map_err(|e| SnipError::InvalidYaml(e.to_string()))
    }

    /// Replaces, adds or removes one top-level key.
    fn set(&mut self, key: &str, value: Option<&Value>) -> Result<()> {
        let pos = self.entries.iter().position(|e| e.key == key);
        match (pos, value) {
            (Some(i), Some(v)) => self.entries[i].body = emit_entry(key, v),
            (Some(i), None) => {
                let gone = self.entries.remove(i);
                // Keep the blank line that separated the entries.
                if let Some(prev) = i.checked_sub(1).and_then(|p| self.entries.get_mut(p)) {
                    if prev.trail.is_empty() {
                        prev.trail = gone.trail;
                    }
                }
            }
            (None, Some(v)) => {
                let at = self.insert_position(key);
                let entry = Entry {
                    key: key.to_string(),
                    lead: String::new(),
                    body: emit_entry(key, v),
                    trail: String::new(),
                };
                // A new last entry takes over the blank lines of the old one.
                if at == self.entries.len() {
                    if let Some(prev) = self.entries.last_mut() {
                        let trail = std::mem::take(&mut prev.trail);
                        self.entries.push(Entry { trail, ..entry });
                        return self.check(key, value);
                    }
                }
                self.entries.insert(at, entry);
            }
            (None, None) => return Ok(()),
        }
        self.check(key, value)
    }

    fn check(&self, key: &str, value: Option<&Value>) -> Result<()> {
        let parsed = self.value()?;
        if parsed.get(key) != value {
            return Err(SnipError::InvalidInput(format!(
                "the new `{key}` value did not round-trip; edit it in the YAML view"
            )));
        }
        Ok(())
    }

    /// After the nearest key that comes before `key` in [`FORM_KEYS`], or
    /// before the nearest one that comes after it, or at the end.
    fn insert_position(&self, key: &str) -> usize {
        let rank = |k: &str| FORM_KEYS.iter().position(|f| *f == k);
        let Some(r) = rank(key) else { return self.entries.len() };
        let before = self
            .entries
            .iter()
            .rposition(|e| rank(&e.key).is_some_and(|x| x < r));
        if let Some(i) = before {
            return i + 1;
        }
        self.entries
            .iter()
            .position(|e| rank(&e.key).is_some_and(|x| x > r))
            .unwrap_or(self.entries.len())
    }
}

/// The key of a top-level line such as `trigger: ":hi"` or `"label": x`.
fn key_of(line: &str) -> Option<String> {
    if let Some(q) = line.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let end = line[1..].find(q)? + 1;
        let rest = line[end + 1..].trim_start();
        return rest.starts_with(':').then(|| line[1..end].to_string());
    }
    if line.starts_with(['-', '?', '{', '[', '&', '*', '!', '|', '>']) {
        return None;
    }
    let colon = line.find(": ").or_else(|| line.strip_suffix(':').map(str::len))?;
    Some(line[..colon].trim_end().to_string())
}

/// `key: value` as block YAML at column 0, with an LF after each line.
pub fn emit_entry(key: &str, value: &Value) -> String {
    let mut out = String::new();
    emit_pair(&mut out, 0, &plain_or_quoted(key), value);
    out
}

fn emit_pair(out: &mut String, indent: usize, key: &str, value: &Value) {
    let pad = " ".repeat(indent);
    match value {
        Value::Mapping(m) if !m.is_empty() => {
            out.push_str(&format!("{pad}{key}:\n"));
            emit_mapping(out, indent + 2, m);
        }
        Value::Sequence(l) if !l.is_empty() => {
            out.push_str(&format!("{pad}{key}:\n"));
            emit_sequence(out, indent + 2, l);
        }
        Value::String(s) if block_ok(s) => {
            out.push_str(&format!("{pad}{key}: {}\n", block_header(s)));
            emit_block_lines(out, indent + 2, s);
        }
        other => out.push_str(&format!("{pad}{key}: {}\n", inline(other))),
    }
}

fn emit_mapping(out: &mut String, indent: usize, m: &Mapping) {
    for (k, v) in m {
        emit_pair(out, indent, &plain_or_quoted(&scalar_text(k)), v);
    }
}

fn emit_sequence(out: &mut String, indent: usize, l: &[Value]) {
    let pad = " ".repeat(indent);
    for item in l {
        match item {
            Value::Mapping(m) if !m.is_empty() => {
                // The first pair goes on the dash line.
                let mut inner = String::new();
                emit_mapping(&mut inner, indent + 2, m);
                let first = inner.strip_prefix(&" ".repeat(indent + 2)).unwrap_or(&inner);
                out.push_str(&format!("{pad}- {first}"));
            }
            Value::String(s) if block_ok(s) => {
                out.push_str(&format!("{pad}- {}\n", block_header(s)));
                emit_block_lines(out, indent + 2, s);
            }
            Value::Sequence(inner) if !inner.is_empty() => {
                out.push_str(&format!("{pad}-\n"));
                emit_sequence(out, indent + 2, inner);
            }
            other => out.push_str(&format!("{pad}- {}\n", inline(other))),
        }
    }
}

/// Whether a string can be a literal block (`|`): it has a line break, no
/// carriage return or other control character, and its first line does not
/// start with a space.
fn block_ok(s: &str) -> bool {
    s.contains('\n')
        && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
        && !s.starts_with([' ', '\t', '\n'])
}

fn block_header(s: &str) -> &'static str {
    let trailing = s.len() - s.trim_end_matches('\n').len();
    match trailing {
        0 => "|-",
        1 => "|",
        _ => "|+",
    }
}

fn emit_block_lines(out: &mut String, indent: usize, s: &str) {
    let pad = " ".repeat(indent);
    let body = s.strip_suffix('\n').unwrap_or(s);
    for line in body.split('\n') {
        if line.is_empty() {
            out.push('\n');
        } else {
            out.push_str(&format!("{pad}{line}\n"));
        }
    }
}

fn inline(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => plain_or_quoted(s),
        Value::Sequence(_) => "[]".into(),
        Value::Mapping(_) => "{}".into(),
        Value::Tagged(_) => serde_norway::to_string(v).unwrap_or_default().trim_end().to_string(),
    }
}

/// A plain scalar when it reads back as the same string, else a double
/// quoted one.
fn plain_or_quoted(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || " _-./()".contains(c) || !c.is_ascii())
        && !s.starts_with([' ', '-', '.'])
        && !s.ends_with(' ')
        && !s.chars().any(char::is_control)
        && matches!(serde_norway::from_str::<Value>(s), Ok(Value::String(ref back)) if back == s);
    if plain {
        return s.to_string();
    }
    let mut q = String::from('"');
    for c in s.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            '\n' => q.push_str("\\n"),
            '\t' => q.push_str("\\t"),
            '\r' => q.push_str("\\r"),
            c if c.is_control() => q.push_str(&format!("\\u{:04x}", c as u32)),
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_braces_finds_the_name_being_typed() {
        let t = "Hi {{na";
        assert_eq!(open_braces(t, t.len()), Some((3, "na")));
        assert_eq!(open_braces("Hi {{", 5), Some((3, "")));
        // A closed variable or a space ends it.
        assert_eq!(open_braces("{{a}} b", 7), None);
        assert_eq!(open_braces("{{a b", 5), None);
        assert_eq!(open_braces("no braces", 9), None);
        // Not on a character boundary.
        assert_eq!(open_braces("{{é", 3), None);
    }

    #[test]
    fn braces_span_takes_a_closing_pair_after_the_caret() {
        assert_eq!(braces_span("{{na}} x", 0, 4), 0..6);
        assert_eq!(braces_span("{{na x", 0, 4), 0..4);
    }

    #[test]
    fn a_new_variable_gets_a_free_name_and_defaults() {
        let taken = vec!["date1".to_string(), "x".to_string()];
        assert_eq!(free_var_name("date", &taken), "date2");
        assert_eq!(var_get(&new_var("d", "date"), &["params", "format"]), "%Y-%m-%d");
        assert_eq!(var_get(&new_var("c", "clipboard"), &["params"]), "");
        assert!(VAR_TYPES.iter().all(|(t, _)| !var_help(t).is_empty()));
    }

    const ITEM: &str = "## greeting\n# 问候\ntrigger: \":hi\"   # inline\nreplace: |\n  Hello,\n\n  world\nlabel: 'Say hi'\nvars:\n  - name: d\n    type: date\n    params:\n      format: \"%H:%M\"\nfoo: kept   # unknown key";

    #[test]
    fn reads_the_fields() {
        let f = MatchForm::from_yaml(ITEM).unwrap();
        assert_eq!(f.comments, vec!["# greeting", "问候"]);
        assert_eq!(f.triggers, vec![":hi"]);
        assert_eq!(f.output, "replace");
        assert_eq!(f.body, "Hello,\n\nworld\n");
        assert_eq!(f.label, "Say hi");
        assert_eq!(f.vars.len(), 1);
        assert_eq!(var_get(&f.vars[0], &["params", "format"]), "%H:%M");
        assert_eq!(f.other_keys, vec!["foo"]);
    }

    #[test]
    fn unchanged_form_keeps_the_text() {
        let f = MatchForm::from_yaml(ITEM).unwrap();
        assert_eq!(f.apply(ITEM).unwrap(), ITEM);
    }

    #[test]
    fn a_change_touches_only_its_key() {
        let mut f = MatchForm::from_yaml(ITEM).unwrap();
        f.label = "Greeting".into();
        let out = f.apply(ITEM).unwrap();
        assert_eq!(out, ITEM.replace("label: 'Say hi'", "label: Greeting"));
    }

    #[test]
    fn multiline_body_is_a_literal_block() {
        let mut f = MatchForm::from_yaml(ITEM).unwrap();
        f.body = "Line one\nLine \"two\"".into();
        let out = f.apply(ITEM).unwrap();
        assert!(out.contains("replace: |-\n  Line one\n  Line \"two\"\nlabel:"), "{out}");
        assert!(out.contains("trigger: \":hi\"   # inline"));
    }

    #[test]
    fn second_trigger_switches_to_a_list() {
        let mut f = MatchForm::from_yaml(ITEM).unwrap();
        f.triggers.push(",,hi".into());
        let out = f.apply(ITEM).unwrap();
        assert!(out.starts_with("## greeting\n# 问候\ntriggers:\n  - \":hi\"\n  - \",,hi\"\nreplace: |"), "{out}");
        // Back to one trigger: a one-item list stays a list.
        let mut g = MatchForm::from_yaml(&out).unwrap();
        g.triggers.pop();
        let back = g.apply(&out).unwrap();
        assert!(back.contains("triggers:\n  - \":hi\"\n"), "{back}");
    }

    #[test]
    fn new_keys_go_in_form_order() {
        let mut f = MatchForm::from_yaml(ITEM).unwrap();
        f.word = true;
        f.search_terms = vec!["hello".into(), "问候".into()];
        let out = f.apply(ITEM).unwrap();
        assert!(
            out.contains("label: 'Say hi'\nsearch_terms:\n  - hello\n  - 问候\nword: true\nvars:"),
            "{out}"
        );
        // Off again removes the key.
        let mut g = MatchForm::from_yaml(&out).unwrap();
        g.word = false;
        g.search_terms.clear();
        assert_eq!(g.apply(&out).unwrap(), ITEM);
    }

    #[test]
    fn variable_edit_rewrites_only_vars() {
        let mut f = MatchForm::from_yaml(ITEM).unwrap();
        let mut v = new_var("out", "shell");
        var_set(&mut v, &["params", "cmd"], "echo one\necho two", Param::Multiline);
        var_set(&mut v, &["params", "trim"], "true", Param::Bool);
        f.vars.push(v);
        let out = f.apply(ITEM).unwrap();
        assert!(out.contains("  - name: out\n    type: shell\n    params:\n      cmd: |-\n        echo one\n        echo two\n      trim: true\n"), "{out}");
        assert!(out.contains("label: 'Say hi'\n"));
        assert!(out.ends_with("foo: kept   # unknown key"));
    }

    #[test]
    fn comments_change() {
        let mut f = MatchForm::from_yaml(ITEM).unwrap();
        f.comments = vec!["only one".into()];
        let out = f.apply(ITEM).unwrap();
        assert!(out.starts_with("# only one\ntrigger:"));
    }

    #[test]
    fn regex_replaces_triggers() {
        let mut f = MatchForm::from_yaml(ITEM).unwrap();
        f.regex = ":(?P<n>\\d+)x".into();
        let out = f.apply(ITEM).unwrap();
        assert!(out.contains("regex: \":(?P<n>\\\\d+)x\"\nreplace:"), "{out}");
        assert!(!out.contains("trigger"));
    }

    #[test]
    fn a_new_match_from_nothing() {
        let f = MatchForm {
            triggers: vec![",,sig".into()],
            output: "replace".into(),
            body: "Best regards".into(),
            ..Default::default()
        };
        let out = f.apply("replace: \"\"").unwrap();
        assert_eq!(out, "trigger: \",,sig\"\nreplace: Best regards");
    }

    #[test]
    fn flow_items_are_refused() {
        assert!(MatchForm::from_yaml("{trigger: a, replace: b}").is_err());
    }

    #[test]
    fn quoting_round_trips() {
        for s in ["yes", "123", "a: b", "#x", "  lead", "tab\there", "true", ":hi", "中文", "x'y\"z", ""] {
            let e = emit_entry("k", &Value::String(s.into()));
            let back: Value = serde_norway::from_str(&e).unwrap();
            assert_eq!(back.get("k"), Some(&Value::String(s.into())), "{s:?} -> {e}");
        }
    }
}
