//! End to end with real watchers against mock chains (no external network): a 4-validator
//! cluster, each validator watching its own mock Esplora and Solana JSON-RPC endpoints.
//! Deposits are minted only at the configured depth and only when two thirds of the stake
//! saw the identical claim; withdrawals are built from chain-state UTXOs, signed with FROST and
//! broadcast to every validator's endpoint, where the raw transactions are parsed with the
//! `bitcoin` crate / `ed25519-dalek` and their signatures verified.

mod watch_common;

use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash as _;
use bitcoin::secp256k1::{schnorr, Message, Secp256k1, XOnlyPublicKey};
use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use bitcoin::{Amount, ScriptBuf, Transaction, TxOut};
use btb_chain::vault::{Execution, Observer};
use btb_chain::vault_plan::solana_transfer_message;
use btb_chain::{
    account_address, Address, CrossOp, Hash, OracleGenesis, SigningKey, Transaction as BtbTx,
    TxBody, TxKind, TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_node::config::{GenesisFile, NodeParams, Timing};
use btb_node::watch::{Backend, BtcConfig, SolConfig, Watchers};
use btb_node::{hex, net, open_chain, Node};
use btb_oracle::frost::{addr, Scheme};
use btb_runtime::MemKv;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use watch_common::{
    blockhash, deposit_tx, esplora, sol_deposit, solana, BtcChain, Server, SolChain,
};

const CHAIN_ID: u64 = 7;
const EPOCH_LEN: u64 = 70;
const WINDOW: u64 = 24;

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
    fn sign(&mut self, kind: TxKind) -> BtbTx {
        let tx = BtbTx::sign(
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
}

struct Cluster {
    genesis: GenesisFile,
    keys: Vec<SigningKey>,
    ports: Vec<u16>,
    nodes: Vec<Option<Node>>,
    wallet: Wallet,
    watchers: Vec<Arc<Watchers>>,
    btc: Vec<Arc<Mutex<BtcChain>>>,
    sol: Vec<Arc<Mutex<SolChain>>>,
    _servers: Vec<Server>,
    dir: tempfile::TempDir,
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
        let wallet = Wallet {
            key: key(99),
            nonce: 0,
        };
        let per = 1_000 * UNITS_PER_BTB;
        let mut allocations: Vec<(Address, u64)> = keys
            .iter()
            .map(|k| (account_address(&k.verifying_key().to_bytes()), per))
            .collect();
        allocations.push((wallet.addr(), TOTAL_SUPPLY - per * total as u64));
        let genesis = GenesisFile {
            chain_id: CHAIN_ID,
            name: "btb-cross-test".into(),
            timestamp: 0,
            gas_price: 1,
            epoch_len: EPOCH_LEN,
            min_stake: 100 * UNITS_PER_BTB,
            storage: Default::default(),
            oracle: Some(OracleGenesis {
                // no trusted dealer: the validators generate every key on chain
                group_key: Vec::new(),
                holders: Vec::new(),
                dkg_window: WINDOW,
                rules: Default::default(),
                pairs: vec![],
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
            nodes: (0..total).map(|_| None).collect(),
            keys,
            wallet,
            watchers: Vec::new(),
            btc: Vec::new(),
            sol: Vec::new(),
            _servers: Vec::new(),
            dir: tempfile::tempdir().unwrap(),
        };
        for _ in 0..total {
            // every validator has its own endpoints: they are the independent observers
            let b = Arc::new(Mutex::new(BtcChain::default()));
            b.lock().unwrap().mine(10, 1);
            let s = Arc::new(Mutex::new(SolChain {
                slot: 100,
                ..Default::default()
            }));
            let (es, so) = (esplora(b.clone(), String::new()), solana(s.clone()));
            let mut bc = BtcConfig::new(Backend::Esplora {
                url: es.url.clone(),
            });
            bc.hrp = "bcrt".into();
            bc.confirmations = 3;
            bc.poll = Duration::from_millis(100);
            let mut sc = SolConfig::new(so.url.clone());
            sc.poll = Duration::from_millis(100);
            c.watchers
                .push(Watchers::start(Some(bc), Some(sc)).unwrap());
            c.btc.push(b);
            c.sol.push(s);
            c._servers.push(es);
            c._servers.push(so);
        }
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
        p.data_dir = Some(self.dir.path().join(format!("node{i}")));
        p.timing = Timing {
            tick_ms: 10,
            round_timeout_ms: 600,
            idle_block_ms: 100,
        };
        p.dkg_dir = Some(self.dir.path().join(format!("dkg{i}")));
        let obs: Arc<dyn Observer> = self.watchers[i].clone();
        p.observer = Some(obs);
        p.cross.attempt_timeout = Duration::from_millis(1_500);
        p.cross.pool_target = 8;
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
            std::thread::sleep(Duration::from_millis(25));
        }
        let st: Vec<String> = (0..self.nodes.len())
            .map(|i| {
                let s = self.node(i).call("btb_status", json!([])).unwrap();
                format!(
                    "node{i}: h={} peers={} cross={}",
                    s["height"], s["peers"], s["cross"]
                )
            })
            .collect();
        panic!(
            "timed out after {secs}s waiting for {what}\n{}",
            st.join("\n")
        );
    }

    fn run(&mut self, via: usize, kind: TxKind) -> Vec<u8> {
        let tx = self.wallet.sign(kind);
        let r = self
            .node(via)
            .call("btb_sendTransaction", json!([hex::encode(&tx.encode())]))
            .unwrap();
        let id: Hash = hex::decode32(r["id"].as_str().unwrap()).unwrap();
        let mut out = Value::Null;
        self.wait_until("a receipt", 40, || {
            let r = self
                .node(via)
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
        hex::decode(out["output"].as_str().unwrap()).unwrap()
    }

    fn keys_json(&self, node: usize) -> Vec<Value> {
        self.node(node)
            .call("btb_crossKeys", json!([]))
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    fn output_key(&self, node: usize, scheme: Scheme) -> Option<[u8; 32]> {
        let ks = self.keys_json(node);
        let k = ks.iter().find(|k| k["scheme"] == scheme.name())?;
        let o = k["output_key"].as_str()?;
        hex::decode(o).ok()?.try_into().ok()
    }

    fn supply(&self, node: usize, scheme: Scheme) -> (u128, u128, u128) {
        let v = self.node(node).call("btb_vault", json!([])).unwrap();
        let a = v["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["scheme"] == scheme.name())
            .unwrap();
        let f = |k: &str| a[k].as_str().unwrap().parse::<u128>().unwrap();
        (f("credited"), f("burned"), f("supply"))
    }
}

fn withdraw(c: &mut Cluster, asset: Scheme, amount: u64, dest: Vec<u8>) -> u64 {
    let out = c.run(
        1,
        TxKind::Cross(CrossOp::Withdraw {
            asset,
            amount,
            destination: dest,
        }),
    );
    u64::from_le_bytes(out.try_into().unwrap())
}

#[test]
fn deposits_mint_and_withdrawals_are_built_signed_and_broadcast_on_mock_chains() {
    let mut c = Cluster::new(4);
    c.wait_until("peers", 20, || {
        (0..4).all(|i| c.node(i).call("btb_status", json!([])).unwrap()["peers"] == 3)
    });
    c.wait_until("both cross-chain keys", 60, || {
        (0..4).all(|i| {
            c.output_key(i, Scheme::Taproot).is_some() && c.output_key(i, Scheme::Ed25519).is_some()
        })
    });
    let tr = c.output_key(0, Scheme::Taproot).unwrap();
    let ed = c.output_key(0, Scheme::Ed25519).unwrap();
    let vscript = addr::taproot_script_pubkey(&tr);
    for b in &c.btc {
        b.lock().unwrap().address = addr::taproot_address("bcrt", &tr);
    }
    let me = c.wallet.addr();

    // ---- Bitcoin deposit 1: only counts at depth 3; validator 3's endpoint lies about it
    let dep1 = deposit_tx(&vscript, 100_000, Some(&me), 1);
    let dep1_id = dep1.compute_txid();
    for i in 0..4 {
        let mut ch = c.btc[i].lock().unwrap();
        if i == 3 {
            // a different (huge) deposit in place of the real one
            ch.confirm(&deposit_tx(&vscript, 999_999_999, Some(&me), 99), 9);
        } else {
            ch.confirm(&dep1, 9); // tip 9: depth 1
        }
    }
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(
        c.supply(0, Scheme::Taproot),
        (0, 0, 0),
        "depth 1 of 3 must not mint"
    );
    for ch in &c.btc {
        ch.lock().unwrap().mine(2, 1); // depth 3
    }
    c.wait_until("deposit 1 minted on every node", 60, || {
        (0..4).all(|i| c.supply(i, Scheme::Taproot).0 == 100_000)
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        c.supply(2, Scheme::Taproot),
        (100_000, 0, 100_000),
        "the lie did not mint"
    );

    // ---- deposit 2: seen by two validators only (below two thirds): nothing is minted
    let dep2 = deposit_tx(&vscript, 20_000, Some(&me), 2);
    for ch in &c.btc[..2] {
        let mut ch = ch.lock().unwrap();
        ch.confirm(&dep2, 11);
        ch.mine(2, 1);
    }
    std::thread::sleep(Duration::from_millis(2_000));
    assert_eq!(
        c.supply(0, Scheme::Taproot).0,
        100_000,
        "two of four is not enough"
    );
    {
        let mut ch = c.btc[2].lock().unwrap();
        ch.confirm(&dep2, 11);
        ch.mine(2, 1);
    }
    c.wait_until("deposit 2 minted with three of four", 60, || {
        (0..4).all(|i| c.supply(i, Scheme::Taproot).0 == 120_000)
    });

    // ---- withdrawal 1: spends deposit 1, change back to the vault
    let dest = {
        let mut d = vec![0x51, 0x20];
        d.extend_from_slice(&[0xAB; 32]);
        d
    };
    let w1 = withdraw(&mut c, Scheme::Taproot, 40_000, dest.clone());
    c.wait_until("withdrawal 1 broadcast to every endpoint", 90, || {
        c.btc
            .iter()
            .all(|b| b.lock().unwrap().broadcasts.len() == 1)
    });
    let raw1 = c.btc[0].lock().unwrap().broadcasts[0].clone();
    for b in &c.btc {
        assert_eq!(
            b.lock().unwrap().broadcasts[0],
            raw1,
            "every validator built the same tx"
        );
    }
    let tx1: Transaction = deserialize(&raw1).unwrap();
    assert_eq!(tx1.input.len(), 1);
    assert_eq!(tx1.input[0].previous_output.txid, dep1_id);
    assert_eq!(tx1.output[0].script_pubkey.as_bytes(), dest.as_slice());
    let fee1 = 40_000 - tx1.output[0].value.to_sat();
    assert!((900..3_000).contains(&fee1), "fee {fee1}");
    assert_eq!(tx1.output[1].value.to_sat(), 60_000);
    assert_eq!(tx1.output[1].script_pubkey.as_bytes(), vscript.as_slice());
    let secp = Secp256k1::verification_only();
    let verify_btc = |tx: &Transaction, prevouts: &[TxOut]| {
        for i in 0..tx.input.len() {
            let sh = SighashCache::new(tx)
                .taproot_key_spend_signature_hash(
                    i,
                    &Prevouts::All(prevouts),
                    TapSighashType::Default,
                )
                .unwrap();
            let w = &tx.input[i].witness;
            assert_eq!(w.len(), 1);
            let sig = schnorr::Signature::from_slice(w.nth(0).unwrap()).expect("64-byte signature");
            secp.verify_schnorr(
                &sig,
                &Message::from_digest(sh.to_byte_array()),
                &XOnlyPublicKey::from_slice(&tr).unwrap(),
            )
            .unwrap_or_else(|e| panic!("input {i}: {e}"));
        }
    };
    let prev = |sats: u64| TxOut {
        value: Amount::from_sat(sats),
        script_pubkey: ScriptBuf::from_bytes(vscript.clone()),
    };
    verify_btc(&tx1, &[prev(100_000)]);
    let tx1_id = tx1.compute_txid();
    // confirm it on every endpoint: the watchers report Confirmed after three confirmations
    for b in &c.btc {
        let mut b = b.lock().unwrap();
        let tip = b.tip() + 1;
        b.mine(1, 1);
        b.conf.insert(tx1_id.to_string(), tip);
        b.mine(2, 1);
    }
    c.wait_until("withdrawal 1 confirmed", 30, || {
        c.watchers.iter().all(|w| {
            w.execution(&btb_chain::vault::Withdrawal {
                id: w1,
                asset: Scheme::Taproot,
                owner: me,
                amount: 0,
                destination: vec![],
                height: 0,
                status: 2,
                digest: [0; 32],
                requests: vec![],
                plan: vec![],
            }) == Execution::Confirmed
        })
    });

    // ---- withdrawal 2: must use the change of withdrawal 1 and deposit 2, never deposit 1
    let _w2 = withdraw(&mut c, Scheme::Taproot, 70_000, dest.clone());
    c.wait_until("withdrawal 2 broadcast to every endpoint", 90, || {
        c.btc
            .iter()
            .all(|b| b.lock().unwrap().broadcasts.len() == 2)
    });
    let tx2: Transaction = deserialize(&c.btc[0].lock().unwrap().broadcasts[1].clone()).unwrap();
    let mut spent: Vec<(String, u32)> = tx2
        .input
        .iter()
        .map(|i| (i.previous_output.txid.to_string(), i.previous_output.vout))
        .collect();
    spent.sort();
    let mut want = vec![
        (tx1_id.to_string(), 1),
        (dep2.compute_txid().to_string(), 0),
    ];
    want.sort();
    assert_eq!(spent, want, "each UTXO is spent once; deposit 1 is gone");
    // inputs are ordered largest first: the change (60_000), then deposit 2 (20_000)
    verify_btc(&tx2, &[prev(60_000), prev(20_000)]);
    assert_eq!(c.supply(3, Scheme::Taproot), (120_000, 110_000, 10_000));

    // ---- Solana: finalized deposit with a memo, then a withdrawal
    let sig = [1u8; 64];
    let dep = sol_deposit(&sig, 90, &ed, 2_000_000, Some(&hex::encode(&me)));
    for s in &c.sol {
        s.lock().unwrap().txs.push(dep.clone());
    }
    c.wait_until("SOL deposit minted", 60, || {
        (0..4).all(|i| c.supply(i, Scheme::Ed25519).0 == 2_000_000)
    });
    let sdest = vec![0xCDu8; 32];
    let sw = withdraw(&mut c, Scheme::Ed25519, 1_000_000, sdest.clone());
    c.wait_until("SOL withdrawal sent to every endpoint", 90, || {
        c.sol.iter().all(|s| s.lock().unwrap().sent.len() == 1)
    });
    let wire = c.sol[0].lock().unwrap().sent[0].clone();
    for s in &c.sol {
        assert_eq!(s.lock().unwrap().sent[0], wire);
    }
    assert_eq!(wire[0], 1);
    let expect = solana_transfer_message(
        &ed,
        &sdest.clone().try_into().unwrap(),
        1_000_000,
        &blockhash(96),
    );
    assert_eq!(
        &wire[65..],
        expect.as_slice(),
        "agreed anchor blockhash and transfer"
    );
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&ed).unwrap();
    vk.verify_strict(
        &wire[65..],
        &ed25519_dalek::Signature::from_slice(&wire[1..65]).unwrap(),
    )
    .expect("valid ed25519 signature over the Solana message");
    let sig58 = addr::base58_encode(&wire[1..65]);
    for s in &c.sol {
        s.lock()
            .unwrap()
            .status
            .insert(sig58.clone(), "finalized".into());
    }
    c.wait_until("SOL withdrawal finalized", 30, || {
        c.watchers
            .iter()
            .all(|w| w.solana.as_ref().unwrap().execution(sw) == Execution::Confirmed)
    });
    assert_eq!(
        c.supply(0, Scheme::Ed25519),
        (2_000_000, 1_000_000, 1_000_000)
    );
}
