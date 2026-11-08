//! Multi-operator network setup: each operator makes their own key on their own machine, a
//! coordinator assembles the genesis from the operators' *public* files, and each operator writes
//! their node config from the published genesis and peer list. No secret ever leaves the machine
//! it was made on (unlike `init-testnet`, which writes every key on one machine).
//!
//! Three steps, three commands (see docs/TESTNET.md):
//!
//! 1. `btb-node operator-key` (each operator): `validator.key` (mode 0600) and `operator.json`
//!    (public key, account address and peer address) to send to the coordinator.
//! 2. `btb-node assemble-genesis` (coordinator): `genesis.json` and `peers.json` from every
//!    `operator.json`, plus the SHA-256 of `genesis.json` to publish.
//! 3. `btb-node node-config` (each operator): checks the published hash and writes `node.json`
//!    with every other operator as a peer.

use crate::config::{save_key, GenesisFile};
use crate::hex;
use btb_chain::{account_address, Address, TOTAL_SUPPLY, UNITS_PER_BTB};
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// What an operator publishes about themselves. Contains no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operator {
    pub name: String,
    pub public_key: [u8; 32],
    /// `/ip4/<ip>/udp/<port>/quic-v1/p2p/<peer id>` (or `/ip6/...`).
    pub peer: String,
}

impl Operator {
    pub fn account(&self) -> Address {
        account_address(&self.public_key)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "public_key": hex::encode(&self.public_key),
            "account": hex::encode(&self.account()),
            "peer": self.peer,
        })
    }

    /// Parses and checks an operator file: the peer address must name the peer id of the key,
    /// so nobody can publish someone else's key with their own address or the other way round.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .ok_or(format!("operator file needs `{k}`"))
        };
        let name = s("name")?.to_string();
        if name.is_empty() || name.len() > 64 {
            return Err("operator name must be 1 to 64 characters".into());
        }
        let public_key = hex::decode32(s("public_key")?)?;
        let peer = s("peer")?.to_string();
        let id = crate::net::peer_id_of(&public_key).ok_or("public_key is not a valid key")?;
        if !peer.ends_with(&format!("/quic-v1/p2p/{id}")) {
            return Err(format!(
                "operator {name}: peer address must end in /quic-v1/p2p/{id} (the key's peer id)"
            ));
        }
        if let Some(a) = v.get("account").and_then(Value::as_str) {
            if hex::decode32(a)? != account_address(&public_key) {
                return Err(format!(
                    "operator {name}: account does not match public_key"
                ));
            }
        }
        Ok(Operator {
            name,
            public_key,
            peer,
        })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Operator::from_json(&v).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// The multiaddr peers dial: `/ip4|ip6/<ip>/udp/<port>/quic-v1/p2p/<peer id>`.
pub fn peer_address(ip: IpAddr, port: u16, public_key: &[u8; 32]) -> Result<String, String> {
    let id = crate::net::peer_id_of(public_key).ok_or("invalid key")?;
    let proto = if ip.is_ipv4() { "ip4" } else { "ip6" };
    Ok(format!("/{proto}/{ip}/udp/{port}/quic-v1/p2p/{id}"))
}

/// Step 1: make `validator.key` (never overwrites one) and `operator.json` in `dir`.
pub fn operator_key(dir: &Path, name: &str, ip: IpAddr, port: u16) -> Result<Operator, String> {
    use rand::RngCore;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let key_path = dir.join("validator.key");
    if key_path.exists() {
        return Err(format!(
            "{} already exists; refusing to overwrite a validator key",
            key_path.display()
        ));
    }
    let mut s = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut s);
    let key = SigningKey::from_bytes(&s);
    save_key(&key_path, &key)?;
    let public_key = key.verifying_key().to_bytes();
    let op = Operator {
        name: name.to_string(),
        public_key,
        peer: peer_address(ip, port, &public_key)?,
    };
    Operator::from_json(&op.to_json())?;
    write(
        &dir.join("operator.json"),
        &serde_json::to_string_pretty(&op.to_json()).expect("json"),
    )?;
    Ok(op)
}

/// Parameters of `assemble_genesis`. Amounts are whole BTB.
#[derive(Debug, Clone)]
pub struct GenesisPlan {
    pub chain_id: u64,
    pub name: String,
    /// Bonded stake of each validator (also the minimum stake).
    pub stake_btb: u64,
    /// Balance of each validator account, the stake included.
    pub validator_balance_btb: u64,
    /// The account that receives the rest of the supply (a faucet or treasury). It must not be a
    /// validator account.
    pub treasury: Address,
    pub epoch_len: u64,
}

/// The SHA-256 of a file's bytes, as hex: what operators compare before starting.
pub fn file_hash(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(hex::encode(&Sha256::digest(&bytes)))
}

