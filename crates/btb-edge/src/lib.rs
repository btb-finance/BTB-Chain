//! BTB Chain edge caching node (`docs/EDGE.md`).
//!
//! An edge node sits between users and hosting nodes like a CDN. It serves
//!
//! * **site files** (`/site/<contract>/<path>`): cached by SHA-256, immutable, kept until the LRU
//!   evicts them; every body is checked against the hash the hosting node advertises before it is
//!   cached or served;
//! * **large files** (`/file/<root>`, and site files that point at a storage root): fetched from
//!   storage nodes with `btb-storage` (any `k` of `n` pieces, each verified, the whole file
//!   verified against the root) and cached by root;
//! * **query results** (`/c/<contract>/query/<fn>`): cached per (contract, function, arguments,
//!   caller, block height). The newest result stays valid until an upstream SSE subscription
//!   reports a change. `?proof=1` bundles are passed through untouched: clients still verify.
//!
//! Concurrent misses for the same object share one upstream fetch, hosts fail over, ranges and
//! `ETag`/`304` are supported, and requests beyond a free quota need a payment-channel voucher
//! ([`pay`]).

pub mod cache;
pub mod chain;
pub mod flight;
pub mod metrics;
pub mod pay;
pub mod upstream;
pub mod wire;

use cache::{Cache, Hash, Key, QKey};
use chain::{Rpc, StorageSource, Submitter};
use flight::Flight;
use metrics::Metrics;
use pay::{Pay, PayConfig};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use upstream::Upstream;
use wire::{parse_range, pct_encode, RangeSpec, Request, Response};

/// Settings of an edge node.
#[derive(Clone)]
pub struct EdgeConfig {
    pub listen: SocketAddr,
    /// Memory budget of the cache, in bytes.
    pub mem_bytes: usize,
    /// Disk tier for content-addressed objects: directory and byte budget.
    pub disk: Option<(PathBuf, u64)>,
    /// How long a site path is trusted to still name the same hash before the edge asks the
    /// host again (with `If-None-Match`, so an unchanged file costs no body transfer).
    pub site_ttl: Duration,
    /// Largest object the edge will fetch and keep.
    pub max_object_bytes: usize,
    /// Most upstream SSE subscriptions (one per distinct cached "latest" query). Beyond it,
    /// queries are passed through uncached.
    pub max_subscriptions: usize,
    pub max_connections: usize,
    /// Payments. `None`: serve everything for free.
    pub pay: Option<PayConfig>,
    /// How often to settle vouchers on chain. `None`: only on [`Edge::redeem`].
    pub redeem_every: Option<Duration>,
}

impl EdgeConfig {
    pub fn new(listen: SocketAddr) -> Self {
        EdgeConfig {
            listen,
            mem_bytes: 256 << 20,
            disk: None,
            site_ttl: Duration::from_secs(5),
            max_object_bytes: 1 << 30,
            max_subscriptions: 1024,
            max_connections: 512,
            pay: None,
            redeem_every: None,
        }
    }
}

/// What the edge talks to.
pub struct Services {
    pub upstream: Arc<dyn Upstream>,
    /// Large files by root. Without it `/file/<root>` answers 501.
    pub storage: Option<Arc<dyn StorageSource>>,
    /// Node RPC, used to look up payment channels. Required when `pay` is set.
    pub rpc: Option<Arc<dyn Rpc>>,
    /// Settles vouchers. Without it vouchers are only accumulated.
    pub submitter: Option<Arc<dyn Submitter>>,
}

#[derive(Clone)]
struct Fail {
    status: u16,
    msg: String,
}

fn fail(status: u16, msg: impl Into<String>) -> Fail {
    Fail {
        status,
        msg: msg.into(),
    }
}

/// A servable object.
#[derive(Clone)]
struct Obj {
    bytes: Arc<Vec<u8>>,
    etag: String,
    ctype: String,
    /// `HIT`, `MISS`, `REVALIDATED` or `COALESCED`.
    state: &'static str,
    extra: Vec<(String, String)>,
}

