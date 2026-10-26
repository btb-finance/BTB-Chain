//! Cross-chain threshold keys on a real 4-validator cluster (`docs/CROSS_CHAIN.md`): the
//! validators generate a Taproot and an Ed25519 key on chain with no trusted dealer, credit
//! wrapped tokens against deposits that two thirds of the stake observed (mock observer), sign a
//! withdrawal transaction with FROST over the oracle side channel, and keep signing for the same
//! external address after the keys are reshared at the epoch change. The signatures are checked
//! with a BIP-340 verifier (`k256`) and `ed25519-dalek`, which know nothing about BTB.

use btb_chain::vault::{DepositClaim, MockObserver, Observer};
use btb_chain::{
    account_address, Address, CrossOp, Hash, OracleGenesis, SigningKey, Transaction, TxBody,
    TxKind, TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_node::config::{GenesisFile, NodeParams, Timing};
use btb_node::{hex, net, open_chain, Node};
use btb_oracle::frost::{addr, Scheme};
use btb_runtime::MemKv;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    fn sign(&mut self, kind: TxKind) -> Transaction {
        let tx = Transaction::sign(
            TxBody {
                relayer: None,
                session: None,
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
    observer: Arc<MockObserver>,
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
            observer: Arc::new(MockObserver::new()),
            dir: tempfile::tempdir().unwrap(),
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
        p.data_dir = Some(self.dir.path().join(format!("node{i}")));
        p.timing = Timing {
            tick_ms: 10,
            round_timeout_ms: 600,
            idle_block_ms: 100,
        };
        p.dkg_dir = Some(self.dir.path().join(format!("dkg{i}")));
        let obs: Arc<dyn Observer> = self.observer.clone();
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

    fn height(&self, i: usize) -> u64 {
        self.node(i).call("btb_status", json!([])).unwrap()["height"]
            .as_u64()
            .unwrap()
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

    fn signature_of(&self, node: usize, request: u64) -> Option<Vec<u8>> {
        let r = self
            .node(node)
            .call("btb_crossRequest", json!([request]))
            .unwrap();
        hex::decode(r["signature"].as_str()?).ok()
    }
}

fn bip340_ok(output_key: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(vk) = k256::schnorr::VerifyingKey::from_bytes(output_key) else {
        return false;
    };
    let Ok(sig) = k256::schnorr::Signature::try_from(sig) else {
        return false;
    };
    vk.verify_raw(msg, &sig).is_ok()
}

fn ed25519_ok(key: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) else {
        return false;
    };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(sig) else {
        return false;
    };
    vk.verify_strict(msg, &sig).is_ok()
}

fn verify_external(scheme: Scheme, key: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    match scheme {
        Scheme::Taproot => bip340_ok(key, msg, sig),
        Scheme::Ed25519 => ed25519_ok(key, msg, sig),
    }
}

/// Deposit `amount`, wait for the quorum credit, withdraw `burn` of it and return the withdrawal
/// id and the sighash that was signed.
fn deposit_and_withdraw(
    c: &mut Cluster,
    scheme: Scheme,
    txid: u8,
    amount: u64,
    burn: u64,
    already: (u128, u128),
) -> (u64, [u8; 32]) {
    let me = c.wallet.addr();
    c.observer.deposit(DepositClaim {
        asset: scheme,
        txid: [txid; 32],
        index: 0,
        amount,
        recipient: me,
        height: 100,
        block_hash: [txid; 32],
    });
    let (cr0, bu0) = already;
    c.wait_until(
        "the deposit to be credited by two thirds of the stake",
        60,
        || (0..4).all(|i| c.supply(i, scheme).0 == cr0 + u128::from(amount)),
    );
    let dest = match scheme {
        Scheme::Taproot => {
            let mut d = vec![0x51, 0x20];
            d.extend_from_slice(&[0xAB; 32]);
            d
        }
        Scheme::Ed25519 => vec![0xCD; 32],
    };
    let out = c.run(
        1,
        TxKind::Cross(CrossOp::Withdraw {
            asset: scheme,
            amount: burn,
            destination: dest.clone(),
        }),
    );
    let wid = u64::from_le_bytes(out.try_into().unwrap());
    // the burn is immediate: supply == credited - burned on every node
    c.wait_until("the burn", 20, || {
        (0..4).all(|i| {
            c.supply(i, scheme)
                == (
                    cr0 + u128::from(amount),
                    bu0 + u128::from(burn),
                    cr0 + u128::from(amount) - bu0 - u128::from(burn),
                )
        })
    });
    let sighash = MockObserver::sighash_of(&btb_chain::vault::Withdrawal {
        id: wid,
        asset: scheme,
        owner: me,
        amount: burn,
        destination: dest,
        height: 0,
        status: 0,
        digest: [0; 32],
        requests: vec![],
        plan: vec![],
    });
    (wid, sighash)
}

#[test]
fn four_validators_generate_taproot_and_ed25519_keys_and_sign_a_withdrawal_for_each() {
    let mut c = Cluster::new(4);
    c.wait_until("peers", 20, || {
        (0..4).all(|i| c.node(i).call("btb_status", json!([])).unwrap()["peers"] == 3)
    });
    // 1. keys without a trusted dealer, the same on every node, with real addresses
    c.wait_until("both cross-chain keys on every node", 60, || {
        (0..4).all(|i| {
            c.output_key(i, Scheme::Taproot).is_some() && c.output_key(i, Scheme::Ed25519).is_some()
        })
    });
    let tr = c.output_key(0, Scheme::Taproot).unwrap();
    let ed = c.output_key(0, Scheme::Ed25519).unwrap();
    for i in 1..4 {
        assert_eq!(c.output_key(i, Scheme::Taproot), Some(tr));
        assert_eq!(c.output_key(i, Scheme::Ed25519), Some(ed));
    }
    let keys = c.keys_json(0);
    let tr_json = keys.iter().find(|k| k["scheme"] == "taproot").unwrap();
    assert_eq!(tr_json["threshold"], 3);
    let a = tr_json["addresses"]["bitcoin"].as_str().unwrap();
    assert!(a.starts_with("bc1p"), "{a}");
    assert_eq!(addr::parse_taproot_address("bc", a), Some(tr));
    let sol = keys.iter().find(|k| k["scheme"] == "ed25519").unwrap()["addresses"]["solana"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(addr::parse_solana_address(&sol), Some(ed));

    // 2. deposits observed by the validators are credited once two thirds voted; a withdrawal
    //    burns, the validators agree on the transaction, FROST signs its sighash
    let mut signed = Vec::new();
    for (scheme, vkey, txid) in [(Scheme::Taproot, tr, 1u8), (Scheme::Ed25519, ed, 2u8)] {
        let (wid, sighash) = deposit_and_withdraw(&mut c, scheme, txid, 50_000, 20_000, (0, 0));
        c.wait_until("the withdrawal to be signed on chain", 90, || {
            let v = c.node(2).call("btb_vault", json!([])).unwrap();
            v["withdrawals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"] == wid && w["status"] == "signed")
        });
        let v = c.node(0).call("btb_vault", json!([])).unwrap();
        let w = v["withdrawals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == wid)
            .unwrap()
            .clone();
        let rid = w["requests"][0].as_u64().unwrap();
        // every node serves the same signature over RPC
        let sig = c.signature_of(0, rid).expect("signature on chain");
        for i in 1..4 {
            assert_eq!(c.signature_of(i, rid).as_deref(), Some(sig.as_slice()));
        }
        // a standard verifier that knows nothing about BTB accepts it under the external key
        assert!(
            verify_external(scheme, &vkey, &sighash, &sig),
            "{scheme:?} signature"
        );
        assert!(!verify_external(scheme, &vkey, &[0u8; 32], &sig));
        signed.push((scheme, vkey, sig));
    }
    // supply of each wrapped token == observed deposits - withdrawals
    assert_eq!(c.supply(3, Scheme::Taproot), (50_000, 20_000, 30_000));
    assert_eq!(c.supply(3, Scheme::Ed25519), (50_000, 20_000, 30_000));
    // BTB itself never moved because of any of it (the chain checks the invariant every block);
    // the signing was done with fresh nonces: each signer spent one per signature
    let st = c.node(0).call("btb_status", json!([])).unwrap();
    assert!(
        st["cross"]["signatures_made"].as_u64().unwrap() >= 1,
        "{st}"
    );

    // 3. epoch change: the keys are reshared, the addresses are not
    c.wait_until("epoch 1", 120, || {
        (0..4).all(|i| c.height(i) > EPOCH_LEN + 3)
    });
    c.wait_until("reshared keys on every node", 60, || {
        (0..4).all(|i| {
            c.keys_json(i)
                .iter()
                .all(|k| k["origin"].as_u64() == Some(1))
        })
    });
    assert_eq!(
        c.output_key(0, Scheme::Taproot),
        Some(tr),
        "same Bitcoin address"
    );
    assert_eq!(
        c.output_key(0, Scheme::Ed25519),
        Some(ed),
        "same Solana address"
    );
    // ... and the new shares sign for the old address
    for (scheme, vkey, txid, before) in [
        (Scheme::Taproot, tr, 3u8, (50_000u128, 20_000u128)),
        (Scheme::Ed25519, ed, 4u8, (50_000u128, 20_000u128)),
    ] {
        let (wid, sighash) = deposit_and_withdraw(&mut c, scheme, txid, 7_000, 3_000, before);
        c.wait_until("the second withdrawal to be signed", 90, || {
            let v = c.node(1).call("btb_vault", json!([])).unwrap();
            v["withdrawals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"] == wid && w["status"] == "signed")
        });
        let v = c.node(1).call("btb_vault", json!([])).unwrap();
        let rid = v["withdrawals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == wid)
            .unwrap()["requests"][0]
            .as_u64()
            .unwrap();
        let sig = c.signature_of(1, rid).unwrap();
        assert!(
            verify_external(scheme, &vkey, &sighash, &sig),
            "{scheme:?} after resharing"
        );
    }
    assert_eq!(c.supply(0, Scheme::Taproot), (57_000, 23_000, 34_000));
    let _ = signed;
}
