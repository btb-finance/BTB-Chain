//! Edge node tests: caching, verification, ranges, coalescing, invalidation, storage-backed
//! files, payments and failover. Upstreams are fakes (or tiny TCP servers for failover).

use btb_chain::SigningKey;
use btb_edge::chain::{ChainStorage, Rpc, RpcChain, StorageSource, Submitter};
use btb_edge::pay::PayConfig;
use btb_edge::upstream::{Check, Events, HttpUpstream, Upstream};
use btb_edge::wire::{self, UResp};
use btb_edge::{Edge, EdgeConfig, Services};
use btb_storage::erasure;
use btb_storage::http::{FileMeta, Server};
use btb_storage::store::PieceStore;
use btb_voucher::{SignedVoucher, Spender};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const C: &str = "abababababababababababababababababababababababababababababababab";

fn sha(b: &[u8]) -> String {
    btb_storage::hex(&Sha256::digest(b))
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("btb-edge-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ---------------------------------------------------------------- fake upstream

struct SiteFile {
    body: Vec<u8>,
    /// Hash the host claims (differs from `sha(body)` for a tampering host).
    claimed: String,
    root: Option<String>,
}

#[derive(Default)]
struct Fake {
    files: Mutex<HashMap<String, SiteFile>>,
    site_gets: AtomicUsize,
    delay_ms: AtomicU64,
    qval: AtomicU64,
    qheight: AtomicU64,
    query_gets: AtomicUsize,
    subs: Mutex<Vec<Sender<Result<String, String>>>>,
    sub_opens: AtomicUsize,
    down: AtomicBool,
}

impl Fake {
    fn new() -> Arc<Fake> {
        let f = Fake::default();
        f.qval.store(1, Ordering::SeqCst);
        f.qheight.store(10, Ordering::SeqCst);
        Arc::new(f)
    }
    fn publish(&self, path: &str, body: &[u8]) {
        self.files.lock().unwrap().insert(
            format!("/site/{C}/{path}"),
            SiteFile {
                body: body.to_vec(),
                claimed: sha(body),
                root: None,
            },
        );
    }
    fn publish_tampered(&self, path: &str, claimed_body: &[u8], served: &[u8]) {
        self.files.lock().unwrap().insert(
            format!("/site/{C}/{path}"),
            SiteFile {
                body: served.to_vec(),
                claimed: sha(claimed_body),
                root: None,
            },
        );
    }
    fn publish_ref(&self, path: &str, root: &[u8; 32]) {
        self.files.lock().unwrap().insert(
            format!("/site/{C}/{path}"),
            SiteFile {
                body: Vec::new(),
                claimed: String::new(),
                root: Some(btb_storage::hex(root)),
            },
        );
    }
    fn qhex(&self) -> String {
        format!("{:016x}", self.qval.load(Ordering::SeqCst))
    }
    /// A new block changed the query result: bump it and tell subscribers.
    fn new_block(&self, val: u64) {
        self.qval.store(val, Ordering::SeqCst);
        self.qheight.fetch_add(1, Ordering::SeqCst);
        for s in self.subs.lock().unwrap().iter() {
            let _ = s.send(Ok(self.qhex()));
        }
    }
    fn resp(status: u16, headers: &[(&str, String)], body: Vec<u8>) -> UResp {
        UResp {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            body,
        }
    }
}

impl Upstream for Fake {
    fn get(
        &self,
        target: &str,
        headers: &[(String, String)],
        check: Check<'_>,
    ) -> Result<UResp, String> {
        if self.down.load(Ordering::SeqCst) {
            return Err("host down".into());
        }
        let d = self.delay_ms.load(Ordering::SeqCst);
        let (path, qs) = target.split_once('?').unwrap_or((target, ""));
        let r = if path.starts_with("/site/") {
            self.site_gets.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(d));
            let files = self.files.lock().unwrap();
            match files.get(path) {
                None => Fake::resp(
                    404,
                    &[("content-type", "application/json".into())],
                    br#"{"error":"no such file"}"#.to_vec(),
                ),
                Some(f) if f.root.is_some() => Fake::resp(
                    200,
                    &[
                        ("content-type", "video/mp4".into()),
                        ("x-btb-storage-root", f.root.clone().unwrap()),
                    ],
                    Vec::new(),
                ),
                Some(f) => {
                    let inm = headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("if-none-match"))
                        .map(|(_, v)| v.trim_matches('"').to_string());
                    if inm.as_deref() == Some(f.claimed.as_str()) {
                        Fake::resp(304, &[], Vec::new())
                    } else {
                        Fake::resp(
                            200,
                            &[
                                ("content-type", "text/plain".into()),
                                ("x-btb-content-hash", f.claimed.clone()),
                            ],
                            f.body.clone(),
                        )
                    }
                }
            }
        } else if path.contains("/query/") {
            self.query_gets.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(d));
            let h = self.qheight.load(Ordering::SeqCst);
            let result = self.qhex();
            if qs.contains("proof=1") {
                let b = json!({"result": result, "height": h, "proof": {"marker": "merkle"}});
                Fake::resp(
                    200,
                    &[("x-btb-height", h.to_string())],
                    serde_json::to_vec(&b).unwrap(),
                )
            } else {
                Fake::resp(200, &[("x-btb-height", h.to_string())], result.into_bytes())
            }
        } else if path.starts_with("/proof/") {
            Fake::resp(200, &[], br#"{"proof":"raw"}"#.to_vec())
        } else {
            Fake::resp(404, &[], Vec::new())
        };
        check(&r)?;
        Ok(r)
    }

    fn subscribe(&self, _target: &str) -> Result<Events, String> {
        self.sub_opens.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        let _ = tx.send(Ok(self.qhex()));
        self.subs.lock().unwrap().push(tx);
        Ok(Box::new(rx.into_iter()))
    }
}

fn start(fake: &Arc<Fake>, f: impl FnOnce(&mut EdgeConfig)) -> Edge {
    start_with(fake.clone(), None, None, None, f)
}