#[derive(Clone)]
enum SiteTarget {
    Blob(Hash),
    File(Hash),
}

#[derive(Clone)]
struct SiteRef {
    target: SiteTarget,
    ctype: String,
    checked: Instant,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct QBase {
    contract: String,
    func: String,
    args: String,
    caller: String,
    proof: bool,
}

impl QBase {
    fn q(&self, height: u64) -> QKey {
        QKey {
            contract: self.contract.clone(),
            func: self.func.clone(),
            args: self.args.clone(),
            caller: self.caller.clone(),
            proof: self.proof,
            height,
        }
    }
    fn skey(&self) -> SKey {
        (
            self.contract.clone(),
            self.func.clone(),
            self.args.clone(),
            self.caller.clone(),
        )
    }
}

type SKey = (String, String, String, String);

struct LVal {
    height: u64,
    result: String,
    sub: Arc<SubState>,
}

struct SubState {
    gen: AtomicU64,
    alive: AtomicBool,
    first: Mutex<Option<String>>,
}

struct Core {
    cfg: EdgeConfig,
    up: Arc<dyn Upstream>,
    storage: Option<Arc<dyn StorageSource>>,
    cache: Cache,
    metrics: Arc<Metrics>,
    pay: Option<Pay>,
    site_map: Mutex<HashMap<(String, String), SiteRef>>,
    site_flight: Flight<(String, String), Result<Obj, Fail>>,
    file_flight: Flight<Hash, Result<Obj, Fail>>,
    query_flight: Flight<QBase, Result<(Obj, u64), Fail>>,
    latest: Mutex<HashMap<QBase, LVal>>,
    subs: Mutex<HashMap<SKey, Arc<SubState>>>,
    conns: AtomicUsize,
    stop: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn hex32(s: &str) -> Option<Hash> {
    btb_storage::unhex32(s).ok()
}

fn content_type_of(r: &wire::UResp) -> String {
    r.header("content-type")
        .unwrap_or("application/octet-stream")
        .to_string()
}

fn err_text(r: &wire::UResp) -> String {
    serde_json::from_slice::<serde_json::Value>(&r.body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| format!("upstream answered {}", r.status))
}

impl Core {
    // ------------------------------------------------------------------ sites

    fn obj_of_ref(&self, r: &SiteRef) -> Option<Obj> {
        match &r.target {
            SiteTarget::Blob(h) => {
                let bytes = self.cache.get(&Key::Blob(*h))?;
                let tag = btb_storage::hex(h);
                Some(Obj {
                    bytes,
                    etag: tag.clone(),
                    ctype: r.ctype.clone(),
                    state: "HIT",
                    extra: vec![("X-BTB-Content-Hash".into(), tag)],
                })
            }
            SiteTarget::File(root) => self.obj_of_file(root, &r.ctype),
        }
    }

    fn obj_of_file(&self, root: &Hash, ctype: &str) -> Option<Obj> {
        let bytes = self.cache.get(&Key::File(*root))?;
        let tag = btb_storage::hex(root);
        Some(Obj {
            bytes,
            etag: tag.clone(),
            ctype: ctype.to_string(),
            state: "HIT",
            extra: vec![("X-BTB-Storage-Root".into(), tag)],
        })
    }

    fn site(&self, contract: &str, path: &str) -> Result<Obj, Fail> {
        let key = (contract.to_string(), path.to_string());
        let known = lock(&self.site_map).get(&key).cloned();
        if let Some(r) = &known {
            if r.checked.elapsed() < self.cfg.site_ttl {
                if let Some(o) = self.obj_of_ref(r) {
                    return Ok(o);
                }
            }
        }
        let (res, leader) = self.site_flight.run(key.clone(), || {
            self.site_fetch(&key.0, &key.1, known.as_ref())
        });
        let mut o = res.unwrap_or_else(|| Err(fail(502, "upstream fetch failed")))?;
        if !leader {
            self.metrics.inc("coalesced");
            o.state = "COALESCED";
        }
        Ok(o)
    }

