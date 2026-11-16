//! Lossless access to the `matches:` list of one espanso match file.
//!
//! The file is parsed twice. `yaml-edit` gives the exact byte range of each
//! item, and `serde_norway` gives its value. Edits are text splices on those
//! ranges, so every byte outside the edited item stays unchanged.

use std::str::FromStr;

use serde_norway::Value;
use sha2::{Digest, Sha256};

use crate::error::{Result, SnipError};

/// One item of the `matches:` sequence.
#[derive(Debug, Clone)]
pub struct Item {
    /// Byte offset of the first character after `- `.
    pub start: usize,
    /// Byte offset after the last non-whitespace character of the item.
    pub end: usize,
    /// Byte offset of the start of the first line that belongs to the item,
    /// including comment lines directly above the dash.
    pub block_start: usize,
    /// Byte offset after the newline that ends the item's last line.
    pub block_end: usize,
    /// Byte offset of the start of the line that holds the dash.
    pub dash_line_start: usize,
    /// Column of the dash.
    pub dash_col: usize,
    /// Column where the item's keys start.
    pub content_col: usize,
    /// One-based line number of the dash.
    pub line: usize,
    /// The parsed value of the item.
    pub value: Value,
}

impl Item {
    /// The item's own text, from the first key to the last character.
    pub fn raw<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start..self.end]
    }

    /// Everything that belongs to the item: attached comments, dash and body.
    /// The hash covers this text.
    pub fn block<'a>(&self, text: &'a str) -> &'a str {
        &text[self.block_start..self.end]
    }

    /// The item as a standalone YAML mapping, with its attached comments
    /// first. This is the text an editor shows and sends back.
    pub fn editable(&self, text: &str) -> String {
        let mut out = String::new();
        for line in text[self.block_start..self.dash_line_start].lines() {
            out.push_str(line.get(self.dash_col..).unwrap_or(line.trim_start()));
            out.push('\n');
        }
        out.push_str(&dedent(self.raw(text), self.content_col));
        out
    }
}

#[derive(Debug, Clone)]
pub enum Layout {
    /// A block sequence under `matches:`; edits are supported.
    Block,
    /// The file has no `matches:` key; a create appends one.
    Missing,
    /// The file has `matches:`, but in a form this module does not edit
    /// (flow style `[...]`, empty value, or not a sequence).
    Unsupported(String),
}

#[derive(Debug, Clone)]
pub struct MatchFile {
    pub text: String,
    pub bom: bool,
    pub newline: &'static str,
    pub layout: Layout,
    pub items: Vec<Item>,
    /// The file's `global_vars` names and types, used by the portability check.
    pub global_vars: Vec<(String, String)>,
}

pub fn hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

impl MatchFile {
    pub fn parse(source: &str) -> Result<Self> {
        let (bom, text) = match source.strip_prefix('\u{feff}') {
            Some(rest) => (true, rest.to_string()),
            None => (false, source.to_string()),
        };
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };

