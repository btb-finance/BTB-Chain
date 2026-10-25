//! What a validator does for an HTTPS action: fetch from a local HTTP server, run the contract's
//! transform, sign the result. No external network: every request goes to a server this test
//! starts on 127.0.0.1. Security-sensitive: the SSRF rules (addresses, schemes, allowlist,
//! redirects), the size and time caps, and the way failures are reported identically.

#[path = "../../btb-tests/tests/common/mod.rs"]
mod common;

use btb_chain::actions::{Pending, MAX_RESULT_BYTES, STATUS_FAILED, STATUS_OK};
use btb_codec::http::HttpRequest;
use btb_codec::Encode;
use btb_node::actions::{perform, ActionNode, FetchPolicy};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

fn elf() -> &'static [u8] {
    static E: OnceLock<Vec<u8>> = OnceLock::new();
    E.get_or_init(|| common::build_example("actions"))
}

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay_ms: u64,
}

impl Resp {
    fn ok(body: &str) -> Resp {
        Resp {
            status: 200,
            headers: vec![],
            body: body.as_bytes().to_vec(),
            delay_ms: 0,
        }
    }
    fn redirect(status: u16, to: &str) -> Resp {
        Resp {
            status,
            headers: vec![("Location".into(), to.into())],
            body: vec![],
            delay_ms: 0,
        }
    }
}

struct Server {
    port: u16,
    hits: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Req>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn read_request(s: &mut TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let end = loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Req {
        method,
        path,
        headers,
        body,
    })
}

/// A tiny HTTP server: `handler(n, request)` answers the n-th request (counting from 0).
fn serve(handler: impl Fn(usize, &Req) -> Resp + Send + Sync + 'static) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(handler);
    let (h2, s2, st2) = (hits.clone(), seen.clone(), stop.clone());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            if st2.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut c) = conn else { continue };
            let (handler, hits, seen) = (handler.clone(), h2.clone(), s2.clone());
            std::thread::spawn(move || {
                let Some(req) = read_request(&mut c) else {
                    return;
                };
                let n = hits.fetch_add(1, Ordering::SeqCst);
                let resp = handler(n, &req);
                seen.lock().unwrap().push(req);
                if resp.delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(resp.delay_ms));
                }
                let mut out = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    resp.status,
                    resp.body.len()
                );
                for (k, v) in &resp.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let _ = c.write_all(out.as_bytes());
                let _ = c.write_all(&resp.body);
            });
        }
    });
    Server {
        port,
        hits,
        seen,
        stop,
    }
}

fn pending(function: &str, req: HttpRequest, hosts: &[String]) -> Pending {
    Pending {
        requester: [1; 32],
        contract: [2; 32],
        function: function.into(),
        args: vec![],
        request: req.encode(),
        hosts: hosts.to_vec(),
        created: 1,
        deadline: 51,
        escrow: 10,
    }
}

fn run(function: &str, req: HttpRequest, hosts: &[String], policy: &FetchPolicy) -> (u8, Vec<u8>) {
    perform(&pending(function, req, hosts), elf(), policy)
}

fn text(r: &(u8, Vec<u8>)) -> String {
    String::from_utf8_lossy(&r.1).to_string()
}

fn failed(r: (u8, Vec<u8>), why: &str) {
    assert_eq!(
        (r.0, String::from_utf8_lossy(&r.1).to_string()),
        (STATUS_FAILED, why.to_string())
    );
}

fn tm() -> FetchPolicy {
    FetchPolicy::test_mode()
}

// ------------------------------------------------------------------------------ happy path

#[test]
fn a_response_is_fetched_transformed_and_signed() {
    let s = serve(|n, _| Resp::ok(&format!("nonce={}\nprice=42\nts=1700000{n}\n", n * 7919)));
    let r = run("quote", HttpRequest::get(s.url("/q")), &[s.host()], &tm());
    assert_eq!((r.0, r.1), (STATUS_OK, 42u64.encode()));
    assert_eq!(s.hits(), 1);
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, "/q");
}

