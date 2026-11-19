//! Paragraph picks: a match whose paragraphs the user checks in a list.
//! Only the checked paragraphs are inserted.
//!
//! ```yaml
//! - trigger: ";;reply"
//!   snip: { pick: many }
//!   replace: |
//!     Hello,
//!
//!     Thank you for your message.
//!
//!     Best regards
//! ```
//!
//! A blank line separates paragraphs. The checked paragraphs keep their
//! order in the text and are joined with one blank line. `checked: none`
//! starts with no paragraph checked; the default is `all`. espanso ignores
//! the `snip:` key and inserts the whole text.

use serde_norway::Value;

/// How a match picks its paragraphs, from its `snip:` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    /// Whether every paragraph starts checked.
    pub all_checked: bool,
}

/// Reads `snip.pick` and `snip.checked` from a match item. `Ok(None)` if
/// the item has no `snip.pick`. An error is a message for the user.
pub fn read(item: &Value) -> Result<Option<Pick>, String> {
    let Some(snip) = item.get("snip") else { return Ok(None) };
    let checked = snip.get("checked");
    let Some(pick) = snip.get("pick") else {
        return match checked {
            Some(_) => Err("snip.checked has no effect without snip.pick: many".into()),
            None => Ok(None),
        };
    };
    if pick.as_str() != Some("many") {
        return Err(format!("snip.pick must be many, not {}", show(pick)));
    }
    let all_checked = match checked.map(|c| c.as_str()) {
        None | Some(Some("all")) => true,
        Some(Some("none")) => false,
        Some(_) => return Err(format!("snip.checked must be all or none, not {}", show(checked.unwrap()))),
    };
    Ok(Some(Pick { all_checked }))
}

fn show(v: &Value) -> String {
    serde_norway::to_string(v).unwrap_or_default().trim().to_string()
}

/// A text split into paragraphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pieces {
    /// Blank lines before the first paragraph.
    pub head: String,
    pub parts: Vec<String>,
    /// White space after the last paragraph, for example the line break
    /// that a YAML `|` block adds.
    pub tail: String,
}

/// Splits a text at blank lines. A line with only spaces counts as blank.
pub fn split(text: &str) -> Pieces {
    let end = text.trim_end().len();
    let (core, tail) = text.split_at(end);
    let mut start = 0;
    for line in core.split_inclusive('\n') {
        if !line.trim().is_empty() {
            break;
        }
        start += line.len();
    }
    let (head, core) = core.split_at(start);
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in core.split('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Pieces { head: head.to_string(), parts, tail: tail.to_string() }
}

/// The text with only the paragraphs in `chosen`, in text order, joined
/// with one blank line. Indexes out of range are skipped.
pub fn join(pieces: &Pieces, chosen: &[usize]) -> String {
    let mut chosen: Vec<usize> = chosen.iter().copied().filter(|&i| i < pieces.parts.len()).collect();
    chosen.sort_unstable();
    chosen.dedup();
    let body: Vec<&str> = chosen.iter().map(|&i| pieces.parts[i].as_str()).collect();
    format!("{}{}{}", pieces.head, body.join("\n\n"), pieces.tail)
}

/// The variable names that a text uses, as espanso reads them:
/// `{{name}}` or `{{name.field}}`, with optional spaces inside the braces.
/// A name has letters, digits and `_`.
pub fn var_names(text: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = text;
    let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_');
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else { break };
        let inner = after[..close].trim();
        if inner.split('.').count() <= 2 && inner.split('.').all(word) {
            names.push(inner.split('.').next().unwrap_or_default());
            rest = &after[close + 2..];
        } else {
            // Try again one brace later, for `{{{name}}`.
            rest = &rest[open + 1..];
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(yaml: &str) -> Value {
        serde_norway::from_str(yaml).unwrap()
    }

    #[test]
    fn reads_the_snip_key() {
        assert_eq!(read(&item("replace: a")), Ok(None));
        assert_eq!(read(&item("snip: { priority: personal }")), Ok(None));
        assert_eq!(read(&item("snip: { pick: many }")), Ok(Some(Pick { all_checked: true })));
        assert_eq!(read(&item("snip: { pick: many, checked: none }")), Ok(Some(Pick { all_checked: false })));
        assert!(read(&item("snip: { pick: one }")).unwrap_err().contains("must be many"));
        assert!(read(&item("snip: { pick: many, checked: some }")).unwrap_err().contains("all or none"));
        assert!(read(&item("snip: { checked: none }")).unwrap_err().contains("no effect"));
    }

    #[test]
    fn splits_at_blank_lines() {
        let p = split("\nHello,\n\nLine one\nline two\n  \n\n\nBye\n");
        assert_eq!(p.head, "\n");
        assert_eq!(p.parts, ["Hello,", "Line one\nline two", "Bye"]);
        assert_eq!(p.tail, "\n");
    }

    #[test]
    fn keeps_indentation_inside_a_paragraph() {
        let p = split("fn a() {\n    b();\n}");
        assert_eq!(p.parts, ["fn a() {\n    b();\n}"]);
        assert_eq!(p.tail, "");
    }

    #[test]
    fn finds_variable_names() {
        assert_eq!(var_names("a {{x}} b {{ form1.name }} {{x}} {{bad name}} {{{y}}"), ["x", "form1", "x", "y"]);
        assert_eq!(var_names("{{a.b.c}} {{ok}} {{open"), ["ok"]);
    }

    #[test]
    fn joins_in_text_order() {
        let p = split("A\n\nB\n\nC\n");
        assert_eq!(join(&p, &[2, 0, 0, 9]), "A\n\nC\n");
        assert_eq!(join(&p, &[1]), "B\n");
        assert_eq!(join(&p, &[]), "\n");
    }
}