fn start_with(
    up: Arc<dyn Upstream>,
    storage: Option<Arc<dyn StorageSource>>,
    rpc: Option<Arc<dyn Rpc>>,
    submitter: Option<Arc<dyn Submitter>>,
    f: impl FnOnce(&mut EdgeConfig),
) -> Edge {
    let mut cfg = EdgeConfig::new("127.0.0.1:0".parse().unwrap());
    cfg.mem_bytes = 1 << 20;
    cfg.site_ttl = Duration::from_secs(60);
    f(&mut cfg);
    Edge::start(
        cfg,
        Services {
            upstream: up,
            storage,
            rpc,
            submitter,
        },
    )
    .unwrap()
}

fn get(e: &Edge, path: &str, headers: &[(&str, &str)]) -> UResp {
    let h: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    wire::request(
        &e.addr().to_string(),
        "GET",
        path,
        &h,
        &[],
        Duration::from_secs(10),
        64 << 20,
    )
    .unwrap()
}

fn cache_state(r: &UResp) -> &str {
    r.header("x-btb-cache").unwrap_or("")
}

// ------------------------------------------------------------------------ caching

#[test]
fn site_file_miss_then_hit() {
    let fake = Fake::new();
    fake.publish("index.html", b"<h1>hello</h1>");
    let e = start(&fake, |_| {});
    let a = get(&e, &format!("/site/{C}/"), &[]);
    assert_eq!(a.status, 200);
    assert_eq!(a.body, b"<h1>hello</h1>");
    assert_eq!(cache_state(&a), "MISS");
    assert_eq!(
        a.header("x-btb-content-hash"),
        Some(sha(b"<h1>hello</h1>").as_str())
    );
    assert!(a
        .header("content-security-policy")
        .unwrap()
        .starts_with("sandbox"));
    let b = get(&e, &format!("/site/{C}/index.html"), &[]);
    assert_eq!(b.body, a.body);
    assert_eq!(cache_state(&b), "HIT");
    assert_eq!(fake.site_gets.load(Ordering::SeqCst), 1);
    assert_eq!(e.metrics().get("hits"), 1);
    assert_eq!(e.metrics().get("misses"), 1);
    // Missing files are passed through as 404s and not cached.
    assert_eq!(get(&e, &format!("/site/{C}/nope.txt"), &[]).status, 404);
    // Bad input is refused without asking upstream.
    let before = fake.site_gets.load(Ordering::SeqCst);
    assert_eq!(get(&e, &format!("/site/{C}/a/../b"), &[]).status, 400);
    assert_eq!(get(&e, "/site/xyz/index.html", &[]).status, 400);
    assert_eq!(fake.site_gets.load(Ordering::SeqCst), before);
}

#[test]
fn etag_and_304() {
    let fake = Fake::new();
    fake.publish("app.js", b"console.log(1)");
    let e = start(&fake, |_| {});
    let a = get(&e, &format!("/site/{C}/app.js"), &[]);
    let tag = a.header("etag").unwrap().to_string();
    assert_eq!(tag, format!("\"{}\"", sha(b"console.log(1)")));
    let b = get(&e, &format!("/site/{C}/app.js"), &[("If-None-Match", &tag)]);
    assert_eq!(b.status, 304);
    assert!(b.body.is_empty());
    let c = get(
        &e,
        &format!("/site/{C}/app.js"),
        &[("If-None-Match", "\"other\"")],
    );
    assert_eq!(c.status, 200);
}

#[test]
fn republished_file_is_picked_up_after_ttl_with_conditional_fetch() {
    let fake = Fake::new();
    fake.publish("a.txt", b"one");
    let e = start(&fake, |c| c.site_ttl = Duration::from_millis(50));
    let url = format!("/site/{C}/a.txt");
    assert_eq!(get(&e, &url, &[]).body, b"one");
    std::thread::sleep(Duration::from_millis(80));
    // Unchanged: revalidated with a 304, no body transfer, still counted as a hit.
    let r = get(&e, &url, &[]);
    assert_eq!(r.body, b"one");
    assert_eq!(cache_state(&r), "REVALIDATED");
    fake.publish("a.txt", b"two");
    std::thread::sleep(Duration::from_millis(80));
    let r = get(&e, &url, &[]);
    assert_eq!(r.body, b"two");
    assert_eq!(cache_state(&r), "MISS");
    assert_eq!(fake.site_gets.load(Ordering::SeqCst), 3);
}

#[test]
fn tampered_upstream_content_is_rejected_and_not_cached() {
    let fake = Fake::new();
    fake.publish_tampered("index.html", b"<h1>real</h1>", b"<h1>evil</h1>");
    let e = start(&fake, |_| {});
    let url = format!("/site/{C}/index.html");
    let r = get(&e, &url, &[]);
    assert_eq!(r.status, 502);
    assert!(String::from_utf8_lossy(&r.body).contains("SHA-256"));
    assert_eq!(e.metrics().get("tampered_rejected"), 1);
    // Nothing was cached: once the host serves the real file, it is fetched and served.
    fake.publish("index.html", b"<h1>real</h1>");
    let r = get(&e, &url, &[]);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"<h1>real</h1>");
    assert_eq!(cache_state(&r), "MISS");
}

#[test]
fn missing_content_hash_is_rejected() {
    let fake = Fake::new();
    fake.publish("x.txt", b"x");
    fake.files
        .lock()
        .unwrap()
        .get_mut(&format!("/site/{C}/x.txt"))
        .unwrap()
        .claimed = "zz".into();
    let e = start(&fake, |_| {});
    assert_eq!(get(&e, &format!("/site/{C}/x.txt"), &[]).status, 502);
}

