//! Hosting node against a real 4-validator cluster: it follows the chain by verified block sync,
//! answers like the validators, pushes subscriptions, serves sites and proves what it serves.

#[path = "../../btb-tests/tests/common/mod.rs"]
mod common;
#[path = "../../btb-node/tests/lite/mod.rs"]
mod lite;

use btb_chain::site::{self, SiteOp};
use btb_chain::{Address, TxKind};
use btb_codec::Decode;
use btb_host::proof::{self, ProofBundle, ProofError};
use btb_host::{Host, HostConfig};
use btb_node::hex;
use btb_node::net::FollowerPolicy;
use btb_node::wire::{sync_request_frame, sync_response_frame, Inbound, SyncResponse};
use btb_runtime::layout;
use lite::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn counter_elf() -> &'static [u8] {
    static E: OnceLock<Vec<u8>> = OnceLock::new();
    E.get_or_init(|| common::build_example("counter"))
}

struct Env {
    c: Lite,
    host: Host,
    ctr: Address,
}

fn setup() -> Env {
    let mut c = Lite::new(4, 600, Some(FollowerPolicy::default()));
    let ctr = c.deploy(counter_elf());
    let cfg = HostConfig::new(
        c.genesis.clone(),
        c.validator_addrs(),
        "127.0.0.1:0".parse().unwrap(),
    );
    let host = Host::start(cfg).unwrap();
    let e = Env { c, host, ctr };
    e.wait_synced();
    // Caught up far enough to know the contract.
    let path = format!("/c/{}/query/get", hex::encode(&e.ctr));
    e.c.wait_until("the host to know the contract", 30, || {
        e.get(&path).status == 200
    });
    e
}

impl Env {
    fn addr(&self) -> SocketAddr {
        self.host.http_addr()
    }

    /// The host has executed everything the validators proved to be final a moment ago.
    fn wait_synced(&self) {
        let target = self.c.height(0).saturating_sub(3);
        self.c
            .wait_until("the host to catch up", 30, || self.host.height() >= target);
    }

    fn inc(&mut self, by: u64) {
        let tx = self.c.faucet.call(self.ctr, "increment", &(by,));
        self.c.run(&tx);
    }

    fn get(&self, path: &str) -> Reply {
        http(self.addr(), "GET", path, &[])
    }

    fn query_hex(&self) -> String {
        let r = self.get(&format!("/c/{}/query/get", hex::encode(&self.ctr)));
        assert_eq!(r.status, 200, "{}", r.text());
        r.text()
    }

    fn wait_value(&self, want: u64) {
        self.c.wait_until("the host to show the new value", 30, || {
            self.get(&format!("/c/{}/query/get", hex::encode(&self.ctr)))
                .text()
                == hex::encode(&want.to_le_bytes())
        });
    }
}

struct Reply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.text()))
    }
}

fn http(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Reply {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

struct Sse(BufReader<TcpStream>);

impl Sse {
    fn open(addr: SocketAddr, path: &str) -> Sse {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: t\r\nAccept: text/event-stream\r\n\r\n"
        )
        .unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        if !line.contains(" 200 ") {
            let mut rest = String::new();
            let _ = r.read_to_string(&mut rest);
            panic!("{line}{rest}");
        }
        loop {
            line.clear();
            r.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                return Sse(r);
            }
        }
    }

    /// The next event's data, or `None` if nothing arrives within `secs`.
    fn next(&mut self, secs: u64) -> Option<String> {
        self.0
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let end = Instant::now() + Duration::from_secs(secs);
        let mut line = String::new();
        while Instant::now() < end {
            line.clear();
            match self.0.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {
                    if let Some(d) = line.trim_end().strip_prefix("data: ") {
                        return Some(d.to_string());
                    }
                }
                Err(_) => {}
            }
        }
        None
    }
}

fn u64_of(hexs: &str) -> u64 {
    u64::decode(&hex::decode(hexs).unwrap()).unwrap()
}

