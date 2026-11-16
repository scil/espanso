//! Decides whether a match can leave espanso, for example as an input method
//! phrase. `Static` text can go anywhere. `Dynamic` text needs a renderer on
//! the other side (such as a Rime Lua script). `EspansoOnly` cannot be
//! exported.

use serde::Serialize;
use serde_norway::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Portability {
    Static,
    Dynamic,
    EspansoOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortabilityReport {
    pub level: Portability,
    pub reasons: Vec<String>,
}

const DYNAMIC_VAR_TYPES: &[&str] = &["date", "random"];
const STATIC_VAR_TYPES: &[&str] = &["echo"];

pub fn analyze(item: &Value, global_vars: &[(String, String)]) -> PortabilityReport {
    let mut only = Vec::new();
    let mut dynamic = Vec::new();

    if item.get("regex").is_some() {
        only.push("regex trigger".to_string());
    }
    if item.get("image_path").is_some() {
        only.push("image".to_string());
    }
    if item.get("form").is_some() {
        only.push("form".to_string());
    }
    for key in ["markdown", "html"] {
        if item.get(key).is_some() {
            only.push(format!("{key} rich text"));
        }
    }
    let text = ["replace", "markdown", "html", "form"]
        .iter()
        .find_map(|k| item.get(*k).and_then(Value::as_str))
        .unwrap_or("");
    if text.contains("$|$") {
        only.push("cursor position `$|$`".to_string());
    }

    let mut local = Vec::new();
    if let Some(vars) = item.get("vars").and_then(Value::as_sequence) {
        for var in vars {
            let name = var.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = var.get("type").and_then(Value::as_str).unwrap_or("?");
            local.push(name.to_string());
            if DYNAMIC_VAR_TYPES.contains(&kind) {
                dynamic.push(format!("`{name}` is a {kind} variable"));
            } else if !STATIC_VAR_TYPES.contains(&kind) {
                only.push(format!("`{name}` is a {kind} variable"));
            }
        }
    }

    for name in referenced_vars(text) {
        if local.iter().any(|l| l == &name) {
            continue;
        }
        match global_vars.iter().find(|(n, _)| n == &name) {
            Some((_, kind)) if DYNAMIC_VAR_TYPES.contains(&kind.as_str()) => {
                dynamic.push(format!("`{name}` is a global {kind} variable"));
            }
            Some((_, kind)) if STATIC_VAR_TYPES.contains(&kind.as_str()) => {}
            Some((_, kind)) => only.push(format!("`{name}` is a global {kind} variable")),
            None => dynamic.push(format!("`{name}` is defined in another file")),
        }
    }

    if !only.is_empty() {
        PortabilityReport {
            level: Portability::EspansoOnly,
            reasons: only,
        }
    } else if !dynamic.is_empty() {
        PortabilityReport {
            level: Portability::Dynamic,
            reasons: dynamic,
        }
    } else {
        PortabilityReport {
            level: Portability::Static,
            reasons: Vec::new(),
        }
    }
}

/// Names inside `{{...}}`, without filters such as `{{name.upper}}`.
fn referenced_vars(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else { break };
        let name = after[..close]
            .trim()
            .split('.')
            .next()
            .unwrap_or("")
            .to_string();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
        rest = &after[close + 2..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(yaml: &str) -> Value {
        serde_norway::from_str(yaml).unwrap()
    }

    #[test]
    fn levels() {
        let globals = vec![("today".to_string(), "date".to_string())];
        assert_eq!(
            analyze(&item("trigger: a\nreplace: 你好"), &globals).level,
            Portability::Static
        );
        assert_eq!(
            analyze(&item("trigger: a\nreplace: \"{{today}}\""), &globals).level,
            Portability::Dynamic
        );
        assert_eq!(
            analyze(
                &item("trigger: a\nreplace: \"{{o}}\"\nvars:\n  - name: o\n    type: shell"),
                &globals
            )
            .level,
            Portability::EspansoOnly
        );
        assert_eq!(
            analyze(&item("trigger: a\nreplace: \"x$|$y\""), &globals).level,
            Portability::EspansoOnly
        );
        assert_eq!(
            analyze(&item("regex: a\nreplace: b"), &globals).level,
            Portability::EspansoOnly
        );
    }
}
