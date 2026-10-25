//! The piece API of a storage node, and a client for it. Plain HTTP/1.1 over `std::net`
//! (like the node RPC, no TLS: put a proxy in front of it to expose it).
//!
//! | Request | Meaning |
//! | --- | --- |
//! | `PUT /piece/<file>/<index>` | Store a piece. Header `X-Btb-Path`: hex of the Merkle path from the piece root to the file tree root. The node checks that the chain assigned this piece to it and that the piece belongs to the file id. |
//! | `GET /piece/<file>/<index>` | The piece bytes; `X-Btb-Path` carries its path. |
//! | `GET /proof/<file>/<index>/<chunk>` | An encoded `ChunkProof` for that 1 KiB chunk. |
//! | `GET /health` | `ok` |

use crate::erasure::verify_piece;
use crate::store::PieceStore;
use crate::{hex, proof, unhex, unhex32, Hash};
use btb_codec::Encode;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// What a node needs to know about a file to accept one of its pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub k: u8,
    pub m: u8,
    pub piece_len: u64,
}

/// Decides whether this node may store piece `index` of `file` (the chain says so), and
/// returns the file's parameters.
pub type Authorize = Arc<dyn Fn(&Hash, u8) -> Result<FileMeta, String> + Send + Sync>;

const MAX_HEADER: usize = 16 * 1024;
const MAX_CONNS: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(
        listen: SocketAddr,
        store: Arc<PieceStore>,
        auth: Authorize,
        max_piece: usize,
    ) -> std::io::Result<Server> {
        let listener = TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        if active.load(Ordering::Relaxed) >= MAX_CONNS {
                            continue; // drop: too many connections
                        }
                        active.fetch_add(1, Ordering::Relaxed);
                        let (store, auth, active) = (store.clone(), auth.clone(), active.clone());
                        std::thread::spawn(move || {
                            let _ = conn.set_nonblocking(false);
                            let _ = handle(conn, &store, &auth, max_piece);
                            active.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Ok(Server {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Request {
    method: String,
    path: String,
    path_header: Option<String>,
    body: Vec<u8>,
}

fn read_request(conn: &mut TcpStream, max_body: usize) -> Result<Request, (u16, String)> {
    let bad = |m: &str| (400u16, m.to_string());
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > MAX_HEADER {
            return Err(bad("header too large"));
        }
        let n = conn.read(&mut chunk).map_err(|_| bad("read"))?;
        if n == 0 {
            return Err(bad("closed"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| bad("header utf8"))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, path) = (
        first.next().unwrap_or("").to_string(),
        first.next().unwrap_or("").to_string(),
    );
    let (mut len, mut path_header) = (0usize, None);
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                len = v.parse().map_err(|_| bad("content-length"))?;
            } else if k == "x-btb-path" {
                path_header = Some(v.to_string());
            }
        }
    }
    if len > max_body {
        return Err((413, "piece too large".into()));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = conn.read(&mut chunk).map_err(|_| bad("read body"))?;
        if n == 0 {
            return Err(bad("short body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Request {
        method,
        path,
        path_header,
        body,
    })
}

fn respond(conn: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
    let _ = conn.flush();
}

/// `respond`, returning `Ok(())` so a handler can `return reply(..)`.
fn reply(
    conn: &mut TcpStream,
    status: u16,
    headers: &[(&str, String)],
    body: &[u8],
) -> std::io::Result<()> {
    respond(conn, status, headers, body);
    Ok(())
}

fn parse_path(h: &str) -> Result<Vec<Hash>, String> {
    let b = unhex(h)?;
    if !b.len().is_multiple_of(32) || b.len() > 255 * 32 {
        return Err("path must be a multiple of 32 bytes".into());
    }
    Ok(b.chunks(32).map(|c| c.try_into().expect("32")).collect())
}

fn handle(
    mut conn: TcpStream,
    store: &PieceStore,
    auth: &Authorize,
    max_piece: usize,
) -> std::io::Result<()> {
    conn.set_read_timeout(Some(IO_TIMEOUT))?;
    conn.set_write_timeout(Some(IO_TIMEOUT))?;
    let req = match read_request(&mut conn, max_piece) {
        Ok(r) => r,
        Err((code, msg)) => {
            respond(&mut conn, code, &[], msg.as_bytes());
            return Ok(());
        }
    };
    let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let parse_piece = |p: &[&str]| -> Result<(Hash, u8), String> {
        Ok((
            unhex32(p.first().ok_or("missing file")?)?,
            p.get(1)
                .ok_or("missing index")?
                .parse::<u8>()
                .map_err(|_| "bad index".to_string())?,
        ))
    };
    match (req.method.as_str(), parts.as_slice()) {
        ("GET", ["health"]) => respond(&mut conn, 200, &[], b"ok"),
        ("PUT", ["piece", rest @ ..]) if rest.len() == 2 => {
            let (file, index) = match parse_piece(rest) {
                Ok(v) => v,
                Err(e) => return reply(&mut conn, 400, &[], e.as_bytes()),
            };
            let meta = match auth(&file, index) {
                Ok(m) => m,
                Err(e) => return reply(&mut conn, 403, &[], e.as_bytes()),
            };
            let path = match req.path_header.as_deref().map(parse_path) {
                Some(Ok(p)) => p,
                _ => return reply(&mut conn, 400, &[], b"missing or bad X-Btb-Path"),
            };
            if !verify_piece(
                &file,
                meta.size,
                meta.k,
                meta.m,
                meta.piece_len,
                index.into(),
                &req.body,
                &path,
            ) {
                return reply(&mut conn, 400, &[], b"piece does not match the file id");
            }
            match store.put(&file, index, &req.body, &path) {
                Ok(()) => respond(&mut conn, 201, &[], b"stored"),
                Err(e) => respond(&mut conn, 500, &[], e.to_string().as_bytes()),
            }
        }
        ("GET", ["piece", rest @ ..]) if rest.len() == 2 => {
            let (file, index) = match parse_piece(rest) {
                Ok(v) => v,
                Err(e) => return reply(&mut conn, 400, &[], e.as_bytes()),
            };
            match store.get(&file, index) {
                Ok(Some((piece, path))) => {
                    let h = path.concat();
                    respond(&mut conn, 200, &[("X-Btb-Path", hex(&h))], &piece)
                }
                Ok(None) => respond(&mut conn, 404, &[], b"no such piece"),
                Err(e) => respond(&mut conn, 500, &[], e.to_string().as_bytes()),
            }
        }
        ("GET", ["proof", rest @ ..]) if rest.len() == 3 => {
            let (file, index) = match parse_piece(rest) {
                Ok(v) => v,
                Err(e) => return reply(&mut conn, 400, &[], e.as_bytes()),
            };
            let Ok(chunk) = rest[2].parse::<usize>() else {
                return reply(&mut conn, 400, &[], b"bad chunk");
            };
            match store.get(&file, index) {
                Ok(Some((piece, path))) => match chunk_proof(&piece, chunk, path) {
                    Some(p) => respond(&mut conn, 200, &[], &p.encode()),
                    None => respond(&mut conn, 404, &[], b"no such chunk"),
                },
                Ok(None) => respond(&mut conn, 404, &[], b"no such piece"),
                Err(e) => respond(&mut conn, 500, &[], e.to_string().as_bytes()),
            }
        }
        _ => respond(&mut conn, 404, &[], b"not found"),
    }
    Ok(())
}

/// The proof for chunk `chunk` of a stored piece with its stored file-level path.
pub fn chunk_proof(piece: &[u8], chunk: usize, piece_path: Vec<Hash>) -> Option<proof::ChunkProof> {
    let leaves: Vec<Hash> = piece.chunks(proof::CHUNK).map(proof::chunk_leaf).collect();
    let c = piece.chunks(proof::CHUNK).nth(chunk)?;
    Some(proof::ChunkProof {
        chunk: c.to_vec(),
        chunk_path: proof::path(&leaves, chunk),
        piece_path,
    })
}

// ------------------------------------------------------------------ client

/// Status, lower-cased headers, body.
type Response = (u16, Vec<(String, String)>, Vec<u8>);

fn request(
    endpoint: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<Response, String> {
    let host = endpoint
        .strip_prefix("http://")
        .unwrap_or(endpoint)
        .trim_end_matches('/');
    let addr = host
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} does not resolve"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| format!("connect {host}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(60))).ok();
    s.set_write_timeout(Some(Duration::from_secs(60))).ok();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())
        .and_then(|_| s.write_all(body))
        .map_err(|e| format!("send to {host}: {e}"))?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)
        .map_err(|e| format!("read from {host}: {e}"))?;
    let at = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("bad response from {host}"))?;
    let head = std::str::from_utf8(&raw[..at]).map_err(|_| "response header utf8")?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or("bad status line")?;
    let hs = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Ok((status, hs, raw[at + 4..].to_vec()))
}