    fn site_fetch(&self, contract: &str, path: &str, known: Option<&SiteRef>) -> Result<Obj, Fail> {
        let key = (contract.to_string(), path.to_string());
        // Someone may have refreshed it while this call waited to become the leader.
        if let Some(r) = lock(&self.site_map).get(&key) {
            if r.checked.elapsed() < self.cfg.site_ttl {
                if let Some(o) = self.obj_of_ref(r) {
                    return Ok(o);
                }
            }
        }
        let mut headers = vec![("X-BTB-Storage".to_string(), "redirect".to_string())];
        let mut conditional = None;
        if let Some(SiteRef {
            target: SiteTarget::Blob(h),
            ..
        }) = known
        {
            if self.cache.get(&Key::Blob(*h)).is_some() {
                headers.push((
                    "If-None-Match".into(),
                    format!("\"{}\"", btb_storage::hex(h)),
                ));
                conditional = Some(*h);
            }
        }
        let metrics = &self.metrics;
        let check = |r: &wire::UResp| -> Result<(), String> {
            if r.status != 200 || r.header("x-btb-storage-root").is_some() {
                return Ok(());
            }
            let want = r
                .header("x-btb-content-hash")
                .and_then(hex32)
                .ok_or("response has no valid X-BTB-Content-Hash")?;
            if Sha256::digest(&r.body).as_slice() != want {
                metrics.inc("tampered_rejected");
                return Err("content does not match its advertised SHA-256".into());
            }
            Ok(())
        };
        let target = format!("/site/{contract}/{path}");
        self.metrics.inc("upstream_requests");
        let r = self
            .up
            .get(&target, &headers, &check)
            .map_err(|e| fail(502, e))?;
        match r.status {
            304 => {
                conditional.ok_or_else(|| fail(502, "unexpected 304 from upstream"))?;
                let mut m = lock(&self.site_map);
                let sref = m
                    .get_mut(&key)
                    .ok_or_else(|| fail(502, "site entry vanished"))?;
                sref.checked = Instant::now();
                let sref = sref.clone();
                drop(m);
                let mut o = self
                    .obj_of_ref(&sref)
                    .ok_or_else(|| fail(502, "evicted during revalidation"))?;
                o.state = "REVALIDATED";
                Ok(o)
            }
            200 => {
                self.metrics.add("upstream_bytes", r.body.len() as u64);
                let ctype = content_type_of(&r);
                if let Some(root) = r.header("x-btb-storage-root").and_then(hex32) {
                    let mut o = self.file(&root, &ctype)?;
                    if o.state == "HIT" {
                        o.state = "REVALIDATED";
                    }
                    self.remember(&key, SiteTarget::File(root), &ctype);
                    return Ok(o);
                }
                let h = r
                    .header("x-btb-content-hash")
                    .and_then(hex32)
                    .ok_or_else(|| fail(502, "no content hash"))?;
                if r.body.len() > self.cfg.max_object_bytes {
                    return Err(fail(502, "object above the configured size limit"));
                }
                let bytes = Arc::new(r.body);
                self.cache.put(Key::Blob(h), bytes.clone());
                self.remember(&key, SiteTarget::Blob(h), &ctype);
                let tag = btb_storage::hex(&h);
                Ok(Obj {
                    bytes,
                    etag: tag.clone(),
                    ctype,
                    state: "MISS",
                    extra: vec![("X-BTB-Content-Hash".into(), tag)],
                })
            }
            s => {
                lock(&self.site_map).remove(&key);
                Err(fail(
                    if s == 404 || s == 400 { s } else { 502 },
                    err_text(&r),
                ))
            }
        }
    }

    fn remember(&self, key: &(String, String), target: SiteTarget, ctype: &str) {
        let mut m = lock(&self.site_map);
        if m.len() >= 50_000 {
            m.clear();
        }
        m.insert(
            key.clone(),
            SiteRef {
                target,
                ctype: ctype.to_string(),
                checked: Instant::now(),
            },
        );
    }

