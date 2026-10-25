//! The HTTP side of a hosting node.
//!
//! The query and subscription routes have the shape of `btb-dev`, so `@btb/client` works against
//! a hosting node unchanged. A hosting node serves many contracts, so the contract is named by a
//! path prefix: point the client at `http://host/c/<contract hex>` and `query`, `subscribe` just
//! work. A node started with a default contract also answers the unprefixed paths.
//!
//! | Route | Answer |
//! | --- | --- |
//! | `GET /c/<contract>/query/<fn>?args=<hex>&caller=<id>` | result as hex (`text/plain`) |
//! | `GET /c/<contract>/query/<fn>?...&proof=1` | JSON: result, height, proof bundle for the keys it read |
//! | `GET /c/<contract>/subscribe/<fn>?...` | Server-Sent Events: the result, then each change |
//! | `GET /site/<contract>/<path>` | a published static file (`index.html` for `/` and `dir/`) |
//! | `GET /site/<contract>/<path>` (no such file, but `<path>.root` holds a storage root) | the large file, fetched from storage nodes and verified against the root |
//! | `GET /file/<root hex>` | a file stored on storage nodes, fetched by root, verified (any `k` of `n` pieces) |
//! | `GET /api/site/<contract>` | JSON list of published files and their SHA-256 |
//! | `GET /proof/<contract>?key=<hex>[,<hex>..][&height=N]` | proof bundle for contract storage keys |
//! | `GET /proof/<contract>/site/<path>[?height=N]` | proof bundle for a site file |
//! | `GET /proof/raw?key=<hex full state key>[&height=N]` | proof bundle for any state key |
//! | `GET /status`, `GET /health` | node and sync status |
//! | `GET /relay` | the relayer address to sign into a transaction (404 if this node does not relay) |
//! | `POST /tx` (hex or `{"tx": hex}`) | relay a client-signed transaction naming this node as relayer to the validators: `{id, relayer}` |
//! | `GET /tx/<id>` | the receipt, or `null` until this node has executed the block |
//!
//! Mutations are not run here: a signed transaction goes to a validator, either directly
//! (`btb_sendTransaction`) or through `POST /tx`, which earns this node the relay share of the fee.
//! Read endpoints answer cross-origin (`Access-Control-Allow-Origin: *`): everything served is
//! public chain data. Site files are served inside a CSP sandbox with an opaque origin, so one
//! contract's site cannot read another's storage in the browser.

use crate::build;
use crate::state::{Client, HostState, Shared, CLIENT_BACKLOG};
use btb_chain::{site, Executor, Revert};
use btb_node::{hex, Node};
use btb_runtime::layout;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

/// A site file `<path>.root` holds the 64-hex storage root of the large file served at `<path>`.
const ROOT_SUFFIX: &str = ".root";

const MAX_BODY: usize = 64 * 1024;
const MAX_LINE: u64 = 8192;

pub(crate) struct Ctx {
    pub node: Arc<Node>,
    pub shared: Arc<Shared>,
    pub relay: Option<crate::relay::RelayConfig>,
    pub chain_id: u64,
}

struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut o, mut i) = (Vec::new(), 0);
    while i < b.len() {
        match b[i] {
            b'%' if s.get(i + 1..i + 3).is_some_and(|h| hex::decode(h).is_ok()) => {
                o.push(hex::decode(&s[i + 1..i + 3]).expect("checked")[0]);
                i += 3;
                continue;
            }
            b'+' => o.push(b' '),
            c => o.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&o).into_owned()
}

fn read_request(conn: &mut TcpStream) -> Result<Request, String> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut r = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    r.by_ref()
        .take(MAX_LINE)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let mut it = line.split_whitespace();
    let (method, target) = (
        it.next().ok_or("empty request")?.to_string(),
        it.next().ok_or("no target")?.to_string(),
    );
    let mut headers = BTreeMap::new();
    for _ in 0..100 {
        let mut h = String::new();
        r.by_ref()
            .take(MAX_LINE)
            .read_line(&mut h)
            .map_err(|e| e.to_string())?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err("body too large".into());
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).map_err(|e| e.to_string())?;
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let query = qs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect();
    Ok(Request {
        method,
        path: pct_decode(path),
        query,
        headers,
        body,
    })
}

