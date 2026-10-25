//! In-process multi-node tests: real libp2p/QUIC on localhost with ephemeral-free fixed ports
//! picked at start, real consensus, real execution. Clusters run one at a time (a global lock)
//! so the latency numbers are not polluted by other clusters competing for the CPU.

#[path = "../../btb-tests/tests/common/mod.rs"]
mod common;

use btb_chain::{
    account_address, contract_address, Address, Hash, SigningKey, Transaction, TxBody, TxKind,
    TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_codec::{Decode, Encode};
use btb_consensus::{Context, Message, Sig, Validator, ValidatorSet, Vote};
use btb_node::config::{GenesisFile, NodeParams, Timing};
use btb_node::wire::{consensus_frame, Inbound};
use btb_node::{hex, net, open_chain, Node};
use btb_runtime::{KvError, KvStore, MemKv, WriteBatch};
use ed25519_dalek::Signer;
use libp2p::Multiaddr;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const CHAIN_ID: u64 = 7;

fn serial() -> MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

fn counter_elf() -> &'static [u8] {
    static E: OnceLock<Vec<u8>> = OnceLock::new();
    E.get_or_init(|| common::build_example("counter"))
}

/// A `KvStore` whose contents outlive a `Node` (stands in for RocksDB across a restart).
#[derive(Clone, Default)]
struct SharedKv(Arc<Mutex<MemKv>>);

impl KvStore for SharedKv {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        self.0.lock().unwrap().get(key)
    }
    fn scan(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
        self.0.lock().unwrap().scan(start, end, reverse, limit)
    }
    fn commit(&mut self, batch: WriteBatch) -> Result<(), KvError> {
        self.0.lock().unwrap().commit(batch)
    }
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
    fn transfer(&mut self, to: Address, amount: u64) -> Transaction {
        self.sign(TxKind::Transfer { to, amount })
    }
    fn call(&mut self, contract: Address, f: &str, args: &impl Encode) -> Transaction {
        self.sign(TxKind::Call {
            contract,
            kind: btb_vm::FunctionKind::Mutation,
            function: f.into(),
            args: args.encode(),
            gas_limit: 2_000_000,
            max_fee: u64::MAX,
        })
    }
}

fn validators_keys(c: &Cluster) -> Vec<[u8; 32]> {
    c.keys
        .iter()
        .map(|k| k.verifying_key().to_bytes())
        .collect()
}

fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

struct Cluster {
    genesis: GenesisFile,
    keys: Vec<SigningKey>,
    ports: Vec<u16>,
    kvs: Vec<SharedKv>,
    nodes: Vec<Option<Node>>,
    timing: Timing,
    delay_ms: u64,
    faucet: Wallet,
    // Last: nodes must stop before their data directory disappears.
    _dir: tempfile::TempDir,
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

impl Cluster {
    /// `total` validator keys; the first `running` are started as real nodes.
    fn new(total: usize, running: usize, timing: Timing, delay_ms: u64, epoch_len: u64) -> Cluster {
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
            epoch_len,
            min_stake: 100 * UNITS_PER_BTB,
            validators: keys
                .iter()
                .map(|k| (k.verifying_key().to_bytes(), 100 * UNITS_PER_BTB))
                .collect(),
            allocations,
        };
        genesis.validate().unwrap();
        let mut c = Cluster {
            _dir: tempfile::tempdir().unwrap(),
            genesis,
            ports: (0..total).map(|_| free_udp_port()).collect(),
            keys,
            kvs: (0..total).map(|_| SharedKv::default()).collect(),
            nodes: (0..total).map(|_| None).collect(),
            timing,
            delay_ms,
            faucet,
        };
        for i in 0..running {
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
        p.data_dir = Some(self._dir.path().join(format!("node{i}")));
        p.timing = self.timing;
        p.test_one_way_delay_ms = self.delay_ms;
        let state = open_chain(self.kvs[i].clone(), &self.genesis).unwrap();
        self.nodes[i] = Some(Node::start(p, state).unwrap());
    }

    fn kill(&mut self, i: usize) {
        self.nodes[i].take().expect("running").shutdown();
    }

    fn node(&self, i: usize) -> &Node {
        self.nodes[i].as_ref().expect("node is running")
    }

    fn live(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|i| self.nodes[*i].is_some())
            .collect()
    }

    fn status(&self, i: usize) -> Value {
        self.node(i).call("btb_status", json!([])).unwrap()
    }

