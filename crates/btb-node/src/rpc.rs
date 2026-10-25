//! JSON-RPC 2.0 over plain HTTP on a loopback address.
//!
//! `POST /` with a JSON-RPC request, or `GET /status` as a shortcut for `btb_status`. Methods
//! (positional parameters, hex strings for bytes):
//!
//! | Method | Parameters | Result |
//! | --- | --- | --- |
//! | `btb_sendTransaction` | `[tx_hex]` | `{id}` |
//! | `btb_getReceipt` | `[tx_id_hex]` | receipt or `null` |
//! | `btb_getBlock` | `[height or "latest"]` | block summary or `null` |
//! | `btb_getBalance` | `[address_hex]` | `{balance, nonce}` |
//! | `btb_query` | `[contract_hex, function, args_hex?, caller_hex?]` | `{output, gas_used}` |
//! | `btb_getBlocks` | `[before?, limit?]` | newest-first block summaries |
//! | `btb_getTransaction` | `[tx_id_hex]` | transaction fields plus receipt, or `null` |
//! | `btb_getAccount` | `[address_hex]` | balance, nonce, stake, validator info |
//! | `btb_getAccountTxs` | `[address_hex, limit?, before_height?]` | newest-first transactions |
//! | `btb_getEpoch` | `[epoch?]` | validator set of an epoch |
//! | `btb_status` | `[]` | node status |
//!
//! The `btb_storage*` methods (storage nodes, stored files, duties) are in `storage_rpc`.
//!
//! `GET /explorer` serves the embedded block explorer page (no external assets). `GET /dex`
//! serves the native DEX page (order entry, book, clearing prices); its `btb_dex*` methods are
//! in `dex_rpc`.
//!
//! DEV/TESTNET API: no authentication and no TLS, so it only binds to loopback. Put a proxy
//! in front of it to expose it.

use crate::driver::{Input, RpcError, RpcResult};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Semaphore};

const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 2 << 20;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Send one call to the driver and wait for the answer.
pub async fn call(tx: &Sender<Input>, method: &str, params: Value) -> RpcResult {
    let (reply, rx) = oneshot::channel();
    tx.send(Input::Rpc {
        method: method.to_string(),
        params,
        reply,
    })
    .map_err(|_| RpcError::app("node is shutting down"))?;
    match tokio::time::timeout(IO_TIMEOUT, rx).await {
        Ok(Ok(r)) => r,
        _ => Err(RpcError::app("node did not answer")),
    }
}

/// Bind `addr` (must be loopback) and serve until the runtime stops.
pub async fn serve(addr: SocketAddr, tx: Sender<Input>) -> Result<SocketAddr, String> {
    if !addr.ip().is_loopback() {
        return Err(format!(
            "rpc address {addr} is not loopback; the API has no authentication"
        ));
    }
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind rpc {addr}: {e}"))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    let limit = Arc::new(Semaphore::new(64));
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                continue;
            };
            let Ok(permit) = limit.clone().try_acquire_owned() else {
                continue; // too many connections: drop
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(IO_TIMEOUT * 3, handle(conn, tx)).await;
                drop(permit);
            });
        }
    });
    Ok(local)
}

async fn handle(mut conn: TcpStream, tx: Sender<Input>) {
    let Some((method, path, body)) = read_request(&mut conn).await else {
        let _ = respond(
            &mut conn,
            "400 Bad Request",
            &json!({"error": "bad request"}),
        )
        .await;
        return;
    };
    // The DEX page takes `?seed=` (the query string is not part of the route).
    let (status, out) = match (method.as_str(), path.split('?').next().unwrap_or("")) {
        ("GET", "/status") | ("GET", "/") => match call(&tx, "btb_status", json!([])).await {
            Ok(v) => ("200 OK", v),
            Err(e) => ("500 Internal Server Error", json!({"error": e.message})),
        },
        ("GET", "/health") => ("200 OK", json!({"ok": true})),
        ("GET", "/explorer") | ("GET", "/explorer/") => {
            let _ = respond_html(&mut conn, crate::explorer::PAGE).await;
            return;
        }
        ("GET", "/dex") | ("GET", "/dex/") => {
            let _ = respond_html(&mut conn, crate::explorer::DEX_PAGE).await;
            return;
        }
        ("POST", "/") => ("200 OK", rpc_response(&tx, &body).await),
        _ => ("404 Not Found", json!({"error": "not found"})),
    };
    let _ = respond(&mut conn, status, &out).await;
}

async fn rpc_response(tx: &Sender<Input>, body: &[u8]) -> Value {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err_obj(Value::Null, -32700, &format!("parse error: {e}")),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        return err_obj(id, -32600, "invalid request: missing method");
    };
    let params = req.get("params").cloned().unwrap_or_else(|| json!([]));
    if !params.is_array() {
        return err_obj(id, -32602, "params must be an array");
    }
    match call(tx, method, params).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => err_obj(id, e.code, &e.message),
    }
}

fn err_obj(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

async fn respond(conn: &mut TcpStream, status: &str, body: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(body).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes()).await?;
    conn.write_all(&body).await?;
    conn.shutdown().await
}

async fn respond_html(conn: &mut TcpStream, page: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'\r\n\
         Connection: close\r\n\r\n",
        page.len()
    );
    conn.write_all(head.as_bytes()).await?;
    conn.write_all(page.as_bytes()).await?;
    conn.shutdown().await
}

/// (method, path, body). `None` on anything malformed or too large.
async fn read_request(conn: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > MAX_HEADER {
            return None;
        }
        let n = tokio::time::timeout(IO_TIMEOUT, conn.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let mut len = 0usize;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().ok()?;
            }
        }
    }
    if len > MAX_BODY {
        return None;
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = tokio::time::timeout(IO_TIMEOUT, conn.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Some((method, path, body))
}
