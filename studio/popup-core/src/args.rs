use std::path::PathBuf;

use crate::focus::{Insert, LongText};

/// Command line shared by both popups.
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
        }
    }
}

impl Args {
    pub fn parse() -> Args {
        let mut out = Args::default();
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--type" => out.type_text = true,
                "--timing" => out.timing = true,
                "--keep-open" => out.keep_open = true,
                "--exit-after-ms" => out.exit_after_ms = it.next().and_then(|v| v.parse().ok()),
                "--query" => out.query = it.next().unwrap_or_default(),
                "--base" => out.base = it.next().map(PathBuf::from),
                "--allow-injected" => out.allow_injected = true,
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
        crate::focus::set_insert(out.insert);
        out
    }
}