    // ------------------------------------------------------------ storage files

    fn file(&self, root: &Hash, ctype: &str) -> Result<Obj, Fail> {
        if let Some(o) = self.obj_of_file(root, ctype) {
            return Ok(o);
        }
        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| fail(501, "this edge has no storage access"))?;
        let (res, leader) = self.file_flight.run(*root, || {
            if let Some(o) = self.obj_of_file(root, ctype) {
                return Ok(o);
            }
            self.metrics.inc("storage_fetches");
            let data = storage.fetch(root).map_err(|e| {
                self.metrics.inc("storage_failures");
                fail(502, format!("storage fetch failed: {e}"))
            })?;
            if data.len() > self.cfg.max_object_bytes {
                return Err(fail(502, "object above the configured size limit"));
            }
            self.metrics.add("storage_bytes", data.len() as u64);
            let bytes = Arc::new(data);
            self.cache.put(Key::File(*root), bytes.clone());
            let tag = btb_storage::hex(root);
            Ok(Obj {
                bytes,
                etag: tag.clone(),
                ctype: ctype.to_string(),
                state: "MISS",
                extra: vec![("X-BTB-Storage-Root".into(), tag)],
            })
        });
        let mut o = res.unwrap_or_else(|| Err(fail(502, "storage fetch failed")))?;
        if !leader {
            self.metrics.inc("coalesced");
            o.state = "COALESCED";
        }
        Ok(o)
    }

    // ----------------------------------------------------------------- queries

    fn query_obj(
        &self,
        base: &QBase,
        height: u64,
        bytes: Arc<Vec<u8>>,
        state: &'static str,
    ) -> Obj {
        let etag = btb_storage::hex(&Sha256::digest(bytes.as_slice()));
        Obj {
            bytes,
            etag,
            ctype: if base.proof {
                "application/json".into()
            } else {
                "text/plain".into()
            },
            state,
            extra: vec![("X-BTB-Height".into(), height.to_string())],
        }
    }

    fn query(self: &Arc<Self>, base: &QBase, want_height: Option<u64>) -> Result<Obj, Fail> {
        if let Some(h) = want_height {
            return match self.cache.get(&Key::Query(base.q(h))) {
                Some(b) => Ok(self.query_obj(base, h, b, "HIT")),
                None => Err(fail(404, "no cached result at that height")),
            };
        }
        let cur = lock(&self.latest)
            .get(base)
            .filter(|l| l.sub.alive.load(Ordering::SeqCst))
            .map(|l| l.height);
        if let Some(h) = cur {
            if let Some(b) = self.cache.get(&Key::Query(base.q(h))) {
                return Ok(self.query_obj(base, h, b, "HIT"));
            }
        }
        let (res, leader) = self
            .query_flight
            .run(base.clone(), || self.query_fetch(base));
        let (mut o, _) = res.unwrap_or_else(|| Err(fail(502, "upstream fetch failed")))?;
        if !leader {
            self.metrics.inc("coalesced");
            o.state = "COALESCED";
        }
        Ok(o)
    }

    fn ensure_sub(self: &Arc<Self>, base: &QBase) -> Option<Arc<SubState>> {
        let skey = base.skey();
        let mut subs = lock(&self.subs);
        if let Some(s) = subs.get(&skey) {
            if s.alive.load(Ordering::SeqCst) {
                return Some(s.clone());
            }
        }
        if subs.len() >= self.cfg.max_subscriptions {
            return None;
        }
        let sub = Arc::new(SubState {
            gen: AtomicU64::new(0),
            alive: AtomicBool::new(true),
            first: Mutex::new(None),
        });
        subs.insert(skey.clone(), sub.clone());
        let target = format!(
            "/c/{}/subscribe/{}?args={}&caller={}",
            base.contract,
            pct_encode(&base.func),
            pct_encode(&base.args),
            pct_encode(&base.caller)
        );
        let (core, sub2) = (self.clone(), sub.clone());
        let spawned = std::thread::Builder::new()
            .name("btb-edge-sub".into())
            .spawn(move || core.run_sub(skey, sub2, target));
        if spawned.is_err() {
            sub.alive.store(false, Ordering::SeqCst);
            subs.remove(&base.skey());
            return None;
        }
        Some(sub)
    }

