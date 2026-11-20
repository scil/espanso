use std::path::PathBuf;

use crate::focus::{Insert, LongText};
use crate::settings::{self, Settings};

/// Command line shared by both popups. The resident popup (`snip`) starts
/// from its settings file ([`crate::settings`]), and each flag below wins
/// over the key of the same name there.
///
/// - `--type`: type the chosen text into the previous window. Without it,
///   the text goes to standard output (for an espanso `shell` variable).
/// - `--timing`: print timing marks to standard error.
/// - `--keep-open`: do not close when the popup loses focus (for tests).
/// - `--exit-after-ms N`: close N ms after the first frame (for benchmarks).
/// - `--query TEXT`: start with this search text.
/// - `--base DIR`: espanso folder (else `SNIP_BASE`, else the standard one).
/// - `--trigger TEXT`: characters that open the resident popup (`snip`).
///   The default is `,,`.
/// - `--log FILE`: write the resident popup's events to FILE. It records
///   no typed text.
/// - `--allow-injected`: the hook also reads injected key presses. Use it
///   for tests with a tool that types through `SendInput`.
/// - `--short-limit N`: text with at most N UTF-16 units and no line break
///   is short and is always typed. The default is 32.
/// - `--long-text paste|type`: how to insert long text. `paste` (default)
///   pastes it, hidden from clipboard history. `type` types it.
/// - `--type-chunk N`, `--type-gap-ms N`: typing sends N characters per
///   call (default 1) and waits N ms between calls (default 5).
/// - `--prefix-wait-ms N`: a typed trigger that is also the start of a longer
///   trigger waits for the next key. Space, Tab or Enter expands it, and the
///   key is not typed. With N above 0, the short trigger also expands when
///   no key comes in N ms. The default is 0: no time limit.
/// - `--tab accept|move|ignore`: what Tab does in the resident popup.
///   `accept` (default) inserts the selected row, as Enter does. `move`
///   selects the next row, and Shift+Tab the previous one. `ignore` leaves
///   Tab to the application, and the popup closes.
#[derive(Debug, Clone)]
pub struct Args {
    pub type_text: bool,
    pub timing: bool,
    pub keep_open: bool,
    pub exit_after_ms: Option<u64>,
    pub query: String,
    pub base: Option<PathBuf>,
    pub trigger: String,
    pub allow_injected: bool,
    pub log: Option<PathBuf>,
    pub insert: Insert,
    pub prefix_wait: std::time::Duration,
    pub tab: crate::hook::TabKey,
    /// espanso compatibility mode (see [`crate::settings::COMPAT_OFF`]).
    pub espanso_compat: bool,
    /// The settings keys (`trigger`, `tab`, ...) that a command line flag
    /// set. They win over the settings file.
    pub from_cli: Vec<&'static str>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            type_text: false,
            timing: false,
            keep_open: false,
            exit_after_ms: None,
            query: String::new(),
            base: None,
            trigger: ",,".to_string(),
            allow_injected: false,
            log: None,
            insert: Insert::default(),
            prefix_wait: std::time::Duration::ZERO,
            tab: crate::hook::TabKey::default(),
            espanso_compat: false,
            from_cli: Vec::new(),
        }
    }
}

impl Args {
    /// The command line over the defaults.
    pub fn parse() -> Args {
        Args::default().with_cli(std::env::args().skip(1)).install()
    }

    /// The command line over the settings file `file` (see
    /// [`crate::settings`]).
    pub fn parse_with(file: &Settings) -> Args {
        Args::from_settings(file).with_cli(std::env::args().skip(1)).install()
    }

    fn install(self) -> Args {
        crate::focus::set_insert(self.insert);
        self
    }

    /// The defaults with the values of the settings file.
    pub fn from_settings(s: &Settings) -> Args {
        let mut out = Args::default();
        if !s.trigger.is_empty() {
            out.trigger = s.trigger.clone();
        }
        out.tab = crate::hook::TabKey::parse(&s.tab).unwrap_or_default();
        out.insert.long = if s.long_text == "type" { LongText::Type } else { LongText::Paste };
        out.insert.short_limit = s.short_limit;
        out.insert.pace.chunk = s.type_chunk.max(1);
        out.insert.pace.gap = std::time::Duration::from_millis(s.type_gap_ms);
        out.prefix_wait = std::time::Duration::from_millis(s.prefix_wait_ms);
        out.log = if s.log { settings::log_path() } else { None };
        out.espanso_compat = s.espanso_compat;
        out
    }

