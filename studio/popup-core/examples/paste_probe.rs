//! Waits N seconds, then sends Backspaces and types a text into the focused
//! window, the way the resident popup inserts a snippet.
//! Usage: `paste_probe <seconds> <backspaces> <text> [chunk|paste] [gap-ms]`.
//! With `paste` it pastes the way `--long-text paste` does.
//! In the text, `\n` stands for a line break. A text of the form `@N` is
//! replaced by N numbered test lines.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let erase: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let raw = args.get(3).cloned().unwrap_or_else(|| "typed".into());
    let text = match raw.strip_prefix('@').and_then(|n| n.parse::<usize>().ok()) {
        Some(n) => (1..=n)
            .map(|i| format!("{i:03} the quick brown fox jumps over the lazy dog 🦊 éü"))
            .collect::<Vec<_>>()
            .join("\n"),
        None => raw.replace("\\n", "\n"),
    };
    let mut pace = popup_core::focus::Pace::default();
    if let Some(c) = args.get(4).and_then(|s| s.parse().ok()) {
        pace.chunk = c;
    }
    if let Some(g) = args.get(5).and_then(|s| s.parse().ok()) {
        pace.gap = std::time::Duration::from_millis(g);
    }
    std::thread::sleep(std::time::Duration::from_secs(secs));
    popup_core::focus::send_backspaces(erase);
    let start = std::time::Instant::now();
    if args.get(4).map(String::as_str) == Some("paste") {
        popup_core::focus::paste(&text);
    } else {
        popup_core::focus::type_keys(&text, pace);
    }
    let how = match args.get(4).map(String::as_str) {
        Some("paste") => "pasted".to_string(),
        _ => format!("typed, chunk {}, gap {:?},", pace.chunk, pace.gap),
    };
    eprintln!("{} chars {how} in {} ms", text.chars().count(), start.elapsed().as_millis());
}
