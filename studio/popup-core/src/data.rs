use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_norway::Value;
use snip_core::portability::Portability;
use snip_core::Workspace;

/// Most results a popup shows.
pub const MAX_RESULTS: usize = 40;

/// One snippet as the popup shows it.
#[derive(Debug, Clone)]
pub struct Entry {
    /// espanso match id, for entries from [`crate::expand::Engine`]. -1 for
    /// entries from `snip-core`.
    pub id: i32,
    pub file: String,
    pub index: usize,
    /// Triggers joined with spaces, or the regex.
    pub trigger: String,
    pub label: String,
    /// First line of the replacement, at most 80 characters.
    pub preview: String,
    /// True when the popup can insert the text itself: plain text with no
    /// variables. Other snippets need espanso to render them.
    pub insertable: bool,
    haystack: String,
    trigger_lower: String,
}

pub struct Snippets {
    ws: Workspace,
    pub entries: Vec<Entry>,
}

impl Snippets {
    pub fn load(base: Option<PathBuf>) -> Result<Snippets> {
        let ws = Workspace::open(base)?;
        let list = ws.list_snippets(None, None)?;
        let entries = list
            .snippets
            .into_iter()
            .map(|s| {
                let trigger = if s.triggers.is_empty() {
                    s.regex.clone().unwrap_or_default()
                } else {
                    s.triggers.join("  ")
                };
                let insertable = s.kind == "text"
                    && s.portability.level == Portability::Static
                    && !s.preview.contains("{{");
                let mut e = Entry::new(
                    -1,
                    trigger,
                    s.label.clone().unwrap_or_default(),
                    &s.preview,
                    &s.search_terms,
                    insertable,
                );
                e.file = s.file;
                e.index = s.index;
                e
            })
            .collect();
        Ok(Snippets { ws, entries })
    }

    /// Indexes of the entries that match `query`, best first. See
    /// [`filter`].
    pub fn filter(&self, query: &str, prefix: &str) -> Vec<usize> {
        filter(&self.entries, query, prefix)
    }

    /// The full text to insert for an entry, or an error that says why the
    /// popup cannot insert it.
    pub fn text_for(&self, entry: &Entry) -> Result<String> {
        if !entry.insertable {
            return Err(anyhow!("this snippet needs espanso to render it"));
        }
        let detail = self.ws.get_snippet(&entry.file, entry.index)?;
        let value: Value = serde_norway::from_str(&detail.yaml)?;
        value
            .get("replace")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("snippet has no replace text"))
    }
}

impl Entry {
    /// An entry with no file. `text` is the full replacement text.
    pub fn new(
        id: i32,
        trigger: String,
        label: String,
        text: &str,
        search_terms: &[String],
        insertable: bool,
    ) -> Entry {
        let first = text.lines().next().unwrap_or("");
        let mut preview: String = first.chars().take(80).collect();
        if preview.len() < text.len() {
            preview.push('…');
        }
        let haystack = format!("{}\n{}\n{}\n{}", trigger, label, text, search_terms.join("\n"))
            .to_lowercase();
        Entry {
            id,
            file: String::new(),
            index: 0,
            trigger_lower: trigger.to_lowercase(),
            trigger,
            label,
            preview,
            insertable,
            haystack,
        }
    }
}

/// Indexes of the entries that match `query`, best first.
///
/// Each word of the query must appear somewhere in the entry. `prefix` is
/// the popup trigger, such as `;;`. Many triggers start with it, and the
/// user types the keyword after it, so the ranking compares the query with
/// each trigger without that prefix:
///
/// 0. A trigger is the typed text in full: `prefix` and the query (`;;doc`
///    for `doc`). See [`exact_trigger`].
/// 1. A trigger starts with `prefix` and the rest starts with the query
///    (`;;docx` for `doc`).
/// 2. A trigger starts with the query.
/// 3. A trigger contains the query.
/// 4. Any other hit.
///
/// An empty query lists the triggers that start with `prefix` first.
pub fn filter(entries: &[Entry], query: &str, prefix: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let prefix = prefix.to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut hits: Vec<(u8, usize)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| words.iter().all(|w| e.haystack.contains(w)))
        .map(|(i, e)| (rank_lower(e.trigger_lower.split("  "), &q, &prefix), i))
        .collect();
    hits.sort();
    hits.into_iter().take(MAX_RESULTS).map(|(_, i)| i).collect()
}

