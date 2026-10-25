//! HTTPS actions across four real nodes (libp2p/QUIC on localhost, real consensus): a user sends
//! an action request, every validator fetches from a local HTTP server, transforms and signs, the
//! proposer carries the votes into a block, and the chain delivers the agreed result to the
//! contract's callback. No external network: the server is started by the test.

#[path = "../../btb-tests/tests/common/mod.rs"]
mod common;

use btb_chain::{
    account_address, contract_address, ActionOp, Address, Hash, SigningKey, Transaction, TxBody,
    TxKind, TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_codec::{Decode, Encode};
use btb_node::actions::FetchPolicy;
use btb_node::config::{GenesisFile, NodeParams, Timing};
use btb_node::{hex, net, open_chain, Node};
use btb_runtime::MemKv;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const CHAIN_ID: u64 = 7;
/// Escrow of a request: the callback's gas limit plus every validator's answer, with room.
const FEE: u64 = 10_000_000;

fn elf() -> &'static [u8] {
    static E: OnceLock<Vec<u8>> = OnceLock::new();
    E.get_or_init(|| common::build_example("actions"))
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

struct Wallet {
    key: SigningKey,
    nonce: u64,
}

impl Wallet {
    fn addr(&self) -> Address {
        account_address(&self.key.verifying_key().to_bytes())
    }
    fn sign(&mut self, kind: TxKind) -> Transaction {
        let tx = Transaction::sign(
            TxBody {
                chain_id: CHAIN_ID,
                nonce: self.nonce,
                kind,
            },
            &self.key,
        );
        self.nonce += 1;
        tx
    }
}

// ------------------------------------------------------------------------------ the web server

struct Server {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

impl Server {
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/data", self.port)
    }
}

/// `body(n)` is the body for the n-th request, `delay_ms` how long the server sleeps first.
fn serve(body: impl Fn(usize) -> String + Send + Sync + 'static, delay_ms: u64) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (h2, s2) = (hits.clone(), stop.clone());
    let body = Arc::new(body);
    std::thread::spawn(move || {
        for conn in l.incoming() {
            if s2.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut c) = conn else { continue };
            let (hits, body) = (h2.clone(), body.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let mut got = Vec::new();
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let n = hits.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(delay_ms));
                let b = body(n);
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                        b.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    Server { port, hits, stop }
}

// ------------------------------------------------------------------------------ the cluster

struct Cluster {
    genesis: GenesisFile,
    keys: Vec<SigningKey>,
    ports: Vec<u16>,
    nodes: Vec<Option<Node>>,
    faucet: Wallet,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for n in self.nodes.iter_mut() {
            if let Some(n) = n.take() {
                n.shutdown();
            }
        }
    }
}

fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

impl Cluster {
    fn new(total: usize) -> Cluster {
        let keys: Vec<SigningKey> = (0..total).map(|i| key(10 + i as u8)).collect();
        let faucet = Wallet {
            key: key(99),
            nonce: 0,
        };
        let per = 1_000 * UNITS_PER_BTB;
        let mut allocations: Vec<(Address, u64)> = keys
            .iter()
            .map(|k| (account_address(&k.verifying_key().to_bytes()), per))
            .collect();
        allocations.push((faucet.addr(), TOTAL_SUPPLY - per * total as u64));
        let genesis = GenesisFile {
            chain_id: CHAIN_ID,
            name: "btb-test".into(),
            timestamp: 0,
            gas_price: 1,
            epoch_len: 600,
            min_stake: 100 * UNITS_PER_BTB,
            storage: Default::default(),
            oracle: None,
            validators: keys
                .iter()
                .map(|k| (k.verifying_key().to_bytes(), 100 * UNITS_PER_BTB))
                .collect(),
            allocations,
        };
        genesis.validate().unwrap();
        let mut c = Cluster {
            genesis,
            ports: (0..total).map(|_| free_udp_port()).collect(),
            keys,
            nodes: (0..total).map(|_| None).collect(),
            faucet,
        };
        for i in 0..total {
            c.start(i);
        }
        c
    }

    fn addr_of(&self, i: usize) -> String {
        let id = net::peer_id_of(&self.keys[i].verifying_key().to_bytes()).unwrap();
        format!("/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{id}", self.ports[i])
    }

    fn start(&mut self, i: usize) {
        let mut p = NodeParams::new(self.genesis.clone(), self.keys[i].clone());
        p.listen = format!("/ip4/127.0.0.1/udp/{}/quic-v1", self.ports[i]);
        p.peers = (0..self.keys.len())
            .filter(|j| *j != i)
            .map(|j| self.addr_of(j))
            .collect();
        p.timing = Timing {
            tick_ms: 10,
            round_timeout_ms: 500,
            idle_block_ms: 80,
        };
        // plain http to 127.0.0.1 is what a local test needs; the default policy forbids both
        p.fetch_policy = FetchPolicy::test_mode();
        let state = open_chain(MemKv::new(), &self.genesis).unwrap();
        self.nodes[i] = Some(Node::start(p, state).unwrap());
    }

    fn node(&self, i: usize) -> &Node {
        self.nodes[i].as_ref().expect("node is running")
    }

    fn wait_until(&self, what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let pending = self.node(0).call("btb_actionRequests", json!([])).unwrap();
        panic!("timed out after {secs}s waiting for {what}; pending: {pending}");
    }

    fn wait_connected(&self) {
        let n = self.keys.len() - 1;
        self.wait_until("peer connections", 20, || {
            (0..self.keys.len()).all(|i| {
                self.node(i).call("btb_status", json!([])).unwrap()["peers"]
                    .as_u64()
                    .unwrap() as usize
                    >= n
            })
        });
    }

    fn send(&self, via: usize, tx: &Transaction) -> Hash {
        let r = self
            .node(via)
            .call("btb_sendTransaction", json!([hex::encode(&tx.encode())]))
            .unwrap_or_else(|e| panic!("submit failed: {e:?}"));
        hex::decode32(r["id"].as_str().unwrap()).unwrap()
    }

    fn receipt(&self, via: usize, id: &Hash) -> Value {
        let mut out = Value::Null;
        self.wait_until("a receipt", 30, || {
            let r = self
                .node(via)
                .call("btb_getReceipt", json!([hex::encode(id)]))
                .unwrap();
            if r.is_null() {
                false
            } else {
                out = r;
                true
            }
        });
        out
    }

    fn run(&mut self, via: usize, kind: TxKind) -> Value {
        let tx = self.faucet.sign(kind);
        let id = self.send(via, &tx);
        let r = self.receipt(via, &id);
        assert_eq!(r["status"], "success", "{r}");
        r
    }

    /// The contract's delivery log: `(status, data, args, request id)`.
    fn log(&self, node: usize, c: &Address) -> Vec<(u8, Vec<u8>, Vec<u8>, Hash)> {
        let r = self
            .node(node)
            .call("btb_query", json!([hex::encode(c), "last"]))
            .unwrap();
        let raw = Vec::<u8>::decode(&hex::decode(r["output"].as_str().unwrap()).unwrap()).unwrap();
        let mut out = Vec::new();
        let mut r = &raw[..];
        while !r.is_empty() {
            let status = r[0];
            let n = u32::from_le_bytes(r[1..5].try_into().unwrap()) as usize;
            let data = r[5..5 + n].to_vec();
            r = &r[5 + n..];
            let m = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
            let args = r[4..4 + m].to_vec();
            r = &r[4 + m..];
            let id: Hash = r[..32].try_into().unwrap();
            r = &r[32..];
            out.push((status, data, args, id));
        }
        out
    }

    fn request(&mut self, via: usize, c: Address, function: &str, url: &str) -> Hash {
        let r = self.run(
            via,
            TxKind::Action(ActionOp::Request {
                contract: c,
                function: function.into(),
                args: url.to_string().encode(),
                gas_limit: 2_000_000,
                max_fee: FEE,
            }),
        );
        hex::decode32(r["output"].as_str().unwrap()).unwrap_or_else(|_| {
            // the receipt shows the output as hex of the request id
            panic!("no request id in {r}")
        })
    }
}

#[test]
fn four_validators_fetch_transform_agree_and_deliver() {
    let mut c = Cluster::new(4);
    c.wait_connected();

    // deploy the contract, fund it (it pays the deposit of what its callbacks store), allow the
    // local server
    let deploy = c.faucet.sign(TxKind::Deploy {
        code: elf().to_vec(),
        manifest: Default::default(),
    });
    let contract = contract_address(&c.faucet.addr(), deploy.body.nonce);
    let id = c.send(0, &deploy);
    assert_eq!(c.receipt(0, &id)["status"], "success");
    c.run(
        1,
        TxKind::Transfer {
            to: contract,
            amount: UNITS_PER_BTB,
        },
    );

    // ---- 1. the happy path: a nonce that differs per request is stripped by the transform
    let server = serve(
        |n| format!("nonce={}\nprice=42\nts=17000{n}\n", n * 7919),
        0,
    );
    c.run(
        2,
        TxKind::Action(ActionOp::SetHosts {
            contract,
            hosts: vec![server.host()],
        }),
    );
    let req = c.request(3, contract, "quote", &server.url());
    c.wait_until("the callback", 30, || !c.log(0, &contract).is_empty());
    let log = c.log(0, &contract);
    assert_eq!(log.len(), 1, "delivered exactly once");
    assert_eq!(log[0].0, 0, "agreed");
    assert_eq!(log[0].1, 42u64.encode());
    assert_eq!(
        log[0].2,
        server.url().encode(),
        "the request's arguments come back"
    );
    assert_eq!(log[0].3, req);
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        4,
        "each validator fetched once"
    );
    // every node has the same log
    c.wait_until("the log on every node", 30, || {
        (0..4).all(|i| c.log(i, &contract) == log)
    });
    let pending = c.node(0).call("btb_actionRequests", json!([])).unwrap();
    assert_eq!(pending["pending"].as_array().unwrap().len(), 0);

    // ---- 2. no transform: every validator sees different bytes, so they disagree
    let req2 = c.request(0, contract, "raw", &server.url());
    c.wait_until("the second callback", 30, || c.log(0, &contract).len() == 2);
    let log = c.log(0, &contract);
    assert_eq!(log[1].0, 2, "disagreement is reported as an error callback");
    assert!(log[1].1.is_empty());
    assert_eq!(log[1].3, req2);

    // ---- 3. a server that answers after the request's time limit: every validator times out,
    //         and "two thirds agree it failed" is delivered as an agreed failure
    let slow = serve(|_| "price=1\n".into(), 6_000);
    c.run(
        2,
        TxKind::Action(ActionOp::SetHosts {
            contract,
            hosts: vec![server.host(), slow.host()],
        }),
    );
    let req3 = c.request(1, contract, "quote", &slow.url());
    c.wait_until("the third callback", 30, || c.log(0, &contract).len() == 3);
    let log = c.log(0, &contract);
    assert_eq!((log[2].0, log[2].1.as_slice()), (1, &b"timeout"[..]));
    assert_eq!(log[2].3, req3);

    // ---- 4. a host outside the allowlist is refused by the chain before anything is fetched
    let outsider = serve(|_| "price=9\n".into(), 0);
    let tx = c.faucet.sign(TxKind::Action(ActionOp::Request {
        contract,
        function: "quote".into(),
        args: outsider.url().encode(),
        gas_limit: 2_000_000,
        max_fee: FEE,
    }));
    let id = c.send(2, &tx);
    let r = c.receipt(2, &id);
    assert_eq!(r["status"], "reverted");
    assert!(r["detail"].to_string().contains("HostNotAllowed"), "{r}");
    assert_eq!(outsider.hits.load(Ordering::SeqCst), 0);

    // the chain stayed in step
    let h = (0..4)
        .map(|i| {
            c.node(i).call("btb_status", json!([])).unwrap()["height"]
                .as_u64()
                .unwrap()
        })
        .min()
        .unwrap();
    let blocks: Vec<Value> = (0..4)
        .map(|i| c.node(i).call("btb_getBlock", json!([h])).unwrap())
        .collect();
    for b in &blocks {
        assert_eq!(b["state_root_after"], blocks[0]["state_root_after"]);
    }
}
