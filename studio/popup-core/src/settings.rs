//! The settings file of the resident popup (`snip`):
//! `%LOCALAPPDATA%\snip\settings.toml`. The settings window writes it.
//!
//! Each key has the name of its command line flag. A flag on the command
//! line wins over the file. A missing file, a missing key or an unknown
//! value gives the default.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    /// The characters that open the popup.
    pub trigger: String,
    /// What Tab does in the popup: `accept`, `move` or `ignore`.
    pub tab: String,
    /// How long text goes in: `paste` or `type`.
    pub long_text: String,
    /// Text with at most this many UTF-16 units and no line break is typed.
    pub short_limit: usize,
    /// Typing sends this many characters per call.
    pub type_chunk: usize,
    /// Typing waits this many milliseconds between calls.
    pub type_gap_ms: u64,
    /// A prefix trigger expands after this many milliseconds without a key.
    /// 0 means no time limit.
    pub prefix_wait_ms: u64,
    /// Write events to `events.log` next to this file.
    pub log: bool,
    /// espanso compatibility mode: snip expands as espanso does, and the
    /// features that espanso lacks are off. See [`COMPAT_OFF`].
    pub espanso_compat: bool,
}

/// What espanso compatibility mode turns off, one line each. The settings
/// panel shows this list.
pub const COMPAT_OFF: &[&str] = &[
    "Prefix triggers do not wait: a trigger that starts a longer trigger expands at once.",
    "snip.priority is ignored: all matches of a shared trigger go into the pick list.",
    "snip.pick is ignored: a snippet always inserts its whole text.",
    "No paragraph pick (Caps Lock, Ctrl+P) and no text pick (Ctrl+Caps Lock). Caps Lock works as usual.",
];

impl Default for Settings {
    fn default() -> Self {
        Settings {
            trigger: ",,".into(),
            tab: "accept".into(),
            long_text: "paste".into(),
            short_limit: 32,
            type_chunk: 1,
            type_gap_ms: 5,
            prefix_wait_ms: 0,
            log: false,
            espanso_compat: false,
        }
    }
}

/// The folder of the settings file and the default log:
/// `%LOCALAPPDATA%\snip`. It asks Windows for the folder and does not read
/// the `LOCALAPPDATA` variable, because a process that WMI starts (as
/// `snip-rebuild.ps1` does) can lack the variable.
pub fn folder() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("snip"))
}

/// `%LOCALAPPDATA%\snip\settings.toml`.
pub fn path() -> Option<PathBuf> {
    folder().map(|d| d.join("settings.toml"))
}

/// `%LOCALAPPDATA%\snip\events.log`, the log that `log = true` writes.
pub fn log_path() -> Option<PathBuf> {
    folder().map(|d| d.join("events.log"))
}

impl Settings {
    /// Reads the settings file. A missing or broken file gives the
    /// defaults. A broken file also prints the error.
    pub fn load() -> Settings {
        let Some(path) = path() else { return Settings::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => Settings::parse(&text).unwrap_or_else(|e| {
                eprintln!("{}: {e}", path.display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    pub fn parse(text: &str) -> Result<Settings, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    pub fn to_text(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// Writes the settings file. It writes a temporary file first and then
    /// renames it, so a reader never sees half a file.
    pub fn save(&self) -> Result<(), String> {
        let path = path().ok_or("the local application data folder is not known")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_text()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_key_gets_its_default() {
        let s = Settings::parse("trigger = \";;\"\n").unwrap();
        assert_eq!(s.trigger, ";;");
        assert_eq!(s.tab, "accept");
        assert_eq!(s.short_limit, 32);
    }

    #[test]
    fn the_text_reads_back_the_same() {
        let s = Settings { tab: "move".into(), prefix_wait_ms: 300, log: true, espanso_compat: true, ..Settings::default() };
        assert_eq!(Settings::parse(&s.to_text()).unwrap(), s);
    }

    #[test]
    fn a_key_with_a_wrong_type_is_an_error() {
        assert!(Settings::parse("short-limit = \"many\"\n").is_err());
    }
}
