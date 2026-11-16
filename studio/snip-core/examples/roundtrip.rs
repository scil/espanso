//! Checks the splice invariants on every match file in an espanso folder,
//! without writing anything:
//! - replacing an item with its own YAML gives back the exact file;
//! - deleting an item leaves one item fewer and parses;
//! - appending an item leaves one item more and parses.
//!
//! Usage: cargo run --example roundtrip -- <espanso base folder>
//! It prints file names and indexes only, never snippet text.

use snip_core::yamlfile::MatchFile;

fn main() {
    let base = std::env::args()
        .nth(1)
        .expect("usage: roundtrip <espanso base folder>");
    let root = std::path::Path::new(&base).join("match");
    let (mut files, mut items, mut failures) = (0, 0, 0);
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(true)
        .into_iter()
        .flatten()
    {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !entry.file_type().is_file() || !(ext == "yml" || ext == "yaml") {
            continue;
        }
        let rel = path.strip_prefix(&root).unwrap().display().to_string();
        let text = std::fs::read_to_string(path).unwrap();
        let file = match MatchFile::parse(&text) {
            Ok(f) => f,
            Err(e) => {
                println!("PARSE  {rel}: {e}");
                failures += 1;
                continue;
            }
        };
        files += 1;
        for (i, item) in file.items.iter().enumerate() {
            items += 1;
            let own = item.editable(&file.text);
            match file.replace(i, &own) {
                Ok(out) if out == file.text => {}
                Ok(_) => {
                    println!("REPLACE {rel}#{i}: output differs");
                    failures += 1;
                }
                Err(e) => {
                    println!("REPLACE {rel}#{i}: {e}");
                    failures += 1;
                }
            }
            match file
                .delete(i)
                .map(|t| MatchFile::parse(&t).map(|f| f.items.len()))
            {
                Ok(Ok(n)) if n + 1 == file.items.len() => {}
                other => {
                    println!("DELETE {rel}#{i}: {other:?}");
                    failures += 1;
                }
            }
        }
        if !matches!(file.layout, snip_core::yamlfile::Layout::Unsupported(_)) {
            match file
                .append("trigger: \":zz-test\"\nreplace: \"测试\"")
                .map(|t| MatchFile::parse(&t).map(|f| f.items.len()))
            {
                Ok(Ok(n)) if n == file.items.len() + 1 => {}
                other => {
                    println!("APPEND {rel}: {other:?}");
                    failures += 1;
                }
            }
        } else {
            println!("SKIP   {rel}: {:?}", file.layout);
        }
    }
    println!("files: {files}, items: {items}, failures: {failures}");
    if failures > 0 {
        std::process::exit(1);
    }
}