    fn run_sub(&self, skey: SKey, sub: Arc<SubState>, target: String) {
        self.metrics.inc("subscriptions_opened");
        if let Ok(events) = self.up.subscribe(&target) {
            for (i, ev) in events.enumerate() {
                let Ok(data) = ev else { break };
                if self.stop.load(Ordering::SeqCst) {
                    break;
                }
                let mut latest = lock(&self.latest);
                let before = latest.len();
                if i == 0 {
                    // The first event is the result at registration time: anything cached that
                    // differs was fetched before a block the stream already knows about.
                    *lock(&sub.first) = Some(data.clone());
                    latest.retain(|b, l| b.skey() != skey || l.result == data);
                } else {
                    sub.gen.fetch_add(1, Ordering::SeqCst);
                    *lock(&sub.first) = None; // only meaningful until the first block
                    latest.retain(|b, _| b.skey() != skey);
                }
                self.metrics
                    .add("invalidations", (before - latest.len()) as u64);
            }
        }
        // Stream over (error, upstream gone, or shutdown): nothing it guarded may be served.
        sub.alive.store(false, Ordering::SeqCst);
        let mut latest = lock(&self.latest);
        let before = latest.len();
        latest.retain(|b, _| b.skey() != skey);
        self.metrics
            .add("invalidations", (before - latest.len()) as u64);
        let mut subs = lock(&self.subs);
        if subs.get(&skey).is_some_and(|s| Arc::ptr_eq(s, &sub)) {
            subs.remove(&skey);
        }
    }

    fn query_fetch(self: &Arc<Self>, base: &QBase) -> Result<(Obj, u64), Fail> {
        // Subscribed first and generation read before fetching, so a block that lands while the
        // fetch is in flight is noticed (see `run_sub`).
        let sub = self.ensure_sub(base);
        let gen0 = sub.as_ref().map(|s| s.gen.load(Ordering::SeqCst));
        let mut target = format!(
            "/c/{}/query/{}?args={}&caller={}",
            base.contract,
            pct_encode(&base.func),
            pct_encode(&base.args),
            pct_encode(&base.caller)
        );
        if base.proof {
            target.push_str("&proof=1");
        }
        self.metrics.inc("upstream_requests");
        let r = self
            .up
            .get(&target, &[], &|_| Ok(()))
            .map_err(|e| fail(502, e))?;
        if r.status != 200 {
            // Aborts and unknown functions are answers, not cacheable state.
            return Err(fail(
                if matches!(r.status, 400 | 404) {
                    r.status
                } else {
                    502
                },
                err_text(&r),
            ));
        }
        self.metrics.add("upstream_bytes", r.body.len() as u64);
        let height: u64 = r
            .header("x-btb-height")
            .and_then(|h| h.parse().ok())
            .ok_or_else(|| fail(502, "response has no X-BTB-Height"))?;
        let result = if base.proof {
            serde_json::from_slice::<serde_json::Value>(&r.body)
                .ok()
                .and_then(|v| v["result"].as_str().map(str::to_string))
                .ok_or_else(|| fail(502, "proof response has no result"))?
        } else {
            String::from_utf8_lossy(&r.body).into_owned()
        };
        let bytes = Arc::new(r.body);
        if let Some(sub) = sub {
            let mut latest = lock(&self.latest);
            let unchanged = Some(sub.gen.load(Ordering::SeqCst)) == gen0;
            let first_ok = lock(&sub.first).as_ref().is_none_or(|f| *f == result);
            if unchanged && first_ok && sub.alive.load(Ordering::SeqCst) {
                self.cache.put(Key::Query(base.q(height)), bytes.clone());
                latest.insert(
                    base.clone(),
                    LVal {
                        height,
                        result,
                        sub,
                    },
                );
            }
        }
        Ok((self.query_obj(base, height, bytes, "MISS"), height))
    }

