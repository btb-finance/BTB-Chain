//! Minimal LSP server over stdio (Content-Length framed JSON-RPC), std plus `serde_json`.

use crate::{analyze, completions, hover, Severity};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};

/// Read one framed message. `Ok(None)` at end of input.
fn read_msg(r: &mut dyn BufRead) -> std::io::Result<Option<Value>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse::<usize>().ok();
            }
        }
    }
    let Some(len) = len else {
        return Ok(Some(Value::Null));
    };
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

fn send(w: &mut dyn Write, v: &Value) -> std::io::Result<()> {
    let body = v.to_string();
    write!(w, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    w.flush()
}

fn publish(w: &mut dyn Write, uri: &str, text: &str) -> std::io::Result<()> {
    let src = text.to_string();
    // A panic in analysis must never take the server down.
    let diags = std::panic::catch_unwind(|| analyze(&src)).unwrap_or_default();
    let list: Vec<Value> = diags
        .iter()
        .map(|d| {
            json!({
                "range": {
                    "start": {"line": d.start.0, "character": d.start.1},
                    "end": {"line": d.end.0, "character": d.end.1},
                },
                "severity": if d.severity == Severity::Error { 1 } else { 2 },
                "code": d.code,
                "source": "btb",
                "message": d.message,
            })
        })
        .collect();
    send(
        w,
        &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics",
                "params":{"uri":uri,"diagnostics":list}}),
    )
}

fn pos(p: &Value) -> (usize, usize) {
    (
        p["position"]["line"].as_u64().unwrap_or(0) as usize,
        p["position"]["character"].as_u64().unwrap_or(0) as usize,
    )
}

/// Serve until `exit` or end of input. Returns the process exit code (0 after a clean
/// `shutdown`, 1 otherwise).
pub fn run(r: &mut dyn BufRead, w: &mut dyn Write) -> i32 {
    let mut docs: HashMap<String, String> = HashMap::new();
    let mut shutdown = false;
    loop {
        let msg = match read_msg(r) {
            Ok(Some(m)) => m,
            _ => return 1,
        };
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg.get("id").cloned();
        let params = &msg["params"];
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let mut reply: Option<Value> = None;
        match method {
            "initialize" => {
                reply = Some(json!({
                    "capabilities": {
                        "textDocumentSync": 1,
                        "hoverProvider": true,
                        "completionProvider": {"triggerCharacters": [":", "."]},
                    },
                    "serverInfo": {"name": "btb-lsp", "version": env!("CARGO_PKG_VERSION")},
                }));
            }
            "textDocument/didOpen" => {
                let text = params["textDocument"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if publish(w, &uri, &text).is_err() {
                    return 1;
                }
                docs.insert(uri, text);
            }
            "textDocument/didChange" => {
                if let Some(text) = params["contentChanges"]
                    .as_array()
                    .and_then(|c| c.last())
                    .and_then(|c| c["text"].as_str())
                {
                    if publish(w, &uri, text).is_err() {
                        return 1;
                    }
                    docs.insert(uri, text.to_string());
                }
            }
            "textDocument/didClose" => {
                docs.remove(&uri);
                let _ = publish(w, &uri, "");
            }
            "textDocument/hover" => {
                let (l, c) = pos(params);
                let h = docs.get(&uri).and_then(|t| hover(t, l, c));
                reply = Some(match h {
                    Some(md) => json!({"contents": {"kind": "markdown", "value": md}}),
                    None => Value::Null,
                });
            }
            "textDocument/completion" => {
                let (l, c) = pos(params);
                let prefix: String = docs
                    .get(&uri)
                    .and_then(|t| t.lines().nth(l))
                    .map(|s| s.chars().take(c).collect())
                    .unwrap_or_default();
                let items: Vec<Value> = completions(&prefix)
                    .iter()
                    .map(|i| json!({"label": i.label, "detail": i.detail, "kind": i.kind}))
                    .collect();
                reply = Some(json!({"isIncomplete": false, "items": items}));
            }
            "shutdown" => {
                shutdown = true;
                reply = Some(Value::Null);
            }
            "exit" => return if shutdown { 0 } else { 1 },
            _ => {
                if let (Some(id), false) = (&id, method.is_empty()) {
                    let e = json!({"jsonrpc":"2.0","id":id,
                        "error":{"code":-32601,"message":format!("method not found: {method}")}});
                    if send(w, &e).is_err() {
                        return 1;
                    }
                }
            }
        }
        if let (Some(id), Some(result)) = (id, reply) {
            if send(w, &json!({"jsonrpc":"2.0","id":id,"result":result})).is_err() {
                return 1;
            }
        }
    }
}
