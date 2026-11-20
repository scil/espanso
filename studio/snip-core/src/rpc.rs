//! JSON-RPC 2.0 over newline-delimited JSON. Each request is one line on the
//! input; each response is one line on the output. Go and web frontends use
//! this instead of linking the Rust crate.

use std::io::{BufRead, Write};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::SnipError;
use crate::workspace::Workspace;

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Serves requests until the input closes.
pub fn serve(ws: &Workspace, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = handle_line(ws, &line);
        writeln!(output, "{response}")?;
        output.flush()?;
    }
    Ok(())
}

pub fn handle_line(ws: &Workspace, line: &str) -> Value {
    let request: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
                Value::Null,
            )
        }
    };
    match dispatch(ws, &request.method, &request.params) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": request.id, "result": result }),
        Err(Failure::Method) => error_response(
            request.id,
            -32601,
            &format!("unknown method `{}`", request.method),
            Value::Null,
        ),
        Err(Failure::Params(msg)) => error_response(request.id, -32602, &msg, Value::Null),
        Err(Failure::Snip(e)) => error_response(request.id, e.code(), &e.to_string(), e.data()),
    }
}

enum Failure {
    Method,
    Params(String),
    Snip(SnipError),
}

impl From<SnipError> for Failure {
    fn from(e: SnipError) -> Self {
        Failure::Snip(e)
    }
}

/// The method names and parameters, one per line:
///
/// - `info` {}
/// - `files.list` {}
/// - `files.read` {path}
/// - `files.write` {path, text, hash?}
/// - `snippets.list` {query?, file?}
/// - `snippets.get` {file, index}
/// - `snippets.create` {file, yaml}
/// - `snippets.update` {file, index, hash, yaml}
/// - `snippets.delete` {file, index, hash}
/// - `validate` {}
/// - `overlaps` {own?}
pub const METHODS: &[&str] = &[
    "info",
    "files.list",
    "files.read",
    "files.write",
    "snippets.list",
    "snippets.get",
    "snippets.create",
    "snippets.update",
    "snippets.delete",
    "validate",
    "overlaps",
];

fn dispatch(ws: &Workspace, method: &str, p: &Value) -> Result<Value, Failure> {
    let out = match method {
        "info" => json!(ws.info()),
        "files.list" => json!(ws.list_files()?),
        "files.read" => json!(ws.read_file(str_param(p, "path")?)?),
        "files.write" => json!(ws.write_file(
            str_param(p, "path")?,
            str_param(p, "text")?,
            opt_str(p, "hash")
        )?),
        "snippets.list" => json!(ws.list_snippets(opt_str(p, "query"), opt_str(p, "file"))?),
        "snippets.get" => json!(ws.get_snippet(str_param(p, "file")?, index_param(p)?)?),
        "snippets.create" => {
            json!(ws.create_snippet(str_param(p, "file")?, str_param(p, "yaml")?)?)
        }
        "snippets.update" => json!(ws.update_snippet(
            str_param(p, "file")?,
            index_param(p)?,
            str_param(p, "hash")?,
            str_param(p, "yaml")?
        )?),
        "snippets.delete" => {
            ws.delete_snippet(
                str_param(p, "file")?,
                index_param(p)?,
                str_param(p, "hash")?,
            )?;
            json!({ "deleted": true })
        }
        "validate" => json!({ "problems": ws.validate()? }),
        "conflicts" => json!({ "conflicts": ws.conflicts()? }),
        "overlaps" => {
            let own = p.get("own").and_then(Value::as_bool).unwrap_or(false);
            json!({ "overlaps": ws.overlaps(own)? })
        }
        _ => return Err(Failure::Method),
    };
    Ok(out)
}

fn str_param<'a>(p: &'a Value, key: &str) -> Result<&'a str, Failure> {
    p.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::Params(format!("missing string parameter `{key}`")))
}

fn opt_str<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

fn index_param(p: &Value) -> Result<usize, Failure> {
    p.get("index")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
        .ok_or_else(|| Failure::Params("missing integer parameter `index`".into()))
}

fn error_response(id: Value, code: i64, message: &str, data: Value) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if !data.is_null() {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}