/// Step 2: build and check the genesis from the operators. Writes `genesis.json` and
/// `peers.json` (the operators, in genesis order) to `out` and returns the genesis hash.
pub fn assemble_genesis(
    operators: &[Operator],
    plan: &GenesisPlan,
    out: &Path,
) -> Result<String, String> {
    if operators.is_empty() || operators.len() > 100 {
        return Err("need 1 to 100 operators".into());
    }
    for (i, a) in operators.iter().enumerate() {
        for b in &operators[i + 1..] {
            if a.public_key == b.public_key {
                return Err(format!("operators {} and {} share a key", a.name, b.name));
            }
            if a.name == b.name {
                return Err(format!("two operators are named {}", a.name));
            }
        }
        if a.account() == plan.treasury {
            return Err(format!(
                "the treasury account is operator {}'s account",
                a.name
            ));
        }
    }
    if plan.stake_btb == 0 || plan.validator_balance_btb < plan.stake_btb {
        return Err("stake must be positive and at most the validator balance".into());
    }
    let unit = |btb: u64| {
        btb.checked_mul(UNITS_PER_BTB)
            .ok_or_else(|| "amount too large".to_string())
    };
    let stake = unit(plan.stake_btb)?;
    let balance = unit(plan.validator_balance_btb)?;
    let validators_total = balance
        .checked_mul(operators.len() as u64)
        .filter(|t| *t < TOTAL_SUPPLY)
        .ok_or("validator balances exceed the fixed supply")?;
    let mut allocations: Vec<(Address, u64)> =
        operators.iter().map(|o| (o.account(), balance)).collect();
    allocations.push((plan.treasury, TOTAL_SUPPLY - validators_total));
    let genesis = GenesisFile {
        chain_id: plan.chain_id,
        name: plan.name.clone(),
        timestamp: 0,
        gas_price: 1,
        validators: operators.iter().map(|o| (o.public_key, stake)).collect(),
        allocations,
        epoch_len: plan.epoch_len,
        min_stake: stake,
        storage: Default::default(),
        oracle: None,
    };
    genesis.validate()?;
    std::fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let gpath = out.join("genesis.json");
    write(
        &gpath,
        &serde_json::to_string_pretty(&genesis.to_json()).expect("json"),
    )?;
    let peers: Vec<Value> = operators.iter().map(Operator::to_json).collect();
    write(
        &out.join("peers.json"),
        &serde_json::to_string_pretty(&Value::Array(peers)).expect("json"),
    )?;
    file_hash(&gpath)
}

/// Parameters of `node_config`.
#[derive(Debug, Clone)]
pub struct NodeConfigPlan {
    /// The directory with `validator.key`; `node.json` is written there.
    pub dir: PathBuf,
    pub genesis: PathBuf,
    pub peers: PathBuf,
    /// The hash the coordinator published; the genesis file must match it.
    pub genesis_hash: String,
    /// UDP port to listen on (all interfaces).
    pub port: u16,
    /// Loopback RPC port.
    pub rpc_port: u16,
    /// Keep chain state on disk (needs a `--features rocksdb` build).
    pub data_dir: bool,
}