#[test]
fn the_transform_makes_validators_agree_on_content_that_differs_per_request() {
    // Every request gets a different nonce and timestamp, as from a real API.
    let s = serve(|n, _| Resp::ok(&format!("nonce={}\nprice=42\nts=1700000{n}\n", n * 7919)));
    let req = HttpRequest::get(s.url("/q")).limits(4096, 2000);
    let answers: Vec<(u8, Vec<u8>)> = (0..4)
        .map(|_| run("quote", req.clone(), &[s.host()], &tm()))
        .collect();
    assert_eq!(s.hits(), 4);
    assert!(answers.iter().all(|a| a == &answers[0]), "{answers:?}");
    assert_eq!(answers[0], (STATUS_OK, 42u64.encode()));
    // the same action without a transform: the raw bodies differ, so the validators disagree
    let raw: Vec<(u8, Vec<u8>)> = (0..4)
        .map(|_| run("raw", req.clone(), &[s.host()], &tm()))
        .collect();
    let distinct: std::collections::BTreeSet<_> = raw.iter().collect();
    assert_eq!(distinct.len(), 4, "no transform, no agreement: {raw:?}");
}

#[test]
fn a_post_sends_its_body_and_headers() {
    let s = serve(|_, r| {
        Resp::ok(&format!(
            "{} {}",
            r.method,
            String::from_utf8_lossy(&r.body)
        ))
    });
    let req = HttpRequest::post(s.url("/p"), b"hello".to_vec())
        .header("content-type", "text/plain")
        .header("x-key", "abc")
        .limits(4096, 2000);
    let r = run("post", req, &[s.host()], &tm());
    assert_eq!(r, (STATUS_OK, b"POST hello".to_vec()));
    let seen = s.seen.lock().unwrap();
    let h = |k: &str| {
        seen[0]
            .headers
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(h("x-key").as_deref(), Some("abc"));
    assert_eq!(h("content-type").as_deref(), Some("text/plain"));
    assert_eq!(h("user-agent").as_deref(), Some("btb-validator"));
}

// ------------------------------------------------------------------------------ transform

#[test]
fn a_misbehaving_transform_becomes_an_agreed_failure() {
    let s = serve(|_, _| Resp::ok("price=1\n"));
    let req = HttpRequest::get(s.url("/q")).limits(4096, 2000);
    // writes state: trapped by the read-only host
    failed(
        run("trap", req.clone(), &[s.host()], &tm()),
        "transform failed",
    );
    // never ends: out of gas
    let mut p = tm();
    p.transform_gas = 200_000;
    let t = Instant::now();
    failed(
        run("spin", req.clone(), &[s.host()], &p),
        "transform failed",
    );
    assert!(t.elapsed() < Duration::from_secs(20));
    // aborts on a non-200 response
    let s404 = serve(|_, _| Resp {
        status: 404,
        ..Resp::ok("nope")
    });
    let req = HttpRequest::get(s404.url("/q")).limits(4096, 2000);
    failed(
        run("quote", req.clone(), &[s404.host()], &tm()),
        "transform failed",
    );
    // without a transform a non-2xx status is the failure
    failed(run("raw", req, &[s404.host()], &tm()), "http status 404");
}

#[test]
fn a_result_above_the_cap_is_a_failure() {
    let s = serve(|_, _| Resp::ok(&"x".repeat(MAX_RESULT_BYTES + 1)));
    let req = HttpRequest::get(s.url("/q")).limits(4096, 2000);
    failed(run("raw", req, &[s.host()], &tm()), "result too large");
    let s = serve(|_, _| Resp::ok(&"x".repeat(MAX_RESULT_BYTES)));
    let req = HttpRequest::get(s.url("/q")).limits(4096, 2000);
    let r = run("raw", req, &[s.host()], &tm());
    assert_eq!((r.0, r.1.len()), (STATUS_OK, MAX_RESULT_BYTES));
}

// ------------------------------------------------------------------------------ time and size

#[test]
fn slow_servers_time_out_and_dead_ones_fail() {
    let s = serve(|_, _| Resp {
        delay_ms: 3_000,
        ..Resp::ok("price=1\n")
    });
    let t = Instant::now();
    let req = HttpRequest::get(s.url("/q")).limits(4096, 300);
    failed(run("quote", req, &[s.host()], &tm()), "timeout");
    assert!(
        t.elapsed() < Duration::from_millis(2_500),
        "{:?}",
        t.elapsed()
    );
    // nothing listens there
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let host = format!("127.0.0.1:{dead}");
    let req = HttpRequest::get(format!("http://{host}/q")).limits(4096, 1000);
    failed(run("quote", req, &[host], &tm()), "fetch failed");
}

#[test]
fn responses_above_the_size_cap_are_refused() {
    let s = serve(|_, _| Resp::ok(&"y".repeat(10_000)));
    let req = HttpRequest::get(s.url("/q")).limits(1_000, 2000);
    failed(run("raw", req, &[s.host()], &tm()), "response too large");
    // exactly at the cap is fine (the body is 10 000 bytes; 1 024 is the result cap, so use a
    // small body for this edge)
    let s = serve(|_, _| Resp::ok(&"y".repeat(500)));
    let ok = run(
        "raw",
        HttpRequest::get(s.url("/q")).limits(500, 2000),
        &[s.host()],
        &tm(),
    );
    assert_eq!(ok.0, STATUS_OK);
    let over = run(
        "raw",
        HttpRequest::get(s.url("/q")).limits(499, 2000),
        &[s.host()],
        &tm(),
    );
    assert_eq!(over.0, STATUS_FAILED);
}

// ------------------------------------------------------------------------------ SSRF

#[test]
fn the_default_policy_forbids_plain_http_and_local_addresses() {
    let s = serve(|_, _| Resp::ok("price=1\n"));
    let req = HttpRequest::get(s.url("/q")).limits(4096, 2000);
    // strict default: http is not allowed at all
    failed(
        run("quote", req.clone(), &[s.host()], &FetchPolicy::default()),
        "blocked",
    );
    // http allowed, but loopback is not a public address
    let http_only = FetchPolicy {
        allow_http: true,
        ..Default::default()
    };
    failed(
        run("quote", req.clone(), &[s.host()], &http_only),
        "blocked",
    );
    assert_eq!(s.hits(), 0, "the server was never contacted");
    // a name that resolves to loopback is caught at the connection, not just in the URL
    let by_name = format!("http://localhost:{}/q", s.port);
    let host = format!("localhost:{}", s.port);
    failed(
        run(
            "quote",
            HttpRequest::get(by_name.clone()).limits(4096, 2000),
            std::slice::from_ref(&host),
            &http_only,
        ),
        "blocked",
    );
    assert_eq!(s.hits(), 0);
    // test mode lifts both
    let r = run(
        "quote",
        HttpRequest::get(by_name).limits(4096, 2000),
        &[host],
        &tm(),
    );
    assert_eq!(r.0, STATUS_OK, "{}", text(&r));
    assert_eq!(s.hits(), 1);
    // addresses of the usual SSRF targets, even with http allowed: never contacted
    for u in [
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.1/",
        "http://192.168.0.1/",
        "http://172.16.5.5/",
        "http://127.0.0.1:1/",
    ] {
        let host = u
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_string();
        let r = run(
            "raw",
            HttpRequest::get(u).limits(4096, 500),
            &[host],
            &http_only,
        );
        failed(r, "blocked");
    }
}

#[test]
fn only_allowlisted_hosts_are_contacted() {
    let s = serve(|_, _| Resp::ok("price=1\n"));
    let req = HttpRequest::get(s.url("/q")).limits(4096, 2000);
    failed(
        run("quote", req.clone(), &["other.example".into()], &tm()),
        "blocked",
    );
    failed(run("quote", req, &[], &tm()), "blocked");
    assert_eq!(s.hits(), 0);
}

#[test]
fn redirects_stay_inside_the_allowlist() {
    // a -> b (same server, allowed): followed
    let s = serve(|_, r| match r.path.as_str() {
        "/start" => Resp::redirect(302, "/next"),
        "/next" => Resp::ok("price=7\n"),
        "/loop" => Resp::redirect(302, "/loop"),
        "/out" => Resp::redirect(302, "http://127.0.0.1:1/evil"),
        "/out2" => Resp::redirect(307, "http://evil.example/x"),
        "/meta" => Resp::redirect(302, "http://169.254.169.254/latest/"),
        "/post" => Resp::redirect(303, "/echo"),
        "/echo" => Resp::ok(&format!("price={}\n", r.method)),
        _ => Resp {
            status: 404,
            ..Resp::ok("")
        },
    });
    let go = |path: &str, policy: &FetchPolicy| {
        run(
            "quote",
            HttpRequest::get(s.url(path)).limits(4096, 2000),
            &[s.host()],
            policy,
        )
    };
    let r = go("/start", &tm());
    assert_eq!((r.0, r.1), (STATUS_OK, 7u64.encode()));
    assert_eq!(s.hits(), 2);
    // endless redirect: bounded
    failed(go("/loop", &tm()), "fetch failed");
    // a redirect to a host that is not allowlisted is never followed
    let before = s.hits();
    failed(go("/out", &tm()), "blocked");
    failed(go("/out2", &tm()), "blocked");
    failed(go("/meta", &tm()), "blocked");
    assert_eq!(
        s.hits(),
        before + 3,
        "only the redirecting requests themselves"
    );
    // a POST redirected with 303 is retried as a GET
    let r = run(
        "post",
        HttpRequest::post(s.url("/post"), b"data".to_vec()).limits(4096, 2000),
        &[s.host()],
        &tm(),
    );
    assert_eq!(
        r,
        (STATUS_OK, b"price=GET\n".to_vec()),
        "a 303 turns the POST into a GET"
    );
}

// ------------------------------------------------------------------------------ the node's bookkeeping

fn key(s: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[s; 32])
}

