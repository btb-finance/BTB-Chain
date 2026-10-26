//! Signed reports for other chains on a real cluster (`docs/ORACLES.md`, section 4): a contract
//! queues a report, the beacon committee threshold-signs it, a relayer fetches the signature over
//! RPC and verifies it against the committee's group key with nothing but that 48-byte key.

#[path = "../../btb-tests/tests/common/mod.rs"]
mod common;

use btb_chain::{
    account_address, contract_address, Address, Hash, OracleGenesis, SigningKey, Transaction,
    TxBody, TxKind, TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_codec::Encode;
use btb_node::config::{GenesisFile, NodeParams, Timing};
use btb_node::{hex, net, open_chain, Node};
use btb_oracle::report::{report_message, verify, SignedReport};
use btb_oracle::{trusted_deal, GroupKey, KeyShare, Params};
use btb_runtime::MemKv;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const CHAIN_ID: u64 = 7;

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
    shares: Vec<KeyShare>,
    group: GroupKey,
    ports: Vec<u16>,
    nodes: Vec<Option<Node>>,
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
        let (group, shares) = trusted_deal(Params::standard(total), &[42; 32]).unwrap();
        let genesis = GenesisFile {
            chain_id: CHAIN_ID,
            name: "btb-report-test".into(),
            timestamp: 0,
            gas_price: 1,
            epoch_len: 10_000,
            min_stake: 100 * UNITS_PER_BTB,
            storage: Default::default(),
            oracle: Some(OracleGenesis {
                group_key: group.encode(),
                holders: keys.iter().map(|k| k.verifying_key().to_bytes()).collect(),
                dkg_window: 20,
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
            shares,
            group,
            wallet,
            _dir: tempfile::tempdir().unwrap(),
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
        p.data_dir = Some(self._dir.path().join(format!("node{i}")));
        p.timing = Timing {
            tick_ms: 10,
            round_timeout_ms: 600,
            idle_block_ms: 100,
        };
        p.oracle_share = self.shares.get(i).cloned();
        p.dkg_dir = Some(self._dir.path().join(format!("dkg{i}")));
        let state = open_chain(MemKv::new(), &self.genesis).unwrap();
        self.nodes[i] = Some(Node::start(p, state).unwrap());
    }

    fn node(&self, i: usize) -> &Node {
        self.nodes[i].as_ref().expect("node is running")
    }

    fn live(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|i| self.nodes[*i].is_some())
            .collect()
    }

    fn wait_until(&self, what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out after {secs}s waiting for {what}");
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

    fn report(&self, node: usize, id: u64) -> Value {
        self.node(node)
            .call("btb_oracleReport", json!([id]))
            .unwrap()
    }
}

#[test]
fn the_committee_signs_reports_and_a_relayer_verifies_them_with_the_group_key() {
    let mut c = Cluster::new(4);
    c.wait_until("peers", 20, || {
        c.live()
            .iter()
            .all(|i| c.node(*i).call("btb_status", json!([])).unwrap()["peers"] == 3)
    });
    let deploy = c.wallet.sign(TxKind::Deploy {
        code: elf().to_vec(),
        manifest: Default::default(),
    });
    let contract = contract_address(&c.wallet.addr(), deploy.body.nonce);
    {
        let r = c
            .node(0)
            .call(
                "btb_sendTransaction",
                json!([hex::encode(&deploy.encode())]),
            )
            .unwrap();
        let id = hex::decode32(r["id"].as_str().unwrap()).unwrap();
        c.wait_until("deploy", 40, || {
            !c.node(0)
                .call("btb_getReceipt", json!([hex::encode(&id)]))
                .unwrap()
                .is_null()
        });
    }
    // a second contract, to show a report cannot be passed off as another contract's
    let other = {
        let tx = c.wallet.sign(TxKind::Deploy {
            code: elf().to_vec(),
            manifest: btb_chain::Manifest {
                name: "other".into(),
                ..Default::default()
            },
        });
        let addr = contract_address(&c.wallet.addr(), tx.body.nonce);
        c.node(1)
            .call("btb_sendTransaction", json!([hex::encode(&tx.encode())]))
            .unwrap();
        addr
    };
    for target in [contract, other] {
        c.run(
            2,
            TxKind::Transfer {
                to: target,
                amount: UNITS_PER_BTB,
            },
        );
    }
    let publish = |c: &mut Cluster, via: usize, to: Address, data: &[u8]| -> u64 {
        let kind = TxKind::Call {
            contract: to,
            kind: btb_vm::FunctionKind::Mutation,
            function: "publish".into(),
            args: data.to_vec().encode(),
            gas_limit: 2_000_000,
            max_fee: u64::MAX,
        };
        let out = c.run(via, kind);
        u64::from_le_bytes(out.try_into().unwrap())
    };

    let price_report = b"ETH/USD=3412.50 at block 77".to_vec();
    let id0 = publish(&mut c, 3, contract, &price_report);
    let id1 = publish(&mut c, 0, other, &price_report);
    assert_eq!((id0, id1), (0, 1), "ids count up chain wide");

    // every validator ends up with the same signature
    let gk = c.group.public_key.to_bytes();
    c.wait_until("both reports signed on every node", 30, || {
        c.live()
            .iter()
            .all(|i| (0..2).all(|id| !c.report(*i, id)["signature"].is_null()))
    });
    for id in [id0, id1] {
        let first = c.report(0, id);
        for i in c.live() {
            assert_eq!(c.report(i, id)["signature"], first["signature"], "node {i}");
        }
        assert_eq!(first["group_key"], hex::encode(&gk).as_str());
        assert_eq!(first["data"], hex::encode(&price_report).as_str());
    }

    // ---- the relayer: only the report, the signature and the 48-byte key
    let r0 = c.report(1, id0);
    let sig: [u8; 96] = hex::decode(r0["signature"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let signed = SignedReport {
        chain_id: CHAIN_ID,
        contract,
        id: id0,
        data: price_report.clone(),
        signature: sig,
    };
    assert!(signed.verify(&gk));
    // the message the RPC shows is what was signed
    assert_eq!(
        r0["message"],
        hex::encode(&report_message(CHAIN_ID, &contract, id0, &price_report)).as_str()
    );
    // it fails for any other contract, id, chain, data or key
    assert!(
        !verify(&gk, CHAIN_ID, &other, id0, &price_report, &sig),
        "other contract"
    );
    assert!(
        !verify(&gk, CHAIN_ID, &contract, id1, &price_report, &sig),
        "other id"
    );
    assert!(
        !verify(&gk, CHAIN_ID + 1, &contract, id0, &price_report, &sig),
        "other chain"
    );
    assert!(
        !verify(&gk, CHAIN_ID, &contract, id0, b"ETH/USD=9999", &sig),
        "other data"
    );
    let (other_group, _) = trusted_deal(Params::standard(4), &[7; 32]).unwrap();
    assert!(
        !verify(
            &other_group.public_key.to_bytes(),
            CHAIN_ID,
            &contract,
            id0,
            &price_report,
            &sig
        ),
        "other committee"
    );
    // and it is not a beacon value for any height
    let beacon_msg = btb_oracle::beacon_message(CHAIN_ID, 0, id0);
    assert_ne!(
        beacon_msg,
        report_message(CHAIN_ID, &contract, id0, &price_report)
    );
    // unknown ids
    assert!(c.report(0, 99).is_null());

    // ---- a validator going down does not stop reports (3 of 4 shares are enough)
    c.nodes[3].take().unwrap().shutdown();
    let id2 = publish(&mut c, 0, contract, b"after one validator left");
    c.wait_until("a report signed by the remaining three", 30, || {
        (0..3).all(|i| !c.report(i, id2)["signature"].is_null())
    });
    let r2 = c.report(2, id2);
    let sig2: [u8; 96] = hex::decode(r2["signature"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    assert!(verify(
        &gk,
        CHAIN_ID,
        &contract,
        id2,
        b"after one validator left",
        &sig2
    ));
}
