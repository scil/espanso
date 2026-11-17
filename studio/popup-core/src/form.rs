//! Forms and choice lists, as espanso's `form` and `choice` variables
//! describe them. This module turns their render parameters into a [`Form`]
//! that the UI shows, and has no Windows code. See `studio/EXPANDER.md`.

use std::collections::HashMap;

use espanso_render::{Params, Value};

/// One piece of a form layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Plain text. It can hold line breaks.
    Text(String),
    /// The input box of the field with this name.
    Field(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Text { multiline: bool },
    /// A value from a list. espanso's `choice` and `list` fields both pick
    /// one value.
    Choice { values: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    pub default: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub parts: Vec<Part>,
    /// The fields in layout order, each name once.
    pub fields: Vec<Field>,
    /// The snippet text with `{{var.field}}` places, for the preview.
    pub preview: Option<Preview>,
}

/// The snippet text and the names of its form variables. The preview fills
/// `{{name.field}}` with the typed values and leaves other variables as
/// they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub body: String,
    pub vars: Vec<String>,
}

/// The name of the one field of a choice list.
pub const CHOICE_FIELD: &str = "choice";

impl Form {
    /// A form from the parameters of espanso's form extension: `layout` with
    /// `[[name]]` fields, and `fields` with the type, default and values of
    /// each field. It follows `convert_fields` of espanso's worker.
    pub fn from_espanso(layout: &str, fields: &Params) -> Form {
        // A YAML block layout ends with a line break, which is no row.
        let parts = parse_layout(layout.trim_end_matches(['\n', '\r']));
        let mut out: Vec<Field> = Vec::new();
        for part in &parts {
            let Part::Field(name) = part else { continue };
            if out.iter().any(|f| &f.name == name) {
                continue;
            }
            out.push(convert_field(name, fields.get(name)));
        }
        Form { parts, fields: out, preview: None }
    }

    /// A list of values to pick one from, for a `choice` variable.
    pub fn choice(labels: Vec<String>) -> Form {
        Form {
            parts: vec![Part::Field(CHOICE_FIELD.to_string())],
            fields: vec![Field {
                name: CHOICE_FIELD.to_string(),
                kind: Kind::Choice { values: labels },
                default: String::new(),
            }],
            preview: None,
        }
    }

    /// Whether a preview shows more than the input boxes do: the layout
    /// has text, or the snippet text uses a form value.
    pub fn has_preview(&self) -> bool {
        match &self.preview {
            Some(p) => p.vars.iter().any(|v| p.body.contains(&format!("{{{{{v}."))),
            None => !self.is_choice(),
        }
    }

    /// Whether this form is only a list of values to pick from.
    pub fn is_choice(&self) -> bool {
        self.fields.len() == 1
            && self.parts.len() == 1
            && matches!(self.fields[0].kind, Kind::Choice { .. })
    }

    /// The result with these values: the snippet text if known, otherwise
    /// the layout.
    pub fn fill(&self, values: &HashMap<String, String>) -> String {
        match &self.preview {
            Some(p) => {
                let mut body = p.body.clone();
                for var in &p.vars {
                    for f in &self.fields {
                        let value = values.get(&f.name).map(String::as_str).unwrap_or("");
                        body = body.replace(&format!("{{{{{var}.{}}}}}", f.name), value);
                    }
                }
                body
            }
            None => self
                .parts
                .iter()
                .map(|p| match p {
                    Part::Text(t) => t.as_str(),
                    Part::Field(name) => values.get(name).map(String::as_str).unwrap_or(""),
                })
                .collect(),
        }
    }
}