    fn height(&self, i: usize) -> u64 {
        self.status(i)["height"].as_u64().unwrap()
    }

    fn wait_until(&self, what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let st: Vec<String> = self
            .live()
            .iter()
            .map(|i| {
                let s = self.status(*i);
                format!(
                    "node{i}: h={} round={} peers={} cons={}",
                    s["height"], s["round"], s["peers"], s["consensus"]
                )
            })
            .collect();
        panic!(
            "timed out after {secs}s waiting for {what}\n{}",
            st.join("\n")
        );
    }

    /// Wait until every live node has connected to all `n` other validators it can reach.
    fn wait_connected(&self, expect_peers: usize) {
        self.wait_until("peer connections", 20, || {
            self.live()
                .iter()
                .all(|i| self.status(*i)["peers"].as_u64().unwrap() as usize >= expect_peers)
        });
    }

    fn send_transfer(&mut self, via: usize, to: Address, amount: u64) -> Hash {
        let tx = self.faucet.transfer(to, amount);
        self.send(via, &tx)
    }

    fn send(&self, via: usize, tx: &Transaction) -> Hash {
        let r = self
            .node(via)
            .call("btb_sendTransaction", json!([hex::encode(&tx.encode())]))
            .unwrap_or_else(|e| panic!("submit failed: {e:?}"));
        hex::decode32(r["id"].as_str().unwrap()).unwrap()
    }

    /// Wait for the receipt on node `via`; returns it.
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

    fn balance(&self, i: usize, a: &Address) -> u64 {
        let r = self
            .node(i)
            .call("btb_getBalance", json!([hex::encode(a)]))
            .unwrap();
        r["balance"].as_str().unwrap().parse().unwrap()
    }

    fn query_u64(&self, i: usize, c: &Address, f: &str) -> u64 {
        let r = self
            .node(i)
            .call("btb_query", json!([hex::encode(c), f]))
            .unwrap();
        u64::decode(&hex::decode(r["output"].as_str().unwrap()).unwrap()).unwrap()
    }

    /// All live nodes agree on the block at a height every one of them has reached, and on
    /// the state it produced.
    fn assert_same_state(&self) {
        self.wait_until("all live nodes to commit something", 30, || {
            self.live().iter().all(|i| self.height(*i) >= 1)
        });
        let h = self.live().iter().map(|i| self.height(*i)).min().unwrap();
        let blocks: Vec<Value> = self
            .live()
            .iter()
            .map(|i| self.node(*i).call("btb_getBlock", json!([h])).unwrap())
            .collect();
        for b in &blocks {
            assert!(!b.is_null(), "block {h} missing on a node");
            assert_eq!(b["hash"], blocks[0]["hash"], "block {h} differs");
            assert_eq!(b["state_root_after"], blocks[0]["state_root_after"]);
        }
    }

    fn evidence_total(&self) -> u64 {
        self.live()
            .iter()
            .map(|i| {
                self.status(*i)["consensus"]["evidence_found"]
                    .as_u64()
                    .unwrap()
            })
            .sum()
    }
}

fn fast() -> Timing {
    Timing {
        tick_ms: 10,
        round_timeout_ms: 500,
        idle_block_ms: 80,
    }
}

/// Submit `n` transfers round-robin over `via` and return per-tx submit-to-commit latencies in
/// ms as measured by the submitting node.
fn measure(c: &mut Cluster, via: &[usize], n: usize) -> Vec<u64> {
    let bob = key(77);
    let to = account_address(&bob.verifying_key().to_bytes());
    let mut lat = Vec::new();
    for k in 0..n {
        let node = via[k % via.len()];
        let tx = c.faucet.transfer(to, 1_000 + k as u64);
        let id = c.send(node, &tx);
        let r = c.receipt(node, &id);
        assert_eq!(r["status"], "success", "{r}");
        lat.push(c.status(node)["last_commit_latency_ms"].as_u64().unwrap());
        // Let the chain go idle between samples so each one is a cold submit.
        std::thread::sleep(Duration::from_millis(60 + (k as u64 * 37) % 160));
    }
    lat
}

fn summarize(label: &str, lat: &[u64]) -> (u64, u64) {
    let mut s = lat.to_vec();
    s.sort_unstable();
    let med = s[s.len() / 2];
    let max = *s.last().unwrap();
    println!(
        "FINALITY[{label}] n={} min={}ms median={}ms p90={}ms max={}ms",
        s.len(),
        s[0],
        med,
        s[(s.len() * 9 / 10).min(s.len() - 1)],
        max
    );
    (med, max)
}