    // ------------------------------------------------------------------ routes

    fn handle(self: &Arc<Self>, req: &Request, ip: IpAddr) -> Response {
        self.metrics.inc("requests");
        if req.method == "OPTIONS" {
            return Response::new(204, "text/plain", Vec::new());
        }
        let segs: Vec<&str> = req.path.trim_start_matches('/').splitn(4, '/').collect();
        match (req.method.as_str(), segs.as_slice()) {
            (_, ["mutation", ..]) | (_, ["c", _, "mutation", ..]) => {
                return Response::error(
                    501,
                    "edge nodes do not run mutations: send a signed transaction to a validator",
                )
            }
            ("GET" | "HEAD", ["health"]) => {
                return Response::json(200, &serde_json::json!({"ok": true}))
            }
            ("GET" | "HEAD", ["metrics"]) => return self.metrics_response(),
            ("GET" | "HEAD", _) => {}
            _ => return Response::error(405, "use GET"),
        }
        if let Some(pay) = &self.pay {
            let v = req.headers.get("x-btb-voucher").map(String::as_str);
            if let Err(reason) = pay.admit(ip, v) {
                let mut r = Response::json(
                    402,
                    &serde_json::json!({
                        "error": reason,
                        "price": pay.price(),
                        "payee": btb_storage::hex(&self.cfg.pay.as_ref().map_or([0; 32], |p| p.payee)),
                    }),
                );
                r.header("X-BTB-Price", pay.price().to_string());
                return r;
            }
        }
        let (kind, res) = match segs.as_slice() {
            ["site", c, rest @ ..] => ("site", self.route_site(c, &rest.join("/"))),
            ["site"] => (
                "site",
                Err(fail(400, "name a contract: /site/<contract>/<path>")),
            ),
            ["file", root] => ("file", self.route_file(root)),
            ["c", c, "query", f] => ("query", self.route_query(req, c, f)),
            ["c", _, "subscribe", _] => return Response::error(
                501,
                "subscribe on a hosting node: the edge answers queries and invalidates them itself",
            ),
            ["proof", ..] | ["api", "site", ..] => return self.passthrough(req),
            ["status"] | [""] => return self.status_response(),
            _ => return Response::error(404, "not found"),
        };
        match res {
            Ok(o) => {
                self.count(kind, o.state);
                self.respond(req, o, kind)
            }
            Err(f) => Response::error(f.status, &f.msg),
        }
    }

    fn count(&self, kind: &str, state: &str) {
        let hit = matches!(state, "HIT" | "REVALIDATED");
        self.metrics.inc(if hit { "hits" } else { "misses" });
        self.metrics.inc(match (kind, hit) {
            ("site", true) => "site_hits",
            ("site", false) => "site_misses",
            ("file", true) => "file_hits",
            ("file", false) => "file_misses",
            (_, true) => "query_hits",
            (_, false) => "query_misses",
        });
    }

    fn route_site(&self, contract: &str, path: &str) -> Result<Obj, Fail> {
        let c = hex32(contract)
            .map(|h| btb_storage::hex(&h))
            .ok_or_else(|| fail(400, "contract must be 64 hex characters"))?;
        let mut path = path.to_string();
        if path.is_empty() || path.ends_with('/') {
            path.push_str("index.html");
        }
        if btb_chain::site::check_path(&path).is_err() {
            return Err(fail(400, "bad path"));
        }
        let mut o = self.site(&c, &path)?;
        o.extra
            .push(("X-Content-Type-Options".into(), "nosniff".into()));
        o.extra.push((
            "Content-Security-Policy".into(),
            "sandbox allow-scripts allow-forms allow-popups".into(),
        ));
        Ok(o)
    }

