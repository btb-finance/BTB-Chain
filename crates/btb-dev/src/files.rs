//! File storage emulation for the dev server (DEV ONLY).
//!
//! The same API as a hosting node in front of storage nodes, with no nodes: uploaded bytes are
//! kept as plain files, and what the chain would record about them is written to the dev state
//! in the chain's own layout, so `file_info` / `file_pin` in contracts behave the same.
//!
//! * `POST /upload[?periods=N&k=4&m=2&store=1]` (raw body): the file id (the **same root**
//!   `btb store` would print, from the real erasure coding) and, unless `store=0`, a free
//!   `StoreFile` for `periods` payment periods (default 1000; the dev chain counts 10 blocks to
//!   a period). Answers `{"root","size","k","m","stored","url"}`.
//! * `POST /file/<root>/store[?periods=N]`: record an uploaded file as stored (the client-paid
//!   `StoreFile` of a real chain; here it is free).
//! * `POST /upload/<root>/push`: a no-op (a hosting node sends the pieces to storage nodes).
//! * `GET /file/<root>`: the bytes, with the headers a hosting node sends (immutable, `ETag`).
//!
//! `file_pin` from a contract takes the escrow from the contract's balance and keeps it (there
//! are no nodes to pay), and a file reads `expired` once the dev height passes its expiry.

use super::*;
use btb_chain::storage::{record_nodeless, ChainFiles, StorageConfig, StorageOp};
use btb_runtime::{FileService, Overlay};
use btb_vm::host::{FilePin, FileRecord};

/// Largest upload.
pub const MAX_UPLOAD: usize = 64 << 20;
const DEFAULT_PERIODS: u32 = 1000;

/// Storage prices of the dev chain: the real defaults, with short periods so expiry can be
/// tried out.
pub fn dev_config() -> StorageConfig {
    StorageConfig {
        epoch_blocks: 10,
        challenge_blocks: 5,
        repair_blocks: 5,
        ..StorageConfig::default()
    }
}

pub struct DevFiles {
    dir: PathBuf,
    cfg: StorageConfig,
}

impl DevFiles {
    /// Keep files in `dir` (created), or in a temporary directory named after `port`.
    pub fn open(dir: Option<PathBuf>, port: u16) -> Result<Self, String> {
        let dir = dir.unwrap_or_else(|| {
            std::env::temp_dir().join(format!("btb-dev-files-{}-{port}", std::process::id()))
        });
        std::fs::create_dir_all(&dir).map_err(|e| format!("files dir {}: {e}", dir.display()))?;
        Ok(DevFiles {
            dir,
            cfg: dev_config(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, root: &[u8; 32], ext: &str) -> PathBuf {
        self.dir.join(format!("{}.{ext}", demo::hex(root)))
    }

    /// Encode `data` for its root and keep the bytes. Returns `(root, size)`.
    fn put(&self, data: &[u8], k: u8, m: u8) -> Result<([u8; 32], u64), String> {
        let enc = btb_storage::erasure::encode(data, k, m).map_err(|e| e.to_string())?;
        let tmp = self.dir.join(format!(".tmp-{}", demo::hex(&enc.id)));
        std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, self.path(&enc.id, "bin")).map_err(|e| e.to_string())?;
        std::fs::write(
            self.path(&enc.id, "meta"),
            format!("{} {} {}", enc.size, k, m),
        )
        .map_err(|e| e.to_string())?;
        Ok((enc.id, enc.size))
    }

    fn get(&self, root: &[u8; 32]) -> Option<Vec<u8>> {
        std::fs::read(self.path(root, "bin")).ok()
    }

    /// `(size, k, m)` of an uploaded file.
    fn meta(&self, root: &[u8; 32]) -> Option<(u64, u8, u8)> {
        let t = std::fs::read_to_string(self.path(root, "meta")).ok()?;
        let mut p = t.split_whitespace().map(|x| x.parse::<u64>().ok());
        Some((p.next()??, p.next()?? as u8, p.next()?? as u8))
    }

    /// Roots with bytes on disk and their sizes.
    pub fn list(&self) -> Vec<([u8; 32], u64)> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if let Some(h) = n.strip_suffix(".bin") {
                    if let Some(root) = demo::unhex(h).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
                        out.push((root, e.metadata().map_or(0, |m| m.len())));
                    }
                }
            }
        }
        out.sort();
        out
    }

    fn chain(&self) -> ChainFiles {
        ChainFiles {
            cfg: self.cfg,
            nodeless: true,
        }
    }

    /// What `file_info` says about `root` at `height`.
    pub fn status(
        &self,
        store: &dyn KvStore,
        root: &[u8; 32],
        height: u64,
    ) -> Result<FileRecord, KvError> {
        self.chain().info(store, root, height)
    }
}

impl FileService for DevFiles {
    fn info(&self, kv: &dyn KvStore, root: &[u8; 32], height: u64) -> Result<FileRecord, KvError> {
        self.status(kv, root, height)
    }
    fn pin(
        &self,
        kv: &mut dyn KvStore,
        contract: &Address,
        pin: &FilePin,
        height: u64,
    ) -> Result<u64, KvError> {
        self.chain().pin(kv, contract, pin, height)
    }
}

fn reply(
    conn: &mut TcpStream,
    status: &str,
    ctype: &str,
    extra: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        conn,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    )?;
    conn.write_all(body)
}

fn parse_root(s: &str) -> Option<[u8; 32]> {
    demo::unhex(s).and_then(|b| b.try_into().ok())
}