const CORS: &str = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: x-btb-caller, content-type, if-none-match\r\nAccess-Control-Expose-Headers: x-btb-height, x-btb-content-hash, x-btb-storage-root, etag\r\n";

fn write_response(
    conn: &mut TcpStream,
    status: &str,
    ctype: &str,
    extra: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        conn,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{CORS}{extra}Connection: close\r\n\r\n",
        body.len()
    )?;
    conn.write_all(body)?;
    conn.flush()
}

fn json_response(conn: &mut TcpStream, status: &str, v: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(v).unwrap_or_default();
    write_response(
        conn,
        status,
        "application/json",
        "Cache-Control: no-store\r\n",
        &body,
    )
}

fn err(conn: &mut TcpStream, status: &str, msg: &str) -> std::io::Result<()> {
    json_response(conn, status, &json!({ "error": msg }))
}

/// Caller identity, as `btb-dev` reads it: 64 hex characters are the address, any other text
/// is hashed.
fn caller_of(s: &str) -> [u8; 32] {
    match hex::decode(s) {
        Ok(b) if b.len() == 32 => b.try_into().expect("32 bytes"),
        _ => Sha256::digest(s.as_bytes()).into(),
    }
}

fn revert_status(r: &Revert) -> (&'static str, String) {
    match r {
        Revert::NoSuchContract => ("404 Not Found", "no such contract".into()),
        Revert::NoSuchFunction => ("404 Not Found", "no such function".into()),
        Revert::Abort(m) => ("400 Bad Request", format!("aborted: {m}")),
        Revert::Fault(m) => ("500 Internal Server Error", m.clone()),
        other => ("400 Bad Request", format!("{other:?}")),
    }
}

pub(crate) fn handle(ctx: &Ctx, mut conn: TcpStream) {
    let n = ctx.shared.conns.fetch_add(1, Ordering::SeqCst);
    let r = if n >= ctx.shared.max_connections {
        err(&mut conn, "503 Service Unavailable", "too many connections")
    } else {
        match read_request(&mut conn) {
            Ok(req) => route(ctx, &mut conn, &req),
            Err(e) => err(&mut conn, "400 Bad Request", &e),
        }
    };
    let _ = r;
    ctx.shared.conns.fetch_sub(1, Ordering::SeqCst);
}

fn route(ctx: &Ctx, conn: &mut TcpStream, req: &Request) -> std::io::Result<()> {
    if req.method == "OPTIONS" {
        return write_response(conn, "204 No Content", "text/plain", "", b"");
    }
    let segs: Vec<&str> = req.path.trim_start_matches('/').splitn(4, '/').collect();
    match (req.method.as_str(), segs.as_slice()) {
        ("GET", ["health"]) => json_response(conn, "200 OK", &json!({"ok": true})),
        ("GET", ["status"]) | ("GET", [""]) => status(ctx, conn),
        ("GET", ["c", c, kind @ ("query" | "subscribe"), f]) => match parse_contract(c) {
            Ok(contract) => function(ctx, conn, req, contract, kind == &"subscribe", f),
            Err(e) => err(conn, "400 Bad Request", &e),
        },
        ("GET", [kind @ ("query" | "subscribe"), f]) => match ctx.shared.default_contract {
            Some(contract) => function(ctx, conn, req, contract, kind == &"subscribe", f),
            None => err(
                conn,
                "404 Not Found",
                "no default contract: use /c/<contract>/query/<function>",
            ),
        },
        (_, ["mutation", ..]) | (_, ["c", _, "mutation", ..]) => err(
            conn,
            "501 Not Implemented",
            "hosting nodes do not run mutations: sign a transaction and POST it to /tx (relay) or send it to a validator (btb_sendTransaction)",
        ),
        ("GET", ["relay"]) => relay_info(ctx, conn),
        ("POST", ["tx"]) => relay_tx(ctx, conn, req),
        ("GET", ["tx", id]) => receipt(ctx, conn, id),
        ("GET", ["file", root]) => storage_file_route(ctx, conn, req, root),
        ("GET", ["site", c]) => site_file(ctx, conn, req, c, ""),
        ("GET", ["site", c, rest @ ..]) => site_file(ctx, conn, req, c, &rest.join("/")),
        ("GET", ["api", "site", c]) => site_list(ctx, conn, c),
        ("GET", ["proof", "raw"]) => proof_raw(ctx, conn, req),
        ("GET", ["proof", c]) => proof_storage(ctx, conn, req, c),
        ("GET", ["proof", c, "site", path]) => proof_site(ctx, conn, req, c, path),
        ("GET", _) => err(conn, "404 Not Found", "not found"),
        _ => err(conn, "405 Method Not Allowed", "use GET (POST /tx to relay)"),
    }
}