/// Upload a piece with its file-level Merkle path.
pub fn put_piece(
    endpoint: &str,
    file: &Hash,
    index: u8,
    piece: &[u8],
    path: &[Hash],
) -> Result<(), String> {
    let (status, _, body) = request(
        endpoint,
        "PUT",
        &format!("/piece/{}/{index}", hex(file)),
        &[("X-Btb-Path", hex(&path.concat()))],
        piece,
    )?;
    if status == 201 {
        Ok(())
    } else {
        Err(format!(
            "{endpoint} refused piece {index}: {status} {}",
            String::from_utf8_lossy(&body)
        ))
    }
}

/// Download a piece and its Merkle path (the caller verifies them against the file id).
pub fn get_piece(endpoint: &str, file: &Hash, index: u8) -> Result<(Vec<u8>, Vec<Hash>), String> {
    let (status, headers, body) = request(
        endpoint,
        "GET",
        &format!("/piece/{}/{index}", hex(file)),
        &[],
        &[],
    )?;
    if status != 200 {
        return Err(format!("{endpoint}: piece {index}: status {status}"));
    }
    let h = headers
        .iter()
        .find(|(k, _)| k == "x-btb-path")
        .map(|(_, v)| v.as_str())
        .ok_or("response has no X-Btb-Path")?;
    Ok((body, parse_path(h)?))
}

/// Ask a node for a chunk proof (debugging and audits; the daemon builds its own).
pub fn get_proof(
    endpoint: &str,
    file: &Hash,
    index: u8,
    chunk: u64,
) -> Result<proof::ChunkProof, String> {
    use btb_codec::{Decode, Reader};
    let (status, _, body) = request(
        endpoint,
        "GET",
        &format!("/proof/{}/{index}/{chunk}", hex(file)),
        &[],
        &[],
    )?;
    if status != 200 {
        return Err(format!("{endpoint}: proof status {status}"));
    }
    let mut r = Reader::new(&body, 1 << 20);
    proof::ChunkProof::decode_from(&mut r).map_err(|e| format!("bad proof: {e:?}"))
}
