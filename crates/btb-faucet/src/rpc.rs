//! A tiny blocking JSON-RPC client for `btb-node` (plain HTTP/1.1, `Connection: close`).

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Debug)]
pub enum RpcFail {
    /// The node could not be reached or answered garbage.
    Transport(String),
    /// The node answered with a JSON-RPC error.
    App { code: i64, message: String },
}

pub struct RpcClient {
    endpoints: Vec<String>,
    current: AtomicUsize,
    timeout: Duration,
}

impl RpcClient {
    /// `endpoints` are `host:port` of node RPC addresses; the first that works is used and
    /// kept until it fails.
    pub fn new(endpoints: Vec<String>) -> Self {
        assert!(!endpoints.is_empty(), "at least one rpc endpoint");
        RpcClient {
            endpoints,
            current: AtomicUsize::new(0),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, RpcFail> {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let n = self.endpoints.len();
        let start = self.current.load(Ordering::Relaxed);
        let mut last = String::new();
        for k in 0..n {
            let i = (start + k) % n;
            match self.post(&self.endpoints[i], &body) {
                Ok(v) => {
                    self.current.store(i, Ordering::Relaxed);
                    return match v.get("error") {
                        Some(e) => Err(RpcFail::App {
                            code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                            message: e
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        }),
                        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                    };
                }
                Err(e) => last = format!("{}: {e}", self.endpoints[i]),
            }
        }
        Err(RpcFail::Transport(last))
    }

    fn post(&self, endpoint: &str, body: &str) -> Result<Value, String> {
        let addr = endpoint
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("no address")?;
        let mut s = TcpStream::connect_timeout(&addr, self.timeout).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(self.timeout)).ok();
        s.set_write_timeout(Some(self.timeout)).ok();
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {endpoint}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        s.take(4 << 20)
            .read_to_end(&mut raw)
            .map_err(|e| e.to_string())?;
        let at = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("bad http response")?;
        serde_json::from_slice(&raw[at + 4..]).map_err(|e| format!("bad json: {e}"))
    }
}