#[test]
fn byte_ranges() {
    let fake = Fake::new();
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    fake.publish("big.bin", &data);
    let e = start(&fake, |_| {});
    let url = format!("/site/{C}/big.bin");
    let full = get(&e, &url, &[]);
    assert_eq!(full.header("accept-ranges"), Some("bytes"));
    let r = get(&e, &url, &[("Range", "bytes=10-19")]);
    assert_eq!(r.status, 206);
    assert_eq!(r.body, &data[10..20]);
    assert_eq!(r.header("content-range"), Some("bytes 10-19/1000"));
    let r = get(&e, &url, &[("Range", "bytes=990-")]);
    assert_eq!((r.status, r.body.as_slice()), (206, &data[990..]));
    assert_eq!(r.header("content-range"), Some("bytes 990-999/1000"));
    let r = get(&e, &url, &[("Range", "bytes=-5")]);
    assert_eq!((r.status, r.body.as_slice()), (206, &data[995..]));
    let r = get(&e, &url, &[("Range", "bytes=900-5000")]);
    assert_eq!(r.header("content-range"), Some("bytes 900-999/1000"));
    let r = get(&e, &url, &[("Range", "bytes=1000-")]);
    assert_eq!(r.status, 416);
    assert_eq!(r.header("content-range"), Some("bytes */1000"));
    // Several ranges: served whole.
    let r = get(&e, &url, &[("Range", "bytes=0-1,5-6")]);
    assert_eq!((r.status, r.body.len()), (200, 1000));
    // Ranges are served from cache: one upstream fetch in total.
    assert_eq!(fake.site_gets.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_misses_share_one_upstream_fetch() {
    let fake = Fake::new();
    fake.publish("hot.html", b"popular");
    fake.delay_ms.store(300, Ordering::SeqCst);
    let e = Arc::new(start(&fake, |_| {}));
    let n = 12;
    let hs: Vec<_> = (0..n)
        .map(|_| {
            let e = e.clone();
            std::thread::spawn(move || get(&e, &format!("/site/{C}/hot.html"), &[]))
        })
        .collect();
    for h in hs {
        let r = h.join().unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"popular"[..]));
    }
    assert_eq!(fake.site_gets.load(Ordering::SeqCst), 1);
    assert_eq!(e.metrics().get("coalesced"), n - 1);
}

#[test]
fn concurrent_queries_share_one_upstream_fetch() {
    let fake = Fake::new();
    fake.delay_ms.store(300, Ordering::SeqCst);
    let e = Arc::new(start(&fake, |_| {}));
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let e = e.clone();
            std::thread::spawn(move || get(&e, &format!("/c/{C}/query/get?args=00"), &[]))
        })
        .collect();
    for h in hs {
        assert_eq!(h.join().unwrap().status, 200);
    }
    assert_eq!(fake.query_gets.load(Ordering::SeqCst), 1);
}

// ------------------------------------------------------------------------ queries

fn wait_for(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not reached");
}

#[test]
fn query_cached_until_new_block_invalidates() {
    let fake = Fake::new();
    let e = start(&fake, |_| {});
    let url = format!("/c/{C}/query/get?args=00&caller=alice");
    let a = get(&e, &url, &[]);
    assert_eq!(
        (a.status, a.body.as_slice()),
        (200, &b"0000000000000001"[..])
    );
    assert_eq!(a.header("x-btb-height"), Some("10"));
    assert_eq!(cache_state(&a), "MISS");
    let b = get(&e, &url, &[]);
    assert_eq!(cache_state(&b), "HIT");
    assert_eq!(b.body, a.body);
    assert_eq!(fake.query_gets.load(Ordering::SeqCst), 1);
    assert_eq!(fake.sub_opens.load(Ordering::SeqCst), 1);
    // Another caller is another cache entry.
    let c = get(&e, &format!("/c/{C}/query/get?args=00&caller=bob"), &[]);
    assert_eq!(cache_state(&c), "MISS");

    // A block changes the result: the subscription event drops the cached "latest".
    fake.new_block(2);
    wait_for(|| e.metrics().get("invalidations") >= 2);
    let d = get(&e, &url, &[]);
    assert_eq!(d.body, b"0000000000000002");
    assert_eq!(d.header("x-btb-height"), Some("11"));
    assert_eq!(cache_state(&d), "MISS");
    assert_eq!(cache_state(&get(&e, &url, &[])), "HIT");
    // The old height is still addressable; unknown heights are not made up.
    let old = get(&e, &format!("{url}&height=10"), &[]);
    assert_eq!(
        (old.status, old.body.as_slice()),
        (200, &b"0000000000000001"[..])
    );
    assert_eq!(get(&e, &format!("{url}&height=3"), &[]).status, 404);
}