#[test]
fn four_nodes_agree_on_transfer_and_counter_and_finalize_fast() {
    let _g = serial();
    let mut c = Cluster::new(4, 4, fast(), 0, 600);
    c.wait_connected(3);

    // Transfer.
    let bob = account_address(&key(77).verifying_key().to_bytes());
    let tx = c.faucet.transfer(bob, 5 * UNITS_PER_BTB);
    let id = c.send(1, &tx);
    assert_eq!(c.receipt(1, &id)["status"], "success");

    // Deploy the counter, fund it, increment three times.
    let deploy = c.faucet.sign(TxKind::Deploy {
        code: counter_elf().to_vec(),
        manifest: Default::default(),
    });
    let counter = contract_address(&c.faucet.addr(), deploy.body.nonce);
    let id = c.send(2, &deploy);
    assert_eq!(c.receipt(2, &id)["status"], "success");
    let id = c.send_transfer(0, counter, UNITS_PER_BTB);
    assert_eq!(c.receipt(0, &id)["status"], "success");
    let mut last = Hash::default();
    for _ in 0..3 {
        let tx = c.faucet.call(counter, "increment", &(5u64,));
        last = c.send(3, &tx);
    }
    let r = c.receipt(3, &last);
    assert_eq!(r["status"], "success", "{r}");

    // Every node converges on the same chain and sees the same results.
    c.wait_until("counter = 15 everywhere", 30, || {
        c.live()
            .iter()
            .all(|i| c.query_u64(*i, &counter, "get") == 15)
    });
    for i in c.live() {
        assert_eq!(c.balance(i, &bob), 5 * UNITS_PER_BTB, "node {i}");
    }
    c.assert_same_state();

    // Finality latency: cold submits spread over all nodes.
    let lat = measure(&mut c, &[0, 1, 2, 3], 24);
    let (med, max) = summarize("localhost 4 validators", &lat);
    assert!(
        med < 1_000,
        "median finality {med}ms should be well under 1 s"
    );
    assert!(max < 3_000, "max finality {max}ms");
    c.assert_same_state();
    assert_eq!(c.evidence_total(), 0, "honest nodes produce no evidence");
    for i in c.live() {
        let b = c.balance(i, &c.faucet.addr());
        let _ = b; // all nodes answer
    }
}

#[test]
fn killed_node_does_not_stop_progress_and_catches_up_after_restart() {
    let _g = serial();
    let mut c = Cluster::new(4, 4, fast(), 0, 15);
    c.wait_connected(3);
    let bob = account_address(&key(77).verifying_key().to_bytes());
    let id = c.send_transfer(0, bob, 1_000);
    c.receipt(0, &id);

    // Kill validator 3 (a quarter of the stake, and a leader every fourth round).
    c.kill(3);
    let before = c.height(0);
    let t = Instant::now();
    for k in 0..5 {
        let id = c.send_transfer(k % 3, bob, 1_000 + k as u64);
        assert_eq!(c.receipt(k % 3, &id)["status"], "success");
    }
    println!(
        "3 of 4 validators: 5 sequential transfers finalized in {} ms (rounds led by the dead node time out)",
        t.elapsed().as_millis()
    );
    assert!(c.height(0) > before);
    let missed = c.height(0);

    // Restart it on the same ports, safety state, execution log and chain database.
    c.start(3);
    c.wait_connected(3);
    c.wait_until("restarted node to catch up", 40, || {
        c.height(3) >= missed && c.height(3) + 3 >= c.height(0)
    });
    // It has the same balances and the same blocks as everyone else.
    c.wait_until("same balance on the restarted node", 20, || {
        c.balance(3, &bob) == c.balance(0, &bob)
    });
    c.assert_same_state();
    // And it keeps participating: a transaction submitted at the restarted node finalizes.
    let id = c.send_transfer(3, bob, 7);
    assert_eq!(c.receipt(3, &id)["status"], "success");
    assert_eq!(
        c.evidence_total(),
        0,
        "a restart must not look like equivocation"
    );
}

/// A validator that sends garbage, floods, and equivocates (two different votes in a round).
struct Byzantine {
    stop: Arc<AtomicBool>,
    flood: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    net: net::NetHandle,
    _rt: tokio::runtime::Runtime,
}

