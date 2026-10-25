//! Drives the real `btb-lsp` binary over stdio.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Client {
    child: Child,
    tx: ChildStdin,
    rx: BufReader<ChildStdout>,
}

impl Client {
    fn new() -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_btb-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let tx = child.stdin.take().unwrap();
        let rx = BufReader::new(child.stdout.take().unwrap());
        Client { child, tx, rx }
    }
    fn send(&mut self, v: Value) {
        let b = v.to_string();
        write!(self.tx, "Content-Length: {}\r\n\r\n{b}", b.len()).unwrap();
        self.tx.flush().unwrap();
    }
    fn recv(&mut self) -> Value {
        let mut len = 0;
        loop {
            let mut l = String::new();
            assert!(self.rx.read_line(&mut l).unwrap() > 0, "server closed");
            if l.trim().is_empty() {
                break;
            }
            if let Some(v) = l.trim().strip_prefix("Content-Length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut buf = vec![0; len];
        self.rx.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        loop {
            let m = self.recv();
            if m["id"] == id {
                return m;
            }
        }
    }
}

#[test]
fn full_session() {
    let mut c = Client::new();
    let init = c.request(1, "initialize", json!({"capabilities":{}}));
    assert_eq!(init["result"]["capabilities"]["hoverProvider"], true);
    c.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}));

    let uri = "file:///p/src/main.rs";
    let text = "#[btb::query]\npub fn bad() {\n    host::storage_set(b\"a\", b\"b\");\n}\n";
    c.send(
        json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
        "textDocument":{"uri":uri,"languageId":"rust","version":1,"text":text}}}),
    );
    let d = c.recv();
    assert_eq!(d["method"], "textDocument/publishDiagnostics");
    let list = d["params"]["diagnostics"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["severity"], 1);
    assert_eq!(list[0]["range"]["start"]["line"], 2);
    assert!(list[0]["message"]
        .as_str()
        .unwrap()
        .contains("queries cannot write state"));

    let h = c.request(
        2,
        "textDocument/hover",
        json!({
        "textDocument":{"uri":uri},"position":{"line":0,"character":8}}),
    );
    assert!(h["result"]["contents"]["value"]
        .as_str()
        .unwrap()
        .contains("read-only"));

    c.send(
        json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{
        "textDocument":{"uri":uri,"version":2},
        "contentChanges":[{"text":"#[btb::mutation]\npub fn ok() {}\nlet x = 1.0;"}]}}),
    );
    let d = c.recv();
    assert_eq!(d["params"]["diagnostics"].as_array().unwrap().len(), 0); // does not parse

    c.send(
        json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{
        "textDocument":{"uri":uri,"version":3},
        "contentChanges":[{"text":"#[btb::mutation]\npub fn ok() {}\n#[btb::"}]}}),
    );
    let _ = c.recv();
    let comp = c.request(
        3,
        "textDocument/completion",
        json!({
        "textDocument":{"uri":uri},"position":{"line":2,"character":7}}),
    );
    let items = comp["result"]["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["label"] == "query"));

    let unknown = c.request(4, "foo/bar", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);

    let s = c.request(5, "shutdown", Value::Null);
    assert!(s["result"].is_null());
    c.send(json!({"jsonrpc":"2.0","method":"exit"}));
    assert!(c.child.wait().unwrap().success());
}