/// The rank of a snippet with these triggers for `query`, as [`filter`]
/// ranks it: 0 is best, 4 means no trigger fits. The editor list uses it to
/// show the popup's order.
pub fn trigger_rank<'a>(triggers: impl IntoIterator<Item = &'a str>, query: &str, prefix: &str) -> u8 {
    let lower: Vec<String> = triggers.into_iter().map(str::to_lowercase).collect();
    rank_lower(lower.iter().map(String::as_str), &query.trim().to_lowercase(), &prefix.to_lowercase())
}

/// [`trigger_rank`] for triggers, query and prefix in lower case.
fn rank_lower<'a>(triggers: impl IntoIterator<Item = &'a str>, q: &str, prefix: &str) -> u8 {
    let mut best = 4;
    for t in triggers {
        let r = match t.strip_prefix(prefix) {
            Some(rest) if !prefix.is_empty() && !q.is_empty() && rest == q => 0,
            Some(rest) if !prefix.is_empty() && rest.starts_with(q) => 1,
            _ if t.starts_with(q) => 2,
            _ if t.contains(q) => 3,
            _ => 4,
        };
        best = best.min(r);
    }
    best
}

/// The trigger of `entry` that is exactly the typed text, `prefix` and then
/// `query`, ignoring case. It is the trigger as written in the match file.
/// `None` for an empty query or when no trigger is the typed text.
pub fn exact_trigger<'a>(entry: &'a Entry, query: &str, prefix: &str) -> Option<&'a str> {
    if query.trim().is_empty() {
        return None;
    }
    let typed = format!("{prefix}{}", query.trim()).to_lowercase();
    entry
        .trigger
        .split("  ")
        .zip(entry.trigger_lower.split("  "))
        .find(|(_, lower)| *lower == typed)
        .map(|(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(trigger: &str, preview: &str) -> Entry {
        Entry::new(0, trigger.into(), String::new(), preview, &[], true)
    }

    #[test]
    fn ranks_trigger_prefix_first() {
        let e = vec![entry(":x", "hello sig"), entry(":asig", "a"), entry(":sig", "b")];
        assert_eq!(filter(&e, ":sig", ""), vec![2]);
        assert_eq!(filter(&e, "sig", ""), vec![1, 2, 0]);
    }

    #[test]
    fn every_word_must_match() {
        let e = vec![entry(":addr", "北京 海淀"), entry(":home", "北京")];
        assert_eq!(filter(&e, "北京 海淀", ""), vec![0]);
    }

    #[test]
    fn prefix_is_ignored_in_triggers() {
        let e = vec![
            entry("doc", "a"),
            entry(":x", "doc text"),
            entry(";;harvest  ;;doc", "b"),
            entry("x;;doc", "c"),
        ];
        assert_eq!(filter(&e, "doc", ";;"), vec![2, 0, 3, 1]);
    }

    #[test]
    fn empty_query_lists_prefix_triggers_first() {
        let e = vec![entry(":a", "a"), entry(";;b", "b"), entry("c", "c"), entry(";;d", "d")];
        assert_eq!(filter(&e, "", ";;"), vec![1, 3, 0, 2]);
    }

    #[test]
    fn exact_trigger_comes_first() {
        // `;;anki2` loads first, but `;;anki` is the typed text in full.
        let e = vec![entry(";;anki2", "a"), entry(";;x  ;;Anki", "b")];
        assert_eq!(filter(&e, "anki", ";;"), vec![1, 0]);
        assert_eq!(exact_trigger(&e[1], "anki", ";;"), Some(";;Anki"));
        assert_eq!(exact_trigger(&e[0], "anki", ";;"), None);
        assert_eq!(exact_trigger(&e[0], "", ";;"), None);
    }
}
