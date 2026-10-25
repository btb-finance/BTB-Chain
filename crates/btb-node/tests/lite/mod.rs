//! A small in-process validator cluster for tests that need real consensus, networking and
//! execution but not the fault-injection machinery of `cluster.rs`: N validators on localhost,
//! a funded faucet wallet, and helpers to send transactions and wait for results. Shared by
//! `btb-node/tests/followers.rs` and the `btb-host` tests (`#[path]` include).
#![allow(dead_code)]

use btb_chain::{
    account_address, contract_address, Address, Hash, SigningKey, Transaction, TxBody, TxKind,
    TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_codec::Encode;
use btb_node::config::{GenesisFile, NodeParams, Timing};
use btb_node::net::FollowerPolicy;
use btb_node::{hex, net, open_chain, Node};
use btb_runtime::MemKv;
use serde_json::{json, Value};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const CHAIN_ID: u64 = 7;

/// Clusters run one at a time so timing-sensitive waits are not starved by other clusters.
pub fn serial() -> MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub fn fast() -> Timing {
    Timing {
        tick_ms: 10,
        round_timeout_ms: 500,
        idle_block_ms: 80,
    }
}

pub struct Wallet {
    pub key: SigningKey,
    pub nonce: u64,
}

impl Wallet {
    pub fn new(seed: u8) -> Wallet {
        Wallet {
            key: key(seed),
            nonce: 0,
        }
    }
    pub fn addr(&self) -> Address {
        account_address(&self.key.verifying_key().to_bytes())
    }
    pub fn sign(&mut self, kind: TxKind) -> Transaction {
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
    pub fn transfer(&mut self, to: Address, amount: u64) -> Transaction {
        self.sign(TxKind::Transfer { to, amount })
    }
    pub fn call(&mut self, contract: Address, f: &str, args: &impl Encode) -> Transaction {
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

fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

pub struct Lite {
    pub genesis: GenesisFile,
    pub keys: Vec<SigningKey>,
    pub ports: Vec<u16>,
    pub nodes: Vec<Option<Node>>,
    pub faucet: Wallet,
}

impl Drop for Lite {
    fn drop(&mut self) {
        for n in self.nodes.iter_mut() {
            if let Some(n) = n.take() {
                n.shutdown();
            }
        }
    }
}

impl Lite {
    /// `n` validators, all started. `followers`: whether validators serve block sync to
    /// non-validators. Epochs are `epoch_len` blocks long.
    pub fn new(n: usize, epoch_len: u64, followers: Option<FollowerPolicy>) -> Lite {
        Self::with_running(n, n, epoch_len, followers)
    }

    /// `n` validator keys of which the first `running` run as real nodes (the rest are left
    /// for a test to play by hand).
    pub fn with_running(
        n: usize,
        running: usize,
        epoch_len: u64,
        followers: Option<FollowerPolicy>,
    ) -> Lite {
        let keys: Vec<SigningKey> = (0..n).map(|i| key(10 + i as u8)).collect();
        let faucet = Wallet::new(99);
        let per = 1_000 * UNITS_PER_BTB;
        let mut allocations: Vec<(Address, u64)> = keys
            .iter()
            .map(|k| (account_address(&k.verifying_key().to_bytes()), per))
            .collect();
        allocations.push((faucet.addr(), TOTAL_SUPPLY - per * n as u64));
        let genesis = GenesisFile {
            chain_id: CHAIN_ID,
            name: "btb-lite".into(),
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
        let mut c = Lite {
            genesis,
            ports: (0..n).map(|_| free_udp_port()).collect(),
            keys,
            nodes: (0..n).map(|_| None).collect(),
            faucet,
        };
        for i in 0..running {
            let mut p = NodeParams::new(c.genesis.clone(), c.keys[i].clone());
            p.listen = format!("/ip4/127.0.0.1/udp/{}/quic-v1", c.ports[i]);
            p.peers = (0..n).filter(|j| *j != i).map(|j| c.addr_of(j)).collect();
            p.timing = fast();
            p.rpc = Some("127.0.0.1:0".parse().unwrap());
            p.serve_followers = followers;
            let state = open_chain(MemKv::new(), &c.genesis).unwrap();
            c.nodes[i] = Some(Node::start(p, state).unwrap());
        }
        c.wait_until("validators to connect", 20, || {
            (0..running).all(|i| c.status(i)["peers"].as_u64().unwrap() as usize >= running - 1)
        });
        c
    }

    pub fn addr_of(&self, i: usize) -> String {
        let id = net::peer_id_of(&self.keys[i].verifying_key().to_bytes()).unwrap();
        format!("/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{id}", self.ports[i])
    }

    /// Dialable addresses of every validator.
    pub fn validator_addrs(&self) -> Vec<String> {
        (0..self.keys.len()).map(|i| self.addr_of(i)).collect()
    }

    pub fn node(&self, i: usize) -> &Node {
        self.nodes[i].as_ref().expect("running")
    }

    pub fn status(&self, i: usize) -> Value {
        self.node(i).call("btb_status", json!([])).unwrap()
    }

    pub fn height(&self, i: usize) -> u64 {
        self.status(i)["height"].as_u64().unwrap()
    }

    pub fn wait_until(&self, what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let st: Vec<String> = (0..self.nodes.len())
            .filter(|i| self.nodes[*i].is_some())
            .map(|i| format!("node{i}: {}", self.status(i)["height"]))
            .collect();
        panic!(
            "timed out after {secs}s waiting for {what}\n{}",
            st.join("\n")
        );
    }

    pub fn send(&self, tx: &Transaction) -> Hash {
        let r = self
            .node(0)
            .call("btb_sendTransaction", json!([hex::encode(&tx.encode())]))
            .unwrap_or_else(|e| panic!("submit failed: {e:?}"));
        hex::decode32(r["id"].as_str().unwrap()).unwrap()
    }

    /// Send `tx` and wait for its receipt; asserts success.
    pub fn run(&self, tx: &Transaction) -> Value {
        let id = self.send(tx);
        let mut out = Value::Null;
        self.wait_until("a receipt", 30, || {
            let r = self
                .node(0)
                .call("btb_getReceipt", json!([hex::encode(&id)]))
                .unwrap();
            if r.is_null() {
                false
            } else {
                out = r;
                true
            }
        });
        assert_eq!(out["status"], "success", "{out}");
        out
    }

    /// Deploy `elf` from the faucet account and fund the contract; returns its address.
    pub fn deploy(&mut self, elf: &[u8]) -> Address {
        let tx = self.faucet.sign(TxKind::Deploy {
            code: elf.to_vec(),
            manifest: Default::default(),
        });
        let addr = contract_address(&self.faucet.addr(), tx.body.nonce);
        self.run(&tx);
        let fund = self.faucet.transfer(addr, UNITS_PER_BTB);
        self.run(&fund);
        addr
    }
}
