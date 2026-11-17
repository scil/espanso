//! Snippet priority: which match wins when two active matches share a
//! trigger. The model follows XPTemplate: a lower number wins.
//!
//! A match file sets its priority under a top-level `snip:` key. All
//! matches of the file share it, as in XPTemplate. To give one match another
//! priority, move it to another file. A `snip.priority` on a single match has
//! no effect, and the validation warns about it. espanso ignores unknown
//! keys, so the files still load in espanso.
//!
//! ```yaml
//! snip:
//!   priority: personal
//! matches:
//!   - trigger: ";;sig"
//!     replace: "Best regards"
//! ```
//!
//! A value is a number, or a name with an optional offset: `personal` (0),
//! `normal` (16), `package` (64), for example `normal-1` or `package+4`.
//! Without a value, files under `match/packages/` get `package` and all
//! other files get `normal`.
//!
//! espanso's [`Match`] does not keep its source file. [`Origins::load`]
//! finds it from the match store: a match belongs to the smallest loaded
//! file whose query holds it, because a query also holds the matches of the
//! files that the file imports. If two files have the same query, the file
//! whose items hold the match's triggers wins. The position in the file
//! comes from the order of the file's own matches, checked against each
//! item's triggers.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use espanso_config::config::ConfigStore;
use espanso_config::matches::store::MatchStore;
use espanso_config::matches::{Match, MatchCause};
use serde::Serialize;
use serde_norway::Value;

use crate::error::Problem;

pub const PERSONAL: i32 = 0;
pub const NORMAL: i32 = 16;
pub const PACKAGE: i32 = 64;

/// Where a match comes from and how strong it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Origin {
    /// Path relative to the espanso base folder, with `/` separators.
    pub file: String,
    /// Position in the file's `matches:` list, if the item was found.
    pub index: Option<usize>,
    pub priority: i32,
}

/// The origin of every loaded match, by match id.
#[derive(Debug, Default)]
pub struct Origins {
    pub by_id: HashMap<i32, Origin>,
    /// Priority values that do not parse.
    pub problems: Vec<Problem>,
}

impl Origins {
    pub fn load(base: &Path, store: &dyn MatchStore) -> Origins {
        let paths = store.loaded_paths();
        let sets: Vec<Vec<&Match>> = paths.iter().map(|p| store.query(std::slice::from_ref(p)).matches).collect();
        let docs: Vec<Option<Value>> = paths
            .iter()
            .map(|p| {
                let text = fs::read_to_string(p).ok()?;
                serde_norway::from_str::<Value>(text.trim_start_matches('\u{FEFF}')).ok()
            })
            .collect();
        let keys: Vec<Vec<Vec<String>>> =
            docs.iter().map(|d| items_of(d.as_ref()).iter().map(item_key).collect()).collect();

        // The owner of a match is the loaded file with the smallest query
        // that holds it. Two files have the same query when one only
        // imports the other, or in circular imports. Then the file whose
        // items hold the match's triggers wins.
        let mut owner: HashMap<i32, usize> = HashMap::new();
        for (i, set) in sets.iter().enumerate() {
            for m in set {
                let better = match owner.get(&m.id) {
                    None => true,
                    Some(&o) if set.len() != sets[o].len() => set.len() < sets[o].len(),
                    Some(&o) => {
                        let key = match_key(m);
                        !keys[o].contains(&key) && keys[i].contains(&key)
                    }
                };
                if better {
                    owner.insert(m.id, i);
                }
            }
        }

        let mut origins = Origins::default();
        for (i, path) in paths.iter().enumerate() {
            let rel = relative(base, Path::new(path));
            let own: Vec<&Match> = sets[i].iter().copied().filter(|m| owner.get(&m.id) == Some(&i)).collect();
            if own.is_empty() {
                continue;
            }
            let doc = docs[i].as_ref();
            let fallback = if rel.starts_with("match/packages/") { PACKAGE } else { NORMAL };
            let file_priority = doc.and_then(|d| origins.read(d, &rel, "the file")).unwrap_or(fallback);
            let items = items_of(doc);
            origins.warn_match_priorities(items, &rel);

            let mut next = 0;
            for m in own {
                let key = match_key(m);
                let index = (next..items.len()).find(|&j| item_key(&items[j]) == key);
                if let Some(j) = index {
                    next = j + 1;
                }
                origins.by_id.insert(m.id, Origin { file: rel.clone(), index, priority: file_priority });
            }
        }
        origins
    }