    fn route_file(&self, root: &str) -> Result<Obj, Fail> {
        let root = hex32(root).ok_or_else(|| fail(400, "root must be 64 hex characters"))?;
        self.file(&root, "application/octet-stream")
    }

    fn route_query(
        self: &Arc<Self>,
        req: &Request,
        contract: &str,
        func: &str,
    ) -> Result<Obj, Fail> {
        let c = hex32(contract)
            .map(|h| btb_storage::hex(&h))
            .ok_or_else(|| fail(400, "contract must be 64 hex characters"))?;
        let want_height = match req.query.get("height") {
            None => None,
            Some(h) => Some(
                h.parse::<u64>()
                    .map_err(|_| fail(400, "height must be a number"))?,
            ),
        };
        let base = QBase {
            contract: c,
            func: func.to_string(),
            args: req.query.get("args").cloned().unwrap_or_default(),
            caller: req
                .headers
                .get("x-btb-caller")
                .or_else(|| req.query.get("caller"))
                .cloned()
                .unwrap_or_else(|| "anonymous".into()),
            proof: req.query.get("proof").is_some_and(|p| p == "1"),
        };
        self.query(&base, want_height)
    }

    fn passthrough(&self, req: &Request) -> Response {
        let mut target = req
            .path
            .split('/')
            .map(|s| {
                if s.is_empty() {
                    String::new()
                } else {
                    pct_encode(s)
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let q: Vec<String> = req
            .query
            .iter()
            .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
            .collect();
        if !q.is_empty() {
            target.push('?');
            target.push_str(&q.join("&"));
        }
        self.metrics.inc("passthrough");
        self.metrics.inc("upstream_requests");
        match self.up.get(&target, &[], &|_| Ok(())) {
            Ok(r) => {
                self.metrics.add("upstream_bytes", r.body.len() as u64);
                let mut out = Response::new(r.status, &content_type_of(&r), r.body);
                out.header("Cache-Control", "no-store");
                out
            }
            Err(e) => Response::error(502, &e),
        }
    }

    /// Build the response for an object: `ETag`/`304`, ranges, cache headers.
    fn respond(&self, req: &Request, o: Obj, kind: &str) -> Response {
        let len = o.bytes.len();
        let tag = format!("\"{}\"", o.etag);
        let mut r = Response {
            status: 200,
            headers: vec![("Content-Type".into(), o.ctype.clone())],
            body: o.bytes.clone(),
            start: 0,
            end: len,
        };
        r.header("ETag", tag);
        r.header("Accept-Ranges", "bytes");
        r.header("X-BTB-Cache", o.state);
        r.header(
            "Cache-Control",
            if kind == "file" {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
        );
        for (k, v) in o.extra {
            r.header(&k, v);
        }
        if req.headers.get("if-none-match").is_some_and(|v| {
            v.split(',')
                .map(|t| t.trim().trim_start_matches("W/").trim_matches('"'))
                .any(|t| t == o.etag || t == "*")
        }) {
            r.status = 304;
            r.start = 0;
            r.end = 0;
            return r;
        }
        match parse_range(req.headers.get("range").map(String::as_str), len) {
            RangeSpec::Full => {}
            RangeSpec::One(a, b) => {
                r.status = 206;
                r.start = a;
                r.end = b + 1;
                r.header("Content-Range", format!("bytes {a}-{b}/{len}"));
            }
            RangeSpec::Unsatisfiable => {
                let mut e = Response::error(416, "range not satisfiable");
                e.header("Content-Range", format!("bytes */{len}"));
                return e;
            }
        }
        r
    }

    fn gauges(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("cache_memory_bytes", self.cache.mem_bytes() as u64),
            ("cache_memory_entries", self.cache.mem_entries() as u64),
            ("cache_disk_bytes", self.cache.disk_bytes()),
            ("subscriptions", lock(&self.subs).len() as u64),
            (
                "unsettled_units",
                self.pay.as_ref().map_or(0, Pay::unsettled),
            ),
        ]
    }

    fn metrics_response(&self) -> Response {
        Response::new(
            200,
            "text/plain; version=0.0.4",
            self.metrics.render(&self.gauges()).into_bytes(),
        )
    }

    fn status_response(&self) -> Response {
        let mut v = serde_json::json!({"role": "edge-node"});
        for (k, n) in self.gauges() {
            v[k] = n.into();
        }
        v["hits"] = self.metrics.get("hits").into();
        v["misses"] = self.metrics.get("misses").into();
        Response::json(200, &v)
    }
}

// ------------------------------------------------------------------------- server

/// A running edge node.
pub struct Edge {
    core: Arc<Core>,
    addr: SocketAddr,
    threads: Vec<JoinHandle<()>>,
}

impl Edge {
    pub fn start(cfg: EdgeConfig, svc: Services) -> Result<Edge, String> {
        let metrics = Arc::new(Metrics::default());
        let pay = match &cfg.pay {
            Some(p) => {
                let rpc = svc
                    .rpc
                    .clone()
                    .ok_or("payments need node RPC to look up channels")?;
                Some(Pay::new(
                    p.clone(),
                    rpc,
                    svc.submitter.clone(),
                    metrics.clone(),
                ))
            }
            None => None,
        };
        let cache = Cache::new(cfg.mem_bytes, cfg.disk.clone())
            .map_err(|e| format!("cache directory: {e}"))?;
        let listener =
            TcpListener::bind(cfg.listen).map_err(|e| format!("bind {}: {e}", cfg.listen))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let core = Arc::new(Core {
            up: svc.upstream,
            storage: svc.storage,
            cache,
            metrics,
            pay,
            site_map: Mutex::new(HashMap::new()),
            site_flight: Flight::default(),
            file_flight: Flight::default(),
            query_flight: Flight::default(),
            latest: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            conns: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            cfg,
        });
        let mut threads = Vec::new();
        let c = core.clone();
        threads.push(
            std::thread::Builder::new()
                .name("btb-edge-http".into())
                .spawn(move || {
                    while !c.stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((conn, _)) => {
                                let _ = conn.set_nonblocking(false);
                                let c = c.clone();
                                let _ = std::thread::Builder::new()
                                    .name("btb-edge-conn".into())
                                    .spawn(move || serve_conn(&c, conn));
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
        if let (Some(every), true) = (core.cfg.redeem_every, core.pay.is_some()) {
            let c = core.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("btb-edge-redeem".into())
                    .spawn(move || {
                        let mut waited = Duration::ZERO;
                        while !c.stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(50));
                            waited += Duration::from_millis(50);
                            if waited >= every {
                                waited = Duration::ZERO;
                                if let Some(p) = &c.pay {
                                    p.redeem();
                                }
                            }
                        }
                    })
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(Edge {
            core,
            addr,
            threads,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn metrics(&self) -> &Metrics {
        &self.core.metrics
    }

    /// Settle every channel that earned enough, now. Empty without payments or a submitter.
    pub fn redeem(&self) -> Vec<pay::Redeemed> {
        self.core.pay.as_ref().map_or(Vec::new(), Pay::redeem)
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.core.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Edge {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_conn(core: &Arc<Core>, mut conn: TcpStream) {
    let n = core.conns.fetch_add(1, Ordering::SeqCst);
    let resp;
    let mut head = false;
    if n >= core.cfg.max_connections {
        resp = Response::error(503, "too many connections");
    } else {
        match wire::read_request(&conn) {
            Ok(req) => {
                head = req.method == "HEAD";
                let ip = conn
                    .peer_addr()
                    .map_or(IpAddr::from([0, 0, 0, 0]), |a| a.ip());
                resp = core.handle(&req, ip);
            }
            Err(e) => resp = Response::error(400, &e),
        }
    }
    if let Ok(sent) = wire::write_response(&mut conn, &resp, head) {
        core.metrics.add("bytes_out", sent as u64);
    }
    core.conns.fetch_sub(1, Ordering::SeqCst);
}
