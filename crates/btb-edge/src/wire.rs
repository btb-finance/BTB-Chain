//! Minimal blocking HTTP/1.1: a client for upstream hosts and node RPC, and the request parser
//! and response writer of the edge's own server.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const MAX_LINE: u64 = 8192;

/// An upstream (or RPC) response. Header names are lower case.
#[derive(Debug, Clone)]
pub struct UResp {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl UResp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

pub fn connect(host: &str, timeout: Duration) -> Result<TcpStream, String> {
    let host = host.strip_prefix("http://").unwrap_or(host);
    let host = host.trim_end_matches('/');
    let addr = host
        .to_socket_addrs()
        .map_err(|e| format!("address {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("address {host} does not resolve"))?;
    let s =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connect {host}: {e}"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    Ok(s)
}

fn host_header(host: &str) -> &str {
    host.strip_prefix("http://")
        .unwrap_or(host)
        .trim_end_matches('/')
}

fn send_request(
    s: &mut TcpStream,
    host: &str,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(), String> {
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        host_header(host)
    );
    for (k, v) in headers {
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            return Err("bad header".into());
        }
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())
        .and_then(|_| s.write_all(body))
        .map_err(|e| format!("write {host}: {e}"))
}

pub fn read_head(r: &mut impl BufRead) -> Result<(u16, BTreeMap<String, String>), String> {
    let mut line = String::new();
    r.by_ref()
        .take(MAX_LINE)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line {line:?}"))?;
    let mut headers = BTreeMap::new();
    for _ in 0..100 {
        let mut h = String::new();
        r.by_ref()
            .take(MAX_LINE)
            .read_line(&mut h)
            .map_err(|e| e.to_string())?;
        let h = h.trim_end();
        if h.is_empty() {
            return Ok((status, headers));
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Err("too many headers".into())
}

fn read_body(
    r: &mut impl BufRead,
    headers: &BTreeMap<String, String>,
    head_only: bool,
    max: usize,
) -> Result<Vec<u8>, String> {
    if head_only {
        return Ok(Vec::new());
    }
    let mut body = Vec::new();
    if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let mut l = String::new();
            r.by_ref()
                .take(MAX_LINE)
                .read_line(&mut l)
                .map_err(|e| e.to_string())?;
            let n = usize::from_str_radix(l.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| "bad chunk size".to_string())?;
            if n == 0 {
                return Ok(body);
            }
            if body.len() + n > max {
                return Err(format!("body above {max} bytes"));
            }
            let start = body.len();
            body.resize(start + n, 0);
            r.read_exact(&mut body[start..])
                .map_err(|e| e.to_string())?;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf).map_err(|e| e.to_string())?;
        }
    }
    match headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(n) => {
            if n > max {
                return Err(format!("body of {n} bytes is above {max}"));
            }
            body.resize(n, 0);
            r.read_exact(&mut body)
                .map_err(|e| format!("short body: {e}"))?;
        }
        None => {
            r.by_ref()
                .take(max as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|e| e.to_string())?;
            if body.len() > max {
                return Err(format!("body above {max} bytes"));
            }
        }
    }
    Ok(body)
}

/// One request on a fresh connection. `max` bounds the response body.
pub fn request(
    host: &str,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    max: usize,
) -> Result<UResp, String> {
    let mut s = connect(host, timeout)?;
    send_request(&mut s, host, method, target, headers, body)?;
    let mut r = BufReader::new(s);
    let (status, headers) = read_head(&mut r).map_err(|e| format!("{host}: {e}"))?;
    let body = read_body(&mut r, &headers, method == "HEAD" || status == 304, max)
        .map_err(|e| format!("{host}: {e}"))?;
    Ok(UResp {
        status,
        headers,
        body,
    })
}

/// Open a Server-Sent Events stream; returns the reader positioned at the first event.
pub fn open_sse(
    host: &str,
    target: &str,
    timeout: Duration,
) -> Result<BufReader<TcpStream>, String> {
    let mut s = connect(host, timeout)?;
    send_request(
        &mut s,
        host,
        "GET",
        target,
        &[("Accept".into(), "text/event-stream".into())],
        &[],
    )?;
    let mut r = BufReader::new(s);
    let (status, _) = read_head(&mut r).map_err(|e| format!("{host}: {e}"))?;
    if status != 200 {
        return Err(format!("{host}: subscribe answered {status}"));
    }
    Ok(r)
}