fn relay_info(ctx: &Ctx, conn: &mut TcpStream) -> std::io::Result<()> {
    match &ctx.relay {
        Some(r) => json_response(
            conn,
            "200 OK",
            &json!({"relayer": hex::encode(&r.relayer), "chain_id": ctx.chain_id}),
        ),
        None => err(
            conn,
            "404 Not Found",
            "this hosting node does not relay transactions",
        ),
    }
}

/// `POST /tx`: forward a client-signed transaction that names this node as its relayer.
fn relay_tx(ctx: &Ctx, conn: &mut TcpStream, req: &Request) -> std::io::Result<()> {
    let Some(cfg) = &ctx.relay else {
        return err(
            conn,
            "501 Not Implemented",
            "this hosting node does not relay transactions",
        );
    };
    let bytes = match crate::relay::tx_bytes(&req.body) {
        Ok(b) => b,
        Err(e) => return err(conn, "400 Bad Request", &e),
    };
    match crate::relay::submit(cfg, ctx.chain_id, &bytes) {
        Ok(id) => json_response(
            conn,
            "200 OK",
            &json!({"id": id, "relayer": hex::encode(&cfg.relayer)}),
        ),
        Err(crate::relay::Refused(status, msg)) => err(conn, status, &msg),
    }
}

/// `GET /tx/<id>`: the receipt, once this node has executed the block (null before).
fn receipt(ctx: &Ctx, conn: &mut TcpStream, id: &str) -> std::io::Result<()> {
    match ctx.node.call("btb_getReceipt", json!([id])) {
        Ok(v) => json_response(conn, "200 OK", &v),
        Err(e) => err(conn, "400 Bad Request", &e.message),
    }
}

fn parse_contract(s: &str) -> Result<[u8; 32], String> {
    hex::decode32(s).map_err(|_| "contract must be 64 hex characters".to_string())
}

fn status(ctx: &Ctx, conn: &mut TcpStream) -> std::io::Result<()> {
    let mut v = match ctx.node.call("btb_status", json!([])) {
        Ok(v) => v,
        Err(e) => return err(conn, "500 Internal Server Error", &e.message),
    };
    let subs = ctx.shared.subs.lock().map_or(0, |s| s.clients.len());
    v["host"] = json!({
        "role": "hosting-node",
        "subscriptions": subs,
        "connections": ctx.shared.conns.load(Ordering::SeqCst),
        "executed_height": ctx.shared.head.load(Ordering::SeqCst),
    });
    json_response(conn, "200 OK", &v)
}

fn args_of(req: &Request) -> Result<Vec<u8>, String> {
    hex::decode(req.query.get("args").map_or("", String::as_str))
        .map_err(|_| "args must be hex of btb-codec bytes".to_string())
}

fn caller_from(req: &Request) -> [u8; 32] {
    caller_of(
        req.headers
            .get("x-btb-caller")
            .or_else(|| req.query.get("caller"))
            .map_or("anonymous", String::as_str),
    )
}