#[test]
fn follower_stays_in_sync_and_answers_match_validators() {
    let _g = serial();
    let mut e = setup();
    for _ in 0..3 {
        e.inc(5);
    }
    e.wait_value(15);
    // Every validator gives the same answer as the host.
    for i in 0..4 {
        e.c.wait_until("the validator to catch up", 20, || {
            e.c.node(i)
                .call("btb_query", json!([hex::encode(&e.ctr), "get"]))
                .unwrap()["output"]
                == e.query_hex()
        });
    }
    assert_eq!(u64_of(&e.query_hex()), 15);

    // It keeps following.
    e.inc(5);
    e.wait_value(20);
    // The same state: the root the host proves at a height is the root the validators recorded.
    e.wait_synced();
    let h = e.host.height() - 1;
    let key = hex::encode(b"count");
    let b = e.get(&format!(
        "/proof/{}?key={key}&height={h}",
        hex::encode(&e.ctr)
    ));
    assert_eq!(b.status, 200, "{}", b.text());
    let bundle = ProofBundle::from_json(&b.json()).unwrap();
    for i in 0..4 {
        let blk = e.c.node(i).call("btb_getBlock", json!([h])).unwrap();
        assert_eq!(
            blk["state_root_after"],
            hex::encode(&bundle.state_root),
            "validator {i}"
        );
    }
    // The follower really synced (its blocks came from sync, not from its own consensus).
    let st = e.get("/status").json();
    assert!(st["sync"]["blocks_synced"].as_u64().unwrap() >= h);
    assert_eq!(st["sync"]["rejected"], 0);
    assert_eq!(st["host"]["role"], "hosting-node");
    assert!(e.c.status(0)["sync"]["served"].as_u64().unwrap() > 0);

    // Error shapes follow btb-dev.
    let no_fn = e.get(&format!("/c/{}/query/nope", hex::encode(&e.ctr)));
    assert_eq!(no_fn.status, 404);
    assert!(no_fn.json()["error"]
        .as_str()
        .unwrap()
        .contains("no such function"));
    let bad_ctr = e.get(&format!("/c/{}/query/get", hex::encode(&[1u8; 32])));
    assert_eq!(bad_ctr.status, 404);
    assert_eq!(e.get("/c/xyz/query/get").status, 400);
    assert_eq!(
        e.get(&format!("/c/{}/query/get?args=zz", hex::encode(&e.ctr)))
            .status,
        400
    );
    assert_eq!(
        e.get("/query/get").status,
        404,
        "no default contract configured"
    );
    let m = http(
        e.addr(),
        "POST",
        &format!("/c/{}/mutation/increment", hex::encode(&e.ctr)),
        &[],
    );
    assert_eq!(m.status, 501);
    assert!(m.json()["error"].as_str().unwrap().contains("validator"));
    let w = e.get(&format!(
        "/c/{}/query/get?caller=alice",
        hex::encode(&e.ctr)
    ));
    assert_eq!(w.status, 200);
    assert_eq!(e.get("/health").status, 200);
}

fn epoch_ctx(e: &Env, epoch: u64) -> btb_consensus::Context {
    let ep = e.c.node(0).call("btb_getEpoch", json!([epoch])).unwrap();
    proof::context_from_epoch_json(CHAIN_ID, epoch, &ep).unwrap()
}

/// An anchored proof bundle (header and consensus evidence present) for the counter's key.
fn anchored_bundle(e: &Env, key: &str) -> ProofBundle {
    let mut out = None;
    e.c.wait_until("an anchored proof", 30, || {
        let r = e.get(&format!("/proof/{}?key={}", hex::encode(&e.ctr), key));
        let b = ProofBundle::from_json(&r.json()).unwrap();
        let ok = b.commit.is_some() && b.attest.is_some() && b.entries[0].value.is_some();
        out = Some(b);
        ok
    });
    out.unwrap()
}