        let value: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_norway::from_str(&text).map_err(|e| SnipError::InvalidYaml(e.to_string()))?
        };
        let root = match &value {
            Value::Mapping(m) => Some(m),
            Value::Null => None,
            _ => {
                return Err(SnipError::InvalidYaml(
                    "the top level is not a mapping".into(),
                ))
            }
        };
        let values: Option<&Value> = root.and_then(|m| m.get("matches"));
        let global_vars = root
            .and_then(|m| m.get("global_vars"))
            .and_then(Value::as_sequence)
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| {
                        let name = v.get("name")?.as_str()?.to_string();
                        let kind = v
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        Some((name, kind))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut file = MatchFile {
            text,
            bom,
            newline,
            layout: Layout::Missing,
            items: Vec::new(),
            global_vars,
        };

        let Some(values) = values else {
            return Ok(file);
        };
        let Some(values) = values.as_sequence() else {
            file.layout = Layout::Unsupported("`matches` is not a list".into());
            return Ok(file);
        };

        let doc = yaml_edit::Document::from_str(&file.text)
            .map_err(|e| SnipError::InvalidYaml(e.to_string()))?;
        let seq = doc
            .as_mapping()
            .and_then(|m| m.get_sequence("matches"))
            .ok_or_else(|| SnipError::InvalidYaml("cannot locate the `matches` list".into()))?;
        if seq.is_flow_style() {
            file.layout = Layout::Unsupported("`matches` uses flow style `[...]`".into());
            return Ok(file);
        }

        let ranges: Vec<(usize, usize)> = seq
            .values()
            .map(|node| {
                let r = node.byte_range();
                (r.start as usize, r.end as usize)
            })
            .collect();
        if ranges.len() != values.len() {
            return Err(SnipError::InvalidYaml(format!(
                "parsers disagree on the number of matches ({} vs {})",
                ranges.len(),
                values.len()
            )));
        }

        for ((start, end), value) in ranges.into_iter().zip(values.iter()) {
            file.items
                .push(locate_item(&file.text, start, end, value.clone())?);
        }
        file.layout = Layout::Block;
        Ok(file)
    }

    /// The file text with the original byte order mark restored.
    pub fn render(&self, text: &str) -> String {
        if self.bom {
            format!("\u{feff}{text}")
        } else {
            text.to_string()
        }
    }

    pub fn item(&self, index: usize) -> Result<&Item> {
        self.items
            .get(index)
            .ok_or_else(|| SnipError::NotFound(format!("match #{index}")))
    }

    pub fn check_hash(&self, index: usize, expected: &str) -> Result<&Item> {
        let item = self.item(index)?;
        let actual = hash(item.block(&self.text));
        if actual != expected {
            return Err(SnipError::Conflict {
                expected: expected.into(),
                actual,
            });
        }
        Ok(item)
    }

    /// Returns the file text with item `index` replaced by `yaml`.
    ///
    /// `yaml` has the shape of [`Item::editable`]: optional comment lines,
    /// then the mapping. When it equals the item's current text, the file is
    /// returned unchanged. Otherwise the whole item is re-indented, so a
    /// comment line that sat left of the item's indent moves to the indent.
    pub fn replace(&self, index: usize, yaml: &str) -> Result<String> {
        let item = self.item(index)?;
        let normalized = normalize_item(yaml)?;
        if normalize_item(&item.editable(&self.text)).ok().as_ref() == Some(&normalized) {
            return Ok(self.text.clone());
        }
        // Keep the original dash prefix, for example `  - `.
        let prefix = &self.text[item.dash_line_start..item.start];
        let segment = self.render_item(&normalized, prefix, item.dash_col, item.content_col);
        Ok(format!(
            "{}{}{}",
            &self.text[..item.block_start],
            segment,
            &self.text[item.end..]
        ))
    }

    fn render_item(
        &self,
        normalized: &str,
        prefix: &str,
        dash_col: usize,
        content_col: usize,
    ) -> String {
        let (comments, body) = split_comments(normalized);
        let mut out = String::new();
        for comment in comments {
            out.push_str(&" ".repeat(dash_col));
            out.push_str(comment);
            out.push_str(self.newline);
        }
        out.push_str(prefix);
        out.push_str(&reindent(body, content_col, self.newline));
        out
    }

    /// Returns the file text without item `index` and its attached comments.
    pub fn delete(&self, index: usize) -> Result<String> {
        let item = self.item(index)?;
        let before = &self.text[..item.block_start];
        let mut after = &self.text[item.block_end..];
        // Keep one blank line between neighbours, not two.
        if index == 0 || ends_with_blank_line(before) || after.trim().is_empty() {
            after = strip_one_blank_line(after);
        }
        let mut out = if after.trim().is_empty() {
            // The item was the last thing in the file: drop trailing blank lines.
            format!(
                "{}{}",
                before.trim_end_matches([' ', '\t', '\r', '\n']),
                self.newline
            )
        } else {
            format!("{before}{after}")
        };
        if self.items.len() == 1 {
            // An empty `matches:` is null, which espanso rejects.
            out = replace_empty_matches_key(&out, self.newline);
        }
        Ok(out)
    }

    /// Returns the file text with `yaml` appended as a new last item.
    pub fn append(&self, yaml: &str) -> Result<String> {
        let normalized = normalize_item(yaml)?;
        let nl = self.newline;
        match &self.layout {
            Layout::Unsupported(reason) => Err(SnipError::Unsupported(reason.clone())),
            Layout::Missing => {
                let mut out = self.text.clone();
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push_str(nl);
                }
                if !out.trim().is_empty() {
                    out.push_str(nl);
                }
                out.push_str("matches:");
                out.push_str(nl);
                out.push_str(&self.render_item(&normalized, "  - ", 2, 4));
                out.push_str(nl);
                Ok(out)
            }
            Layout::Block => {
                let last = self.items.last().expect("block layout has items");
                let separated = self.items.len() < 2 || self.items_are_blank_separated();
                let mut out = String::with_capacity(self.text.len() + normalized.len() + 16);
                out.push_str(&self.text[..last.block_end]);
                if !out.ends_with('\n') {
                    out.push_str(nl);
                }
                if separated {
                    out.push_str(nl);
                }
                let prefix = format!("{}- ", " ".repeat(last.dash_col));
                out.push_str(&self.render_item(
                    &normalized,
                    &prefix,
                    last.dash_col,
                    last.dash_col + 2,
                ));
                out.push_str(nl);
                out.push_str(&self.text[last.block_end..]);
                Ok(out)
            }
        }
    }

    fn items_are_blank_separated(&self) -> bool {
        // The text between two items holds only blank lines (comments above
        // an item belong to its block), so any newline means a blank line.
        let gap = &self.text[self.items[0].block_end..self.items[1].block_start];
        gap.contains('\n')
    }
}