/// Record `root` as stored for free (the client-paid `StoreFile` of the dev chain).
fn record<S: KvStore + Send + 'static>(
    sh: &Arc<Shared<S>>,
    root: [u8; 32],
    size: u64,
    k: u8,
    m: u8,
    periods: u32,
    owner: [u8; 32],
) -> Result<bool, String> {
    let mut guard = sh.state.lock().unwrap();
    let st = &mut *guard;
    let height = st.height;
    let op = StorageOp::Store {
        root,
        size,
        k,
        m,
        periods,
    };
    op.check_static().map_err(|e| format!("{e:?}"))?;
    let cfg = sh.files.cfg;
    let c = st
        .occ
        .execute_with_retry::<_, String>(MAX_RETRIES, |store, _| {
            let mut ov = Overlay::new(store);
            let fresh =
                record_nodeless(&mut ov, &cfg, &owner, height, &op, false).map_err(|e| e.0)?;
            let (batch, reads) = ov.into_parts();
            Ok((fresh, batch, reads))
        })
        .map_err(|e| format!("{e:?}"))?;
    if !c.batch.is_empty() {
        st.height += 1;
        let ev = admin::commit_event(st.height, &c.batch, &sh.names());
        st.admin.retain(|tx| tx.send(ev.clone()).is_ok());
        // `file_info` readers (subscriptions) may have changed.
        let mut dead = Vec::new();
        for id in st.subs.affected(&c.batch) {
            let Some(client) = st.clients.get(&id) else {
                continue;
            };
            let (who, h) = (client.caller, st.height);
            let store = st.occ.store();
            match st.subs.refresh(id, |ct, f, a| {
                run(sh, store, FunctionKind::Query, ct, f, a, who, h).map(|(o, _, r)| (o, r))
            }) {
                Ok(Some(ch)) => {
                    if client.tx.send(ch.result).is_err() {
                        dead.push(id);
                    }
                }
                Ok(None) => {}
                Err(_) => dead.push(id),
            }
        }
        for id in dead {
            st.subs.unsubscribe(id);
            st.clients.remove(&id);
        }
    }
    Ok(c.output)
}

pub fn route<S: KvStore + Send + 'static>(
    sh: &Arc<Shared<S>>,
    conn: &mut TcpStream,
    req: &Request,
) -> std::io::Result<()> {
    let caller = caller_of(
        req.headers
            .get("x-btb-caller")
            .or_else(|| req.query.get("caller"))
            .map_or("anonymous", String::as_str),
    );
    let num = |k: &str, d: u64| req.query.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let parts: Vec<&str> = req.path.trim_start_matches('/').split('/').collect();
    match (req.method.as_str(), parts.as_slice()) {
        ("POST", ["upload"]) => {
            if req.body.is_empty() {
                return err_json(conn, "400 Bad Request", "cannot store an empty file");
            }
            let (k, m) = (num("k", 4) as u8, num("m", 2) as u8);
            let periods = num("periods", u64::from(DEFAULT_PERIODS)) as u32;
            let (root, size) = match sh.files.put(&req.body, k, m) {
                Ok(r) => r,
                Err(e) => return err_json(conn, "400 Bad Request", &e),
            };
            let store = req.query.get("store").is_none_or(|v| v != "0");
            if store {
                if let Err(e) = record(sh, root, size, k, m, periods, caller) {
                    return err_json(conn, "400 Bad Request", &e);
                }
            }
            let h = demo::hex(&root);
            respond(
                conn,
                "200 OK",
                "application/json",
                &format!(
                    "{{\"root\":\"{h}\",\"size\":{size},\"k\":{k},\"m\":{m},\"stored\":{store},\"url\":\"/file/{h}\"}}"
                ),
            )
        }
        // Same call as on a hosting node; here there are no nodes to send pieces to.
        ("POST", ["upload", root, "push"]) => {
            let Some(root) = parse_root(root) else {
                return err_json(conn, "400 Bad Request", "root must be 64 hex characters");
            };
            if sh.files.meta(&root).is_none() {
                return err_json(conn, "404 Not Found", "no such upload");
            }
            respond(
                conn,
                "200 OK",
                "application/json",
                "{\"pushed\":0,\"failed\":[],\"dev\":true}",
            )
        }
        ("POST", ["file", root, "store"]) => {
            let Some(root) = parse_root(root) else {
                return err_json(conn, "400 Bad Request", "root must be 64 hex characters");
            };
            let Some((size, k, m)) = sh.files.meta(&root) else {
                return err_json(conn, "404 Not Found", "upload the bytes first");
            };
            let periods = num("periods", u64::from(DEFAULT_PERIODS)) as u32;
            match record(sh, root, size, k, m, periods, caller) {
                Ok(fresh) => respond(
                    conn,
                    "200 OK",
                    "application/json",
                    &format!("{{\"stored\":true,\"already\":{}}}", !fresh),
                ),
                Err(e) => err_json(conn, "400 Bad Request", &e),
            }
        }
        ("GET", ["file", root]) => {
            let Some(root) = parse_root(root) else {
                return err_json(conn, "400 Bad Request", "root must be 64 hex characters");
            };
            let tag = demo::hex(&root);
            let extra = format!(
                "ETag: \"{tag}\"\r\nX-BTB-Storage-Root: {tag}\r\nCache-Control: public, max-age=31536000, immutable\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: sandbox allow-scripts allow-forms allow-popups\r\nAccess-Control-Allow-Origin: *\r\n"
            );
            match sh.files.get(&root) {
                Some(data) => reply(conn, "200 OK", "application/octet-stream", &extra, &data),
                None => err_json(conn, "404 Not Found", "no such file"),
            }
        }
        _ => err_json(conn, "404 Not Found", "not found"),
    }
}