impl Byzantine {
    fn start(c: &Cluster, index: usize) -> Byzantine {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<([u8; 32], Inbound)>();
        let sink: net::Sink = Arc::new(move |from, m| {
            let _ = tx.send((from, m));
        });
        let validators: Vec<[u8; 32]> = c
            .keys
            .iter()
            .map(|k| k.verifying_key().to_bytes())
            .collect();
        let peers: Vec<([u8; 32], Multiaddr)> = (0..c.keys.len())
            .filter(|j| *j != index)
            .map(|j| (validators_keys(c)[j], c.addr_of(j).parse().unwrap()))
            .collect();
        let handle = net::start(
            rt.handle(),
            net::NetParams {
                secret: c.keys[index].to_bytes(),
                listen: format!("/ip4/127.0.0.1/udp/{}/quic-v1", c.ports[index])
                    .parse()
                    .unwrap(),
                validators,
                peers,
                rate_per_sec: 1e9,
                rate_burst: 1e9,
                delay: Duration::ZERO,
            },
            sink,
        )
        .unwrap();
        // Epoch 0's set in consensus order, as the chain fixed it.
        let ep = c.node(0).call("btb_getEpoch", json!([0])).unwrap();
        let vals: Vec<Validator> = ep["validators"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| Validator {
                key: hex::decode32(v["key"].as_str().unwrap()).unwrap(),
                stake: v["stake"].as_str().unwrap().parse().unwrap(),
            })
            .collect();
        let my_key = c.keys[index].verifying_key().to_bytes();
        let me = vals.iter().position(|v| v.key == my_key).unwrap() as u32;
        let set = ValidatorSet::new(vals).unwrap();
        let ctx = Context::for_epoch(
            &btb_chain::staking::consensus_chain_id(CHAIN_ID),
            0,
            set,
            None,
        );
        let signer = c.keys[index].clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let flood = Arc::new(AtomicBool::new(false));
        let flood2 = flood.clone();
        let net2 = handle.clone();
        let thread = std::thread::spawn(move || {
            let mut seed = 0x1234_5678_9abc_def1u64;
            let mut rnd = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            let mut burst_done = false;
            let mut last_garbage = Instant::now();
            while !stop2.load(Ordering::Relaxed) {
                if let Ok((_, Inbound::Consensus(epoch, m))) =
                    rx.recv_timeout(Duration::from_millis(5))
                {
                    let Message::Propose(p) = *m else { continue };
                    // Equivocate: vote for the real block and for a made-up one.
                    let real = p.block.id();
                    let mut fake = real;
                    fake[0] ^= 0xff;
                    for id in [real, fake] {
                        let sig = signer.sign(&ctx.vote_bytes(p.block.round, &id));
                        let v = Vote {
                            round: p.block.round,
                            block_id: id,
                            voter: me,
                            sig: Sig(sig.to_bytes()),
                            proposal_sig: p.sig,
                        };
                        net2.broadcast(consensus_frame(epoch, &Message::Vote(v)));
                    }
                    // A valid-looking proposal message mangled in flight.
                    let mut bytes = consensus_frame(epoch, &Message::Propose(p));
                    let n = bytes.len();
                    bytes[(rnd() as usize) % n] ^= 0x5a;
                    net2.broadcast(bytes);
                }
                let flooding = flood2.load(Ordering::Relaxed);
                let every = if flooding { 15 } else { 400 };
                if last_garbage.elapsed() > Duration::from_millis(every) {
                    last_garbage = Instant::now();
                    let len = 1 + (rnd() % 3000) as usize;
                    let mut junk: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
                    junk[0] = [1u8, 2, 9, 0][(rnd() % 4) as usize];
                    net2.broadcast(junk);
                }
                if flooding && !burst_done {
                    burst_done = true;
                    // Flood: a big oversize-ish random frame plus thousands of tiny ones.
                    let mut big: Vec<u8> = (0..1_000_000).map(|_| rnd() as u8).collect();
                    big[0] = 1;
                    net2.broadcast(big);
                    for _ in 0..6_000 {
                        net2.broadcast(vec![1, 0, 0]);
                    }
                }
            }
        });
        Byzantine {
            stop,
            flood,
            thread: Some(thread),
            net: handle,
            _rt: rt,
        }
    }
}