fn locate_item(text: &str, start: usize, end: usize, value: Value) -> Result<Item> {
    let end = start + text[start..end].trim_end().len();
    // Walk back from the item to its dash.
    let before = &text[..start];
    let dash = before
        .trim_end()
        .strip_suffix('-')
        .map(str::len)
        .ok_or_else(|| SnipError::Unsupported("a match item does not start with `- `".into()))?;
    let dash_line_start = line_start(text, dash);
    let dash_col = dash - dash_line_start;
    let content_col = start - line_start(text, start);

    // Attach comment lines directly above the dash (no blank line between).
    let mut block_start = dash_line_start;
    while block_start > 0 {
        let prev_start = line_start(text, block_start - 1);
        let prev = text[prev_start..block_start].trim_end_matches(['\r', '\n']);
        let trimmed = prev.trim_start();
        if trimmed.starts_with('#') && prev.len() - trimmed.len() == dash_col {
            block_start = prev_start;
        } else {
            break;
        }
    }

    let block_end = match text[end..].find('\n') {
        Some(i) => end + i + 1,
        None => text.len(),
    };
    let line = text[..dash].matches('\n').count() + 1;
    Ok(Item {
        start,
        end,
        block_start,
        block_end,
        dash_line_start,
        dash_col,
        content_col,
        line,
        value,
    })
}

/// Splits leading comment lines from the mapping. Blank lines between the
/// comments are dropped.
fn split_comments(normalized: &str) -> (Vec<&str>, &str) {
    let mut comments = Vec::new();
    let mut rest = normalized;
    loop {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            comments.push(trimmed);
        } else if !trimmed.is_empty() {
            return (comments, rest);
        }
        if tail.is_empty() {
            return (comments, "");
        }
        rest = tail;
    }
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |i| i + 1)
}

fn ends_with_blank_line(text: &str) -> bool {
    let t = text.trim_end_matches([' ', '\t']);
    t.ends_with("\n\n") || t.ends_with("\r\n\r\n")
}