    /// The priority of a match. Unknown matches get `normal`.
    pub fn priority(&self, id: i32) -> i32 {
        self.by_id.get(&id).map_or(NORMAL, |o| o.priority)
    }

    /// A priority belongs to a file. Warns about each match that sets one.
    fn warn_match_priorities(&mut self, items: &[Value], rel: &str) {
        for (j, item) in items.iter().enumerate() {
            if item.get("snip").and_then(|s| s.get("priority")).is_some() {
                self.problems.push(Problem {
                    file: rel.to_string(),
                    level: "warning".into(),
                    message: format!(
                        "match {} sets snip.priority, which has no effect: a priority belongs to the file. Move the match to a file with that priority.",
                        j + 1
                    ),
                });
            }
        }
    }

    /// Reads `snip.priority` from a file.
    fn read(&mut self, node: &Value, rel: &str, what: &str) -> Option<i32> {
        let value = node.get("snip")?.get("priority")?;
        let parsed = match value {
            Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
            Value::String(s) => parse(s),
            _ => None,
        };
        if parsed.is_none() {
            self.problems.push(Problem {
                file: rel.to_string(),
                level: "warning".into(),
                message: format!(
                    "snip priority of {what} is not a number or personal, normal, package with an optional +n or -n: {}",
                    serde_norway::to_string(value).unwrap_or_default().trim()
                ),
            });
        }
        parsed
    }
}

/// Parses a priority: a number, or a name with an optional offset.
pub fn parse(text: &str) -> Option<i32> {
    let text = text.trim();
    if let Ok(n) = text.parse() {
        return Some(n);
    }
    let split = text.find(['+', '-']).unwrap_or(text.len());
    let (name, offset) = text.split_at(split);
    let base = match name.trim() {
        "personal" => PERSONAL,
        "normal" => NORMAL,
        "package" => PACKAGE,
        _ => return None,
    };
    let offset = match offset.trim() {
        "" => 0,
        o => o.parse::<i32>().ok()?,
    };
    Some(base + offset)
}

/// Two or more active matches with the same trigger.
#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub trigger: String,
    /// The winner first, then the others in the order they lose.
    pub matches: Vec<Contender>,
    /// The two strongest matches have the same priority. espanso and the
    /// `snip` engine then show a list of the strongest matches.
    pub tie: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contender {
    pub id: i32,
    #[serde(flatten)]
    pub origin: Option<Origin>,
}

/// Picks the strongest of `ids`: the lowest priority. Among equal
/// priorities the first one wins.
pub fn best<I: IntoIterator<Item = i32>>(ids: I, origins: &Origins) -> Option<i32> {
    let mut best: Option<(i32, i32)> = None;
    for id in ids {
        let p = origins.priority(id);
        if best.is_none_or(|(_, bp)| p < bp) {
            best = Some((id, p));
        }
    }
    best.map(|(id, _)| id)
}

/// Every trigger that two or more matches of one configuration share. A
/// conflict that several configurations have is listed once.
pub fn conflicts(configs: &dyn ConfigStore, store: &dyn MatchStore, origins: &Origins) -> Vec<Conflict> {
    let mut seen: HashSet<(String, Vec<i32>)> = HashSet::new();
    let mut out = Vec::new();
    let mut all = vec![configs.default()];
    all.extend(configs.configs());
    for config in all {
        let set = store.query(config.match_paths());
        let mut by_trigger: Vec<(String, Vec<i32>)> = Vec::new();
        for m in &set.matches {
            let MatchCause::Trigger(cause) = &m.cause else { continue };
            for t in &cause.triggers {
                let key = if cause.propagate_case { t.to_lowercase() } else { t.clone() };
                match by_trigger.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, ids)) if !ids.contains(&m.id) => ids.push(m.id),
                    Some(_) => {}
                    None => by_trigger.push((key, vec![m.id])),
                }
            }
        }
        for (trigger, ids) in by_trigger {
            if ids.len() < 2 {
                continue;
            }
            let mut ranked = ids.clone();
            // A stable sort keeps the load order among equal priorities.
            ranked.sort_by_key(|&id| origins.priority(id));
            let mut sorted = ids;
            sorted.sort_unstable();
            if !seen.insert((trigger.clone(), sorted)) {
                continue;
            }
            let tie = origins.priority(ranked[0]) == origins.priority(ranked[1]);
            let matches = ranked
                .into_iter()
                .map(|id| Contender { id, origin: origins.by_id.get(&id).cloned() })
                .collect();
            out.push(Conflict { trigger, matches, tie });
        }
    }
    out
}

