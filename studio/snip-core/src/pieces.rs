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
//! A blank line separates paragraphs. A paragraph with a list splits once
//! more: its lines before the list are one row, and each list item is a
//! row. The checked rows keep their order in the text. Rows of one
//! paragraph are joined with a line break, paragraphs with one blank line.
//! `checked: none` starts with no row checked; the default is `all`.
//! espanso ignores
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

/// A text split into rows: paragraphs, and the lead and list items of a
/// paragraph with a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pieces {
    /// Blank lines before the first paragraph.
    pub head: String,
    pub parts: Vec<String>,
    /// The paragraph of each part, counted from 0.
    pub groups: Vec<usize>,
    /// Whether each part is a list item.
    pub items: Vec<bool>,
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
    let mut paragraphs = Vec::new();
    let mut current = Vec::new();
    for line in core.split('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    let mut pieces =
        Pieces { head: head.to_string(), parts: vec![], groups: vec![], items: vec![], tail: tail.to_string() };
    for (group, lines) in paragraphs.iter().enumerate() {
        for (text, item) in split_list(lines) {
            pieces.parts.push(text);
            pieces.groups.push(group);
            pieces.items.push(item);
        }
    }
    pieces
}

/// The indent of a line that starts a list item: `- `, `* `, `+ `, `1. `
/// or `1) ` after spaces. `None` for other lines.
fn item_indent(line: &str) -> Option<usize> {
    let rest = line.trim_start();
    let indent = line.len() - rest.len();
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let starts = if digits > 0 {
        rest[digits..].starts_with(". ") || rest[digits..].starts_with(") ")
    } else {
        ["- ", "* ", "+ "].iter().any(|m| rest.starts_with(m))
    };
    starts.then_some(indent)
}

/// Splits the lines of a paragraph into its lead and its list items. Each
/// item at the smallest indent starts a row; other lines stay in the row
/// above them. A paragraph with no list, or with one item and no lead, is
/// one row. The flag tells whether a row is a list item.
fn split_list(lines: &[&str]) -> Vec<(String, bool)> {
    let whole = || vec![(lines.join("\n"), false)];
    let Some(top) = lines.iter().filter_map(|l| item_indent(l)).min() else { return whole() };
    let mut rows: Vec<(Vec<&str>, bool)> = Vec::new();
    for &line in lines {
        let starts = item_indent(line) == Some(top);
        match rows.last_mut() {
            Some(row) if !starts => row.0.push(line),
            _ => rows.push((vec![line], starts)),
        }
    }
    if rows.len() < 2 {
        return whole();
    }
    rows.into_iter().map(|(l, item)| (l.join("\n"), item)).collect()
}

/// The text with only the parts in `chosen`, in text order. Parts of one
/// paragraph are joined with a line break, paragraphs with one blank line.
/// Indexes out of range are skipped.
pub fn join(pieces: &Pieces, chosen: &[usize]) -> String {
    let mut chosen: Vec<usize> = chosen.iter().copied().filter(|&i| i < pieces.parts.len()).collect();
    chosen.sort_unstable();
    chosen.dedup();
    let mut body = String::new();
    for (n, &i) in chosen.iter().enumerate() {
        if n > 0 {
            body.push_str(if pieces.groups[chosen[n - 1]] == pieces.groups[i] { "\n" } else { "\n\n" });
        }
        body.push_str(&pieces.parts[i]);
    }
    format!("{}{}{}", pieces.head, body, pieces.tail)
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
    fn splits_a_list_into_items() {
        let p = split("Intro\n\nAdd:\n  - one\n    more\n  - two\n    - deep\n  - three\n\nBye");
        assert_eq!(p.parts, ["Intro", "Add:", "  - one\n    more", "  - two\n    - deep", "  - three", "Bye"]);
        assert_eq!(p.groups, [0, 1, 1, 1, 1, 2]);
        assert_eq!(p.items, [false, false, true, true, true, false]);
        assert_eq!(join(&p, &[1, 3, 5]), "Add:\n  - two\n    - deep\n\nBye");
        assert_eq!(join(&p, &[2, 4]), "  - one\n    more\n  - three");
    }

    #[test]
    fn keeps_a_single_item_whole() {
        assert_eq!(split("- only\n  more").parts, ["- only\n  more"]);
        assert_eq!(split("1. a\n2) b\n-x").parts, ["1. a", "2) b\n-x"]);
        assert_eq!(split("-x\n*y").parts, ["-x\n*y"]);
    }

    #[test]
    fn joins_in_text_order() {
        let p = split("A\n\nB\n\nC\n");
        assert_eq!(join(&p, &[2, 0, 0, 9]), "A\n\nC\n");
        assert_eq!(join(&p, &[1]), "B\n");
        assert_eq!(join(&p, &[]), "\n");
    }
}