#[test]
fn query_proofs_pass_through_untouched() {
    let fake = Fake::new();
    let e = start(&fake, |_| {});
    let url = format!("/c/{C}/query/get?args=00&proof=1");
    let a = get(&e, &url, &[]);
    let v: Value = serde_json::from_slice(&a.body).unwrap();
    assert_eq!(v["proof"]["marker"], "merkle");
    assert_eq!(v["result"], "0000000000000001");
    let b = get(&e, &url, &[]);
    assert_eq!(b.body, a.body);
    assert_eq!(cache_state(&b), "HIT");
    // The plain variant is its own entry; both go stale together.
    get(&e, &format!("/c/{C}/query/get?args=00"), &[]);
    fake.new_block(5);
    wait_for(|| e.metrics().get("invalidations") >= 2);
    let c = get(&e, &url, &[]);
    assert_eq!(
        serde_json::from_slice::<Value>(&c.body).unwrap()["result"],
        "0000000000000005"
    );
    // Proof routes are relayed, not cached.
    let p = get(&e, &format!("/proof/{C}?key=00"), &[]);
    assert_eq!(p.body, br#"{"proof":"raw"}"#);
}

#[test]
fn stale_query_is_not_served_when_the_subscription_dies() {
    let fake = Fake::new();
    let e = start(&fake, |_| {});
    let url = format!("/c/{C}/query/get?args=01");
    get(&e, &url, &[]);
    assert_eq!(cache_state(&get(&e, &url, &[])), "HIT");
    // The stream ends: blocks could be missed from now on, so the entry may not be served.
    fake.subs.lock().unwrap().clear();
    wait_for(|| e.metrics().get("invalidations") >= 1);
    fake.qval.store(9, Ordering::SeqCst);
    let r = get(&e, &url, &[]);
    assert_eq!(r.body, b"0000000000000009");
    assert_eq!(cache_state(&r), "MISS");
    wait_for(|| fake.sub_opens.load(Ordering::SeqCst) == 2);
}

#[test]
fn subscription_limit_bypasses_the_cache() {
    let fake = Fake::new();
    let e = start(&fake, |c| c.max_subscriptions = 1);
    get(&e, &format!("/c/{C}/query/get?args=01"), &[]);
    let u2 = format!("/c/{C}/query/get?args=02");
    assert_eq!(cache_state(&get(&e, &u2, &[])), "MISS");
    assert_eq!(cache_state(&get(&e, &u2, &[])), "MISS");
}

#[test]
fn mutations_are_not_served() {
    let fake = Fake::new();
    let e = start(&fake, |_| {});
    assert_eq!(get(&e, &format!("/c/{C}/mutation/x"), &[]).status, 501);
}

#[test]
fn metrics_endpoint() {
    let fake = Fake::new();
    fake.publish("a.txt", b"aaaa");
    let e = start(&fake, |_| {});
    let url = format!("/site/{C}/a.txt");
    get(&e, &url, &[]);
    get(&e, &url, &[]);
    std::thread::sleep(Duration::from_millis(50));
    let m = String::from_utf8(get(&e, "/metrics", &[]).body).unwrap();
    for want in [
        "btb_edge_hits 1",
        "btb_edge_misses 1",
        "btb_edge_site_hits 1",
        "btb_edge_bytes_out 8",
        "btb_edge_upstream_bytes 4",
        "btb_edge_cache_memory_bytes 4",
    ] {
        assert!(m.contains(want), "missing {want} in:\n{m}");
    }
}

// ---------------------------------------------------------------- cache budgets

#[test]
fn lru_evicts_by_bytes_and_spills_to_disk() {
    use btb_edge::cache::{Cache, Key};
    let dir = tmp("disk");
    let cache = Cache::new(250, Some((dir.clone(), 1000))).unwrap();
    let blob = |i: u8| Key::Blob([i; 32]);
    for i in 0..4u8 {
        cache.put(blob(i), Arc::new(vec![i; 100]));
    }
    // Memory holds at most 250 bytes: blobs 2 and 3 (the newest).
    assert_eq!(cache.mem_bytes(), 200);
    assert_eq!(cache.mem_entries(), 2);
    // The older ones are on disk, and come back verified.
    assert_eq!(*cache.get(&blob(0)).unwrap(), vec![0u8; 100]);
    assert!(cache.disk_bytes() > 0);
    // Disk budget 1000 with 132-byte files: older files are dropped.
    for i in 10..30u8 {
        cache.put(blob(i), Arc::new(vec![i; 100]));
    }
    assert!(cache.disk_bytes() <= 1000);
    assert!(cache.get(&blob(1)).is_none());
    // Bit rot on disk is a miss, not a wrong answer.
    let victim = (10..28u8).find(|i| {
        cache.mem_entries() > 0 && {
            let name = format!("b-{}", btb_storage::hex(&[*i; 32]));
            dir.join(name).exists()
        }
    });
    let i = victim.expect("some blob on disk");
    let path = dir.join(format!("b-{}", btb_storage::hex(&[i; 32])));
    let mut raw = std::fs::read(&path).unwrap();
    raw[0] ^= 1;
    std::fs::write(&path, raw).unwrap();
    assert!(cache.get(&blob(i)).is_none());
    assert!(!path.exists());
    // Without a disk tier, over-budget items are simply not kept.
    let mem_only = Cache::new(150, None).unwrap();
    mem_only.put(blob(1), Arc::new(vec![1; 100]));
    mem_only.put(blob(2), Arc::new(vec![2; 100]));
    assert!(mem_only.get(&blob(1)).is_none());
    assert!(mem_only.get(&blob(2)).is_some());
    mem_only.put(blob(3), Arc::new(vec![3; 1000]));
    assert!(mem_only.get(&blob(3)).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disk_tier_survives_a_restart() {
    use btb_edge::cache::{Cache, Key};
    let dir = tmp("restart");
    {
        let c = Cache::new(10, Some((dir.clone(), 10_000))).unwrap();
        c.put(Key::File([7; 32]), Arc::new(vec![7; 500]));
    }
    let c = Cache::new(10, Some((dir.clone(), 10_000))).unwrap();
    assert_eq!(c.disk_bytes(), 532);
    assert_eq!(*c.get(&Key::File([7; 32])).unwrap(), vec![7u8; 500]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ----------------------------------------------------------------- storage files

struct FakeStorageRpc {
    files: Mutex<HashMap<String, Value>>,
}

impl Rpc for FakeStorageRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        assert_eq!(method, "btb_storageFile");
        Ok(self
            .files
            .lock()
            .unwrap()
            .get(params[0].as_str().unwrap())
            .cloned()
            .unwrap_or(Value::Null))
    }
}

struct StorageNet {
    root: [u8; 32],
    data: Vec<u8>,
    stores: Vec<Arc<PieceStore>>,
    enc: erasure::Encoded,
    servers: Vec<Server>,
    rpc: Arc<FakeStorageRpc>,
    dirs: Vec<PathBuf>,
}

/// 2-of-4 erasure coded file spread over four real piece servers.
fn storage_net(name: &str, data: Vec<u8>) -> StorageNet {
    let (k, m) = (2u8, 2u8);
    let enc = erasure::encode(&data, k, m).unwrap();
    let meta = FileMeta {
        size: enc.size,
        k,
        m,
        piece_len: enc.piece_len as u64,
    };
    let (mut stores, mut servers, mut dirs, mut pieces) = (vec![], vec![], vec![], vec![]);
    for i in 0..(k + m) {
        let dir = tmp(&format!("{name}-node{i}"));
        let store = Arc::new(PieceStore::open(&dir).unwrap());
        let id = enc.id;
        let auth: btb_storage::http::Authorize = Arc::new(move |f, _| {
            if *f == id {
                Ok(meta)
            } else {
                Err("unknown file".into())
            }
        });
        let srv = Server::start(
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            store.clone(),
            auth,
            1 << 20,
        )
        .unwrap();
        let ep = srv.addr().to_string();
        btb_storage::http::put_piece(
            &ep,
            &enc.id,
            i,
            &enc.pieces[usize::from(i)],
            &enc.piece_path(usize::from(i)),
        )
        .unwrap();
        pieces.push(json!({
            "index": i, "node": btb_storage::hex(&[i + 1; 32]), "endpoint": ep,
            "state": "holding", "proven": 1, "missed": 0
        }));
        stores.push(store);
        servers.push(srv);
        dirs.push(dir);
    }
    let file = json!({
        "id": btb_storage::hex(&enc.id), "owner": btb_storage::hex(&[9; 32]),
        "size": enc.size, "k": k, "m": m, "piece_len": enc.piece_len,
        "escrow": "1000", "live": 4, "pieces": pieces
    });
    let rpc = Arc::new(FakeStorageRpc {
        files: Mutex::new([(btb_storage::hex(&enc.id), file)].into()),
    });
    StorageNet {
        root: enc.id,
        data,
        stores,
        enc,
        servers,
        rpc,
        dirs,
    }
}

impl StorageNet {
    fn source(&self) -> Arc<dyn StorageSource> {
        Arc::new(ChainStorage(Arc::new(RpcChain(self.rpc.clone()))))
    }
    /// Replace piece `i` on its node with garbage (the Merkle path still names the right piece).
    fn corrupt(&self, i: usize) {
        let mut bad = self.enc.pieces[i].clone();
        bad[0] ^= 0xff;
        self.stores[i]
            .put(&self.root, i as u8, &bad, &self.enc.piece_path(i))
            .unwrap();
    }
    fn cleanup(self) {
        drop(self.servers);
        for d in self.dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

#[test]
fn storage_backed_file_is_verified_with_k_of_n_pieces_and_cached() {
    let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let net = storage_net("ok", data);
    let fake = Fake::new();
    fake.publish_ref("movie.mp4", &net.root);
    let e = start_with(fake.clone(), Some(net.source()), None, None, |_| {});
    let url = format!("/file/{}", btb_storage::hex(&net.root));

    // One corrupt piece and one dead node: still k = 2 good pieces to rebuild from.
    net.corrupt(0);
    let r = get(&e, &url, &[]);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, net.data);
    assert_eq!(cache_state(&r), "MISS");
    assert_eq!(
        r.header("etag"),
        Some(format!("\"{}\"", btb_storage::hex(&net.root)).as_str())
    );
    assert!(r.header("cache-control").unwrap().contains("immutable"));

    // Cached by root: no storage traffic for the next request, ranges work on it.
    let r = get(&e, &url, &[("Range", "bytes=100-199")]);
    assert_eq!((r.status, r.body.as_slice()), (206, &net.data[100..200]));
    assert_eq!(cache_state(&r), "HIT");
    assert_eq!(e.metrics().get("storage_fetches"), 1);

    // A site path that the host answers with a storage root is served the same way.
    let r = get(&e, &format!("/site/{C}/movie.mp4"), &[]);
    assert_eq!((r.status, r.body.as_slice()), (200, net.data.as_slice()));
    assert_eq!(r.header("content-type"), Some("video/mp4"));
    assert_eq!(e.metrics().get("storage_fetches"), 1);
    assert_eq!(
        get(
            &e,
            &url,
            &[(
                "If-None-Match",
                &format!("\"{}\"", btb_storage::hex(&net.root))
            )]
        )
        .status,
        304
    );
    net.cleanup();
}

#[test]
fn storage_file_with_too_few_valid_pieces_is_refused() {
    let net = storage_net("bad", vec![42u8; 5000]);
    let e = start_with(Fake::new(), Some(net.source()), None, None, |_| {});
    for i in 0..3 {
        net.corrupt(i);
    }
    let url = format!("/file/{}", btb_storage::hex(&net.root));
    let r = get(&e, &url, &[]);
    assert_eq!(r.status, 502);
    assert_eq!(e.metrics().get("storage_failures"), 1);
    // Unknown roots and bad roots.
    assert_eq!(
        get(&e, &format!("/file/{}", btb_storage::hex(&[1; 32])), &[]).status,
        502
    );
    assert_eq!(get(&e, "/file/xyz", &[]).status, 400);
    // Once the pieces are repaired the file is served: nothing bad was cached.
    for i in 0..3 {
        net.stores[i]
            .put(
                &net.root,
                i as u8,
                &net.enc.pieces[i],
                &net.enc.piece_path(i),
            )
            .unwrap();
    }
    assert_eq!(get(&e, &url, &[]).body, vec![42u8; 5000]);
    net.cleanup();
}

#[test]
fn storage_file_without_storage_access_is_501() {
    let e = start(&Fake::new(), |_| {});
    assert_eq!(
        get(&e, &format!("/file/{}", btb_storage::hex(&[1; 32])), &[]).status,
        501
    );
}

// ---------------------------------------------------------------------- payments

const CHAIN_ID: u64 = 77;

struct ChanRpc {
    payee: [u8; 32],
    payer_key: [u8; 32],
    deposit: AtomicU64,
    open: AtomicBool,
    id: [u8; 32],
    lookups: AtomicUsize,
}

impl Rpc for ChanRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "btb_status" => Ok(json!({"chain_id": CHAIN_ID})),
            "btb_channel" => {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                if !self.open.load(Ordering::SeqCst)
                    || params[0].as_str() != Some(btb_storage::hex(&self.id).as_str())
                {
                    return Ok(Value::Null);
                }
                Ok(json!({
                    "payer": btb_storage::hex(&[1; 32]),
                    "payer_key": btb_storage::hex(&self.payer_key),
                    "payee": btb_storage::hex(&self.payee),
                    "deposit": self.deposit.load(Ordering::SeqCst).to_string(),
                    "expiry_height": 1000, "closing": null, "height": 5
                }))
            }
            m => Err(format!("unexpected {m}")),
        }
    }
}

#[derive(Default)]
struct Settle {
    closed: Mutex<Vec<SignedVoucher>>,
}
impl Submitter for Settle {
    fn close(&self, v: &SignedVoucher) -> Result<(), String> {
        self.closed.lock().unwrap().push(*v);
        Ok(())
    }
}

struct PayNet {
    edge: Edge,
    rpc: Arc<ChanRpc>,
    settle: Arc<Settle>,
    payer: SigningKey,
    spender: Spender,
    fake: Arc<Fake>,
}

fn pay_net(free: u64, price: u64, redeem_min: u64) -> PayNet {
    let payer = SigningKey::from_bytes(&[5; 32]);
    let id = [3u8; 32];
    let payee = [8u8; 32];
    let rpc = Arc::new(ChanRpc {
        payee,
        payer_key: payer.verifying_key().to_bytes(),
        deposit: AtomicU64::new(1000),
        open: AtomicBool::new(true),
        id,
        lookups: AtomicUsize::new(0),
    });
    let settle = Arc::new(Settle::default());
    let fake = Fake::new();
    fake.publish("a.txt", b"paid content");
    let edge = start_with(
        fake.clone(),
        None,
        Some(rpc.clone()),
        Some(settle.clone()),
        |c| {
            c.pay = Some(PayConfig {
                payee,
                chain_id: None,
                free_requests: free,
                window: Duration::from_secs(3600),
                price,
                redeem_min,
            });
        },
    );
    PayNet {
        edge,
        rpc,
        settle,
        payer,
        spender: Spender::new(CHAIN_ID, id, 1000),
        fake,
    }
}

fn hexv(v: &SignedVoucher) -> String {
    btb_storage::hex(&v.to_bytes())
}

impl PayNet {
    fn url(&self) -> String {
        format!("/site/{C}/a.txt")
    }
    fn req(&self, v: Option<&SignedVoucher>) -> UResp {
        match v {
            Some(v) => get(&self.edge, &self.url(), &[("X-BTB-Voucher", &hexv(v))]),
            None => get(&self.edge, &self.url(), &[]),
        }
    }
}

#[test]
fn free_quota_then_voucher_required() {
    let mut n = pay_net(2, 10, 1);
    assert_eq!(n.req(None).status, 200);
    assert_eq!(n.req(None).status, 200);
    let r = n.req(None);
    assert_eq!(r.status, 402);
    assert_eq!(r.header("x-btb-price"), Some("10"));
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert!(v["error"].as_str().unwrap().contains("quota"));
    assert_eq!(v["payee"], btb_storage::hex(&[8; 32]));
    // Pay: each request carries the next cumulative voucher.
    for _ in 0..3 {
        let sv = n.spender.pay(10, &n.payer).unwrap();
        let r = n.req(Some(&sv));
        assert_eq!((r.status, r.body.as_slice()), (200, &b"paid content"[..]));
    }
    assert_eq!(n.edge.metrics().get("paid_served"), 3);
    assert_eq!(n.edge.metrics().get("free_served"), 2);
    assert_eq!(n.edge.metrics().get("payment_required"), 1);
    // Health and metrics stay free.
    assert_eq!(get(&n.edge, "/health", &[]).status, 200);
    assert_eq!(get(&n.edge, "/metrics", &[]).status, 200);
}

#[test]
fn invalid_vouchers_are_rejected() {
    let mut n = pay_net(0, 10, 1);
    let good = n.spender.pay(10, &n.payer).unwrap();

    // Garbage and truncated vouchers.
    let r = get(&n.edge, &n.url(), &[("X-BTB-Voucher", "nothex")]);
    assert_eq!(r.status, 402);
    let r = get(&n.edge, &n.url(), &[("X-BTB-Voucher", &hexv(&good)[..100])]);
    assert_eq!(r.status, 402);

    // Signed by somebody else.
    let mut other = Spender::new(CHAIN_ID, [3; 32], 1000);
    let forged = other.pay(10, &SigningKey::from_bytes(&[6; 32])).unwrap();
    let r = n.req(Some(&forged));
    assert_eq!(r.status, 402);
    assert!(String::from_utf8_lossy(&r.body).contains("bad signature"));

    // Flipped signature bit; changed amount under the old signature.
    let mut bad = good;
    bad.signature[0] ^= 1;
    assert_eq!(n.req(Some(&bad)).status, 402);
    let mut bumped = good;
    bumped.voucher.amount = 1;
    assert_eq!(n.req(Some(&bumped)).status, 402);

    // Another chain, and another channel (unknown on chain).
    let mut wrong_chain = Spender::new(CHAIN_ID + 1, [3; 32], 1000);
    let wc = wrong_chain.pay(10, &n.payer).unwrap();
    let r = n.req(Some(&wc));
    assert_eq!(r.status, 402);
    assert!(String::from_utf8_lossy(&r.body).contains("wrong chain"));
    let mut wrong_chan = Spender::new(CHAIN_ID, [4; 32], 1000);
    let wch = wrong_chan.pay(10, &n.payer).unwrap();
    assert_eq!(n.req(Some(&wch)).status, 402);

    // Above the channel's deposit.
    let mut rich = Spender::new(CHAIN_ID, [3; 32], 5000);
    let big = rich.pay(2000, &n.payer).unwrap();
    assert_eq!(n.req(Some(&big)).status, 402);

    // Nothing above was accepted: the genuine first voucher still works.
    assert_eq!(n.req(Some(&good)).status, 200);
}

#[test]
fn channel_must_pay_this_edge() {
    let mut n = pay_net(0, 10, 1);
    let rpc2 = Arc::new(ChanRpc {
        payee: [99; 32],
        payer_key: n.payer.verifying_key().to_bytes(),
        deposit: AtomicU64::new(1000),
        open: AtomicBool::new(true),
        id: [3; 32],
        lookups: AtomicUsize::new(0),
    });
    let e = start_with(n.fake.clone(), None, Some(rpc2), None, |c| {
        c.pay = Some(PayConfig {
            payee: [8; 32],
            chain_id: Some(CHAIN_ID),
            free_requests: 0,
            window: Duration::from_secs(1),
            price: 10,
            redeem_min: 1,
        });
    });
    let sv = n.spender.pay(10, &n.payer).unwrap();
    let r = get(&e, &n.url(), &[("X-BTB-Voucher", &hexv(&sv))]);
    assert_eq!(r.status, 402);
    assert!(String::from_utf8_lossy(&r.body).contains("another account"));
}

#[test]
fn vouchers_must_grow_and_cover_the_price() {
    let mut n = pay_net(0, 10, 1);
    let v1 = n.spender.pay(10, &n.payer).unwrap(); // total 10
    let v2 = n.spender.pay(10, &n.payer).unwrap(); // total 20
    assert_eq!(n.req(Some(&v1)).status, 200);
    assert_eq!(n.req(Some(&v2)).status, 200);
    // Replay of an old voucher and of the newest: neither is new money.
    let r = n.req(Some(&v1));
    assert_eq!(r.status, 402);
    assert!(String::from_utf8_lossy(&r.body).contains("does not exceed"));
    assert_eq!(n.req(Some(&v2)).status, 402);
    // Same amount with a higher nonce is not an increase either.
    let mut same = v2;
    same.voucher.nonce = 99;
    let same = same.voucher.sign(&n.payer);
    assert_eq!(n.req(Some(&same)).status, 402);
    // A voucher that raises the total by less than the price is accepted as money but does not
    // pay for the request.
    let mut small = Spender::new(CHAIN_ID, [3; 32], 1000);
    small.pay(25, &n.payer).unwrap();
    let v3 = small.pay(5, &n.payer).unwrap(); // total 30, nonce 2: not above v2's nonce
    let mut v3 = v3.voucher;
    v3.nonce = 3;
    let v3 = v3.sign(&n.payer);
    let r = n.req(Some(&v3));
    assert_eq!(r.status, 200); // 30 covers 3 x 10
    let v4 = {
        let mut v = v3.voucher;
        v.amount = 35;
        v.nonce = 4;
        v.sign(&n.payer)
    };
    let r = n.req(Some(&v4));
    assert_eq!(r.status, 402, "35 < 40 owed for the fourth request");
    assert!(String::from_utf8_lossy(&r.body).contains("40 is owed"));
    // Topping up (a higher voucher) pays it.
    let v5 = {
        let mut v = v3.voucher;
        v.amount = 45;
        v.nonce = 5;
        v.sign(&n.payer)
    };
    assert_eq!(n.req(Some(&v5)).status, 200);
}

#[test]
fn deposit_top_up_is_picked_up() {
    let mut n = pay_net(0, 10, 1);
    let mut big = Spender::new(CHAIN_ID, [3; 32], 5000);
    big.pay(990, &n.payer).unwrap();
    let v = big.pay(20, &n.payer).unwrap(); // total 1010 > deposit 1000
    assert_eq!(n.req(Some(&v)).status, 402);
    n.rpc.deposit.store(2000, Ordering::SeqCst); // payer topped up on chain
    assert_eq!(n.req(Some(&v)).status, 200);
    let _ = &mut n;
}

#[test]
fn edge_redeems_the_best_voucher_with_a_close_transaction() {
    let mut n = pay_net(0, 10, 50);
    let mut last = None;
    for _ in 0..3 {
        let sv = n.spender.pay(10, &n.payer).unwrap();
        assert_eq!(n.req(Some(&sv)).status, 200);
        last = Some(sv);
    }
    // 30 earned, below redeem_min 50: nothing settles yet.
    assert!(n.edge.redeem().is_empty());
    assert!(n.settle.closed.lock().unwrap().is_empty());
    for _ in 0..2 {
        let sv = n.spender.pay(10, &n.payer).unwrap();
        assert_eq!(n.req(Some(&sv)).status, 200);
        last = Some(sv);
    }
    let done = n.edge.redeem();
    assert_eq!(done.len(), 1);
    assert_eq!((done[0].amount, done[0].result.is_ok()), (50, true));
    // Exactly the newest voucher was submitted.
    assert_eq!(*n.settle.closed.lock().unwrap(), vec![last.unwrap()]);
    assert_eq!(n.edge.metrics().get("redeemed_units"), 50);
    // The channel is closed on chain: further vouchers on it are refused.
    n.rpc.open.store(false, Ordering::SeqCst);
    let sv = n.spender.pay(10, &n.payer).unwrap();
    assert_eq!(n.req(Some(&sv)).status, 402);
    assert!(n.edge.redeem().is_empty());
}

#[test]
fn quota_is_per_client_address_and_voucher_requests_skip_it() {
    let mut n = pay_net(1, 10, 1);
    assert_eq!(n.req(None).status, 200);
    assert_eq!(n.req(None).status, 402);
    // A voucher request is paid, not free, and does not touch the quota counter.
    let sv = n.spender.pay(10, &n.payer).unwrap();
    assert_eq!(n.req(Some(&sv)).status, 200);
    assert_eq!(n.edge.metrics().get("free_served"), 1);
}

// ---------------------------------------------------------------------- failover

/// A hosting node stand-in: answers every GET from a closure.
struct Mock {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

fn mock(f: impl Fn(&str) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static) -> Mock {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let (hits, stop) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let (h2, s2) = (hits.clone(), stop.clone());
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        while !s2.load(Ordering::SeqCst) {
            let Ok((mut c, _)) = l.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            c.set_nonblocking(false).unwrap();
            h2.fetch_add(1, Ordering::SeqCst);
            let mut r = std::io::BufReader::new(c.try_clone().unwrap());
            let mut first = String::new();
            let _ = std::io::BufRead::read_line(&mut r, &mut first);
            // Drain headers.
            loop {
                let mut h = String::new();
                if std::io::BufRead::read_line(&mut r, &mut h).unwrap_or(0) == 0
                    || h.trim().is_empty()
                {
                    break;
                }
            }
            let (status, headers, body) = f(first.split_whitespace().nth(1).unwrap_or(""));
            use std::io::Write;
            let mut out = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (k, v) in headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            let _ = c.write_all(out.as_bytes());
            let _ = c.write_all(&body);
        }
    });
    Mock { addr, hits, stop }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn site_answer(
    body: &'static [u8],
    claimed: &'static [u8],
) -> impl Fn(&str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    move |_| {
        (
            200,
            vec![
                ("X-BTB-Content-Hash".into(), sha(claimed)),
                ("Content-Type".into(), "text/plain".into()),
            ],
            body.to_vec(),
        )
    }
}

fn http_up(hosts: Vec<String>) -> Arc<dyn Upstream> {
    Arc::new(HttpUpstream::new(
        hosts,
        Duration::from_secs(2),
        Duration::from_secs(30),
        1 << 20,
    ))
}

fn dead_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().to_string() // closed on drop: connection refused
}