/// The items of a match file's `matches:` list.
fn items_of(doc: Option<&Value>) -> &[Value] {
    doc.and_then(|d| d.get("matches")).and_then(Value::as_sequence).map_or(&[], Vec::as_slice)
}

/// The triggers of a loaded match, or its regex, to find its item.
fn match_key(m: &Match) -> Vec<String> {
    match &m.cause {
        MatchCause::Trigger(c) => c.triggers.clone(),
        MatchCause::Regex(c) => vec![c.regex.clone()],
        MatchCause::None => vec![],
    }
}

/// The same key read from a YAML item, as espanso's loader builds it.
fn item_key(item: &Value) -> Vec<String> {
    let strings = |v: &Value| -> Vec<String> {
        match v {
            Value::String(s) => vec![s.clone()],
            Value::Sequence(seq) => seq.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
            _ => vec![],
        }
    };
    if let Some(t) = item.get("trigger") {
        return strings(t).into_iter().filter(|t| !t.trim().is_empty()).collect();
    }
    if let Some(t) = item.get("triggers") {
        return strings(t).into_iter().filter(|t| !t.trim().is_empty()).collect();
    }
    if let Some(Value::String(r)) = item.get("regex") {
        return vec![r.clone()];
    }
    vec![]
}

/// `path` relative to `base`, with `/` separators. espanso resolves links,
/// so the base folder is also tried in its resolved form, without the
/// Windows `\\?\` prefix. A path outside the base stays absolute.
pub(crate) fn relative(base: &Path, path: &Path) -> String {
    let resolved = base.canonicalize().ok().map(|p| {
        let text = p.to_string_lossy().into_owned();
        std::path::PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
    });
    let rel = std::iter::once(base)
        .chain(resolved.as_deref())
        .find_map(|b| path.strip_prefix(b).ok())
        .unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers_and_names() {
        assert_eq!(parse("3"), Some(3));
        assert_eq!(parse("-2"), Some(-2));
        assert_eq!(parse("personal"), Some(0));
        assert_eq!(parse("normal-1"), Some(15));
        assert_eq!(parse("package+4"), Some(68));
        assert_eq!(parse(" normal "), Some(16));
        assert_eq!(parse("lang"), None);
        assert_eq!(parse("normal+x"), None);
    }

    fn folder(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("config")).unwrap();
        fs::create_dir_all(dir.path().join("match/packages/p")).unwrap();
        fs::write(dir.path().join("config/default.yml"), "").unwrap();
        for (rel, text) in files {
            fs::write(dir.path().join(rel), text).unwrap();
        }
        dir
    }

    fn load(dir: &Path) -> (Box<dyn ConfigStore>, Box<dyn MatchStore>, Origins) {
        let (configs, store, errors) = espanso_config::load(dir).unwrap();
        // espanso must ignore the `snip:` keys without a warning.
        let messages: Vec<String> =
            errors.iter().flat_map(|set| set.errors.iter().map(|e| format!("{:#}", e.error))).collect();
        assert!(messages.is_empty(), "{messages:?}");
        let origins = Origins::load(dir, store.as_ref());
        (configs, store, origins)
    }

    #[test]
    fn finds_files_indexes_and_priorities() {
        let dir = folder(&[
            (
                "match/base.yml",
                "imports:\n  - _extra.yml\nmatches:\n  - trigger: \":a\"\n    replace: a\n  - trigger: \":b\"\n    replace: b\n    snip:\n      priority: personal\n",
            ),
            ("match/_extra.yml", "snip:\n  priority: normal-2\nmatches:\n  - trigger: \":c\"\n    replace: c\n"),
            ("match/packages/p/package.yml", "matches:\n  - triggers: [\":d\", \":e\"]\n    replace: d\n"),
        ]);
        let base = dunce(dir.path());
        let (_, _, origins) = load(&base);
        let mut got: Vec<_> = origins.by_id.values().cloned().collect();
        got.sort_by(|a, b| (&a.file, a.index).cmp(&(&b.file, b.index)));
        let want = vec![
            Origin { file: "match/_extra.yml".into(), index: Some(0), priority: 14 },
            Origin { file: "match/base.yml".into(), index: Some(0), priority: NORMAL },
            // A match priority has no effect.
            Origin { file: "match/base.yml".into(), index: Some(1), priority: NORMAL },
            Origin { file: "match/packages/p/package.yml".into(), index: Some(0), priority: PACKAGE },
        ];
        assert_eq!(got, want);
        assert_eq!(origins.problems.len(), 1);
        assert_eq!(origins.problems[0].file, "match/base.yml");
        assert!(origins.problems[0].message.starts_with("match 2 sets snip.priority"), "{:?}", origins.problems);
    }

    #[test]
    fn reports_conflicts_with_the_winner_first() {
        let dir = folder(&[
            ("match/base.yml", "matches:\n  - trigger: \":x\"\n    replace: base\n  - trigger: \":y\"\n    replace: y1\n  - trigger: \":y\"\n    replace: y2\n"),
            ("match/mine.yml", "snip:\n  priority: personal\nmatches:\n  - trigger: \":x\"\n    replace: mine\n"),
            ("match/packages/p/package.yml", "matches:\n  - trigger: \":x\"\n    replace: package\n"),
        ]);
        let base = dunce(dir.path());
        let (configs, store, origins) = load(&base);
        let mut found = conflicts(configs.as_ref(), store.as_ref(), &origins);
        found.sort_by(|a, b| a.trigger.cmp(&b.trigger));
        assert_eq!(found.len(), 2);

        let x = &found[0];
        assert_eq!(x.trigger, ":x");
        assert!(!x.tie);
        let files: Vec<_> = x.matches.iter().map(|c| c.origin.as_ref().unwrap().file.as_str()).collect();
        assert_eq!(files, ["match/mine.yml", "match/base.yml", "match/packages/p/package.yml"]);

        let y = &found[1];
        assert!(y.tie);
        let indexes: Vec<_> = y.matches.iter().map(|c| c.origin.as_ref().unwrap().index).collect();
        assert_eq!(indexes, [Some(1), Some(2)]);

        let ids = x.matches.iter().map(|c| c.id).rev();
        assert_eq!(best(ids, &origins), Some(x.matches[0].id));
    }

    #[test]
    fn a_file_that_only_imports_does_not_own_the_matches() {
        let dir = folder(&[
            ("match/a.yml", "imports:
  - _b.yml
"),
            ("match/_b.yml", "matches:
  - trigger: \":b\"
    replace: b
"),
        ]);
        let (_, _, origins) = load(&dunce(dir.path()));
        let got: Vec<_> = origins.by_id.values().cloned().collect();
        assert_eq!(got, [Origin { file: "match/_b.yml".into(), index: Some(0), priority: NORMAL }]);
    }

    #[test]
    fn warns_about_a_bad_value() {
        let dir = folder(&[("match/base.yml", "snip:\n  priority: high\nmatches:\n  - trigger: \":a\"\n    replace: a\n")]);
        let base = dunce(dir.path());
        let (_, _, origins) = load(&base);
        assert_eq!(origins.problems.len(), 1);
        assert_eq!(origins.by_id.values().next().unwrap().priority, NORMAL);
    }

    /// The plain temporary path. A canonical Windows path (`\\?\`) breaks
    /// the `glob` crate that espanso uses.
    fn dunce(p: &Path) -> std::path::PathBuf {
        p.to_path_buf()
    }
}