#[test]
fn proofs_verify_and_tampering_fails() {
    let _g = serial();
    let mut e = setup();
    e.inc(7);
    e.wait_value(7);
    let key = hex::encode(b"count");
    let full_key = layout::storage_key(&e.ctr, b"count");
    let b = anchored_bundle(&e, &key);
    let ctx = epoch_ctx(&e, 0);

    let v = proof::verify_bundle(&b, Some(&ctx)).expect("an honest bundle verifies");
    assert!(v.header_bound && v.quorum_committed);
    assert!(
        v.root_attested && v.attested_epoch == Some(0),
        "the root is anchored to a quorum"
    );
    assert_eq!(v.entries.len(), 1);
    assert_eq!(v.entries[0].0, full_key);
    assert_eq!(u64_of(&hex::encode(v.entries[0].1.as_ref().unwrap())), 7);
    // The root is the one the validators recorded for that block.
    let blk = e.c.node(0).call("btb_getBlock", json!([b.height])).unwrap();
    assert_eq!(blk["state_root_after"], hex::encode(&v.state_root));
    // Without a trust anchor the quorum evidence is simply not checked.
    let weak = proof::verify_bundle(&b, None).unwrap();
    assert!(weak.header_bound && !weak.quorum_committed && !weak.root_attested);
    // And survives a trip through JSON.
    assert_eq!(ProofBundle::from_json(&b.to_json()).unwrap(), b);

    // --- the state root attestation: forged roots, certificates and claims are rejected
    let trust: BTreeMap<u64, btb_consensus::Context> = BTreeMap::from([(0, ctx.clone())]);
    let att = b.attest.clone().unwrap();
    let bad_att = |t: &ProofBundle| match proof::verify_bundle_with(t, &trust) {
        Err(ProofError::BadAttestation(m)) => m,
        other => panic!("expected BadAttestation, got {other:?}"),
    };
    // Another root with the same certificate: the Merkle proofs, the roots and the claim all
    // have to agree, so every way of swapping the root breaks something.
    let mut t = b.clone();
    t.attest.as_mut().unwrap().roots[0][0] ^= 1;
    assert!(bad_att(&t).contains("not the bundle's root"));
    let mut t = b.clone();
    for r in t.attest.as_mut().unwrap().roots.iter_mut() {
        r[1] ^= 1; // a consistent lie about every attested root
    }
    t.state_root[1] ^= 1;
    assert_eq!(
        proof::verify_bundle_with(&t, &trust),
        Err(ProofError::BadEntry(0))
    );
    // The lie also fails when the Merkle proofs are replaced to fit it: the claim's digest
    // is signed and does not fold from the forged roots.
    let mut t = b.clone();
    t.entries.clear();
    t.header = None;
    t.commit = None;
    t.state_root[1] ^= 1;
    {
        let a = t.attest.as_mut().unwrap();
        a.roots[0] = t.state_root;
    }
    assert!(bad_att(&t).contains("fold"));
    // A forged claim (its block id changes, so the certificate no longer matches) ...
    let mut t = b.clone();
    t.attest.as_mut().unwrap().block.exec[9] ^= 1;
    assert!(bad_att(&t).contains("another block"));
    // ... and a claim re-certified by a minority of the validators: a flipped signature, too
    // few signers, and a certificate of another block all fail the quorum check.
    let mut t = b.clone();
    t.attest.as_mut().unwrap().qc.votes[0].1 .0[0] ^= 1;
    assert!(bad_att(&t).contains("quorum"));
    let mut t = b.clone();
    t.attest.as_mut().unwrap().qc.votes.truncate(1);
    assert!(bad_att(&t).contains("quorum"));
    let mut t = b.clone();
    t.attest.as_mut().unwrap().qc.block_id[0] ^= 1;
    assert!(bad_att(&t).contains("another block"));
    // Wrong height bookkeeping.
    let mut t = b.clone();
    t.attest.as_mut().unwrap().roots.pop();
    assert!(!bad_att(&t).is_empty());
    let mut t = b.clone();
    t.attest.as_mut().unwrap().prev_digest = Some([5; 32]);
    assert!(bad_att(&t).contains("fold"));
    // Validators of another epoch or another set cannot vouch for it.
    let other_set = btb_consensus::Context::for_epoch(
        &btb_chain::staking::consensus_chain_id(CHAIN_ID),
        0,
        btb_consensus::ValidatorSet::new(
            (60..64u8)
                .map(|i| btb_consensus::Validator {
                    key: key_of(i),
                    stake: 10,
                })
                .collect(),
        )
        .unwrap(),
        None,
    );
    let mut only_att = b.clone();
    only_att.header = None;
    only_att.commit = None;
    assert!(matches!(
        proof::verify_bundle_with(&only_att, &BTreeMap::from([(0, other_set)])),
        Err(ProofError::BadAttestation(_))
    ));
    assert!(
        proof::verify_bundle_with(&only_att, &trust)
            .unwrap()
            .root_attested
    );
    // No context for the attestation's epoch: not an error, simply not attested.
    let none = proof::verify_bundle_with(&b, &BTreeMap::new()).unwrap();
    assert!(!none.root_attested);
    // The certified claim is about this very chain: the validators' own record of the block
    // the certificate is for names the same digest.
    let att_blk =
        e.c.node(0)
            .call("btb_getBlock", json!([att.block.height]))
            .unwrap();
    assert_eq!(
        att_blk["attests_height"],
        att.roots.len() as u64 - 1 + b.height
    );
    assert_eq!(att.roots[0], v.state_root);
    // JSON round trip keeps the attestation; a forged block id is refused while parsing.
    let mut j = b.to_json();
    j["attest"]["block"]["exec"] = json!("00");
    assert!(matches!(
        ProofBundle::from_json(&j),
        Err(ProofError::Format(_))
    ));

    // --- tampering
    let mut t = b.clone();
    *t.entries[0].value.as_mut().unwrap().last_mut().unwrap() ^= 1;
    assert_eq!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadEntry(0))
    );
    let mut t = b.clone();
    t.entries[0].value = None; // "absent" for a key that exists
    assert_eq!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadEntry(0))
    );
    let mut t = b.clone();
    t.entries[0].key = layout::storage_key(&e.ctr, b"other");
    assert_eq!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadEntry(0))
    );
    let mut t = b.clone();
    t.state_root[0] ^= 1;
    assert_eq!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadEntry(0))
    );
    let mut t = b.clone();
    t.entries[0].proof.siblings[0][0][0] ^= 1;
    assert_eq!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadEntry(0))
    );
    // A different root with a header that names it: the entries no longer match.
    let mut t = b.clone();
    t.header.as_mut().unwrap().state_root[0] ^= 1;
    assert_eq!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadHeader)
    );
    // A header whose transaction root was changed no longer matches the committed payload.
    let mut t = b.clone();
    t.header.as_mut().unwrap().tx_root[0] ^= 1;
    assert!(matches!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadCommit(_))
    ));
    // Altered committed block / forged certificate.
    let mut t = b.clone();
    t.commit.as_mut().unwrap().sync.blocks[0].payload.push(1);
    assert!(matches!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadCommit(_))
    ));
    let mut t = b.clone();
    let last = t.commit.as_mut().unwrap().sync.blocks.last_mut().unwrap();
    last.justify.votes[0].1 .0[0] ^= 1;
    assert!(matches!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::BadCommit(_))
    ));
    // Evidence without a header, and evidence checked against the wrong validators.
    let mut t = b.clone();
    t.header = None;
    assert_eq!(
        proof::verify_bundle(&t, Some(&ctx)),
        Err(ProofError::CommitWithoutHeader)
    );
    let other = btb_consensus::Context::for_epoch(
        &btb_chain::staking::consensus_chain_id(CHAIN_ID),
        0,
        btb_consensus::ValidatorSet::new(
            (60..64u8)
                .map(|i| btb_consensus::Validator {
                    key: key_of(i),
                    stake: 10,
                })
                .collect(),
        )
        .unwrap(),
        Some([3; 32]),
    );
    assert!(matches!(
        proof::verify_bundle(&b, Some(&other)),
        Err(ProofError::BadCommit(_))
    ));
    // JSON with a forged header hash is rejected while parsing.
    let mut j = b.to_json();
    j["header"]["timestamp"] = json!(b.header.unwrap().timestamp + 1);
    assert!(matches!(
        ProofBundle::from_json(&j),
        Err(ProofError::Format(_))
    ));

    // --- absence is provable too
    let r = e.get(&format!(
        "/proof/{}?key={}",
        hex::encode(&e.ctr),
        hex::encode(b"nothing-here")
    ));
    let a = ProofBundle::from_json(&r.json()).unwrap();
    let v = proof::verify_bundle(&a, None).unwrap();
    assert_eq!(v.entries[0].1, None);
    let mut t = a.clone();
    t.entries[0].value = Some(vec![1]);
    assert_eq!(proof::verify_bundle(&t, None), Err(ProofError::BadEntry(0)));

    // --- ?proof=1 on a query: the result plus proofs of the keys it read
    let r = e.get(&format!("/c/{}/query/get?proof=1", hex::encode(&e.ctr)));
    assert_eq!(r.status, 200, "{}", r.text());
    let j = r.json();
    assert_eq!(j["incomplete"], false);
    assert_eq!(u64_of(j["result"].as_str().unwrap()), 7);
    let qb = ProofBundle::from_json(&j["proof"]).unwrap();
    let qv = proof::verify_bundle(&qb, None).unwrap();
    assert_eq!(qv.entries.len(), 1);
    assert_eq!(qv.entries[0].0, full_key);
    assert_eq!(hex::encode(qv.entries[0].1.as_ref().unwrap()), j["result"]);
    assert!(
        !qv.header_bound,
        "the head's root is named by no header yet"
    );

    // The checked-in fixture used by the TypeScript tests verifies in Rust too.
    if std::env::var("BTB_UPDATE_FIXTURES").is_ok_and(|v| v == "1") {
        write_fixture(&e, &b, &a);
    }
    check_fixture();
}