fn strip_one_blank_line(text: &str) -> &str {
    let trimmed = text.trim_start_matches([' ', '\t']);
    trimmed
        .strip_prefix("\r\n")
        .or_else(|| trimmed.strip_prefix('\n'))
        .unwrap_or(text)
}

fn replace_empty_matches_key(text: &str, nl: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        if bare.trim_end() == "matches:" && !bare.starts_with(' ') {
            out.push_str("matches: []");
            out.push_str(if line.ends_with('\n') { nl } else { "" });
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Checks that `yaml` is one mapping and returns it without a common indent,
/// with LF line ends and no trailing whitespace.
pub fn normalize_item(yaml: &str) -> Result<String> {
    let unix = yaml.replace("\r\n", "\n");
    let lines: Vec<&str> = unix.trim_end().lines().collect();
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let body: Vec<&str> = lines
        .iter()
        .map(|l| {
            if l.len() >= indent {
                &l[indent..]
            } else {
                l.trim_start()
            }
        })
        .collect();
    let body = body.join("\n");
    let body = body.trim_start_matches('\n').to_string();

    let value: Value =
        serde_norway::from_str(&body).map_err(|e| SnipError::InvalidYaml(e.to_string()))?;
    if !value.is_mapping() {
        return Err(SnipError::InvalidInput(
            "a match must be a YAML mapping such as `trigger: ...` and `replace: ...`".into(),
        ));
    }
    Ok(body)
}

/// Removes the list indent (`col` spaces) from every line after the first, so
/// the item reads as a standalone mapping. Lines with less indent stay as is.
pub fn dedent(raw: &str, col: usize) -> String {
    let pad = " ".repeat(col);
    raw.replace("\r\n", "\n")
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                line
            } else {
                line.strip_prefix(&pad).unwrap_or(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Indents every line after the first by `col` spaces. Blank lines stay empty.
fn reindent(body: &str, col: usize, nl: &str) -> String {
    let pad = " ".repeat(col);
    body.split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 || line.is_empty() {
                line.to_string()
            } else {
                format!("{pad}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join(nl)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Personal snippets\n# 中文注释\nimports:\n  - \"_sub.yml\"\n\nmatches:\n  # greeting\n  - trigger: \":hi\"\n    replace: \"你好，世界\"   # inline comment\n\n  - trigger: \":sig\"\n    replace: |\n      Best regards,\n      Ray\n\n  # list of triggers\n  - triggers: [\":ty\", \":thx\"]\n    replace: \"Thank you!\"\n    word: true\n\n# trailing comment\n";

    #[test]
    fn finds_items_and_attached_comments() {
        let f = MatchFile::parse(SAMPLE).unwrap();
        assert_eq!(f.items.len(), 3);
        assert_eq!(
            f.items[0].raw(&f.text),
            "trigger: \":hi\"\n    replace: \"你好，世界\"   # inline comment"
        );
        assert!(f.text[f.items[0].block_start..].starts_with("  # greeting\n"));
        assert!(f.text[f.items[2].block_start..].starts_with("  # list of triggers\n"));
        assert_eq!(f.items[1].dash_col, 2);
        assert_eq!(f.items[1].content_col, 4);
        assert_eq!(f.items[1].line, 11);
    }

    #[test]
    fn replace_touches_only_the_item() {
        let f = MatchFile::parse(SAMPLE).unwrap();
        let out = f
            .replace(1, "trigger: \":sig\"\nreplace: |\n  Cheers,\n\n  Ray")
            .unwrap();
        let expected = SAMPLE.replace(
            "replace: |\n      Best regards,\n      Ray",
            "replace: |\n      Cheers,\n\n      Ray",
        );
        assert_eq!(out, expected);
        MatchFile::parse(&out).unwrap();
    }

    #[test]
    fn delete_removes_item_and_its_comment() {
        let f = MatchFile::parse(SAMPLE).unwrap();
        let out = f.delete(2).unwrap();
        assert!(!out.contains("list of triggers"));
        assert!(!out.contains(":thx"));
        assert!(out.contains("# trailing comment"));
        assert!(!out.contains("\n\n\n"));
        assert_eq!(MatchFile::parse(&out).unwrap().items.len(), 2);

        let out = f.delete(0).unwrap();
        assert!(!out.contains("# greeting"));
        assert!(out.starts_with("# Personal snippets"));
        assert_eq!(MatchFile::parse(&out).unwrap().items.len(), 2);
    }

    #[test]
    fn append_follows_file_style() {
        let f = MatchFile::parse(SAMPLE).unwrap();
        let out = f.append("trigger: \":new\"\nreplace: \"新的\"").unwrap();
        let g = MatchFile::parse(&out).unwrap();
        assert_eq!(g.items.len(), 4);
        assert_eq!(
            g.items[3].raw(&g.text),
            "trigger: \":new\"\n    replace: \"新的\""
        );
        assert!(out.ends_with("# trailing comment\n"));
    }

    #[test]
    fn append_to_file_without_matches() {
        let f = MatchFile::parse("# only a comment\n").unwrap();
        let out = f.append("trigger: a\nreplace: b").unwrap();
        assert_eq!(
            out,
            "# only a comment\n\nmatches:\n  - trigger: a\n    replace: b\n"
        );
    }

    #[test]
    fn crlf_and_bom_survive() {
        let src = format!("\u{feff}{}", SAMPLE.replace('\n', "\r\n"));
        let f = MatchFile::parse(&src).unwrap();
        let out = f.render(&f.replace(0, "trigger: \":hi\"\nreplace: hey").unwrap());
        assert!(out.starts_with('\u{feff}'));
        assert!(!out.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn deleting_last_item_leaves_valid_list() {
        let f = MatchFile::parse("matches:\n  - trigger: a\n    replace: b\n").unwrap();
        let out = f.delete(0).unwrap();
        assert_eq!(out, "matches: []\n");
    }

    #[test]
    fn comments_above_an_item_are_editable() {
        let f = MatchFile::parse(SAMPLE).unwrap();
        assert_eq!(
            f.items[0].editable(&f.text),
            "# greeting\ntrigger: \":hi\"\nreplace: \"你好，世界\"   # inline comment"
        );
        // Change the comment and add a second one.
        let out = f
            .replace(0, "# hello\n# 问候\ntrigger: \":hi\"\nreplace: \"你好\"")
            .unwrap();
        assert!(out.contains("matches:\n  # hello\n  # 问候\n  - trigger: \":hi\"\n    replace: \"你好\"\n\n  - trigger: \":sig\""));
        let g = MatchFile::parse(&out).unwrap();
        assert_eq!(g.items.len(), 3);
        // Remove all comments.
        let out = f.replace(0, "trigger: \":hi\"\nreplace: \"你好\"").unwrap();
        assert!(out.contains("matches:\n  - trigger: \":hi\"\n"));
        // A new item with a comment.
        let out = f.append("# new one\ntrigger: \":n\"\nreplace: n").unwrap();
        assert!(out.contains("\n  # new one\n  - trigger: \":n\"\n    replace: n\n"));
        MatchFile::parse(&out).unwrap();
    }

    #[test]
    fn deleting_the_final_item_leaves_no_trailing_blank_line() {
        let f = MatchFile::parse(
            "matches:\n  - trigger: a\n    replace: b\n\n  - trigger: c\n    replace: d\n",
        )
        .unwrap();
        assert_eq!(
            f.delete(1).unwrap(),
            "matches:\n  - trigger: a\n    replace: b\n"
        );
    }

    #[test]
    fn rejects_non_mapping_item() {
        let f = MatchFile::parse(SAMPLE).unwrap();
        assert!(f.replace(0, "- trigger: x").is_err());
        assert!(f.replace(0, "just text").is_err());
    }

    #[test]
    fn flow_style_is_reported() {
        let f = MatchFile::parse("matches: [{trigger: a, replace: b}]\n").unwrap();
        assert!(matches!(f.layout, Layout::Unsupported(_)));
    }
}
