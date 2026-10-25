//! Oracles on a real cluster: libp2p/QUIC on localhost, real consensus, real execution, with the
//! threshold-BLS beacon and price feeds running. Clusters run one at a time.

#[path = "../../btb-tests/tests/common/mod.rs"]
mod common;

use btb_chain::{
    account_address, contract_address, Address, Hash, Manifest, OracleGenesis, PairConfig,
    SigningKey, Transaction, TxBody, TxKind, TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_codec::{Decode, Encode};
use btb_node::config::{GenesisFile, NodeParams, Timing};
use btb_node::oracle::PriceSource;
use btb_node::{hex, net, open_chain, Node};
use btb_oracle::{beacon_message, trusted_deal, Beacon, GroupKey, KeyShare, Params};
use btb_runtime::{KvError, KvStore, MemKv, WriteBatch};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const CHAIN_ID: u64 = 7;
const BTC: u128 = 6_500_000_000_000;

fn serial() -> MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

fn oracle_elf() -> &'static [u8] {
    static E: OnceLock<Vec<u8>> = OnceLock::new();
    E.get_or_init(|| common::build_example("oracle"))
}

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

fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
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
                relayer: None,
                chain_id: CHAIN_ID,
                nonce: self.nonce,
                kind,
            },
            &self.key,
        );
        self.nonce += 1;
        tx
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

/// What validator `i` reports for BTC/USD (pair 0); ETH/USD (pair 1) is `BTC / 20`.
type PriceFn = Arc<dyn Fn(usize, u32) -> Option<u128> + Send + Sync>;

struct Cluster {
    genesis: GenesisFile,
    keys: Vec<SigningKey>,
    shares: Vec<KeyShare>,
    group: GroupKey,
    /// The validators generate the beacon key on chain; `shares` and `group` are unused.
    #[allow(dead_code)]
    dkg: bool,
    ports: Vec<u16>,
    kvs: Vec<SharedKv>,
    nodes: Vec<Option<Node>>,
    prices: PriceFn,
    wallet: Wallet,
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

fn honest_prices() -> PriceFn {
    Arc::new(|i, pair| {
        Some(match pair {
            0 => BTC + i as u128,
            _ => BTC / 20 + i as u128,
        })
    })
}

impl Cluster {
    fn new(total: usize, running: usize, prices: PriceFn) -> Cluster {
        Self::build(total, running, prices, None)
    }

