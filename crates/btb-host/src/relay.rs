//! Transaction relay: a hosting node that forwards client-signed transactions to validators.
//!
//! The client picks the relayer and signs it into the transaction, so the node cannot change
//! it; it accepts a transaction only if it names the node's own relayer account (`--relayer`),
//! and then earns 20% of that transaction's fee at execution (`docs/CHAIN.md#fees`, the
//! account must be registered with a `Host` transaction). Forwarding goes to the validators'
//! JSON-RPC (`--relay-rpc`), first answer wins.

use btb_chain::Address;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Settings of the relay.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// The account that clients sign into their transactions (and that registered as a host).
    pub relayer: Address,
    /// Validator JSON-RPC endpoints, `host:port`, tried in order.
    pub rpc: Vec<String>,
}

/// A refusal with the HTTP status to answer.
pub(crate) struct Refused(pub &'static str, pub String);

/// The transaction bytes in a request body: hex text, or JSON `{"tx": "<hex>"}`.
pub(crate) fn tx_bytes(body: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(body).map_err(|_| "body must be hex text or JSON")?;
    let text = text.trim();
    let hex = if text.starts_with('{') {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("bad JSON: {e}"))?;
        v["tx"]
            .as_str()
            .ok_or("JSON body needs a \"tx\" hex string")?
            .to_string()
    } else {
        text.to_string()
    };
    btb_node::hex::decode(hex.trim_start_matches("0x")).map_err(|_| "tx must be hex".to_string())
}

/// Check `bytes` and forward it. Returns the transaction id (hex).
pub(crate) fn submit(cfg: &RelayConfig, chain_id: u64, bytes: &[u8]) -> Result<String, Refused> {
    btb_chain::host::check_relayed(bytes, chain_id, &cfg.relayer)
        .map_err(|e| Refused("400 Bad Request", e))?;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "btb_sendTransaction",
        "params": [btb_node::hex::encode(bytes)],
    })
    .to_string();
    let mut errors = Vec::new();
    for addr in &cfg.rpc {
        match post(addr, &body) {
            Ok(v) => {
                if let Some(e) = v.get("error") {
                    // The validator answered; another one would say the same.
                    let m = e.get("message").and_then(Value::as_str).unwrap_or("error");
                    return Err(Refused(
                        "400 Bad Request",
                        format!("validator refused: {m}"),
                    ));
                }
                return v["result"]["id"]
                    .as_str()
                    .map(String::from)
                    .ok_or_else(|| Refused("502 Bad Gateway", "validator sent no id".into()));
            }
            Err(e) => errors.push(format!("{addr}: {e}")),
        }
    }
    Err(Refused(
        "502 Bad Gateway",
        format!("no validator answered: {}", errors.join("; ")),
    ))
}

/// A minimal blocking HTTP/1.1 JSON POST to `/`.
fn post(addr: &str, body: &str) -> Result<Value, String> {
    let sock = addr
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no address")?;
    let t = Duration::from_secs(10);
    let mut s = TcpStream::connect_timeout(&sock, t).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(t)).ok();
    s.set_write_timeout(Some(t)).ok();
    write!(
        s,
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.take(1 << 20)
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response")?;
    serde_json::from_slice(&raw[split + 4..]).map_err(|e| format!("bad response: {e}"))
}