#[test]
fn fails_over_from_a_dead_host_and_remembers() {
    let good = mock(site_answer(b"fine", b"fine"));
    let up = http_up(vec![dead_addr(), good.addr.to_string()]);
    let e = start_with(up, None, None, None, |c| {
        c.site_ttl = Duration::from_millis(1)
    });
    let r = get(&e, &format!("/site/{C}/a.txt"), &[]);
    assert_eq!((r.status, r.body.as_slice()), (200, &b"fine"[..]));
    // The dead host is set aside: later fetches go straight to the good one.
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(get(&e, &format!("/site/{C}/a.txt"), &[]).status, 200);
    assert_eq!(good.hits.load(Ordering::SeqCst), 2);
}

#[test]
fn fails_over_on_server_errors_and_on_tampered_content() {
    let broken = mock(|_| (503, vec![], b"overloaded".to_vec()));
    let evil = mock(site_answer(b"evil", b"real"));
    let good = mock(site_answer(b"real", b"real"));
    let up = http_up(vec![
        broken.addr.to_string(),
        evil.addr.to_string(),
        good.addr.to_string(),
    ]);
    let e = start_with(up, None, None, None, |_| {});
    let r = get(&e, &format!("/site/{C}/a.txt"), &[]);
    assert_eq!((r.status, r.body.as_slice()), (200, &b"real"[..]));
    assert_eq!(e.metrics().get("tampered_rejected"), 1);
    assert_eq!(broken.hits.load(Ordering::SeqCst), 1);
    assert_eq!(evil.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn all_hosts_down_is_a_502_and_a_404_is_not_a_reason_to_fail_over() {
    let e = start_with(
        http_up(vec![dead_addr(), dead_addr()]),
        None,
        None,
        None,
        |_| {},
    );
    assert_eq!(get(&e, &format!("/site/{C}/a.txt"), &[]).status, 502);

    let missing = mock(|_| (404, vec![], br#"{"error":"no such file"}"#.to_vec()));
    let other = mock(site_answer(b"x", b"x"));
    let up = http_up(vec![missing.addr.to_string(), other.addr.to_string()]);
    let e = start_with(up, None, None, None, |_| {});
    assert_eq!(get(&e, &format!("/site/{C}/a.txt"), &[]).status, 404);
    assert_eq!(other.hits.load(Ordering::SeqCst), 0);
}

// ------------------------------------------------------------------------ transaction relay

struct RelayRpc {
    sent: Mutex<Vec<String>>,
}

impl Rpc for RelayRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "btb_status" => Ok(json!({"chain_id": CHAIN_ID})),
            "btb_sendTransaction" => {
                self.sent
                    .lock()
                    .unwrap()
                    .push(params[0].as_str().unwrap().to_string());
                Ok(json!({"id": "ab".repeat(32)}))
            }
            "btb_getReceipt" => Ok(Value::Null),
            m => Err(format!("unexpected {m}")),
        }
    }
}

fn post(e: &Edge, path: &str, body: &str) -> UResp {
    wire::request(
        &e.addr().to_string(),
        "POST",
        path,
        &[],
        body.as_bytes(),
        Duration::from_secs(10),
        1 << 20,
    )
    .unwrap()
}

#[test]
fn edge_relays_only_transactions_that_name_it() {
    use btb_chain::{Transaction, TxBody, TxKind};
    let relayer = [0x42u8; 32];
    let key = SigningKey::from_bytes(&[3; 32]);
    let sign = |relayer: Option<[u8; 32]>| {
        Transaction::sign(
            TxBody {
                chain_id: CHAIN_ID,
                nonce: 0,
                relayer,
                session: None,
                kind: TxKind::Transfer {
                    to: [9; 32],
                    amount: 1,
                },
            },
            &key,
        )
    };
    let rpc = Arc::new(RelayRpc {
        sent: Mutex::new(Vec::new()),
    });
    let e = start_with(Fake::new(), None, Some(rpc.clone()), None, |c| {
        c.relay = Some(btb_edge::RelayConfig {
            relayer,
            chain_id: None,
        })
    });
    let info = get(&e, "/relay", &[]);
    assert_eq!(info.status, 200);
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    assert_eq!(info["relayer"], btb_storage::hex(&relayer));

    let good = sign(Some(relayer));
    let r = post(&e, "/tx", &btb_storage::hex(&good.encode()));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(rpc.sent.lock().unwrap().len(), 1);
    // JSON body works too
    let body = json!({"tx": btb_storage::hex(&good.encode())}).to_string();
    assert_eq!(post(&e, "/tx", &body).status, 200);

    // not named, named someone else, relayer changed after signing, wrong chain, garbage
    let mut changed = sign(Some(relayer));
    changed.body.relayer = Some([0x43; 32]);
    let mut other_chain = sign(Some(relayer));
    other_chain.body.chain_id = 1;
    for (what, tx) in [
        ("none", btb_storage::hex(&sign(None).encode())),
        ("other", btb_storage::hex(&sign(Some([0x43; 32])).encode())),
        ("changed", btb_storage::hex(&changed.encode())),
        ("chain", btb_storage::hex(&other_chain.encode())),
        ("garbage", "zz".to_string()),
    ] {
        let r = post(&e, "/tx", &tx);
        assert_eq!(r.status, 400, "{what}");
    }
    assert_eq!(rpc.sent.lock().unwrap().len(), 2, "nothing else forwarded");
    // an edge without relay config refuses
    let plain = start(&Fake::new(), |_| {});
    assert_eq!(post(&plain, "/tx", "00").status, 501);
    assert_eq!(get(&plain, "/relay", &[]).status, 404);
}