fn key_of(seed: u8) -> [u8; 32] {
    key(seed).verifying_key().to_bytes()
}

fn fixture_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk-ts/test/fixtures/proof.json")
}

/// A real bundle from a 4-validator run, with the context data to verify it, for `sdk-ts`.
fn write_fixture(e: &Env, b: &ProofBundle, absent: &ProofBundle) {
    let ep = e.c.node(0).call("btb_getEpoch", json!([0])).unwrap();
    let fixture = json!({
        "note": "Real output of a btb-host proof endpoint (4 validators, counter contract). \
                 Regenerate: BTB_UPDATE_FIXTURES=1 cargo test -p btb-host --test host proofs_verify",
        "contract": hex::encode(&e.ctr),
        "key": hex::encode(b"count"),
        "full_key": hex::encode(&layout::storage_key(&e.ctr, b"count")),
        "value": hex::encode(b.entries[0].value.as_ref().unwrap()),
        "bundle": b.to_json(),
        "absent_key": hex::encode(&absent.entries[0].key),
        "absent_bundle": absent.to_json(),
        "chain_id": CHAIN_ID,
        "epoch": ep,
    });
    std::fs::write(
        fixture_path(),
        serde_json::to_string_pretty(&fixture).unwrap() + "\n",
    )
    .unwrap();
}

