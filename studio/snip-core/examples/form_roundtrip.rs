//! Reads every match of an espanso folder into the editor form and writes
//! the unchanged form back. The text must stay the same. Writes nothing.
//!
//! cargo run -p snip-core --example form_roundtrip -- <espanso base folder>
use snip_core::matchedit::MatchForm;

fn main() {
    let base = std::env::args().nth(1).map(Into::into);
    let ws = snip_core::Workspace::open(base).expect("open folder");
    let list = ws.list_snippets(None, None).expect("list");
    let (mut ok, mut refused, mut changed) = (0, 0, 0);
    for s in &list.snippets {
        let d = ws.get_snippet(&s.file, s.index).expect("get");
        match MatchForm::from_yaml(&d.yaml).and_then(|f| f.apply(&d.yaml)) {
            Ok(out) if out == d.yaml.trim_end() => ok += 1,
            Ok(out) => {
                changed += 1;
                if changed <= 5 {
                    println!("CHANGED {}#{}\n--- before\n{}\n--- after\n{}\n", s.file, s.index, d.yaml, out);
                }
            }
            Err(e) => {
                refused += 1;
                if refused <= 10 {
                    println!("REFUSED {}#{}: {e}", s.file, s.index);
                }
            }
        }
    }
    println!("{} matches: {ok} unchanged, {changed} changed, {refused} refused", list.snippets.len());
}
