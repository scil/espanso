//! Prints the first matches of a query, in popup order.
//! Usage: `filter_probe <query> [trigger prefix]`.
fn main() {
    let q = std::env::args().nth(1).unwrap_or_default();
    let prefix = std::env::args().nth(2).unwrap_or_default();
    let s = popup_core::data::Snippets::load(None).expect("load snippets");
    for &i in s.filter(&q, &prefix).iter().take(3) {
        let e = &s.entries[i];
        let text = s.text_for(e).unwrap_or_else(|err| format!("<{err}>"));
        println!("{} | {} | {:?}", e.trigger, e.label, text.chars().take(40).collect::<String>());
    }
}
