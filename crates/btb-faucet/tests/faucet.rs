//! The faucet against a real in-process 4-validator cluster (libp2p/QUIC on localhost, real
//! consensus and execution), over real HTTP. Tests run one at a time on one shared cluster.

use btb_chain::{
    account_address, Address, SigningKey, Transaction, TxBody, TxKind, TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_faucet::limits::LimitConfig;
use btb_faucet::{pow, server, to_hex, Config, Faucet};
use btb_node::config::{GenesisFile, NodeParams};
use btb_node::{net, open_chain, Node};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const CHAIN_ID: u64 = 7;
const BTB: u64 = UNITS_PER_BTB;

struct Net {
    nodes: Vec<Node>,
    rpc: Vec<SocketAddr>,
    faucet_key: SigningKey,
    _dir: tempfile::TempDir,
}

fn network() -> &'static Net {
    static N: OnceLock<Net> = OnceLock::new();
    N.get_or_init(|| {
        let keys: Vec<SigningKey> = (0..4)
            .map(|i| SigningKey::from_bytes(&[10 + i; 32]))
            .collect();
        let faucet_key = SigningKey::from_bytes(&[99; 32]);
        let per = 1_000 * BTB;
        let mut allocations: Vec<(Address, u64)> = keys
            .iter()
            .map(|k| (account_address(&k.verifying_key().to_bytes()), per))
            .collect();
        allocations.push((
            account_address(&faucet_key.verifying_key().to_bytes()),
            TOTAL_SUPPLY - per * 4,
        ));
        let genesis = GenesisFile {
            chain_id: CHAIN_ID,
            name: "btb-faucet-test".into(),
            timestamp: 0,
            gas_price: 1,
            epoch_len: 600,
            min_stake: 100 * BTB,
            storage: Default::default(),
            oracle: None,
            validators: keys
                .iter()
                .map(|k| (k.verifying_key().to_bytes(), 100 * BTB))
                .collect(),
            allocations,
        };
        genesis.validate().unwrap();
        let ports: Vec<u16> = (0..4)
            .map(|_| {
                std::net::UdpSocket::bind("127.0.0.1:0")
                    .unwrap()
                    .local_addr()
                    .unwrap()
                    .port()
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let addr_of = |i: usize| {
            let id = net::peer_id_of(&keys[i].verifying_key().to_bytes()).unwrap();
            format!("/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{id}", ports[i])
        };
        let mut nodes = Vec::new();
        for i in 0..4 {
            let mut p = NodeParams::new(genesis.clone(), keys[i].clone());
            p.listen = format!("/ip4/127.0.0.1/udp/{}/quic-v1", ports[i]);
            p.peers = (0..4).filter(|j| *j != i).map(addr_of).collect();
            p.rpc = Some("127.0.0.1:0".parse().unwrap());
            p.data_dir = Some(dir.path().join(format!("node{i}")));
            let state = open_chain(btb_runtime::MemKv::new(), &genesis).unwrap();
            nodes.push(Node::start(p, state).unwrap());
        }
        let rpc = nodes.iter().map(|n| n.rpc_addr().unwrap()).collect();
        let n = Net {
            nodes,
            rpc,
            faucet_key,
            _dir: dir,
        };
        wait("the cluster to produce blocks", 30, || {
            n.nodes[0].call("btb_status", json!([])).unwrap()["height"]
                .as_u64()
                .unwrap()
                >= 2
        });
        n
    })
}

fn serial() -> MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

fn rpc(n: &Net, method: &str, params: Value) -> Value {
    n.nodes[0].call(method, params).unwrap()
}

fn balance(n: &Net, a: &Address) -> u64 {
    rpc(n, "btb_getBalance", json!([to_hex(a)]))["balance"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// A fresh random-looking recipient.
fn recipient(tag: &str) -> Address {
    static C: Mutex<u64> = Mutex::new(0);
    let mut c = C.lock().unwrap();
    *c += 1;
    let mut s = [0u8; 32];
    s[..8].copy_from_slice(&c.to_le_bytes());
    s[8..8 + tag.len().min(16)].copy_from_slice(&tag.as_bytes()[..tag.len().min(16)]);
    account_address(&SigningKey::from_bytes(&s).verifying_key().to_bytes())
}

struct Started {
    faucet: Arc<Faucet>,
    handle: server::Handle,
}

impl Started {
    fn url(&self) -> SocketAddr {
        self.handle.local_addr()
    }
}

fn start_with(n: &Net, tweak: impl FnOnce(&mut Config)) -> Started {
    let mut cfg = Config::new(vec![n.rpc[0].to_string()]);
    cfg.limits = LimitConfig {
        address_cooldown: Duration::from_secs(3600),
        ip_max: 1000,
        ip_window: Duration::from_secs(3600),
    };
    tweak(&mut cfg);
    let faucet = Arc::new(Faucet::new(cfg, n.faucet_key.clone()));
    let handle = server::serve(faucet.clone(), "127.0.0.1:0".parse().unwrap()).unwrap();
    Started { faucet, handle }
}

struct Resp {
    status: u16,
    headers: String,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("json {e}: {}", self.body))
    }
}

fn http(addr: SocketAddr, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> Resp {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Resp {
        status,
        headers: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

fn drip(s: &Started, addr: &str, extra: &[(&str, &str)]) -> Resp {
    http(
        s.url(),
        "POST",
        "/drip",
        extra,
        &json!({ "address": addr }).to_string(),
    )
}

fn wait_receipt(n: &Net, id: &str) -> Value {
    let mut out = Value::Null;
    wait("a receipt", 30, || {
        out = rpc(n, "btb_getReceipt", json!([id]));
        !out.is_null()
    });
    out
}

#[test]
fn drip_works_and_shows_in_the_explorer_api() {
    let _g = serial();
    let n = network();
    let s = start_with(n, |_| {});
    let to = recipient("works");

    // the embedded page and the info endpoint
    let page = http(s.url(), "GET", "/", &[], "");
    assert_eq!(page.status, 200);
    assert!(page.body.contains("BTB testnet faucet"));
    let info = http(s.url(), "GET", "/info", &[], "").json();
    assert_eq!(info["chain_id"], CHAIN_ID);
    assert_eq!(info["address"], to_hex(&s.faucet.address()));
    assert_eq!(info["default_amount_btb"], 100);

    let r = drip(&s, &to_hex(&to), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    let j = r.json();
    assert_eq!(j["amount_btb"], 100);
    let id = j["tx_id"].as_str().unwrap().to_string();
    let receipt = wait_receipt(n, &id);
    assert_eq!(receipt["status"], "success");
    assert_eq!(balance(n, &to), 100 * BTB);
    // /tx/<id> proxies the receipt
    let st = http(s.url(), "GET", &format!("/tx/{id}"), &[], "").json();
    assert_eq!(st["status"], "success");

    // explorer read API on the node: transaction, account, account history, block list, page
    let tx = rpc(n, "btb_getTransaction", json!([id]));
    assert_eq!(tx["kind"], "transfer");
    assert_eq!(tx["to"], to_hex(&to));
    assert_eq!(tx["from"], to_hex(&s.faucet.address()));
    assert_eq!(tx["amount"], (100 * BTB).to_string());
    assert_eq!(tx["receipt"]["status"], "success");
    assert!(
        tx["receipt"]["fee"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(tx["height"], receipt["height"]);
    let acc = rpc(n, "btb_getAccount", json!([to_hex(&to)]));
    assert_eq!(acc["balance"], (100 * BTB).to_string());
    let txs = rpc(n, "btb_getAccountTxs", json!([to_hex(&to), 10]));
    assert_eq!(txs["txs"].as_array().unwrap().len(), 1);
    assert_eq!(txs["txs"][0]["id"], id);
    assert_eq!(txs["txs"][0]["status"], "success");
    let faucet_txs = rpc(
        n,
        "btb_getAccountTxs",
        json!([to_hex(&s.faucet.address()), 100]),
    );
    assert!(faucet_txs["txs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["id"] == id));
    let blocks = rpc(n, "btb_getBlocks", json!([null, 5]));
    let list = blocks["blocks"].as_array().unwrap();
    assert!(!list.is_empty() && list[0]["height"] == blocks["head"]);
    assert!(list
        .windows(2)
        .all(|w| w[0]["height"].as_u64() > w[1]["height"].as_u64()));
    let block = rpc(n, "btb_getBlock", json!([tx["height"]]));
    assert!(block["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| *t == json!(id)));
    assert!(block["fees"].as_str().unwrap().parse::<u64>().unwrap() > 0);
    assert!(rpc(n, "btb_getTransaction", json!([to_hex(&[1u8; 32])])).is_null());
    let ep = rpc(n, "btb_getEpoch", json!([0]));
    assert_eq!(ep["validators"].as_array().unwrap().len(), 4);
    assert_eq!(ep["validators"][0]["jailed"], false);
    let ex = http(n.rpc[0], "GET", "/explorer", &[], "");
    assert_eq!(ex.status, 200);
    assert!(ex.headers.contains("text/html"));
    assert!(ex.body.contains("BTB Explorer"));
}

#[test]
fn bad_requests_are_rejected_and_cost_nothing() {
    let _g = serial();
    let n = network();
    let s = start_with(n, |c| c.max_amount = 500 * BTB);
    let good = recipient("bad");
    let before = rpc(n, "btb_getBalance", json!([to_hex(&s.faucet.address())]))["nonce"].clone();
    for bad in [
        "",
        "abc",
        "zz".repeat(32).as_str(),
        &to_hex(&good)[..62],
        &format!("{}00", to_hex(&good)),
        &format!("{}0", to_hex(&good)),
        &to_hex(&s.faucet.address()),
    ] {
        let r = drip(&s, bad, &[]);
        assert_eq!(r.status, 400, "address {bad:?}: {}", r.body);
        assert!(r.json()["error"].is_string());
    }
    let post = |body: &str| http(s.url(), "POST", "/drip", &[], body);
    assert_eq!(post("not json").status, 400);
    assert_eq!(post("{}").status, 400);
    assert_eq!(post(&json!({"address": 5}).to_string()).status, 400);
    for amount in [json!(0), json!(501), json!("lots"), json!(-3), json!(1.5)] {
        let r = post(&json!({"address": to_hex(&good), "amount": amount}).to_string());
        assert_eq!(r.status, 400, "amount {amount}: {}", r.body);
    }
    assert_eq!(http(s.url(), "GET", "/nope", &[], "").status, 404);
    assert_eq!(http(s.url(), "GET", "/drip", &[], "").status, 405);
    // nothing was sent, and the address is not locked out by the failed attempts
    assert_eq!(
        rpc(n, "btb_getBalance", json!([to_hex(&s.faucet.address())]))["nonce"],
        before
    );
    let ok = post(&json!({"address": to_hex(&good), "amount": 500}).to_string());
    assert_eq!(ok.status, 200, "{}", ok.body);
    wait_receipt(n, ok.json()["tx_id"].as_str().unwrap());
    assert_eq!(balance(n, &good), 500 * BTB);
}

#[test]
fn rate_limits_per_address_and_per_ip() {
    let _g = serial();
    let n = network();
    // Per address: a second request is refused with Retry-After.
    let s = start_with(n, |c| c.limits.address_cooldown = Duration::from_secs(1000));
    let a = recipient("addr");
    let first = drip(&s, &to_hex(&a), &[]);
    assert_eq!(first.status, 200, "{}", first.body);
    let second = drip(&s, &to_hex(&a), &[]);
    assert_eq!(second.status, 429);
    assert!(second.headers.contains("retry-after: "));
    let wait_s = second.json()["retry_after"].as_u64().unwrap();
    assert!((990..=1001).contains(&wait_s), "{wait_s}");
    // uppercase / 0x spelling is the same address
    let third = drip(&s, &format!("0x{}", to_hex(&a).to_uppercase()), &[]);
    assert_eq!(third.status, 429);
    wait_receipt(n, first.json()["tx_id"].as_str().unwrap());
    assert_eq!(balance(n, &a), 100 * BTB, "exactly one drip arrived");

    // Per IP: 3 per window across different addresses; a spoofed header does not help.
    let s = start_with(n, |c| {
        c.limits.ip_max = 3;
        c.limits.ip_window = Duration::from_secs(1000);
    });
    let mut ids = Vec::new();
    for i in 0..3 {
        let r = drip(&s, &to_hex(&recipient(&format!("ip{i}"))), &[]);
        assert_eq!(r.status, 200, "{}", r.body);
        ids.push(r.json()["tx_id"].as_str().unwrap().to_string());
    }
    let denied = drip(
        &s,
        &to_hex(&recipient("ip3")),
        &[("X-Forwarded-For", "203.0.113.9")],
    );
    assert_eq!(denied.status, 429, "{}", denied.body);
    assert!(denied.json()["error"].as_str().unwrap().contains("IP"));
    for id in ids {
        wait_receipt(n, &id);
    }

    // Behind a trusted proxy the last X-Forwarded-For entry is the client.
    let s = start_with(n, |c| {
        c.trust_proxy = true;
        c.limits.ip_max = 1;
        c.limits.ip_window = Duration::from_secs(1000);
    });
    let d = |who: &str, tag: &str| {
        drip(
            &s,
            &to_hex(&recipient(tag)),
            &[("X-Forwarded-For", &format!("198.51.100.1, {who}"))],
        )
    };
    let r1 = d("203.0.113.1", "p1");
    assert_eq!(r1.status, 200, "{}", r1.body);
    assert_eq!(d("203.0.113.1", "p2").status, 429);
    let r3 = d("203.0.113.2", "p3");
    assert_eq!(r3.status, 200, "{}", r3.body);
    wait_receipt(n, r1.json()["tx_id"].as_str().unwrap());
    wait_receipt(n, r3.json()["tx_id"].as_str().unwrap());
}

#[test]
fn proof_of_work_is_enforced() {
    let _g = serial();
    let n = network();
    let s = start_with(n, |c| c.pow_bits = 10);
    let a = recipient("pow");
    let ah = to_hex(&a);
    let body = |c: &str, nonce: u64, addr: &str| {
        json!({"address": addr, "pow": {"challenge": c, "nonce": nonce}}).to_string()
    };
    let post = |b: &str| http(s.url(), "POST", "/drip", &[], b);

    assert_eq!(drip(&s, &ah, &[]).status, 400, "no proof");
    let ch = http(s.url(), "GET", "/challenge", &[], "").json();
    assert_eq!(ch["bits"], 10);
    let c_hex = ch["challenge"].as_str().unwrap().to_string();
    let c: [u8; 16] = btb_faucet::parse_hex(&c_hex).unwrap().try_into().unwrap();
    let nonce = pow::solve(&c, &a, 10);

    // a solution for another address is refused (and burns the challenge)
    let other = recipient("pow-other");
    assert_eq!(post(&body(&c_hex, nonce, &to_hex(&other))).status, 400);
    assert_eq!(
        post(&body(&c_hex, nonce, &ah)).status,
        400,
        "challenge is single use"
    );

    let ch = http(s.url(), "GET", "/challenge", &[], "").json();
    let c_hex = ch["challenge"].as_str().unwrap().to_string();
    let c: [u8; 16] = btb_faucet::parse_hex(&c_hex).unwrap().try_into().unwrap();
    // a wrong nonce (one that does not meet the difficulty) fails
    let wrong = (0u64..)
        .find(|k| pow::leading_zero_bits(&pow::digest(&c, &a, *k)) < 10)
        .unwrap();
    assert_eq!(post(&body(&c_hex, wrong, &ah)).status, 400);
    // an unknown challenge fails
    assert_eq!(post(&body(&to_hex(&[7u8; 16]), 0, &ah)).status, 400);

    let ch = http(s.url(), "GET", "/challenge", &[], "").json();
    let c_hex = ch["challenge"].as_str().unwrap().to_string();
    let c: [u8; 16] = btb_faucet::parse_hex(&c_hex).unwrap().try_into().unwrap();
    let ok = post(&body(&c_hex, pow::solve(&c, &a, 10), &ah));
    assert_eq!(ok.status, 200, "{}", ok.body);
    wait_receipt(n, ok.json()["tx_id"].as_str().unwrap());
    assert_eq!(balance(n, &a), 100 * BTB);
}

#[test]
fn concurrent_drips_get_distinct_nonces() {
    let _g = serial();
    let n = network();
    let s = start_with(n, |_| {});
    let faucet_addr = to_hex(&s.faucet.address());
    let start_nonce = rpc(n, "btb_getBalance", json!([faucet_addr]))["nonce"]
        .as_u64()
        .unwrap();
    const K: usize = 24;
    let recips: Vec<Address> = (0..K).map(|i| recipient(&format!("c{i}"))).collect();
    let url = s.url();
    let go = Arc::new(std::sync::Barrier::new(K));
    let threads: Vec<_> = recips
        .iter()
        .map(|a| {
            let (a, go) = (to_hex(a), go.clone());
            std::thread::spawn(move || {
                go.wait();
                let r = http(
                    url,
                    "POST",
                    "/drip",
                    &[],
                    &json!({"address": a, "amount": 7}).to_string(),
                );
                (r.status, r.body)
            })
        })
        .collect();
    let mut nonces = Vec::new();
    let mut ids = Vec::new();
    for t in threads {
        let (status, body) = t.join().unwrap();
        assert_eq!(status, 200, "{body}");
        let j: Value = serde_json::from_str(&body).unwrap();
        nonces.push(j["nonce"].as_u64().unwrap());
        ids.push(j["tx_id"].as_str().unwrap().to_string());
    }
    nonces.sort_unstable();
    assert_eq!(
        nonces,
        (start_nonce..start_nonce + K as u64).collect::<Vec<_>>(),
        "contiguous distinct nonces"
    );
    for id in &ids {
        assert_eq!(wait_receipt(n, id)["status"], "success");
    }
    for a in &recips {
        assert_eq!(balance(n, a), 7 * BTB);
    }
}

#[test]
fn nonce_conflicts_with_other_users_of_the_key_are_retried() {
    let _g = serial();
    let n = network();
    // The faucet starts with next = 0, far behind the chain: NonceTooLow tells it where to go.
    let s = start_with(n, |_| {});
    let first = recipient("stale");
    let r = drip(&s, &to_hex(&first), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    wait_receipt(n, r.json()["tx_id"].as_str().unwrap());
    assert_eq!(balance(n, &first), 100 * BTB);

    // Someone else signs with the faucet key and takes the nonce the faucet would use next.
    let faucet_addr = to_hex(&s.faucet.address());
    let nonce = rpc(n, "btb_getBalance", json!([faucet_addr]))["nonce"]
        .as_u64()
        .unwrap();
    let thief = recipient("thief");
    let tx = Transaction::sign(
        TxBody {
            relayer: None,
            chain_id: CHAIN_ID,
            nonce,
            kind: TxKind::Transfer {
                to: thief,
                amount: 3 * BTB,
            },
        },
        &n.faucet_key,
    );
    let stolen = rpc(n, "btb_sendTransaction", json!([to_hex(&tx.encode())]));
    let second = recipient("after-thief");
    let r = drip(&s, &to_hex(&second), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        r.json()["nonce"].as_u64().unwrap(),
        nonce + 1,
        "skipped the taken nonce"
    );
    wait_receipt(n, stolen["id"].as_str().unwrap());
    wait_receipt(n, r.json()["tx_id"].as_str().unwrap());
    assert_eq!(balance(n, &thief), 3 * BTB);
    assert_eq!(balance(n, &second), 100 * BTB);
}

#[test]
fn node_failover_and_unreachable_node() {
    let _g = serial();
    let n = network();
    let dead: SocketAddr = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    // First endpoint is dead: the faucet falls over to the second.
    let s = start_with(n, |c| c.rpc = vec![dead.to_string(), n.rpc[1].to_string()]);
    let a = recipient("failover");
    let r = drip(&s, &to_hex(&a), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    wait_receipt(n, r.json()["tx_id"].as_str().unwrap());
    assert_eq!(balance(n, &a), 100 * BTB);

    // No node at all: 503, and the failed attempt does not start the address cooldown.
    let s = start_with(n, |c| c.rpc = vec![dead.to_string()]);
    let b = recipient("nonode");
    let r = drip(&s, &to_hex(&b), &[]);
    assert_eq!(r.status, 503, "{}", r.body);
    let again = drip(&s, &to_hex(&b), &[]);
    assert_eq!(again.status, 503, "not 429: the reservation was released");
    assert_eq!(http(s.url(), "GET", "/health", &[], "").status, 200);
}