/// Percent-encode everything but unreserved characters.
pub fn pct_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut o, mut i) = (Vec::new(), 0);
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                o.push(v);
                i += 3;
                continue;
            }
        }
        o.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&o).into_owned()
}

/// A request received by the edge.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
}

pub fn read_request(conn: &TcpStream) -> Result<Request, String> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut r = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    r.by_ref()
        .take(MAX_LINE)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let mut it = line.split_whitespace();
    let method = it.next().ok_or("empty request")?.to_string();
    let target = it.next().ok_or("no target")?.to_string();
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
    })
}

/// What the edge answers: a status, headers, and a window into shared bytes.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: std::sync::Arc<Vec<u8>>,
    pub start: usize,
    pub end: usize,
}

impl Response {
    pub fn new(status: u16, ctype: &str, body: Vec<u8>) -> Response {
        let end = body.len();
        Response {
            status,
            headers: vec![("Content-Type".into(), ctype.into())],
            body: std::sync::Arc::new(body),
            start: 0,
            end,
        }
    }
    pub fn json(status: u16, v: &serde_json::Value) -> Response {
        let mut r = Response::new(
            status,
            "application/json",
            serde_json::to_vec(v).unwrap_or_default(),
        );
        r.header("Cache-Control", "no-store");
        r
    }
    pub fn error(status: u16, msg: &str) -> Response {
        Response::json(status, &serde_json::json!({ "error": msg }))
    }
    pub fn header(&mut self, k: &str, v: impl Into<String>) {
        self.headers.push((k.into(), v.into()));
    }
    pub fn len(&self) -> usize {
        self.end - self.start
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        402 => "Payment Required",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

const CORS: &str = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: x-btb-caller, x-btb-voucher, content-type, if-none-match, range\r\nAccess-Control-Expose-Headers: x-btb-height, x-btb-content-hash, x-btb-cache, x-btb-price, etag, content-range, accept-ranges\r\n";

/// Write `r` (without the body for `HEAD` and `304`). Returns the body bytes sent.
pub fn write_response(conn: &mut TcpStream, r: &Response, head: bool) -> std::io::Result<usize> {
    let mut out = format!("HTTP/1.1 {} {}\r\n", r.status, reason(r.status));
    for (k, v) in &r.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\n{CORS}Connection: close\r\n\r\n",
        r.len()
    ));
    conn.write_all(out.as_bytes())?;
    let send = !head && r.status != 304;
    if send {
        conn.write_all(&r.body[r.start..r.end])?;
    }
    conn.flush()?;
    Ok(if send { r.len() } else { 0 })
}

/// A parsed `Range` header against a body of `len` bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum RangeSpec {
    /// No usable range: serve everything with 200 (absent, malformed, or several ranges).
    Full,
    /// Inclusive byte window.
    One(usize, usize),
    Unsatisfiable,
}

pub fn parse_range(h: Option<&str>, len: usize) -> RangeSpec {
    let Some(spec) = h.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return RangeSpec::Full;
    };
    if spec.contains(',') {
        return RangeSpec::Full;
    }
    let Some((a, b)) = spec.split_once('-') else {
        return RangeSpec::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix: the last `b` bytes.
        let Ok(n) = b.parse::<usize>() else {
            return RangeSpec::Full;
        };
        if n == 0 || len == 0 {
            return RangeSpec::Unsatisfiable;
        }
        return RangeSpec::One(len.saturating_sub(n), len - 1);
    }
    let Ok(start) = a.parse::<usize>() else {
        return RangeSpec::Full;
    };
    let end = if b.is_empty() {
        len.saturating_sub(1)
    } else {
        match b.parse::<usize>() {
            Ok(e) => e.min(len.saturating_sub(1)),
            Err(_) => return RangeSpec::Full,
        }
    };
    if start >= len || start > end {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::One(start, end)
}