    /// Applies the flags in `args` and records the settings keys they set.
    pub fn with_cli(mut self, args: impl IntoIterator<Item = String>) -> Args {
        let out = &mut self;
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            if let Some(key) = setting_key(&a) {
                if !out.from_cli.contains(&key) {
                    out.from_cli.push(key);
                }
            }
            match a.as_str() {
                "--type" => out.type_text = true,
                "--timing" => out.timing = true,
                "--keep-open" => out.keep_open = true,
                "--exit-after-ms" => out.exit_after_ms = it.next().and_then(|v| v.parse().ok()),
                "--query" => out.query = it.next().unwrap_or_default(),
                "--base" => out.base = it.next().map(PathBuf::from),
                "--allow-injected" => out.allow_injected = true,
                "--espanso-compat" => out.espanso_compat = true,
                "--log" => out.log = it.next().map(PathBuf::from),
                "--long-text" => match it.next().as_deref() {
                    Some("paste") => out.insert.long = LongText::Paste,
                    Some("type") => out.insert.long = LongText::Type,
                    v => eprintln!("--long-text wants paste or type, got {v:?}"),
                },
                "--short-limit" => {
                    if let Some(n) = it.next().and_then(|v| v.parse().ok()) {
                        out.insert.short_limit = n;
                    }
                }
                "--type-chunk" => {
                    if let Some(n) = it.next().and_then(|v| v.parse::<usize>().ok()) {
                        out.insert.pace.chunk = n.max(1);
                    }
                }
                "--type-gap-ms" => {
                    if let Some(n) = it.next().and_then(|v| v.parse().ok()) {
                        out.insert.pace.gap = std::time::Duration::from_millis(n);
                    }
                }
                "--prefix-wait-ms" => {
                    if let Some(n) = it.next().and_then(|v| v.parse().ok()) {
                        out.prefix_wait = std::time::Duration::from_millis(n);
                    }
                }
                "--tab" => match it.next().as_deref().and_then(crate::hook::TabKey::parse) {
                    Some(t) => out.tab = t,
                    None => eprintln!("--tab wants accept, move or ignore"),
                },
                "--trigger" => {
                    if let Some(t) = it.next().filter(|t| !t.is_empty()) {
                        out.trigger = t;
                    }
                }
                other => eprintln!("ignored argument: {other}"),
            }
        }
        self
    }
}

/// The settings key of a command line flag, if it has one.
fn setting_key(flag: &str) -> Option<&'static str> {
    Some(match flag {
        "--trigger" => "trigger",
        "--tab" => "tab",
        "--long-text" => "long-text",
        "--short-limit" => "short-limit",
        "--type-chunk" => "type-chunk",
        "--type-gap-ms" => "type-gap-ms",
        "--prefix-wait-ms" => "prefix-wait-ms",
        "--log" => "log",
        "--espanso-compat" => "espanso-compat",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn the_file_sets_the_values() {
        let s = Settings { trigger: ";;".into(), tab: "move".into(), prefix_wait_ms: 300, ..Settings::default() };
        let a = Args::from_settings(&s).with_cli(cli(&[]));
        assert_eq!(a.trigger, ";;");
        assert_eq!(a.tab, crate::hook::TabKey::Move);
        assert_eq!(a.prefix_wait, std::time::Duration::from_millis(300));
        assert!(a.from_cli.is_empty());
    }

    #[test]
    fn a_flag_wins_over_the_file_and_is_recorded() {
        let s = Settings { tab: "move".into(), ..Settings::default() };
        let a = Args::from_settings(&s).with_cli(cli(&["--tab", "ignore", "--timing"]));
        assert_eq!(a.tab, crate::hook::TabKey::Ignore);
        assert_eq!(a.from_cli, vec!["tab"]);
    }
}