/// Splits a layout into text and `[[name]]` fields. A name has letters,
/// digits and `_`, as in espanso.
pub fn parse_layout(layout: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut rest = layout;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let name_len = after
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
            .map_or(after.len(), |(i, _)| i);
        if name_len > 0 && after[name_len..].starts_with("]]") {
            text.push_str(&rest[..start]);
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(Part::Field(after[..name_len].to_string()));
            rest = &after[name_len + 2..];
        } else {
            text.push_str(&rest[..start + 2]);
            rest = after;
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    parts
}

fn convert_field(name: &str, params: Option<&Value>) -> Field {
    let params = match params {
        Some(Value::Object(p)) => Some(p),
        _ => None,
    };
    let get = |key: &str| params.and_then(|p| p.get(key));
    let default = match get("default") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let kind = match get("type") {
        Some(Value::String(t)) if t == "choice" || t == "list" => Kind::Choice {
            values: get("values").map(|v| values(v, get("trim_string_values"))).unwrap_or_default(),
        },
        _ => Kind::Text { multiline: matches!(get("multiline"), Some(Value::Bool(true))) },
    };
    Field { name: name.to_string(), kind, default }
}

/// The values of a `choice` or `list` field: a list of strings, or a string
/// with one value per line.
fn values(value: &Value, trim: Option<&Value>) -> Vec<String> {
    let trim = !matches!(trim, Some(Value::Bool(false)));
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        Value::String(s) => s
            .lines()
            .filter_map(|line| {
                if !trim {
                    return Some(line.to_string());
                }
                let line = line.trim();
                (!line.is_empty()).then(|| line.to_string())
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// One row of a pick list in the popup: a match of a tie, or a value of a
/// `choice` variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickItem {
    /// The first line of the row. Titles of one list are unique.
    pub title: String,
    /// The second line, for example the snippet text. Can be empty.
    pub detail: String,
    /// A short tag at the right, for example the file. Can be empty.
    pub badge: String,
}

impl PickItem {
    pub fn new(title: impl Into<String>) -> Self {
        PickItem { title: title.into(), detail: String::new(), badge: String::new() }
    }

    /// The text that the filter searches.
    pub fn search_text(&self) -> String {
        format!("{} {} {}", self.title, self.detail, self.badge)
    }
}

/// The values of a list filtered by `query`: indexes of values that hold
/// every word of the query, ignoring case.
pub fn filter(values: &[String], query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    values
        .iter()
        .enumerate()
        .filter(|(_, v)| {
            let v = v.to_lowercase();
            words.iter().all(|w| v.contains(w.as_str()))
        })
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    #[test]
    fn layout_splits_into_parts() {
        assert_eq!(
            parse_layout("Key: [[a]] and [[b_1]]!\n[x] [[ no ]]"),
            vec![
                Part::Text("Key: ".into()),
                Part::Field("a".into()),
                Part::Text(" and ".into()),
                Part::Field("b_1".into()),
                Part::Text("!\n[x] [[ no ]]".into()),
            ]
        );
        assert_eq!(parse_layout("应用名：[[app]]")[1], Part::Field("app".into()));
    }

    #[test]
    fn fields_follow_the_layout() {
        let mut fields = Params::new();
        fields.insert(
            "b".into(),
            obj(&[
                ("type", Value::String("list".into())),
                ("values", Value::String(" x \n\n y\n".into())),
                ("default", Value::String("y".into())),
            ]),
        );
        fields.insert("c".into(), obj(&[("multiline", Value::Bool(true))]));
        let form = Form::from_espanso("[[c]] [[a]] [[b]] [[a]]", &fields);
        let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["c", "a", "b"]);
        assert_eq!(form.fields[0].kind, Kind::Text { multiline: true });
        assert_eq!(form.fields[1].kind, Kind::Text { multiline: false });
        assert_eq!(form.fields[2].kind, Kind::Choice { values: vec!["x".into(), "y".into()] });
        assert_eq!(form.fields[2].default, "y");
    }

    #[test]
    fn fill_uses_the_snippet_text() {
        let mut form = Form::from_espanso("App: [[app]]", &Params::new());
        let values = HashMap::from([("app".to_string(), "git".to_string())]);
        assert_eq!(form.fill(&values), "App: git");
        form.preview = Some(Preview {
            body: "scoop update {{form1.app}} {{other}}".into(),
            vars: vec!["form1".into()],
        });
        assert_eq!(form.fill(&values), "scoop update git {{other}}");
    }

    #[test]
    fn preview_only_when_the_text_uses_the_form() {
        let mut form = Form::from_espanso("File:\n[[file]]\n", &Params::new());
        assert_eq!(form.parts.len(), 2);
        form.preview = Some(Preview { body: "{{edit}}".into(), vars: vec!["form1".into()] });
        assert!(!form.has_preview());
        form.preview = Some(Preview { body: "a {{form1.file}}".into(), vars: vec!["form1".into()] });
        assert!(form.has_preview());
    }

    #[test]
    fn filter_matches_all_words() {
        let v = vec!["Apple pie".to_string(), "pear".to_string(), "pineapple".to_string()];
        assert_eq!(filter(&v, "APP"), [0, 2]);
        assert_eq!(filter(&v, "pie app"), [0]);
        assert_eq!(filter(&v, ""), [0, 1, 2]);
    }
}
