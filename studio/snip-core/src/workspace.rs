//! The espanso configuration folder seen as a set of files and snippets.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_norway::Value;

use crate::error::{Problem, Result, SnipError};
use crate::portability::{self, PortabilityReport};
use crate::priority::{self, Conflict, Origins, Overlap};
use crate::yamlfile::{hash, Item, MatchFile};

/// Environment variable that overrides the espanso base folder.
pub const BASE_ENV: &str = "SNIP_BASE";

#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub base: String,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub path: String,
    /// `match` or `config`.
    pub kind: &'static str,
    pub read_only: bool,
    pub hash: String,
    pub size: u64,
    /// Number of matches, for match files that parse.
    pub matches: Option<usize>,
    /// Parse error, for files that do not parse.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snippet {
    pub file: String,
    pub index: usize,
    pub line: usize,
    /// Hash of the item's text. Pass it back to update or delete the item.
    pub hash: String,
    pub triggers: Vec<String>,
    pub regex: Option<String>,
    pub label: Option<String>,
    /// `text`, `markdown`, `html`, `image` or `form`.
    pub kind: &'static str,
    /// The replacement text, shortened.
    pub preview: String,
    pub word: bool,
    pub search_terms: Vec<String>,
    pub read_only: bool,
    pub portability: PortabilityReport,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnippetDetail {
    #[serde(flatten)]
    pub snippet: Snippet,
    /// The item's YAML without the list indent, comments included.
    pub yaml: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnippetList {
    pub snippets: Vec<Snippet>,
    /// Match files that could not be read as snippets.
    pub file_errors: Vec<Problem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileText {
    pub path: String,
    pub text: String,
    pub hash: String,
    pub read_only: bool,
}

pub struct Workspace {
    base: PathBuf,
}

impl Workspace {
    /// Opens `base`, or the folder named by `SNIP_BASE`, or espanso's default
    /// folder (`%APPDATA%\espanso` on Windows, `~/.config/espanso` on Linux,
    /// `~/Library/Application Support/espanso` on macOS).
    pub fn open(base: Option<PathBuf>) -> Result<Self> {
        let base = base
            .or_else(|| std::env::var_os(BASE_ENV).map(PathBuf::from))
            .or_else(|| dirs::config_dir().map(|d| d.join("espanso")))
            .ok_or_else(|| SnipError::NotFound("espanso base folder".into()))?;
        if !base.join("match").is_dir() && !base.join("config").is_dir() {
            return Err(SnipError::NotFound(format!(
                "{} has no `match` or `config` folder",
                base.display()
            )));
        }
        Ok(Self { base })
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn info(&self) -> Info {
        Info {
            base: self.base.display().to_string(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    pub fn list_files(&self) -> Result<Vec<FileEntry>> {
        let mut out = Vec::new();
        for (kind, dir) in [("config", "config"), ("match", "match")] {
            for rel in self.yaml_files(dir) {
                let abs = self.base.join(&rel);
                let text = read_text(&abs)?;
                let (matches, error) = if kind == "match" {
                    match MatchFile::parse(&text) {
                        Ok(f) => (Some(f.items.len()), None),
                        Err(e) => (None, Some(e.to_string())),
                    }
                } else {
                    match serde_norway::from_str::<Value>(&text) {
                        Ok(_) => (None, None),
                        Err(e) => (None, Some(e.to_string())),
                    }
                };
                out.push(FileEntry {
                    read_only: is_read_only(&rel),
                    hash: hash(&text),
                    size: text.len() as u64,
                    path: rel,
                    kind,
                    matches,
                    error,
                });
            }
        }
        Ok(out)
    }

    pub fn list_snippets(&self, query: Option<&str>, file: Option<&str>) -> Result<SnippetList> {
        let needle = query.map(str::to_lowercase).filter(|q| !q.is_empty());
        let files = match file {
            Some(f) => vec![self.check_path(f)?],
            None => self.yaml_files("match"),
        };
        let mut list = SnippetList {
            snippets: Vec::new(),
            file_errors: Vec::new(),
        };
        for rel in files {
            let text = read_text(&self.base.join(&rel))?;
            let parsed = match MatchFile::parse(&text) {
                Ok(p) => p,
                Err(e) => {
                    list.file_errors.push(Problem {
                        file: rel.clone(),
                        level: "error".into(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            for (index, item) in parsed.items.iter().enumerate() {
                let snippet = summarize(&rel, index, item, &parsed);
                if needle
                    .as_deref()
                    .is_none_or(|q| snippet_matches(&snippet, q))
                {
                    list.snippets.push(snippet);
                }
            }
        }
        Ok(list)
    }

    pub fn get_snippet(&self, file: &str, index: usize) -> Result<SnippetDetail> {
        let rel = self.check_path(file)?;
        let parsed = MatchFile::parse(&read_text(&self.base.join(&rel))?)?;
        let item = parsed.item(index)?;
        Ok(SnippetDetail {
            snippet: summarize(&rel, index, item, &parsed),
            yaml: item.editable(&parsed.text),
        })
    }

    /// Replaces one match. `hash` is the value from `list_snippets` or
    /// `get_snippet`; a different value means the item changed on disk.
    pub fn update_snippet(
        &self,
        file: &str,
        index: usize,
        hash: &str,
        yaml: &str,
    ) -> Result<SnippetDetail> {
        let rel = self.writable_match_path(file)?;
        let parsed = MatchFile::parse(&read_text(&self.base.join(&rel))?)?;
        parsed.check_hash(index, hash)?;
        let new_text = parsed.replace(index, yaml)?;
        self.commit_match(&rel, &parsed, &new_text)?;
        self.get_snippet(&rel, index)
    }

    /// Appends a match to `file`. The file is created when it does not exist.
    pub fn create_snippet(&self, file: &str, yaml: &str) -> Result<SnippetDetail> {
        let rel = self.writable_match_path(file)?;
        let abs = self.base.join(&rel);
        let parsed = if abs.exists() {
            MatchFile::parse(&read_text(&abs)?)?
        } else {
            MatchFile::parse("")?
        };
        let new_text = parsed.append(yaml)?;
        self.commit_match(&rel, &parsed, &new_text)?;
        self.get_snippet(&rel, parsed.items.len())
    }

    pub fn delete_snippet(&self, file: &str, index: usize, hash: &str) -> Result<()> {
        let rel = self.writable_match_path(file)?;
        let parsed = MatchFile::parse(&read_text(&self.base.join(&rel))?)?;
        parsed.check_hash(index, hash)?;
        let new_text = parsed.delete(index)?;
        self.commit_match(&rel, &parsed, &new_text)
    }

    pub fn read_file(&self, file: &str) -> Result<FileText> {
        let rel = self.check_path(file)?;
        let text = read_text(&self.base.join(&rel))?;
        Ok(FileText {
            hash: hash(&text),
            read_only: is_read_only(&rel),
            path: rel,
            text,
        })
    }

    /// Writes a whole file. `expected_hash` is the hash from `read_file`, or
    /// `None` to create a new file.
    pub fn write_file(
        &self,
        file: &str,
        text: &str,
        expected_hash: Option<&str>,
    ) -> Result<FileText> {
        let rel = self.check_path(file)?;
        if is_read_only(&rel) {
            return Err(SnipError::ReadOnly(rel));
        }
        let abs = self.base.join(&rel);
        let old = if abs.exists() {
            Some(read_text(&abs)?)
        } else {
            None
        };
        match (&old, expected_hash) {
            (Some(old), Some(expected)) if hash(old) != expected => {
                return Err(SnipError::Conflict {
                    expected: expected.into(),
                    actual: hash(old),
                });
            }
            (Some(old), None) => {
                return Err(SnipError::Conflict {
                    expected: "(new file)".into(),
                    actual: hash(old),
                });
            }
            (None, Some(expected)) => {
                return Err(SnipError::NotFound(format!(
                    "{rel} (expected hash {expected})"
                )))
            }
            _ => {}
        }
        if !rel.starts_with("config/") {
            MatchFile::parse(text)?;
            self.check_new_problems(&rel, old.as_deref().unwrap_or(""), text)?;
        } else {
            serde_norway::from_str::<Value>(text)
                .map_err(|e| SnipError::InvalidYaml(e.to_string()))?;
        }
        write_atomic(&abs, text)?;
        self.read_file(&rel)
    }

    /// Runs espanso's own loader on the whole folder, then reports bad
    /// `snip.priority` values and triggers that several matches share.
    pub fn validate(&self) -> Result<Vec<Problem>> {
        let (configs, store, errors) = espanso_config::load(&self.base)?;
        let mut problems: Vec<Problem> = errors
            .into_iter()
            .flat_map(|set| {
                let file = self.relative(&set.file);
                set.errors.into_iter().map(move |r| Problem {
                    file: file.clone(),
                    level: level_name(&r.level),
                    message: format!("{:#}", r.error),
                })
            })
            .collect();
        let origins = Origins::load(&self.base, store.as_ref());
        problems.extend(origins.problems.iter().cloned());
        for c in priority::conflicts(configs.as_ref(), store.as_ref(), &origins) {
            problems.push(conflict_problem(&c));
        }
        Ok(problems)
    }

    /// The global variables that espanso loads, as (name, type), sorted by
    /// name, each name once. espanso's loader follows `imports`, `includes`
    /// and `extra_includes`, so a file outside `match/` counts too. The
    /// union covers every configuration, the per-application ones too.
    pub fn global_vars(&self) -> Result<Vec<(String, String)>> {
        let (configs, store, _) = espanso_config::load(&self.base)?;
        let mut out: Vec<(String, String)> = configs
            .configs()
            .iter()
            .flat_map(|c| store.query(c.match_paths()).global_vars)
            .map(|v| (v.name.clone(), v.var_type.clone()))
            .collect();
        out.sort();
        out.dedup_by(|a, b| a.0 == b.0);
        Ok(out)
    }

    /// Triggers that several active matches share, with the winner first.
    pub fn conflicts(&self) -> Result<Vec<Conflict>> {
        let (configs, store, _) = espanso_config::load(&self.base)?;
        let origins = Origins::load(&self.base, store.as_ref());
        Ok(priority::conflicts(configs.as_ref(), store.as_ref(), &origins))
    }

    /// Pairs of triggers where one is the start of the other. With `own`,
    /// pairs that only involve files under `match/packages/` are left out.
    pub fn overlaps(&self, own: bool) -> Result<Vec<Overlap>> {
        let (configs, store, _) = espanso_config::load(&self.base)?;
        let origins = Origins::load(&self.base, store.as_ref());
        let mut out = priority::overlaps(configs.as_ref(), store.as_ref(), &origins);
        if own {
            let in_package =
                |c: &priority::Contender| c.origin.as_ref().is_some_and(|o| o.file.starts_with("match/packages/"));
            out.retain(|o| !o.short_matches.iter().chain(&o.long_matches).all(in_package));
        }
        Ok(out)
    }

    fn commit_match(&self, rel: &str, parsed: &MatchFile, new_text: &str) -> Result<()> {
        MatchFile::parse(new_text)?;
        self.check_new_problems(rel, &parsed.text, new_text)?;
        write_atomic(&self.base.join(rel), &parsed.render(new_text))
    }

    /// Fails when espanso's loader reports errors for `new` that it did not
    /// report for `old`. Both versions load from the same temporary path, so
    /// errors about imports that only resolve in the real folder cancel out.
    fn check_new_problems(&self, rel: &str, old: &str, new: &str) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let name = Path::new(rel)
            .file_name()
            .map_or_else(|| "file.yml".into(), |n| n.to_owned());
        let path = dir.path().join(name);
        let load = |text: &str| -> Result<Vec<(String, String)>> {
            fs::write(&path, text)?;
            let (_, errors) =
                espanso_config::matches::store::load(&[path.to_string_lossy().into_owned()]);
            Ok(errors
                .into_iter()
                .flat_map(|set| set.errors)
                .map(|r| (level_name(&r.level), format!("{:#}", r.error)))
                .collect())
        };
        let before = load(old)?;
        let after = load(new)?;
        let added: Vec<Problem> = after
            .into_iter()
            .filter(|p| !before.contains(p))
            .map(|(level, message)| Problem {
                file: rel.to_string(),
                level,
                message,
            })
            .collect();
        if added.is_empty() {
            Ok(())
        } else {
            Err(SnipError::Validation(added))
        }
    }

    fn writable_match_path(&self, file: &str) -> Result<String> {
        let rel = self.check_path(file)?;
        if !rel.starts_with("match/") {
            return Err(SnipError::InvalidInput(format!(
                "{rel} is not under `match/`"
            )));
        }
        if is_read_only(&rel) {
            return Err(SnipError::ReadOnly(rel));
        }
        Ok(rel)
    }

    /// Accepts only relative `.yml`/`.yaml` paths inside the base folder:
    /// under `match/` or `config/`, or a file that espanso loads through
    /// `imports`, `includes` or `extra_includes` (for example a
    /// `_global_vars.yml` in the folder root).
    fn check_path(&self, file: &str) -> Result<String> {
        let rel = file.replace('\\', "/");
        let path = Path::new(&rel);
        let safe = path.components().all(|c| matches!(c, Component::Normal(_)));
        let ext_ok = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yml" | "yaml")
        );
        let top_ok = || rel.starts_with("match/") || rel.starts_with("config/") || self.is_loaded(&rel);
        if !safe || !ext_ok || !top_ok() {
            return Err(SnipError::InvalidInput(format!(
                "`{file}` must be a relative .yml path under match/ or config/, or a file that espanso loads"
            )));
        }
        Ok(rel)
    }

    /// The match files that espanso loads for any configuration, as
    /// canonical paths.
    fn loaded_files(&self) -> Result<Vec<PathBuf>> {
        let (configs, _, _) = espanso_config::load(&self.base)?;
        let mut out: Vec<PathBuf> = configs
            .configs()
            .iter()
            .flat_map(|c| c.match_paths().to_vec())
            .filter_map(|p| Path::new(&p).canonicalize().ok())
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// True when espanso loads the file at `rel`.
    fn is_loaded(&self, rel: &str) -> bool {
        let Ok(abs) = self.base.join(rel).canonicalize() else { return false };
        self.loaded_files().is_ok_and(|files| files.contains(&abs))
    }

    /// The file that holds the global variables: the first loaded file with
    /// a `global_vars` list, as a path relative to the base folder. Files
    /// under `match/packages/` do not count, because snip does not write
    /// them.
    pub fn global_vars_file(&self) -> Result<Option<String>> {
        let base = self.base.canonicalize()?;
        for abs in self.loaded_files()? {
            let Ok(rel) = abs.strip_prefix(&base) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if is_read_only(&rel) {
                continue;
            }
            let Ok(text) = read_text(&abs) else { continue };
            if MatchFile::parse(&text).is_ok_and(|m| !m.global_vars.is_empty()) {
                return Ok(Some(rel));
            }
        }
        Ok(None)
    }

    /// Adds `var` (one variable as YAML, with `name` and `type`) to the
    /// `global_vars` list of `global_vars_file()`, or of `fallback` when no
    /// loaded file has that list. The rest of the file, comments too, stays
    /// as it is. Returns the file that changed.
    pub fn add_global_var(&self, var: &Value, fallback: &str) -> Result<String> {
        let name = var
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| SnipError::InvalidInput("the variable has no name".into()))?;
        if self.global_vars()?.iter().any(|(n, _)| n == name) {
            return Err(SnipError::InvalidInput(format!("a global variable named `{name}` already exists")));
        }
        let file = match self.global_vars_file()? {
            Some(f) => f,
            None => fallback.to_string(),
        };
        let current = self.read_file(&file)?;
        let entry = serde_norway::to_string(var).map_err(|e| SnipError::InvalidYaml(e.to_string()))?;
        let text = with_global_var(&current.text, &entry);
        self.write_file(&file, &text, Some(&current.hash))?;
        Ok(file)
    }

    fn yaml_files(&self, dir: &str) -> Vec<String> {
        let root = self.base.join(dir);
        let mut out: Vec<String> = walkdir::WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                let name = e.file_name().to_string_lossy();
                // `_manifest.yml` describes a package; it holds no matches.
                !name.starts_with('.')
                    && name != "_manifest.yml"
                    && (name.ends_with(".yml") || name.ends_with(".yaml"))
            })
            .map(|e| self.relative(e.path()))
            .collect();
        out.sort();
        out
    }

    fn relative(&self, path: &Path) -> String {
        priority::relative(&self.base, path)
    }
}

/// A conflict as a problem, always `info`: a tie shows a list, and a
/// conflict that priority settles expands the winner.
fn conflict_problem(c: &Conflict) -> Problem {
    let place = |m: &priority::Contender| match &m.origin {
        Some(o) => match o.index {
            Some(i) => format!("{} match {} (priority {})", o.file, i + 1, o.priority),
            None => format!("{} (priority {})", o.file, o.priority),
        },
        None => format!("match id {}", m.id),
    };
    let winner = &c.matches[0];
    let others: Vec<String> = c.matches[1..].iter().map(place).collect();
    let message = if c.tie {
        format!(
            "trigger {:?} has {} matches with the same priority: {} and {}. espanso and snip show a list to pick one. Set snip.priority to expand one at once.",
            c.trigger,
            c.matches.len(),
            place(winner),
            others.join(", ")
        )
    } else {
        format!("trigger {:?}: {} wins over {}", c.trigger, place(winner), others.join(", "))
    };
    Problem {
        file: winner.origin.as_ref().map_or_else(String::new, |o| o.file.clone()),
        level: "info".into(),
        message,
    }
}

fn level_name(level: &espanso_config::error::ErrorLevel) -> String {
    match level {
        espanso_config::error::ErrorLevel::Error => "error".into(),
        espanso_config::error::ErrorLevel::Warning => "warning".into(),
    }
}

/// `text` with `entry` (one variable as YAML, without the leading `- `)
/// added at the end of its top-level `global_vars:` list. A file without
/// that list gets one at its end.
fn with_global_var(text: &str, entry: &str) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = lines.iter().position(|l| l.trim_end().starts_with("global_vars:"));
    // The new entry takes the indent of the list's first entry.
    let indent = start
        .and_then(|s| lines[s + 1..].iter().find(|l| l.trim_start().starts_with("- ")))
        .map_or(2, |l| l.len() - l.trim_start().len());
    let pad = " ".repeat(indent);
    let item: String = entry
        .trim_end()
        .lines()
        .enumerate()
        .map(|(i, l)| if i == 0 { format!("{pad}- {l}{nl}") } else { format!("{pad}  {l}{nl}") })
        .collect();
    let Some(start) = start else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(nl);
        }
        out.push_str("global_vars:");
        out.push_str(nl);
        out.push_str(&item);
        return out;
    };
    // The list ends before the next top-level key. Blank lines and
    // comments at the end of the list stay after the new entry.
    let mut end = start + 1;
    let mut last = start + 1;
    while end < lines.len() {
        let l = lines[end];
        let t = l.trim_start();
        let top = !l.starts_with([' ', '\t']) && !t.is_empty() && !t.starts_with('#') && !t.starts_with('-');
        if top {
            break;
        }
        if !t.trim().is_empty() && !t.starts_with('#') {
            last = end + 1;
        }
        end += 1;
    }
    let mut out: String = lines[..last].concat();
    if !out.ends_with('\n') {
        out.push_str(nl);
    }
    out.push_str(&item);
    out.push_str(&lines[last..].concat());
    out
}

/// Files under `match/packages/` belong to the espanso package manager.
fn is_read_only(rel: &str) -> bool {
    rel.starts_with("match/packages/")
}

fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SnipError::NotFound(path.display().to_string()),
        _ => SnipError::Io(e),
    })
}

/// Writes through a temporary sibling and a rename, so espanso never reads a
/// half-written file. The `.tmp` name is ignored by espanso's file watcher.
/// Symbolic links are written in place to keep the link.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let is_link = fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_link {
        fs::write(path, text)?;
        return Ok(());
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".snipcore.tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn summarize(file: &str, index: usize, item: &Item, parsed: &MatchFile) -> Snippet {
    let v = &item.value;
    let s = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let mut triggers = Vec::new();
    if let Some(t) = s("trigger") {
        triggers.push(t);
    }
    if let Some(list) = v.get("triggers").and_then(Value::as_sequence) {
        triggers.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
    }
    let (kind, body) = if let Some(t) = s("replace") {
        ("text", t)
    } else if let Some(t) = s("markdown") {
        ("markdown", t)
    } else if let Some(t) = s("html") {
        ("html", t)
    } else if let Some(t) = s("image_path") {
        ("image", t)
    } else if let Some(t) = s("form") {
        ("form", t)
    } else {
        ("text", String::new())
    };
    let search_terms = v
        .get("search_terms")
        .and_then(Value::as_sequence)
        .map(|l| {
            l.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Snippet {
        file: file.to_string(),
        index,
        line: item.line,
        hash: hash(item.block(&parsed.text)),
        triggers,
        regex: s("regex"),
        label: s("label"),
        kind,
        preview: body.chars().take(200).collect(),
        word: v.get("word").and_then(Value::as_bool).unwrap_or(false),
        search_terms,
        read_only: is_read_only(file),
        portability: portability::analyze(v, &parsed.global_vars),
    }
}

fn snippet_matches(s: &Snippet, needle: &str) -> bool {
    let hit = |t: &str| t.to_lowercase().contains(needle);
    s.triggers.iter().any(|t| hit(t))
        || s.regex.as_deref().is_some_and(hit)
        || s.label.as_deref().is_some_and(hit)
        || hit(&s.preview)
        || s.search_terms.iter().any(|t| hit(t))
        || hit(&s.file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_vars_come_from_files_that_the_config_includes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("config")).unwrap();
        fs::create_dir_all(dir.path().join("match")).unwrap();
        fs::write(dir.path().join("config/default.yml"), "extra_includes:\n  - \"../_g.yml\"\n").unwrap();
        // The file is outside `match/`, so only the include loads it.
        fs::write(
            dir.path().join("_g.yml"),
            "global_vars:\n  - name: who\n    type: echo\n    params:\n      echo: me\n  - name: paste\n    type: clipboard\n",
        )
        .unwrap();
        fs::write(dir.path().join("match/base.yml"), "matches:\n  - trigger: \":a\"\n    replace: \"{{who}}\"\n").unwrap();
        let ws = Workspace::open(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            ws.global_vars().unwrap(),
            vec![("paste".to_string(), "clipboard".to_string()), ("who".to_string(), "echo".to_string())]
        );
    }

    #[test]
    fn a_new_global_variable_goes_to_the_end_of_the_list() {
        let text = "# top\nglobal_vars:\n  - name: a\n    type: echo # note\n\n# tail\nmatches:\n  - trigger: x\n";
        assert_eq!(
            with_global_var(text, "name: b\ntype: date\n"),
            "# top\nglobal_vars:\n  - name: a\n    type: echo # note\n  - name: b\n    type: date\n\n# tail\nmatches:\n  - trigger: x\n"
        );
        assert_eq!(
            with_global_var("global_vars:\n- name: a\n", "name: b\n"),
            "global_vars:\n- name: a\n- name: b\n"
        );
        assert_eq!(with_global_var("matches: []", "name: b\n"), "matches: []\nglobal_vars:\n  - name: b\n");
    }

    #[test]
    fn a_variable_moves_into_the_loaded_global_vars_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("config")).unwrap();
        fs::create_dir_all(dir.path().join("match")).unwrap();
        fs::write(dir.path().join("config/default.yml"), "extra_includes:\n  - \"../_g.yml\"\n").unwrap();
        fs::write(dir.path().join("_g.yml"), "global_vars:\n  - name: who\n    type: echo\n    params:\n      echo: me\n")
            .unwrap();
        fs::write(dir.path().join("match/base.yml"), "matches:\n  - trigger: \":a\"\n    replace: \"x\"\n").unwrap();
        fs::write(dir.path().join("stray.yml"), "matches: []\n").unwrap();
        let ws = Workspace::open(Some(dir.path().to_path_buf())).unwrap();

        // A loaded file outside match/ is readable. A file that espanso
        // does not load is not.
        assert!(ws.read_file("_g.yml").is_ok());
        assert!(ws.read_file("stray.yml").is_err());
        assert_eq!(ws.global_vars_file().unwrap().as_deref(), Some("_g.yml"));

        let var: Value = serde_norway::from_str("name: sig\ntype: echo\nparams:\n  echo: Ray\n").unwrap();
        assert_eq!(ws.add_global_var(&var, "match/base.yml").unwrap(), "_g.yml");
        let names: Vec<String> = ws.global_vars().unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["sig".to_string(), "who".to_string()]);
        // The same name twice is refused.
        assert!(ws.add_global_var(&var, "match/base.yml").is_err());
    }
}