/// Step 3: check the genesis hash and that this key is one of its validators, then write
/// `node.json` with every other operator as a peer. Returns the path written.
pub fn node_config(p: &NodeConfigPlan) -> Result<PathBuf, String> {
    let got = file_hash(&p.genesis)?;
    if !got.eq_ignore_ascii_case(p.genesis_hash.trim()) {
        return Err(format!(
            "genesis hash mismatch: the file hashes to {got}, the published hash is {}; \
             do not start with this file",
            p.genesis_hash.trim()
        ));
    }
    let genesis = GenesisFile::load(&p.genesis)?;
    genesis.validate()?;
    let key = crate::config::load_key(&p.dir.join("validator.key"))?;
    let me = key.verifying_key().to_bytes();
    if !genesis.validators.iter().any(|(k, _)| *k == me) {
        return Err("this validator.key is not a validator in the genesis".into());
    }
    let text = std::fs::read_to_string(&p.peers)
        .map_err(|e| format!("read {}: {e}", p.peers.display()))?;
    let list: Value = serde_json::from_str(&text).map_err(|e| format!("peers.json: {e}"))?;
    let ops = list
        .as_array()
        .ok_or("peers.json must be a list")?
        .iter()
        .map(Operator::from_json)
        .collect::<Result<Vec<_>, _>>()?;
    for (k, _) in &genesis.validators {
        if !ops.iter().any(|o| o.public_key == *k) {
            return Err("peers.json is missing a validator of the genesis".into());
        }
    }
    let peers: Vec<&str> = ops
        .iter()
        .filter(|o| o.public_key != me)
        .map(|o| o.peer.as_str())
        .collect();
    let genesis_abs = std::fs::canonicalize(&p.genesis).map_err(|e| e.to_string())?;
    let mut cfg = json!({
        "key_file": "validator.key",
        "genesis": genesis_abs.to_string_lossy(),
        "listen": format!("/ip4/0.0.0.0/udp/{}/quic-v1", p.port),
        "peers": peers,
        "rpc": format!("127.0.0.1:{}", p.rpc_port),
    });
    if p.data_dir {
        cfg["data_dir"] = json!("data");
    }
    let path = p.dir.join("node.json");
    write(&path, &serde_json::to_string_pretty(&cfg).expect("json"))?;
    Ok(path)
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NodeParams;

    fn plan(treasury: Address) -> GenesisPlan {
        GenesisPlan {
            chain_id: 42,
            name: "btb-testnet".into(),
            stake_btb: 100_000,
            validator_balance_btb: 1_000_000,
            treasury,
            epoch_len: 600,
        }
    }

    fn setup(n: usize) -> (tempfile::TempDir, Vec<Operator>) {
        let t = tempfile::tempdir().unwrap();
        let ops = (0..n)
            .map(|i| {
                operator_key(
                    &t.path().join(format!("op{i}")),
                    &format!("op{i}"),
                    format!("203.0.113.{}", i + 1).parse().unwrap(),
                    9000,
                )
                .unwrap()
            })
            .collect();
        (t, ops)
    }

    #[test]
    fn three_steps_produce_loadable_configs() {
        let (t, ops) = setup(4);
        // The coordinator only sees the public files.
        let loaded: Vec<Operator> = (0..4)
            .map(|i| Operator::load(&t.path().join(format!("op{i}/operator.json"))).unwrap())
            .collect();
        assert_eq!(loaded, ops);
        let out = t.path().join("published");
        let hash = assemble_genesis(&loaded, &plan([9; 32]), &out).unwrap();
        for (i, op) in ops.iter().enumerate() {
            let dir = t.path().join(format!("op{i}"));
            let path = node_config(&NodeConfigPlan {
                dir: dir.clone(),
                genesis: out.join("genesis.json"),
                peers: out.join("peers.json"),
                genesis_hash: hash.clone(),
                port: 9000,
                rpc_port: 8545,
                data_dir: false,
            })
            .unwrap();
            let p = NodeParams::load(&path).unwrap();
            assert_eq!(p.peers.len(), 3);
            assert!(!p.peers.contains(&op.peer));
            assert_eq!(p.genesis.validators.len(), 4);
            assert_eq!(p.key.verifying_key().to_bytes(), op.public_key);
            assert_eq!(p.rpc.unwrap().to_string(), "127.0.0.1:8545");
        }
    }

    #[test]
    fn genesis_balances_add_up_to_the_supply() {
        let (t, ops) = setup(3);
        let out = t.path().join("g");
        assemble_genesis(&ops, &plan([9; 32]), &out).unwrap();
        let g = GenesisFile::load(&out.join("genesis.json")).unwrap();
        let sum: u128 = g.allocations.iter().map(|(_, v)| u128::from(*v)).sum();
        assert_eq!(sum, u128::from(TOTAL_SUPPLY));
        assert_eq!(g.allocations.last().unwrap().0, [9; 32]);
    }

    #[test]
    fn refuses_bad_input() {
        let (t, ops) = setup(2);
        let out = t.path().join("g");
        // duplicate key
        let dup = vec![
            ops[0].clone(),
            Operator {
                name: "x".into(),
                ..ops[0].clone()
            },
        ];
        assert!(assemble_genesis(&dup, &plan([9; 32]), &out).is_err());
        // treasury is a validator account
        assert!(assemble_genesis(&ops, &plan(ops[0].account()), &out).is_err());
        // more than the supply
        let mut p = plan([9; 32]);
        p.validator_balance_btb = 600_000_000;
        assert!(assemble_genesis(&ops, &p, &out).is_err());
        // a peer address that names another key's peer id
        let mut v = ops[0].to_json();
        v["peer"] = json!(ops[1].peer);
        assert!(Operator::from_json(&v).is_err());
        // a second key in the same directory
        assert!(operator_key(
            &t.path().join("op0"),
            "op0",
            "1.2.3.4".parse().unwrap(),
            9000
        )
        .is_err());
    }

    #[test]
    fn node_config_checks_hash_and_membership() {
        let (t, ops) = setup(2);
        let out = t.path().join("g");
        let hash = assemble_genesis(&ops, &plan([9; 32]), &out).unwrap();
        let mk = |dir: PathBuf, h: &str| NodeConfigPlan {
            dir,
            genesis: out.join("genesis.json"),
            peers: out.join("peers.json"),
            genesis_hash: h.into(),
            port: 9000,
            rpc_port: 8545,
            data_dir: true,
        };
        assert!(node_config(&mk(t.path().join("op0"), &"0".repeat(64)))
            .unwrap_err()
            .contains("mismatch"));
        // an outsider's key is refused
        operator_key(
            &t.path().join("outsider"),
            "o",
            "1.2.3.4".parse().unwrap(),
            9000,
        )
        .unwrap();
        assert!(node_config(&mk(t.path().join("outsider"), &hash))
            .unwrap_err()
            .contains("not a validator"));
        // a tampered genesis no longer matches
        let g = out.join("genesis.json");
        let text = std::fs::read_to_string(&g)
            .unwrap()
            .replace("btb-testnet", "btb-evilnet");
        std::fs::write(&g, text).unwrap();
        assert!(node_config(&mk(t.path().join("op0"), &hash)).is_err());
    }
}