    /// `dkg`: `(epoch_len, dkg_window)` -> no trusted dealer; the validators generate the key.
    fn build(total: usize, running: usize, prices: PriceFn, dkg: Option<(u64, u64)>) -> Cluster {
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
        let (group, shares) = trusted_deal(Params::standard(total), &[42; 32]).unwrap();
        let genesis = GenesisFile {
            chain_id: CHAIN_ID,
            name: "btb-oracle-test".into(),
            timestamp: 0,
            gas_price: 1,
            epoch_len: dkg.map_or(10_000, |d| d.0),
            min_stake: 100 * UNITS_PER_BTB,
            storage: Default::default(),
            oracle: Some(OracleGenesis {
                group_key: if dkg.is_some() {
                    Vec::new()
                } else {
                    group.encode()
                },
                holders: if dkg.is_some() {
                    Vec::new()
                } else {
                    keys.iter().map(|k| k.verifying_key().to_bytes()).collect()
                },
                dkg_window: dkg.map_or(20, |d| d.1),
                pairs: vec![
                    PairConfig {
                        id: 0,
                        name: "BTC/USD".into(),
                        decimals: 8,
                    },
                    PairConfig {
                        id: 1,
                        name: "ETH/USD".into(),
                        decimals: 8,
                    },
                ],
                max_age_blocks: 200,
            }),
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
            kvs: (0..total).map(|_| SharedKv::default()).collect(),
            nodes: (0..total).map(|_| None).collect(),
            keys,
            shares: if dkg.is_some() { Vec::new() } else { shares },
            group,
            dkg: dkg.is_some(),
            prices,
            wallet: faucet,
            _dir: tempfile::tempdir().unwrap(),
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
        p.timing = Timing {
            tick_ms: 10,
            round_timeout_ms: 600,
            idle_block_ms: 100,
        };
        p.oracle_share = self.shares.get(i).cloned();
        p.dkg_dir = Some(self._dir.path().join(format!("dkg{i}")));
        p.price_interval_ms = 150;
        let f = self.prices.clone();
        p.price_source = Some(PriceSource::new(move |pair| f(i, pair)));
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
            std::thread::sleep(Duration::from_millis(25));
        }
        let st: Vec<String> = self
            .live()
            .iter()
            .map(|i| {
                let s = self.status(*i);
                format!(
                    "node{i}: h={} round={} peers={} oracle={}",
                    s["height"], s["round"], s["peers"], s["oracle"]
                )
            })
            .collect();
        panic!(
            "timed out after {secs}s waiting for {what}\n{}",
            st.join("\n")
        );
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
        self.wait_until("a receipt", 40, || {
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

    /// Submit through `via`, wait for the receipt, require success; returns the output bytes.
    fn run(&self, via: usize, tx: &Transaction) -> Vec<u8> {
        let id = self.send(via, tx);
        let r = self.receipt(via, &id);
        assert_eq!(r["status"], "success", "{r}");
        hex::decode(r["output"].as_str().unwrap()).unwrap()
    }

    fn price(&self, i: usize, pair: u32) -> Option<Value> {
        let r = self.node(i).call("btb_oraclePrice", json!([pair])).unwrap();
        (!r.is_null()).then_some(r)
    }

    fn wait_connected(&self) {
        let n = self.live().len() - 1;
        self.wait_until("peer connections", 20, || {
            self.live()
                .iter()
                .all(|i| self.status(*i)["peers"].as_u64().unwrap() as usize >= n)
        });
    }

    fn deploy_oracle(&mut self, via: usize) -> Address {
        let me = self.wallet.addr();
        let nonce = self.wallet.nonce;
        let tx = self.wallet.sign(TxKind::Deploy {
            code: oracle_elf().to_vec(),
            manifest: Manifest {
                name: "oracle".into(),
                crons: vec![],
                indexes: vec![],
            },
        });
        let out = self.run(via, &tx);
        let c = contract_address(&me, nonce);
        assert_eq!(out, c.to_vec());
        let fund = self.wallet.sign(TxKind::Transfer {
            to: c,
            amount: 50 * UNITS_PER_BTB,
        });
        self.run(via, &fund);
        c
    }

    /// Every live node holds the same, valid beacon for `height` (once they all reached it).
    fn assert_beacon(&self, height: u64) -> Beacon {
        self.wait_until("every node to reach the height", 30, || {
            self.live().iter().all(|i| self.height(*i) >= height)
        });
        let mut first: Option<Beacon> = None;
        for i in self.live() {
            let v = self
                .node(i)
                .call("btb_oracleBeacon", json!([height]))
                .unwrap();
            assert!(!v.is_null(), "node {i} has no beacon for {height}");
            let b = Beacon {
                epoch: v["epoch"].as_u64().unwrap(),
                height: v["height"].as_u64().unwrap(),
                signature: hex::decode(v["signature"].as_str().unwrap())
                    .unwrap()
                    .try_into()
                    .unwrap(),
            };
            assert!(
                b.verify(CHAIN_ID, &self.group),
                "node {i}: beacon {height} invalid"
            );
            assert_eq!(b.height, height);
            match &first {
                None => first = Some(b),
                Some(f) => assert_eq!(*f, b, "beacon {height} differs between nodes"),
            }
        }
        // the message is the documented one
        let b = first.unwrap();
        let msg = beacon_message(CHAIN_ID, 0, height);
        assert!(btb_oracle::bls::verify(
            &self.group.public_key,
            &msg,
            &btb_oracle::bls::G2::from_bytes(&b.signature).unwrap()
        ));
        b
    }

    fn assert_same_block(&self, h: u64) {
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
}

fn roll(c: &Cluster, via: usize, w: &mut Wallet, contract: Address) -> u64 {
    let tx = w.call(contract, "roll", &());
    u64::decode(&c.run(via, &tx)).unwrap()
}

#[test]
fn four_validators_run_beacon_randomness_and_price_feeds() {
    let _g = serial();
    let mut c = Cluster::new(4, 4, honest_prices());
    c.wait_connected();
    let contract = c.deploy_oracle(0);
    let mut w = Wallet {
        key: key(99),
        nonce: 0,
    };
    std::mem::swap(&mut w.nonce, &mut c.wallet.nonce);

    // Randomness: dice rolls through different nodes, all faces within 1..=6, and all nodes
    // executed identical blocks (so identical randomness).
    let mut seen = [0u32; 7];
    for k in 0..24 {
        let v = roll(&c, k % 4, &mut w, contract);
        assert!((1..=6).contains(&v), "roll {v}");
        seen[v as usize] += 1;
    }
    assert!(
        seen[1..].iter().filter(|n| **n > 0).count() >= 4,
        "{seen:?}"
    );
    // draws are unique 32-byte values
    let mut draws = Vec::new();
    for k in 0..4 {
        let tx = w.call(contract, "draw", &());
        draws.push(c.run(k % 4, &tx));
    }
    draws.sort();
    draws.dedup();
    assert_eq!(draws.len(), 4);

    // The marker is enforced on a live chain too: an unmarked mutation traps, a query traps.
    let tx = w.call(contract, "unmarked", &());
    let id = c.send(1, &tx);
    let r = c.receipt(1, &id);
    assert_eq!(r["status"], "reverted", "{r}");
    assert!(
        r["detail"]
            .as_str()
            .unwrap()
            .contains("RandomnessForbidden"),
        "{r}"
    );
    let q = c
        .node(2)
        .call("btb_query", json!([hex::encode(&contract), "cheat_query"]));
    let err = q.expect_err("a query cannot use randomness");
    assert!(err.message.contains("RandomnessForbidden"), "{err:?}");

    // Prices: the stake-weighted median of the four reports (BTC + 0..3 => lower median).
    c.wait_until("the BTC/USD price", 30, || c.price(0, 0).is_some());
    c.wait_until("the ETH/USD price", 30, || c.price(2, 1).is_some());
    for i in 0..4 {
        let p = c.price(i, 0).unwrap();
        let v: u128 = p["price"].as_str().unwrap().parse().unwrap();
        assert!((BTC..=BTC + 3).contains(&v), "{p}");
        assert_eq!(p["decimals"], 8);
        assert_eq!(p["name"], "BTC/USD");
    }
    // A contract reads it in a mutation and in a query.
    let tx = w.call(contract, "use_price", &(0u32, 1_000u64));
    let out = c.run(3, &tx);
    let v = u128::decode(&out).unwrap();
    assert!((BTC..=BTC + 3).contains(&v), "{v}");
    let q = c
        .node(1)
        .call(
            "btb_query",
            json!([hex::encode(&contract), "quote", hex::encode(&0u32.encode())]),
        )
        .unwrap();
    let got =
        Option::<(u128, u64, u8)>::decode(&hex::decode(q["output"].as_str().unwrap()).unwrap())
            .unwrap()
            .expect("price");
    assert!((BTC..=BTC + 3).contains(&got.0));
    assert_eq!(got.2, 8);

    // Beacons: verifiable by anyone, equal on every node, for several heights.
    let h = c.height(0);
    for height in [1, 2, h.saturating_sub(3).max(1)] {
        c.assert_beacon(height);
    }
    let a = c.assert_beacon(3);
    let b = c.assert_beacon(4);
    assert_ne!(a.signature, b.signature);
    c.assert_same_block(h.min(c.height(3)));
    // No node needed to ask for or reject anything unusual.
    for i in 0..4 {
        assert_eq!(c.status(i)["oracle"]["rejected_partials"], 0);
    }
}

#[test]
fn beacon_and_prices_stay_live_with_one_validator_down() {
    let _g = serial();
    let mut c = Cluster::new(4, 4, honest_prices());
    c.wait_connected();
    let contract = c.deploy_oracle(0);
    let mut w = Wallet {
        key: key(99),
        nonce: 0,
    };
    std::mem::swap(&mut w.nonce, &mut c.wallet.nonce);
    assert!((1..=6).contains(&roll(&c, 0, &mut w, contract)));
    // f = 1 of 4 fails: 3 of 4 partials still reach the threshold (3), consensus has 3 of 4 votes.
    c.kill(3);
    for k in 0..6 {
        let v = roll(&c, k % 3, &mut w, contract);
        assert!((1..=6).contains(&v));
    }
    let before = c
        .price(0, 0)
        .map(|p| p["height"].as_u64().unwrap())
        .unwrap_or(0);
    // the three live validators hold 75% of the stake (>= 2/3): prices keep updating
    c.wait_until("a newer price", 30, || {
        c.price(0, 0)
            .is_some_and(|p| p["height"].as_u64().unwrap() > before)
    });
    let h = c.live().iter().map(|i| c.height(*i)).min().unwrap();
    c.assert_beacon(h);
    c.assert_same_block(h);
}

#[test]
fn a_validator_reporting_garbage_prices_cannot_move_the_median() {
    let _g = serial();
    // validator 3 reports 1 unit; equal stakes mean it holds 25% < 1/3
    let prices: PriceFn = Arc::new(|i, pair| {
        Some(match (i, pair) {
            (3, _) => 1,
            (_, 0) => BTC + i as u128,
            _ => BTC / 20 + i as u128,
        })
    });
    let c = Cluster::new(4, 4, prices);
    c.wait_connected();
    c.wait_until("the BTC/USD price", 40, || c.price(0, 0).is_some());
    for _ in 0..8 {
        for i in 0..4 {
            if let Some(p) = c.price(i, 0) {
                let v: u128 = p["price"].as_str().unwrap().parse().unwrap();
                assert!(
                    (BTC..=BTC + 2).contains(&v),
                    "outlier moved the price to {v}"
                );
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_node_that_joins_late_catches_up_and_gets_the_beacons_it_needs() {
    let _g = serial();
    // Three of four validators run (3 >= threshold 3); the fourth starts after many blocks.
    let mut c = Cluster::new(4, 3, honest_prices());
    c.wait_connected();
    let contract = c.deploy_oracle(0);
    let mut w = Wallet {
        key: key(99),
        nonce: 0,
    };
    std::mem::swap(&mut w.nonce, &mut c.wallet.nonce);
    for k in 0..8 {
        roll(&c, k % 3, &mut w, contract);
    }
    let target = c.live().iter().map(|i| c.height(*i)).min().unwrap();
    c.start(3);
    // It has to replay every block with its beacon, fetched from the others by `Want`.
    c.wait_until("the late node to catch up", 60, || c.height(3) >= target);
    let h = target;
    c.assert_beacon(h);
    c.assert_same_block(h);
    // and it takes part afterwards
    c.wait_connected();
    for k in 0..4 {
        assert!((1..=6).contains(&roll(&c, 3, &mut w, contract)), "{k}");
    }
}

// ------------------------------------------------------------------------------ on-chain DKG

/// What `btb_oracleConfig` says about the key of the current epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyView {
    epoch: u64,
    origin: u64,
    group_key: Vec<u8>,
    holders: Vec<String>,
}

fn key_view(c: &Cluster, i: usize) -> Option<KeyView> {
    let v = c.node(i).call("btb_oracleConfig", json!([])).ok()?;
    let k = v.get("key").filter(|k| !k.is_null())?;
    Some(KeyView {
        epoch: v["epoch"].as_u64()?,
        origin: k["origin"].as_u64()?,
        group_key: hex::decode(k["group_key"].as_str()?).ok()?,
        holders: k["holders"]
            .as_array()?
            .iter()
            .map(|h| h.as_str().unwrap().to_string())
            .collect(),
    })
}

/// Check the beacon of `height` on node `i` against the group key `pk` (the message is the
/// documented one, so anybody could do this).
fn verify_beacon(c: &Cluster, i: usize, height: u64, pk: &[u8]) -> Beacon {
    let v = c.node(i).call("btb_oracleBeacon", json!([height])).unwrap();
    assert!(!v.is_null(), "node {i} has no beacon for {height}");
    let b = Beacon {
        epoch: v["epoch"].as_u64().unwrap(),
        height: v["height"].as_u64().unwrap(),
        signature: hex::decode(v["signature"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap(),
    };
    let pk = btb_oracle::bls::G1::from_bytes(pk.try_into().unwrap()).unwrap();
    let sig = btb_oracle::bls::G2::from_bytes(&b.signature).unwrap();
    assert!(
        btb_oracle::bls::verify(&pk, &beacon_message(CHAIN_ID, b.epoch, b.height), &sig),
        "beacon {height} (epoch {}) does not verify under the group key",
        b.epoch
    );
    b
}

#[test]
fn validators_generate_the_beacon_key_on_chain_and_reshare_it_across_epochs() {
    let _g = serial();
    // 5 validators, 50-block epochs, a 14-block DKG window (deals in the first 7).
    let (epoch_len, window) = (50u64, 14u64);
    let c = Cluster::build(5, 5, honest_prices(), Some((epoch_len, window)));
    c.wait_connected();

    // No trusted party: the key does not exist until the validators have generated it, and the
    // blocks of the window have no beacon.
    assert!(key_view(&c, 0).is_none() || c.height(0) > 1);
    c.wait_until("the genesis key", 60, || {
        (0..5).all(|i| key_view(&c, i).is_some())
    });
    let k0 = key_view(&c, 0).unwrap();
    assert_eq!((k0.epoch, k0.origin, k0.holders.len()), (0, 0, 5));
    for i in 1..5 {
        assert_eq!(
            key_view(&c, i).unwrap(),
            k0,
            "node {i} disagrees on the key"
        );
    }
    for h in 1..=window {
        let b = c.node(0).call("btb_oracleBeacon", json!([h])).unwrap();
        assert!(b.is_null(), "block {h} is inside the DKG window: no beacon");
    }
    let h = c.height(0).max(window + 2);
    c.wait_until("beacons after the window", 30, || {
        (0..5).all(|i| c.height(i) >= h + 2)
    });
    verify_beacon(&c, 0, h, &k0.group_key);
    for i in 0..5 {
        assert_eq!(
            c.status(i)["oracle"]["holds_share"],
            true,
            "node {i} derived its share"
        );
        assert!(c.status(i)["oracle"]["dkg_deals_sent"].as_u64().unwrap() >= 1);
    }
    let sessions = c.node(0).call("btb_oracleDkg", json!([])).unwrap();
    assert_eq!(sessions[0]["status"], "done");
    assert_eq!(sessions[0]["deals"].as_array().unwrap().len(), 5);
    assert!(sessions[0]["disqualified"].as_array().unwrap().is_empty());

    // Validator 4 leaves: it unbonds, and epoch 1's set (and beacon committee) is {0..3}.
    // This must land before the tail window locks the next set (height 37).
    assert!(c.height(0) < epoch_len - window, "too late for the test");
    // (its consensus key doubles as the operator key, so the DKG deal already used a nonce)
    let acct = c
        .node(0)
        .call(
            "btb_getAccount",
            json!([hex::encode(&account_address(
                &key(14).verifying_key().to_bytes()
            ))]),
        )
        .unwrap();
    let mut leaver = Wallet {
        key: key(14),
        nonce: acct["nonce"].as_u64().unwrap(),
    };
    let tx = leaver.sign(TxKind::Unstake {
        amount: 100 * UNITS_PER_BTB,
    });
    c.run(0, &tx);

    // Walk through epochs 1, 2 and 3 recording the key each epoch uses.
    let mut seen: std::collections::BTreeMap<u64, KeyView> = std::collections::BTreeMap::new();
    seen.insert(0, k0.clone());
    c.wait_until("epoch 3", 150, || {
        for i in 0..4 {
            if let Some(k) = key_view(&c, i) {
                seen.entry(k.epoch).or_insert(k);
            }
        }
        (0..4).all(|i| c.status(i)["epoch"].as_u64().unwrap() >= 3)
    });
    c.wait_until("the key of epoch 3 on every node", 30, || {
        for i in 0..4 {
            if let Some(k) = key_view(&c, i) {
                seen.entry(k.epoch).or_insert(k);
            }
        }
        seen.contains_key(&3)
    });
    for e in 0..=3u64 {
        let k = seen
            .get(&e)
            .unwrap_or_else(|| panic!("no key seen for epoch {e}"));
        assert_eq!(
            k.group_key, k0.group_key,
            "resharing keeps one group key (epoch {e})"
        );
        assert_eq!(k.origin, e, "every epoch's key was generated on chain");
    }
    assert_eq!(seen[&0].holders.len(), 5);
    for e in 1..=3 {
        assert_eq!(seen[&e].holders.len(), 4, "epoch {e}: the leaver is out");
    }
    let leaver_key = hex::encode(&key(14).verifying_key().to_bytes());
    for e in 1..=3 {
        assert!(
            !seen[&e].holders.contains(&leaver_key),
            "epoch {e} still lists the removed validator"
        );
    }
    assert!(seen[&0].holders.contains(&leaver_key));

    // Beacons of every epoch verify under the one group key, and the nodes agree on blocks.
    c.wait_until("blocks of epoch 3", 60, || {
        (0..4).all(|i| c.height(i) >= 3 * epoch_len + 3)
    });
    let h = (0..4).map(|i| c.height(i)).min().unwrap();
    assert!(h > 3 * epoch_len);
    for height in [
        window + 3,
        epoch_len - 1,
        epoch_len + 1,
        2 * epoch_len + 5,
        3 * epoch_len + 1,
        h,
    ] {
        let b = verify_beacon(&c, 0, height, &k0.group_key);
        for i in 1..4 {
            assert_eq!(
                verify_beacon(&c, i, height, &k0.group_key),
                b,
                "beacon {height} differs between nodes"
            );
        }
    }
    let blocks: Vec<Value> = (0..4)
        .map(|i| c.node(i).call("btb_getBlock", json!([h])).unwrap())
        .collect();
    for b in &blocks {
        assert!(!b.is_null(), "block {h} missing on a node");
        assert_eq!(b["hash"], blocks[0]["hash"], "block {h} differs");
    }
    for i in 0..4 {
        assert_eq!(c.status(i)["oracle"]["rejected_partials"], 0);
    }
    // The board of the last two generations is auditable over RPC.
    let sessions = c.node(0).call("btb_oracleDkg", json!([])).unwrap();
    let last = sessions.as_array().unwrap().last().unwrap().clone();
    assert!(last["status"] == "done" || last["status"] == "open");
    assert_eq!(
        last["from"].as_u64().unwrap(),
        last["target"].as_u64().unwrap() - 1
    );
}

#[test]
fn a_restarted_validator_keeps_its_share_and_the_next_resharing_still_works() {
    let _g = serial();
    let (epoch_len, window) = (40u64, 12u64);
    let mut c = Cluster::build(4, 4, honest_prices(), Some((epoch_len, window)));
    c.wait_connected();
    c.wait_until("the genesis key", 60, || {
        (0..4).all(|i| key_view(&c, i).is_some())
    });
    let k0 = key_view(&c, 0).unwrap();
    // restart node 1 after the first resharing
    c.wait_until("epoch 1", 90, || {
        (0..4).all(|i| c.status(i)["epoch"].as_u64().unwrap() >= 1)
    });
    c.kill(1);
    // 3 of 4 holders still sign (threshold 3); the chain keeps going
    c.wait_until("progress with node 1 down", 30, || {
        let h = c.height(0);
        c.live().iter().all(|i| c.height(*i) >= h) && c.height(0) > epoch_len + 2
    });
    c.start(1);
    c.wait_connected();
    c.wait_until("node 1 holds a share again", 30, || {
        c.status(1)["oracle"]["holds_share"] == true
    });
    c.wait_until("node 1 caught up", 60, || c.height(1) + 3 >= c.height(0));
    // the next epoch change reshares again, with node 1 dealing from its recovered share
    c.wait_until("epoch 3", 150, || {
        (0..4).all(|i| c.status(i)["epoch"].as_u64().unwrap() >= 3)
    });
    let k3 = key_view(&c, 0).unwrap();
    assert_eq!(k3.group_key, k0.group_key);
    assert_eq!(k3.origin, k3.epoch);
    // The restarted node dealt again from its recovered share (its counters restarted at 0), and
    // the last generation on the board has at least the threshold of dealers (an individual
    // deal can miss the short deal phase on a loaded machine).
    c.wait_until("the epoch 3 key generation on the board", 30, || {
        let s = c.node(1).call("btb_oracleDkg", json!([])).unwrap();
        s.as_array()
            .unwrap()
            .iter()
            .any(|x| x["target"] == 3 && x["status"] == "done")
    });
    assert!(
        c.status(1)["oracle"]["dkg_deals_sent"].as_u64().unwrap() >= 1,
        "node 1 did not deal after its restart: {}",
        c.status(1)["oracle"]
    );
    let sessions = c.node(1).call("btb_oracleDkg", json!([])).unwrap();
    let last = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["target"] == 3)
        .unwrap();
    assert!(last["deals"].as_array().unwrap().len() >= 3, "{last}");
}