#[test]
fn the_action_node_runs_each_request_once_and_pools_votes() {
    let s = serve(|_, _| Resp::ok("nonce=1\nprice=42\n"));
    let mut node = ActionNode::new(7, key(1), tm());
    let id = [9u8; 32];
    let p = pending(
        "quote",
        HttpRequest::get(s.url("/q")).limits(4096, 2000),
        &[s.host()],
    );
    node.start(&[(id, p.clone())], |_| Some(elf().to_vec()));
    node.start(&[(id, p.clone())], |_| Some(elf().to_vec()));
    let mut votes = Vec::new();
    let end = Instant::now() + Duration::from_secs(20);
    while votes.is_empty() && Instant::now() < end {
        votes = node.drain();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(votes.len(), 1);
    assert_eq!(s.hits(), 1, "started once");
    let v = &votes[0];
    assert!(v.verify(7));
    assert_eq!(
        (v.request, v.status, v.data.clone()),
        (id, STATUS_OK, 42u64.encode())
    );
    assert_eq!(node.inflight(), 0);
    // proposals carry it until the chain has recorded it
    let pending_ids: std::collections::BTreeSet<_> = [id].into();
    assert_eq!(
        node.for_proposal(&pending_ids, |_| Default::default())
            .len(),
        1
    );
    let recorded = |_: &_| [v.key].into_iter().collect();
    assert!(node.for_proposal(&pending_ids, recorded).is_empty());
    // votes from others: only validators', only verified ones
    let other = btb_chain::ActionVote::sign(7, &key(2), id, STATUS_OK, b"42".to_vec());
    let validators = [
        key(1).verifying_key().to_bytes(),
        key(2).verifying_key().to_bytes(),
    ];
    assert!(node.on_vote(other.clone(), &validators));
    assert!(!node.on_vote(other.clone(), &validators), "already held");
    let stranger = btb_chain::ActionVote::sign(7, &key(3), id, STATUS_OK, b"42".to_vec());
    assert!(!node.on_vote(stranger, &validators));
    let wrong_chain = btb_chain::ActionVote::sign(8, &key(2), [1; 32], STATUS_OK, vec![]);
    assert!(!node.on_vote(wrong_chain, &validators));
    assert_eq!(node.rejected, 2);
    assert_eq!(
        node.for_proposal(&pending_ids, |_| Default::default())
            .len(),
        2
    );
    // a settled request is forgotten (after the grace period for unknown ones, immediately for
    // started ones)
    node.prune(&Default::default());
    node.start(&[(id, p)], |_| Some(elf().to_vec()));
    assert_eq!(
        node.inflight(),
        1,
        "a request that came back is fetched again"
    );
}
