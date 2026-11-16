//! The espanso configuration folder seen as a set of files and snippets.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_norway::Value;

use crate::error::{Problem, Result, SnipError};
use crate::portability::{self, PortabilityReport};
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
        if rel.starts_with("match/") {
            MatchFile::parse(text)?;
            self.check_new_problems(&rel, old.as_deref().unwrap_or(""), text)?;
        } else {
            serde_norway::from_str::<Value>(text)
                .map_err(|e| SnipError::InvalidYaml(e.to_string()))?;
        }
        write_atomic(&abs, text)?;
        self.read_file(&rel)
    }

    /// Runs espanso's own loader on the whole folder.
    pub fn validate(&self) -> Result<Vec<Problem>> {
        let (_, _, errors) = espanso_config::load(&self.base)?;
        Ok(errors
            .into_iter()
            .flat_map(|set| {
                let file = self.relative(&set.file);
                set.errors.into_iter().map(move |r| Problem {
                    file: file.clone(),
                    level: level_name(&r.level),
                    message: format!("{:#}", r.error),
                })
            })
            .collect())
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

    /// Accepts only relative `.yml`/`.yaml` paths under `match/` or `config/`.
    fn check_path(&self, file: &str) -> Result<String> {
        let rel = file.replace('\\', "/");
        let path = Path::new(&rel);
        let safe = path.components().all(|c| matches!(c, Component::Normal(_)));
        let top_ok = rel.starts_with("match/") || rel.starts_with("config/");
        let ext_ok = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yml" | "yaml")
        );
        if !safe || !top_ok || !ext_ok {
            return Err(SnipError::InvalidInput(format!(
                "`{file}` must be a relative .yml path under match/ or config/"
            )));
        }
        Ok(rel)
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
        let rel = path.strip_prefix(&self.base).unwrap_or(path);
        rel.to_string_lossy().replace('\\', "/")
    }
}

fn level_name(level: &espanso_config::error::ErrorLevel) -> String {
    match level {
        espanso_config::error::ErrorLevel::Error => "error".into(),
        espanso_config::error::ErrorLevel::Warning => "warning".into(),
    }
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
