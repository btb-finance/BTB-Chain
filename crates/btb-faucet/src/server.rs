//! Std-only HTTP/1.1 server: a thread per connection (bounded), short timeouts, small bodies.

use crate::{DripError, DripRequest, Faucet};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PAGE: &str = include_str!("../web/index.html");
const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 4 * 1024;
const MAX_CONNS: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Handle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting and wait for the accept loop.
    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Block until the server stops (it only stops on `shutdown`/drop).
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Bind `bind` and serve in the background.
pub fn serve(faucet: Arc<Faucet>, bind: SocketAddr) -> std::io::Result<Handle> {
    let listener = TcpListener::bind(bind)?;
    let addr = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let live = Arc::new(AtomicUsize::new(0));
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("faucet-accept".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                if live.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                    live.fetch_sub(1, Ordering::SeqCst);
                    continue; // too many connections: drop
                }
                let (f, done) = (faucet.clone(), live.clone());
                let spawned = std::thread::Builder::new()
                    .name("faucet-conn".into())
                    .spawn(move || {
                        handle(&f, conn);
                        done.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    live.fetch_sub(1, Ordering::SeqCst);
                }
            }
        })?;
    Ok(Handle {
        addr,
        stop,
        thread: Some(thread),
    })
}

struct Request {
    method: String,
    path: String,
    forwarded: Option<String>,
    body: Vec<u8>,
}

fn read_request(conn: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > MAX_HEADER {
            return None;
        }
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_string(), first.next()?);
    let (path, _query) = target.split_once('?').unwrap_or((target, ""));
    let (mut len, mut forwarded) = (0usize, None);
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().ok()?;
            } else if k.eq_ignore_ascii_case("x-forwarded-for") {
                forwarded = Some(v.trim().to_string());
            }
        }
    }
    if len > MAX_BODY {
        return None;
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Some(Request {
        method,
        path: path.to_string(),
        forwarded,
        body,
    })
}

fn respond(conn: &mut TcpStream, status: u16, ctype: &str, extra: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
    let _ = conn.flush();
}

fn json_reply(conn: &mut TcpStream, status: u16, v: &Value) {
    respond(
        conn,
        status,
        "application/json",
        "",
        v.to_string().as_bytes(),
    );
}

fn error_reply(conn: &mut TcpStream, e: &DripError) {
    let extra = e
        .retry_after
        .map(|s| format!("Retry-After: {s}\r\n"))
        .unwrap_or_default();
    let body = json!({"error": e.message, "retry_after": e.retry_after});
    respond(
        conn,
        e.status,
        "application/json",
        &extra,
        body.to_string().as_bytes(),
    );
}

fn client_ip(f: &Faucet, peer: IpAddr, forwarded: Option<&str>) -> IpAddr {
    if f.cfg.trust_proxy {
        // The proxy appends the address it saw; earlier entries are client-controlled.
        if let Some(ip) = forwarded
            .and_then(|h| h.rsplit(',').next())
            .and_then(|s| s.trim().parse::<IpAddr>().ok())
        {
            return ip;
        }
    }
    peer
}

fn handle(f: &Faucet, mut conn: TcpStream) {
    let _ = conn.set_read_timeout(Some(IO_TIMEOUT));
    let _ = conn.set_write_timeout(Some(IO_TIMEOUT));
    let Ok(peer) = conn.peer_addr() else { return };
    let Some(req) = read_request(&mut conn) else {
        json_reply(&mut conn, 400, &json!({"error": "bad request"}));
        return;
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => respond(
            &mut conn,
            200,
            "text/html; charset=utf-8",
            "Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'\r\n",
            PAGE.as_bytes(),
        ),
        ("GET", "/health") => json_reply(&mut conn, 200, &json!({"ok": true})),
        ("GET", "/info") => json_reply(&mut conn, 200, &f.info()),
        ("GET", "/challenge") => match f.challenge() {
            Ok(v) => json_reply(&mut conn, 200, &v),
            Err(e) => error_reply(&mut conn, &e),
        },
        ("GET", p) if p.starts_with("/tx/") => match f.tx_status(&p[4..]) {
            Ok(v) => json_reply(&mut conn, 200, &v),
            Err(e) => error_reply(&mut conn, &e),
        },
        ("POST", "/drip") => {
            let ip = client_ip(f, peer.ip(), req.forwarded.as_deref());
            let parsed = serde_json::from_slice::<Value>(&req.body)
                .map_err(|_| DripError {
                    status: 400,
                    message: "body must be JSON".into(),
                    retry_after: None,
                })
                .and_then(|v| DripRequest::from_json(&v));
            match parsed.and_then(|r| f.drip(&r, ip)) {
                Ok(d) => json_reply(
                    &mut conn,
                    200,
                    &json!({
                        "tx_id": d.tx_id,
                        "nonce": d.nonce,
                        "amount": d.amount.to_string(),
                        "amount_btb": d.amount / crate::UNITS_PER_BTB,
                    }),
                ),
                Err(e) => error_reply(&mut conn, &e),
            }
        }
        (_, "/drip") | (_, "/info") | (_, "/challenge") | (_, "/") => {
            json_reply(&mut conn, 405, &json!({"error": "method not allowed"}))
        }
        _ => json_reply(&mut conn, 404, &json!({"error": "not found"})),
    }
}