impl Drop for Byzantine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn garbage_flooding_equivocating_validator_does_not_stop_progress() {
    let _g = serial();
    let timing = Timing {
        tick_ms: 10,
        round_timeout_ms: 400,
        idle_block_ms: 80,
    };
    let mut c = Cluster::new(4, 3, timing, 0, 600);
    let byz = Byzantine::start(&c, 3);
    c.wait_connected(3);
    let _ = &byz.net;

    let bob = account_address(&key(77).verifying_key().to_bytes());
    let t = Instant::now();
    let mut sent = 0u64;
    let mut total = 0u64;
    let mut run = |c: &mut Cluster, n: usize| {
        for _ in 0..n {
            let via = (sent % 3) as usize;
            let id = c.send_transfer(via, bob, 100 + sent);
            let r = c.receipt(via, &id);
            assert_eq!(r["status"], "success", "{r}");
            total += 100 + sent;
            sent += 1;
        }
    };
    // Phase 1: the byzantine validator votes twice per round and sprinkles garbage.
    run(&mut c, 8);
    println!(
        "equivocating validator active: 8 transfers finalized in {} ms",
        t.elapsed().as_millis()
    );
    c.wait_until("double-vote evidence", 20, || c.evidence_total() >= 1);
    // Phase 2: it also floods (a 1 MB junk frame, thousands of tiny ones, junk every 15 ms).
    byz.flood.store(true, Ordering::Relaxed);
    let t = Instant::now();
    run(&mut c, 8);
    println!(
        "while flooded: 8 more transfers finalized in {} ms",
        t.elapsed().as_millis()
    );
    c.assert_same_state();
    for i in 0..3 {
        assert_eq!(c.balance(i, &bob), total);
    }
    let malformed: u64 = (0..3)
        .map(|i| c.status(i)["net"]["dropped_malformed"].as_u64().unwrap())
        .sum();
    let limited: u64 = (0..3)
        .map(|i| {
            let n = &c.status(i)["net"];
            n["dropped_rate"].as_u64().unwrap() + n["dropped_banned"].as_u64().unwrap()
        })
        .sum();
    println!("garbage dropped: malformed={malformed} rate-limited-or-banned={limited}");
    assert!(malformed > 0, "garbage frames must be counted and dropped");
    assert!(
        limited > 0,
        "the flood must hit the per-peer limits or the ban"
    );
    drop(byz);
}

#[test]
fn finality_with_emulated_wan_latency() {
    let _g = serial();
    // 100 ms one way between every pair (200 ms RTT) on top of localhost QUIC.
    let timing = Timing {
        tick_ms: 10,
        round_timeout_ms: 2_000,
        idle_block_ms: 150,
    };
    let mut c = Cluster::new(4, 4, timing, 100, 600);
    c.wait_connected(3);
    let lat = measure(&mut c, &[0, 1, 2, 3], 12);
    let (med, max) = summarize("emulated 100 ms one-way, 4 validators", &lat);
    assert!(med < 1_500, "median {med}ms");
    assert!(max < 4_000, "max {max}ms");
    c.assert_same_state();
}

#[test]
fn consensus_instances_rotate_every_epoch() {
    let _g = serial();
    // 8 blocks per epoch and a block every ~80 ms when idle: several epoch changes in seconds.
    let mut c = Cluster::new(4, 4, fast(), 0, 8);
    c.wait_connected(3);
    let bob = account_address(&key(77).verifying_key().to_bytes());
    let mut total = 0;
    let mut lat = Vec::new();
    // Keep submitting across epoch boundaries until the chain is well into epoch 4.
    let mut k = 0u64;
    while c.height(0) < 4 * 8 + 3 {
        let via = (k % 4) as usize;
        let id = c.send_transfer(via, bob, 10 + k);
        let r = c.receipt(via, &id);
        assert_eq!(r["status"], "success", "{r}");
        total += 10 + k;
        lat.push(c.status(via)["last_commit_latency_ms"].as_u64().unwrap());
        k += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
    c.wait_until("every node in epoch 4 or later", 20, || {
        c.live()
            .iter()
            .all(|i| c.status(*i)["epoch"].as_u64().unwrap() >= 4)
    });
    for i in c.live() {
        assert_eq!(c.balance(i, &bob), total, "node {i}");
    }
    c.assert_same_state();
    summarize("across 4 epoch boundaries", &lat);
    let ep = c.node(0).call("btb_getEpoch", json!([4])).unwrap();
    assert_eq!(ep["start_height"], 32);
    assert_eq!(ep["validators"].as_array().unwrap().len(), 4);
    assert_eq!(c.evidence_total(), 0);
}
