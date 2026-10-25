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
    /// Keep chain state and block history in RocksDB under the node's data dir.
    disk: bool,
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
        Self::new_with(total, running, timing, delay_ms, epoch_len, false)
    }

    fn new_with(
        total: usize,
        running: usize,
        timing: Timing,
        delay_ms: u64,
        epoch_len: u64,
        disk: bool,
    ) -> Cluster {
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
            disk,
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
        #[cfg(feature = "rocksdb")]
        if self.disk {
            let dir = self._dir.path().join(format!("node{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            let kv = btb_runtime::rocks::RocksKv::open(dir.join("chain")).unwrap();
            let state = open_chain(kv, &self.genesis).unwrap();
            self.nodes[i] = Some(Node::start(p, state).unwrap());
            return;
        }
        assert!(!self.disk, "disk clusters need --features rocksdb");
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

fn epoch_of(c: &Cluster, i: usize) -> u64 {
    c.status(i)["epoch"].as_u64().unwrap()
}

fn tx_ids_of(v: &Value) -> Vec<String> {
    v["evidence_submitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn double_voter_is_reported_slashed_jailed_and_rotated_out() {
    let _g = serial();
    let timing = Timing {
        tick_ms: 10,
        round_timeout_ms: 400,
        idle_block_ms: 80,
    };
    // Validator 3 is the byzantine one: it votes twice in every round of epoch 0.
    let mut c = Cluster::new(4, 3, timing, 0, 24);
    let byz = Byzantine::start(&c, 3);
    c.wait_connected(3);
    let byz_key = hex::encode(&c.keys[3].verifying_key().to_bytes());

    c.wait_until("a node to file SubmitEvidence", 30, || {
        (0..3).any(|i| !tx_ids_of(&c.status(i)).is_empty())
    });
    // However many double votes follow, a node files at most one transaction for the offender.
    std::thread::sleep(Duration::from_millis(800));
    let mut filed = Vec::new();
    for i in 0..3 {
        let ids = tx_ids_of(&c.status(i));
        assert!(ids.len() <= 1, "node {i} filed {} reports", ids.len());
        filed.extend(ids);
    }
    println!(
        "evidence transactions filed across 3 honest nodes: {}",
        filed.len()
    );
    // The chain executed them; exactly one was accepted (one slash per operator per epoch).
    let mut accepted = 0;
    for id in &filed {
        let id = hex::decode32(id).unwrap();
        let r = c.receipt(0, &id);
        if r["status"] == "success" {
            accepted += 1;
        } else {
            assert_eq!(r["status"], "invalid", "{r}");
        }
    }
    assert_eq!(accepted, 1, "exactly one report is accepted");

    // Epoch 0 had four validators; epoch 1 drops the jailed one.
    c.wait_until("epoch 1", 90, || epoch_of(&c, 0) >= 1);
    let e0 = c.node(0).call("btb_getEpoch", json!([0])).unwrap();
    assert_eq!(e0["validators"].as_array().unwrap().len(), 4);
    let e1 = c.node(0).call("btb_getEpoch", json!([1])).unwrap();
    let keys1: Vec<&str> = e1["validators"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys1.len(), 3, "{keys1:?}");
    assert!(
        !keys1.contains(&byz_key.as_str()),
        "offender still in epoch 1"
    );

    // Finality continues in the new set, across another boundary.
    let bob = account_address(&key(77).verifying_key().to_bytes());
    let mut k = 0u64;
    while epoch_of(&c, 0) < 2 || k < 4 {
        let via = (k % 3) as usize;
        let id = c.send_transfer(via, bob, 10 + k);
        assert_eq!(c.receipt(via, &id)["status"], "success");
        k += 1;
        std::thread::sleep(Duration::from_millis(40));
    }
    c.assert_same_state();
    drop(byz);
}

/// A tiny validator-keyed peer that is not a running node: it can send arbitrary frames and
/// collects what it receives.
struct RawPeer {
    net: net::NetHandle,
    rx: std::sync::mpsc::Receiver<([u8; 32], Inbound)>,
    _rt: tokio::runtime::Runtime,
}

impl RawPeer {
    fn start(c: &Cluster, index: usize) -> RawPeer {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: net::Sink = Arc::new(move |from, m| {
            let _ = tx.send((from, m));
        });
        let all = validators_keys(c);
        let peers: Vec<([u8; 32], Multiaddr)> = (0..c.keys.len())
            .filter(|j| *j != index)
            .map(|j| (all[j], c.addr_of(j).parse().unwrap()))
            .collect();
        let net = net::start(
            rt.handle(),
            net::NetParams {
                secret: c.keys[index].to_bytes(),
                listen: format!("/ip4/127.0.0.1/udp/{}/quic-v1", c.ports[index])
                    .parse()
                    .unwrap(),
                validators: all,
                peers,
                rate_per_sec: 1e9,
                rate_burst: 1e9,
                delay: Duration::ZERO,
            },
            sink,
        )
        .unwrap();
        RawPeer { net, rx, _rt: rt }
    }

    /// The next sync response from `from`, ignoring other traffic.
    fn sync_response(&self, from: &[u8; 32], secs: u64) -> Option<btb_node::wire::SyncResponse> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Ok((f, Inbound::SyncResp(r))) = self.rx.recv_timeout(Duration::from_millis(50)) {
                if f == *from {
                    return Some(*r);
                }
            }
        }
        None
    }
}

#[test]
fn sync_server_proves_what_it_serves_and_ignores_forged_responses() {
    use btb_node::sync::{verify, Expect};
    use btb_node::wire::{sync_request_frame, sync_response_frame, SyncResponse};
    let _g = serial();
    let mut c = Cluster::new(4, 3, fast(), 0, 4);
    let peer = RawPeer::start(&c, 3);
    c.wait_connected(3);
    c.wait_until("past the first epoch", 40, || c.height(0) >= 7);
    let all = validators_keys(&c);

    // The context a syncing node would derive for epoch 0 from its own state.
    let ep = c.node(0).call("btb_getEpoch", json!([0])).unwrap();
    let set = ValidatorSet::new(
        ep["validators"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| Validator {
                key: hex::decode32(v["key"].as_str().unwrap()).unwrap(),
                stake: v["stake"].as_str().unwrap().parse().unwrap(),
            })
            .collect(),
    )
    .unwrap();
    let anchor = hex::decode32(ep["anchor"].as_str().unwrap()).unwrap();
    let ctx = Context::for_epoch(
        &btb_chain::staking::consensus_chain_id(CHAIN_ID),
        0,
        set,
        Some(anchor),
    );
    let expect = |first: u64, parent| Expect {
        ctx: &ctx,
        epoch: 0,
        first_height: first,
        parent,
        end_height: 4,
    };

    // From height 1: the whole first epoch, closed by a proof, from every honest node.
    for (target, key) in all.iter().enumerate().take(3) {
        peer.net.send(*key, sync_request_frame(1));
        let r = peer.sync_response(key, 10).expect("an answer");
        assert_eq!(
            verify(&r, &expect(1, anchor)),
            Ok(4),
            "node {target}: {r:?}"
        );
        // The wire encoding survives a round trip.
        assert_eq!(SyncResponse::decode(&r.encode()), Some(r.clone()));
    }
    // From the middle of the epoch.
    peer.net.send(all[0], sync_request_frame(1));
    let first = peer.sync_response(&all[0], 10).unwrap();
    peer.net.send(all[1], sync_request_frame(3));
    let mid = peer.sync_response(&all[1], 10).unwrap();
    assert_eq!(
        verify(&mid, &expect(3, first.blocks[1].id())),
        Ok(2),
        "{mid:?}"
    );
    // Beyond the head, and height 0: an empty answer, not an error.
    for h in [10_000u64, 0] {
        peer.net.send(all[0], sync_request_frame(h));
        let r = peer.sync_response(&all[0], 10).unwrap();
        assert!(r.blocks.is_empty());
    }
    // A response tampered with in flight no longer verifies.
    let mut forged = first.clone();
    forged.qc.as_mut().expect("closing proof").votes[0].1 .0[0] ^= 1;
    assert!(verify(&forged, &expect(1, anchor)).is_err());
    let mut cut = first.clone();
    cut.blocks[2].payload.push(1);
    assert!(verify(&cut, &expect(1, anchor)).is_err());

    // Unsolicited responses (forged, replayed, truncated, empty) are ignored by the nodes.
    let junk = [
        sync_response_frame(&first),
        sync_response_frame(&forged),
        sync_response_frame(&SyncResponse::default()),
        sync_response_frame(&first)[..40].to_vec(),
        vec![4u8; 5000],
    ];
    for j in junk {
        for key in all.iter().take(3) {
            peer.net.send(*key, j.clone());
        }
    }
    c.wait_until("junk to be counted", 20, || {
        (0..3).all(|i| c.status(i)["sync"]["unsolicited"].as_u64().unwrap() >= 3)
    });
    let before = c.height(0);
    let id = c.send_transfer(1, account_address(&key(77).verifying_key().to_bytes()), 5);
    assert_eq!(c.receipt(1, &id)["status"], "success");
    assert!(c.height(0) >= before);
    for i in 0..3 {
        let st = c.status(i);
        assert_eq!(st["sync"]["rejected"], 0);
        assert_eq!(st["sync"]["blocks_synced"], 0, "nobody was behind");
        assert!(st["net"]["dropped_malformed"].as_u64().unwrap() >= 2);
    }
    c.assert_same_state();
}

/// Node 3 is offline for many more epochs than peers keep consensus instances for and must
/// catch up through block sync. With `disk`, state and history live in RocksDB.
fn lagging_node_catches_up(disk: bool) {
    let mut c = Cluster::new_with(4, 4, fast(), 0, 4, disk);
    c.wait_connected(3);
    let bob = account_address(&key(77).verifying_key().to_bytes());
    let mut total = 0;
    let id = c.send_transfer(0, bob, 1_000);
    total += 1_000;
    let early = c.receipt(0, &id);
    assert_eq!(early["status"], "success");
    c.wait_until("node 3 to see it", 20, || c.balance(3, &bob) == total);
    let early_height = early["height"].as_u64().unwrap();
    let early_block = c
        .node(3)
        .call("btb_getBlock", json!([early_height]))
        .unwrap();
    assert!(!early_block.is_null());

    c.kill(3);
    let e_kill = epoch_of(&c, 0);
    let h_kill = c.height(0);
    // 11 more epochs of 4 blocks with node 3 away; transactions along the way.
    let t = Instant::now();
    let mut k = 0u64;
    while epoch_of(&c, 0) < e_kill + 11 {
        let via = (k % 3) as usize;
        let id = c.send_transfer(via, bob, 10 + k);
        assert_eq!(c.receipt(via, &id)["status"], "success");
        total += 10 + k;
        k += 1;
    }
    println!(
        "node 3 offline for {} epochs ({} blocks) in {} ms",
        epoch_of(&c, 0) - e_kill,
        c.height(0) - h_kill,
        t.elapsed().as_millis()
    );

    c.start(3);
    if disk {
        // History from before the restart is there at once, from disk.
        let b = c
            .node(3)
            .call("btb_getBlock", json!([early_height]))
            .unwrap();
        assert_eq!(b["hash"], early_block["hash"]);
        let r = c
            .node(3)
            .call("btb_getReceipt", json!([early["id"]]))
            .unwrap();
        assert_eq!(r["status"], "success");
    }
    c.wait_connected(3);
    c.wait_until("node 3 to catch up through block sync", 90, || {
        c.height(3) + 3 >= c.height(0) && epoch_of(&c, 3) + 1 >= epoch_of(&c, 0)
    });
    let synced = c.status(3)["sync"]["blocks_synced"].as_u64().unwrap();
    println!(
        "node 3 block-synced {synced} blocks over {} epochs",
        epoch_of(&c, 0) - e_kill
    );
    assert!(
        synced >= 4 * 9,
        "catching up more than 8 epochs needs block sync (synced {synced})"
    );
    assert_eq!(c.status(3)["sync"]["rejected"], 0);
    c.wait_until("same balance", 20, || {
        c.balance(3, &bob) == c.balance(0, &bob)
    });
    assert_eq!(c.balance(3, &bob), total);
    c.assert_same_state();
    // History: with RocksDB every height, also the ones before the restart, is on node 3.
    let upto = c.height(3).min(c.height(0)).saturating_sub(1);
    if disk {
        for h in 1..=upto {
            let a = c.node(3).call("btb_getBlock", json!([h])).unwrap();
            let b = c.node(0).call("btb_getBlock", json!([h])).unwrap();
            assert!(!a.is_null(), "block {h} missing on the restarted node");
            assert_eq!(a["hash"], b["hash"], "block {h}");
            assert_eq!(a["transactions"], b["transactions"], "block {h}");
        }
    } else {
        // Synced heights are served; the memory store of the restarted node starts empty.
        let h = h_kill + 5;
        let a = c.node(3).call("btb_getBlock", json!([h])).unwrap();
        let b = c.node(0).call("btb_getBlock", json!([h])).unwrap();
        assert_eq!(a["hash"], b["hash"]);
    }
    // And it takes part again.
    let id = c.send_transfer(3, bob, 7);
    assert_eq!(c.receipt(3, &id)["status"], "success");
    assert_eq!(c.evidence_total(), 0, "a long absence is not equivocation");
    if disk {
        // Whole network restart from disk: same chain, history intact, still live.
        let h = c.height(0);
        let hash = c.node(0).call("btb_getBlock", json!([h])).unwrap()["hash"].clone();
        for i in 0..4 {
            c.kill(i);
        }
        for i in 0..4 {
            c.start(i);
        }
        c.wait_connected(3);
        for i in 0..4 {
            let b = c.node(i).call("btb_getBlock", json!([h])).unwrap();
            assert_eq!(b["hash"], hash, "node {i} lost block {h}");
        }
        let id = c.send_transfer(2, bob, 9);
        assert_eq!(c.receipt(2, &id)["status"], "success");
        c.wait_until("everyone past the restart height", 60, || {
            (0..4).all(|i| c.height(i) > h)
        });
        c.assert_same_state();
        assert_eq!(c.evidence_total(), 0, "a restart is not equivocation");
    }
}

#[test]
fn node_offline_for_more_than_eight_epochs_catches_up_by_block_sync() {
    let _g = serial();
    lagging_node_catches_up(false);
}

#[cfg(feature = "rocksdb")]
#[test]
fn rocksdb_nodes_restart_from_disk_and_a_lagging_one_block_syncs() {
    let _g = serial();
    lagging_node_catches_up(true);
}