fn function(
    ctx: &Ctx,
    conn: &mut TcpStream,
    req: &Request,
    contract: [u8; 32],
    subscribe: bool,
    name: &str,
) -> std::io::Result<()> {
    let args = match args_of(req) {
        Ok(a) => a,
        Err(e) => return err(conn, "400 Bad Request", &e),
    };
    let caller = caller_from(req);
    if subscribe {
        return sse(ctx, conn, contract, name, args, caller);
    }
    let name = name.to_string();
    let want_proof = req.query.get("proof").is_some_and(|p| p == "1");
    let r = ctx
        .node
        .with_chain::<crate::state::Recording, _, _>(move |st| {
            let height = st.head().map_err(|e| (500, e.0.clone()))?.height;
            let (out, gas, reads) = Executor::sequential()
                .query_tracked(st, &contract, &name, &args, &caller)
                .map_err(|r| {
                    let (s, m) = revert_status(&r);
                    (
                        s.split(' ')
                            .next()
                            .and_then(|c| c.parse().ok())
                            .unwrap_or(500),
                        m,
                    )
                })?;
            let proof = if want_proof {
                let points: Vec<Vec<u8>> = reads.points().cloned().collect();
                let incomplete = reads.ranges().next().is_some() || points.len() > build::MAX_KEYS;
                let keys: Vec<Vec<u8>> = points.into_iter().take(build::MAX_KEYS).collect();
                let b = build::bundle(st, None, height, &keys).map_err(|m| (500, m))?;
                Some((b.to_json(), incomplete))
            } else {
                None
            };
            Ok::<_, (u16, String)>((out, gas, height, proof))
        });
    let (out, gas, height, proof) = match r {
        Ok(Ok(v)) => v,
        Ok(Err((code, m))) => return err(conn, status_line(code), &m),
        Err(e) => return err(conn, "503 Service Unavailable", &e),
    };
    let extra = format!("X-BTB-Height: {height}\r\nCache-Control: no-store\r\n");
    match proof {
        None => write_response(
            conn,
            "200 OK",
            "text/plain",
            &extra,
            hex::encode(&out).as_bytes(),
        ),
        Some((bundle, incomplete)) => {
            let body = json!({
                "result": hex::encode(&out),
                "height": height,
                "gas_used": gas,
                // True when the query also scanned key ranges (or read more keys than a
                // bundle carries): the proofs then cover only the point reads.
                "incomplete": incomplete,
                "proof": bundle,
            });
            let body = serde_json::to_vec(&body).unwrap_or_default();
            write_response(conn, "200 OK", "application/json", &extra, &body)
        }
    }
}

fn status_line(code: u16) -> &'static str {
    match code {
        400 => "400 Bad Request",
        404 => "404 Not Found",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    }
}

type Registered = Result<(Vec<u8>, Receiver<Vec<u8>>, u64), (u16, String)>;