fn check_fixture() {
    let Ok(text) = std::fs::read_to_string(fixture_path()) else {
        panic!("sdk-ts/test/fixtures/proof.json is missing: BTB_UPDATE_FIXTURES=1 cargo test -p btb-host --test host");
    };
    let f: Value = serde_json::from_str(&text).unwrap();
    let b = ProofBundle::from_json(&f["bundle"]).unwrap();
    let ctx =
        proof::context_from_epoch_json(f["chain_id"].as_u64().unwrap(), 0, &f["epoch"]).unwrap();
    let v = proof::verify_bundle(&b, Some(&ctx)).expect("checked-in fixture verifies");
    assert!(v.header_bound && v.quorum_committed && v.root_attested);
}

#[test]
fn subscription_pushes_after_commit() {
    let _g = serial();
    let mut e = setup();
    let path = format!("/c/{}/subscribe/get", hex::encode(&e.ctr));
    let mut s = Sse::open(e.addr(), &path);
    let first = s.next(10).expect("an initial event");
    assert_eq!(u64_of(&first), 0);
    e.c.wait_until("the host to count the subscriber", 10, || {
        e.host.subscriptions() == 1
    });

    e.inc(4);
    assert_eq!(u64_of(&s.next(20).expect("a push after the commit")), 4);
    e.inc(3);
    assert_eq!(u64_of(&s.next(20).expect("a second push")), 7);

    // Commits that do not touch what the query read push nothing.
    let tx = e.c.faucet.transfer([5; 32], 10);
    e.c.run(&tx);
    e.wait_synced();
    assert!(s.next(1).is_none(), "an unrelated commit pushed an event");

    // A closed connection is cleaned up.
    drop(s);
    e.inc(1);
    e.c.wait_until("the subscriber to be removed", 20, || {
        e.host.subscriptions() == 0
    });
}

/// The deployer (the faucet account) publishes a file.
fn publish(e: &mut Env, path: &str, content: &[u8]) -> Value {
    let tx = e.c.faucet.sign(TxKind::Site(SiteOp::Publish {
        contract: e.ctr,
        path: path.into(),
        content: content.to_vec(),
    }));
    e.c.run(&tx)
}