fn sse(
    ctx: &Ctx,
    conn: &mut TcpStream,
    contract: [u8; 32],
    name: &str,
    args: Vec<u8>,
    caller: [u8; 32],
) -> std::io::Result<()> {
    let shared = ctx.shared.clone();
    let name_owned = name.to_string();
    let r: Result<Registered, String> =
        ctx.node
            .with_chain::<crate::state::Recording, _, _>(move |st: &mut HostState| {
                // Run and register in one step on the driver thread, so no block lands in between.
                let (out, _, reads) = Executor::sequential()
                    .query_tracked(st, &contract, &name_owned, &args, &caller)
                    .map_err(|r| {
                        let (s, m) = revert_status(&r);
                        (
                            s.split(' ')
                                .next()
                                .and_then(|c| c.parse().ok())
                                .unwrap_or(500),
                            m,
                        )
                    })?;
                let (tx, rx) = sync_channel(CLIENT_BACKLOG);
                let mut subs = shared.subs.lock().unwrap_or_else(|e| e.into_inner());
                let id = subs
                    .mgr
                    .subscribe(contract, &name_owned, &args, reads, &out);
                subs.clients.insert(id, Client { caller, tx });
                Ok((out, rx, id))
            });
    let (first, rx, id) = match r {
        Ok(Ok(v)) => v,
        Ok(Err((code, m))) => return err(conn, status_line(code), &m),
        Err(e) => return err(conn, "503 Service Unavailable", &e),
    };
    let result = (|| {
        write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n{CORS}Connection: close\r\n\r\n"
        )?;
        write!(conn, "data: {}\n\n", hex::encode(&first))?;
        conn.flush()?;
        let mut idle = 0u32;
        while !ctx.shared.stop.load(Ordering::SeqCst) {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(out) => {
                    idle = 0;
                    write!(conn, "data: {}\n\n", hex::encode(&out))?;
                    conn.flush()?;
                }
                Err(RecvTimeoutError::Timeout) => {
                    idle += 1;
                    if idle.is_multiple_of(60) {
                        conn.write_all(b": ping\n\n")?;
                        conn.flush()?;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    })();
    let mut subs = ctx.shared.subs.lock().unwrap_or_else(|e| e.into_inner());
    subs.mgr.unsubscribe(id);
    subs.clients.remove(&id);
    result
}

// ------------------------------------------------------------------ sites

fn site_file(
    ctx: &Ctx,
    conn: &mut TcpStream,
    req: &Request,
    contract: &str,
    path: &str,
) -> std::io::Result<()> {
    let Ok(contract) = parse_contract(contract) else {
        return err(
            conn,
            "400 Bad Request",
            "contract must be 64 hex characters",
        );
    };
    let mut path = path.to_string();
    if path.is_empty() || path.ends_with('/') {
        path.push_str("index.html");
    }
    if site::check_path(&path).is_err() {
        return err(conn, "400 Bad Request", "bad path");
    }
    let p = path.clone();
    let r = ctx
        .node
        .with_chain::<crate::state::Recording, _, _>(move |st| {
            let h = st.head().map(|h| h.height).unwrap_or(0);
            let f = site::get(st.store(), &contract, &p)?;
            if f.is_some() || p.len() + ROOT_SUFFIX.len() > site::MAX_PATH_BYTES {
                return Ok((f, None, h));
            }
            // Not on chain: a `<path>.root` file may point at a large file on storage nodes.
            let ptr = site::get(st.store(), &contract, &format!("{p}{ROOT_SUFFIX}"))?;
            let root = ptr
                .and_then(|f| String::from_utf8(f.content).ok())
                .and_then(|t| hex::decode32(t.trim()).ok());
            Ok::<_, btb_chain::state::StateError>((f, root, h))
        });
    let (file, height) = match r {
        Ok(Ok((Some(f), _, h))) => (f, h),
        Ok(Ok((None, Some(root), h))) => {
            return storage_backed(ctx, conn, req, &root, crate::site::content_type(&path), h)
        }
        Ok(Ok((None, None, _))) => return err(conn, "404 Not Found", "no such file"),
        Ok(Err(e)) => return err(conn, "500 Internal Server Error", &e.to_string()),
        Err(e) => return err(conn, "503 Service Unavailable", &e),
    };
    let tag = hex::encode(&file.hash);
    let extra = format!(
        "ETag: \"{tag}\"\r\nX-BTB-Content-Hash: {tag}\r\nX-BTB-Height: {height}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: sandbox allow-scripts allow-forms allow-popups\r\n"
    );
    if req
        .headers
        .get("if-none-match")
        .is_some_and(|v| v.trim_matches('"') == tag)
    {
        return write_response(conn, "304 Not Modified", "text/plain", &extra, b"");
    }
    write_response(
        conn,
        "200 OK",
        crate::site::content_type(&path),
        &extra,
        &file.content,
    )
}

/// The chain as `btb-storage` sees it: node RPC, read only.
struct NodeChain<'a>(&'a Node);

impl btb_storage::chain::Chain for NodeChain<'_> {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.0.call(method, params).map_err(|e| e.message)
    }
    fn send(&self, _: btb_chain::storage::StorageOp) -> Result<Value, String> {
        Err("a hosting node does not send storage transactions".into())
    }
    fn address(&self) -> [u8; 32] {
        [0; 32]
    }
}

fn storage_file_route(
    ctx: &Ctx,
    conn: &mut TcpStream,
    req: &Request,
    root: &str,
) -> std::io::Result<()> {
    let Ok(root) = hex::decode32(root) else {
        return err(conn, "400 Bad Request", "root must be 64 hex characters");
    };
    let height = ctx.shared.head.load(Ordering::SeqCst);
    storage_backed(ctx, conn, req, &root, "application/octet-stream", height)
}

/// Serve the file with storage root `root`: any `k` verified pieces from storage nodes, decoded
/// and checked against the root. A caller that sends `X-BTB-Storage: redirect` (an edge node,
/// which fetches from storage nodes itself) gets only the root, no body. `ETag` is the root:
/// the content is immutable.
fn storage_backed(
    ctx: &Ctx,
    conn: &mut TcpStream,
    req: &Request,
    root: &[u8; 32],
    ctype: &str,
    height: u64,
) -> std::io::Result<()> {
    let tag = hex::encode(root);
    let extra = format!(
        "ETag: \"{tag}\"\r\nX-BTB-Storage-Root: {tag}\r\nX-BTB-Height: {height}\r\nCache-Control: public, max-age=31536000, immutable\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: sandbox allow-scripts allow-forms allow-popups\r\n"
    );
    if req
        .headers
        .get("if-none-match")
        .is_some_and(|v| v.trim_matches('"') == tag)
    {
        return write_response(conn, "304 Not Modified", "text/plain", &extra, b"");
    }
    if req
        .headers
        .get("x-btb-storage")
        .is_some_and(|v| v.eq_ignore_ascii_case("redirect"))
    {
        return write_response(conn, "200 OK", ctype, &extra, b"");
    }
    match btb_storage::client::fetch(&NodeChain(&ctx.node), root) {
        Ok(data) => write_response(conn, "200 OK", ctype, &extra, &data),
        Err(e) => err(
            conn,
            "502 Bad Gateway",
            &format!("storage fetch failed: {e}"),
        ),
    }
}