#[test]
fn site_is_served_after_publish_and_strangers_cannot_publish() {
    let _g = serial();
    let mut e = setup();
    let html = b"<!doctype html><title>t</title><script src=app.js></script>";
    let js = b"document.title = 'hi';";
    publish(&mut e, "index.html", html);
    let r = publish(&mut e, "app.js", js);
    assert_eq!(r["output"], hex::encode(&Sha256::digest(js)));
    publish(
        &mut e,
        "img/logo.svg",
        b"<svg xmlns='http://www.w3.org/2000/svg'/>",
    );
    let c = hex::encode(&e.ctr);
    e.c.wait_until("the host to serve the site", 30, || {
        e.get(&format!("/site/{c}/img/logo.svg")).status == 200
    });

    let r = e.get(&format!("/site/{c}/"));
    assert_eq!(r.status, 200);
    assert_eq!(r.body, html);
    assert_eq!(r.headers["content-type"], "text/html; charset=utf-8");
    let tag = hex::encode(&Sha256::digest(html));
    assert_eq!(r.headers["etag"], format!("\"{tag}\""));
    assert_eq!(r.headers["x-btb-content-hash"], tag);
    assert!(r.headers["content-security-policy"].starts_with("sandbox"));
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    assert_eq!(
        e.get(&format!("/site/{c}")).body,
        html,
        "no path serves index.html"
    );
    let js_r = e.get(&format!("/site/{c}/app.js"));
    assert_eq!(js_r.body, js);
    assert_eq!(
        js_r.headers["content-type"],
        "text/javascript; charset=utf-8"
    );
    let svg = e.get(&format!("/site/{c}/img/logo.svg"));
    assert_eq!(svg.headers["content-type"], "image/svg+xml");
    // Conditional requests.
    let again = http(
        e.addr(),
        "GET",
        &format!("/site/{c}/app.js"),
        &[(
            "If-None-Match",
            &format!("\"{}\"", hex::encode(&Sha256::digest(js))),
        )],
    );
    assert_eq!(again.status, 304);
    // Missing and malformed paths.
    assert_eq!(e.get(&format!("/site/{c}/missing.html")).status, 404);
    assert_eq!(e.get(&format!("/site/{c}/../x")).status, 400);
    assert_eq!(e.get(&format!("/site/{c}/a//b")).status, 400);
    assert_eq!(
        e.get(&format!("/site/{}/index.html", hex::encode(&[7u8; 32])))
            .status,
        404
    );
    // The listing.
    let l = e.get(&format!("/api/site/{c}")).json();
    let paths: Vec<&str> = l["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["app.js", "img/logo.svg", "index.html"]);

    // A stranger is refused on chain, and the page does not change.
    let mut mallory = Wallet::new(55);
    let fund = e.c.faucet.transfer(mallory.addr(), 1_000_000_000);
    e.c.run(&fund);
    let evil = mallory.sign(TxKind::Site(SiteOp::Publish {
        contract: e.ctr,
        path: "index.html".into(),
        content: b"<h1>defaced</h1>".to_vec(),
    }));
    let id = e.c.send(&evil);
    let mut receipt = Value::Null;
    e.c.wait_until("the refusal", 30, || {
        receipt =
            e.c.node(0)
                .call("btb_getReceipt", json!([hex::encode(&id)]))
                .unwrap();
        !receipt.is_null()
    });
    assert_eq!(receipt["status"], "reverted", "{receipt}");
    assert!(
        receipt["detail"].as_str().unwrap().contains("NotDeployer"),
        "{receipt}"
    );
    e.wait_synced();
    assert_eq!(e.get(&format!("/site/{c}/")).body, html);
    let evil2 = mallory.sign(TxKind::Site(SiteOp::Publish {
        contract: e.ctr,
        path: "evil.html".into(),
        content: b"x".to_vec(),
    }));
    e.c.send(&evil2);
    e.c.wait_until("the second block", 10, || e.c.height(0) > 0);
    std::thread::sleep(Duration::from_millis(500));
    e.wait_synced();
    assert_eq!(e.get(&format!("/site/{c}/evil.html")).status, 404);

    // Site files are provable like any other key.
    let r = e.get(&format!("/proof/{c}/site/index.html"));
    assert_eq!(r.status, 200, "{}", r.text());
    let b = ProofBundle::from_json(&r.json()).unwrap();
    let v = proof::verify_bundle(&b, None).unwrap();
    let file = site::SiteFile::from_value(v.entries[0].1.as_ref().unwrap()).unwrap();
    assert_eq!(file.content, html);
    assert_eq!(v.entries[0].0, site::site_key(&e.ctr, "index.html"));
}

/// A validator key played by hand: it answers the host's sync requests with whatever the test
/// wants. Real nodes treat it as an ordinary validator.
struct Byzantine {
    net: btb_node::net::NetHandle,
    rx: std::sync::mpsc::Receiver<([u8; 32], Inbound)>,
    _rt: tokio::runtime::Runtime,
}

#[test]
fn large_site_file_points_at_a_storage_root() {
    let _g = serial();
    let mut e = setup();
    // The pointer is an ordinary small site file holding the storage root in hex. This root is
    // not on chain, so fetching the bytes must fail (nothing unverified is ever served), while
    // an edge node that asks for the redirect gets the root and fetches from storage itself.
    let root = hex::encode(&[0xabu8; 32]);
    publish(&mut e, "movie.mp4.root", root.as_bytes());
    let c = hex::encode(&e.ctr);
    e.c.wait_until("the host to serve the pointer", 30, || {
        e.get(&format!("/site/{c}/movie.mp4.root")).status == 200
    });
    let url = format!("/site/{c}/movie.mp4");
    let r = http(e.addr(), "GET", &url, &[("X-BTB-Storage", "redirect")]);
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
    assert_eq!(r.headers["x-btb-storage-root"], root);
    assert_eq!(r.headers["content-type"], "application/octet-stream");
    assert_eq!(r.headers["etag"], format!("\"{root}\""));
    let r = http(
        e.addr(),
        "GET",
        &url,
        &[("If-None-Match", &format!("\"{root}\""))],
    );
    assert_eq!(r.status, 304);
    let r = e.get(&url);
    assert_eq!(r.status, 502, "{}", r.text());
    assert!(r.text().contains("storage fetch failed"), "{}", r.text());
    // The same file by root, and malformed roots.
    assert_eq!(e.get(&format!("/file/{root}")).status, 502);
    assert_eq!(e.get("/file/not-hex").status, 400);
    // A path with neither a file nor a pointer is still a 404.
    assert_eq!(e.get(&format!("/site/{c}/other.mp4")).status, 404);
}

fn byzantine(c: &Lite, index: usize) -> Byzantine {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let sink: btb_node::net::Sink = std::sync::Arc::new(move |from, msg| {
        let _ = tx.lock().unwrap().send((from, msg));
    });
    let keys: Vec<[u8; 32]> = c
        .keys
        .iter()
        .map(|k| k.verifying_key().to_bytes())
        .collect();
    let peers = (0..keys.len())
        .filter(|j| *j != index)
        .map(|j| (keys[j], c.addr_of(j).parse().unwrap()))
        .collect();
    let net = btb_node::net::start(
        rt.handle(),
        btb_node::net::NetParams {
            secret: c.keys[index].to_bytes(),
            listen: format!("/ip4/127.0.0.1/udp/{}/quic-v1", c.ports[index])
                .parse()
                .unwrap(),
            validators: keys,
            peers,
            rate_per_sec: 1e9,
            rate_burst: 1e9,
            delay: Duration::ZERO,
            followers: Some(FollowerPolicy::default()),
        },
        sink,
    )
    .unwrap();
    Byzantine { net, rx, _rt: rt }
}

#[test]
fn follower_rejects_forged_sync_from_a_malicious_peer() {
    use btb_consensus::{QuorumCert, Sig};
    use ed25519_dalek::Signer;
    let _g = serial();
    // Validators 0..3 run; key 3 is played by the test and is the host's only peer.
    let c = Lite::with_running(4, 3, 600, Some(FollowerPolicy::default()));
    let bad = byzantine(&c, 3);
    c.wait_until("a few blocks", 30, || c.height(0) >= 8);
    let v0 = c.keys[0].verifying_key().to_bytes();

    // A genuine answer from an honest validator, to forge from.
    bad.net.send(v0, sync_request_frame(1));
    let genuine: SyncResponse = {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                Instant::now() < end,
                "no sync answer from the honest validator"
            );
            if let Ok((_, Inbound::SyncResp(r))) = bad.rx.recv_timeout(Duration::from_millis(100)) {
                break *r;
            }
        }
    };
    assert!(genuine.blocks.len() >= 3);
    let ep = c.node(0).call("btb_getEpoch", json!([0])).unwrap();
    let ctx = proof::context_from_epoch_json(CHAIN_ID, 0, &ep).unwrap();

    let m = genuine.blocks.len();
    let mut forged: Vec<SyncResponse> = Vec::new();
    // 1. A flipped bit in the certificate that proves the chain.
    let mut f = genuine.clone();
    f.blocks[m - 1].justify.votes[0].1 .0[0] ^= 1;
    forged.push(f);
    // 2. A payload swapped for one that pays the attacker, hash fixed up.
    let mut f = genuine.clone();
    f.blocks[1].payload = vec![0xee; 40];
    f.blocks[1].payload_hash = btb_consensus::Block::hash_payload(&f.blocks[1].payload);
    forged.push(f);
    // 3. A certificate signed by keys that are not validators.
    let mut f = genuine.clone();
    let (round, id) = (
        f.blocks[m - 1].justify.round,
        f.blocks[m - 1].justify.block_id,
    );
    let outsiders: Vec<ed25519_dalek::SigningKey> = (200..204u8).map(key).collect();
    f.blocks[m - 1].justify = QuorumCert {
        round,
        block_id: id,
        votes: (0..3)
            .map(|i| {
                (
                    i as u32,
                    Sig(outsiders[i].sign(&ctx.vote_bytes(round, &id)).to_bytes()),
                )
            })
            .collect(),
    };
    forged.push(f);
    // 4. The right chain claimed for another epoch.
    let mut f = genuine.clone();
    f.epoch = 5;
    forged.push(f);
    // 5. Blocks skipped.
    let mut f = genuine.clone();
    f.blocks.remove(1);
    forged.push(f);

    let mut cfg = HostConfig::new(
        c.genesis.clone(),
        vec![c.addr_of(3)],
        "127.0.0.1:0".parse().unwrap(),
    );
    cfg.poll_ms = 20;
    let host = Host::start(cfg).unwrap();
    let rejected = |h: &Host| {
        h.node().call("btb_status", json!([])).unwrap()["sync"]["rejected"]
            .as_u64()
            .unwrap()
    };

    let end = Instant::now() + Duration::from_secs(40);
    let mut i = 0usize;
    while rejected(&host) < forged.len() as u64 {
        assert!(Instant::now() < end, "the host never asked for blocks");
        if let Ok((from, Inbound::SyncReq(_))) = bad.rx.recv_timeout(Duration::from_millis(50)) {
            bad.net
                .send(from, sync_response_frame(&forged[i % forged.len()]));
            i += 1;
        }
    }
    // Garbage is dropped without effect too.
    if let Ok((from, _)) = bad.rx.recv_timeout(Duration::from_millis(500)) {
        bad.net.send(from, vec![4u8; 3000]);
    }
    assert_eq!(host.height(), 0, "the host executed a forged block");
    let st = host.node().call("btb_status", json!([])).unwrap();
    assert_eq!(st["height"], 0);
    assert_eq!(st["sync"]["blocks_synced"], 0);

    // Control: the genuine answer is accepted by the very same host.
    let end = Instant::now() + Duration::from_secs(40);
    while host.height() == 0 {
        assert!(Instant::now() < end, "the genuine answer was not accepted");
        if let Ok((from, Inbound::SyncReq(h))) = bad.rx.recv_timeout(Duration::from_millis(50)) {
            let resp = if h == 1 {
                genuine.clone()
            } else {
                SyncResponse::default()
            };
            bad.net.send(from, sync_response_frame(&resp));
        }
    }
    assert!(host.height() >= 1);
}

#[test]
fn sdk_ts_client_works_against_the_host() {
    let _g = serial();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk-ts");
    let vitest = root.join("node_modules/.bin/vitest");
    if !vitest.exists() {
        eprintln!("skipping: run `npm ci` in sdk-ts to enable the TypeScript client test");
        return;
    }
    let mut e = setup();
    e.inc(3);
    e.wait_value(3);
    let base = format!("http://{}/c/{}", e.addr(), hex::encode(&e.ctr));
    // The TypeScript test only reads; this test moves the counter while it is subscribed.
    let child = std::process::Command::new(&vitest)
        .args(["run", "test/host.test.ts"])
        .current_dir(&root)
        .env("BTB_HOST_URL", &base)
        .env("BTB_HOST_ROOT", format!("http://{}", e.addr()))
        .env("BTB_HOST_CONTRACT", hex::encode(&e.ctr))
        .env("BTB_HOST_EXPECT", "3")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run vitest");
    // While the TS subscription is open, move the counter so it can see a push.
    std::thread::sleep(Duration::from_millis(1500));
    for _ in 0..4 {
        e.inc(1);
        std::thread::sleep(Duration::from_millis(700));
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "vitest failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