fn site_list(ctx: &Ctx, conn: &mut TcpStream, contract: &str) -> std::io::Result<()> {
    let Ok(contract) = parse_contract(contract) else {
        return err(
            conn,
            "400 Bad Request",
            "contract must be 64 hex characters",
        );
    };
    let r = ctx
        .node
        .with_chain::<crate::state::Recording, _, _>(move |st| site::list(st.store(), &contract));
    match r {
        Ok(Ok(files)) => json_response(
            conn,
            "200 OK",
            &json!({"files": files.iter()
                .map(|(p, h)| json!({"path": p, "sha256": hex::encode(h)})).collect::<Vec<_>>()}),
        ),
        Ok(Err(e)) => err(conn, "500 Internal Server Error", &e.to_string()),
        Err(e) => err(conn, "503 Service Unavailable", &e),
    }
}

// ------------------------------------------------------------------ proofs

fn proof_for(
    ctx: &Ctx,
    conn: &mut TcpStream,
    req: &Request,
    keys: Vec<Vec<u8>>,
) -> std::io::Result<()> {
    let height: Option<u64> = match req.query.get("height") {
        None => None,
        Some(h) => match h.parse() {
            Ok(h) => Some(h),
            Err(_) => return err(conn, "400 Bad Request", "height must be a number"),
        },
    };
    let shared = ctx.shared.clone();
    let r = ctx
        .node
        .with_chain::<crate::state::Recording, _, _>(move |st| {
            let log = shared.log.lock().unwrap_or_else(|e| e.into_inner());
            match height {
                Some(v) => build::bundle(st, Some(&log), v, &keys),
                None => build::newest_anchored(st, &log, &keys),
            }
        });
    match r {
        Ok(Ok(b)) => json_response(conn, "200 OK", &b.to_json()),
        Ok(Err(m)) => err(conn, "400 Bad Request", &m),
        Err(e) => err(conn, "503 Service Unavailable", &e),
    }
}

fn key_list(req: &Request) -> Result<Vec<Vec<u8>>, String> {
    let raw = req.query.get("key").ok_or("missing ?key=<hex>")?;
    raw.split(',')
        .map(|k| hex::decode(k).map_err(|_| "key must be hex".to_string()))
        .collect()
}

fn proof_raw(ctx: &Ctx, conn: &mut TcpStream, req: &Request) -> std::io::Result<()> {
    match key_list(req) {
        Ok(keys) => proof_for(ctx, conn, req, keys),
        Err(e) => err(conn, "400 Bad Request", &e),
    }
}

fn proof_storage(ctx: &Ctx, conn: &mut TcpStream, req: &Request, c: &str) -> std::io::Result<()> {
    let (Ok(contract), Ok(keys)) = (parse_contract(c), key_list(req)) else {
        return err(
            conn,
            "400 Bad Request",
            "need a 64-hex contract and ?key=<hex>",
        );
    };
    let keys = keys
        .iter()
        .map(|k| layout::storage_key(&contract, k))
        .collect();
    proof_for(ctx, conn, req, keys)
}

fn proof_site(
    ctx: &Ctx,
    conn: &mut TcpStream,
    req: &Request,
    c: &str,
    path: &str,
) -> std::io::Result<()> {
    let Ok(contract) = parse_contract(c) else {
        return err(
            conn,
            "400 Bad Request",
            "contract must be 64 hex characters",
        );
    };
    if site::check_path(path).is_err() {
        return err(conn, "400 Bad Request", "bad path");
    }
    proof_for(ctx, conn, req, vec![site::site_key(&contract, path)])
}
