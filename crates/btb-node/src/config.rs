//! Genesis file, node key file and node configuration.
//!
//! All files are JSON. Relative paths inside a node config are resolved against the directory
//! of the config file.

use crate::hex;
use btb_chain::staking::GenesisValidator;
use btb_chain::{
    account_address, Address, ChainConfig, GenesisConfig, TOTAL_SUPPLY, UNITS_PER_BTB,
};
use btb_consensus::{Validator, ValidatorSet};
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Genesis: chain parameters, the validator set with stakes and every initial balance.
#[derive(Debug, Clone)]
pub struct GenesisFile {
    pub chain_id: u64,
    pub name: String,
    pub timestamp: u64,
    /// Epoch-0 validators: consensus key and bonded stake in base units. The operator account
    /// is the account of the key, and the stake is bonded out of its allocation.
    pub validators: Vec<([u8; 32], u64)>,
    pub allocations: Vec<(Address, u64)>,
    pub gas_price: u64,
    /// Blocks per epoch (the validator set is re-selected every epoch).
    pub epoch_len: u64,
    /// Bonded stake needed to be selected into a set.
    pub min_stake: u64,
}

impl GenesisFile {
    pub fn chain_config(&self) -> ChainConfig {
        let mut c = ChainConfig::new(self.chain_id);
        c.gas_price = self.gas_price;
        c.staking.epoch_len = self.epoch_len;
        c.staking.min_stake = self.min_stake;
        c
    }

    pub fn genesis_validators(&self) -> Vec<GenesisValidator> {
        self.validators
            .iter()
            .map(|(key, stake)| GenesisValidator {
                operator: account_address(key),
                consensus_key: *key,
                network_addr: Vec::new(),
                stake: *stake,
            })
            .collect()
    }

    pub fn chain_genesis(&self) -> GenesisConfig {
        GenesisConfig {
            config: self.chain_config(),
            timestamp: self.timestamp,
            allocations: self.allocations.clone(),
        }
    }

    pub fn validator_set(&self) -> Result<ValidatorSet, String> {
        ValidatorSet::new(
            self.validators
                .iter()
                .map(|(key, stake)| Validator {
                    key: *key,
                    stake: *stake,
                })
                .collect(),
        )
        .map_err(|e| format!("validator set: {e}"))
    }

    /// Structural checks (the chain re-checks the supply when it builds the genesis state).
    pub fn validate(&self) -> Result<(), String> {
        self.chain_config().validate()?;
        self.validator_set()?;
        let sum: u128 = self.allocations.iter().map(|(_, v)| u128::from(*v)).sum();
        if sum != u128::from(TOTAL_SUPPLY) {
            return Err(format!(
                "allocations sum to {sum}, must equal the fixed supply {TOTAL_SUPPLY}"
            ));
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        json!({
            "chain_id": self.chain_id,
            "name": self.name,
            "timestamp": self.timestamp,
            "gas_price": self.gas_price,
            "epoch_len": self.epoch_len,
            "min_stake": self.min_stake,
            "validators": self.validators.iter()
                .map(|(k, s)| json!({"key": hex::encode(k), "stake": s})).collect::<Vec<_>>(),
            "allocations": self.allocations.iter()
                .map(|(a, v)| json!({"address": hex::encode(a), "amount": v})).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Result<Self, String> {
        let u64_of = |v: &Value, k: &str| -> Result<u64, String> {
            v.get(k)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("genesis: missing number `{k}`"))
        };
        let str_of = |v: &Value, k: &str| -> Result<String, String> {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("genesis: missing string `{k}`"))
        };
        let list = |k: &str| -> Result<&Vec<Value>, String> {
            v.get(k)
                .and_then(Value::as_array)
                .ok_or_else(|| format!("genesis: missing list `{k}`"))
        };
        let validators = list("validators")?
            .iter()
            .map(|e| Ok((hex::decode32(&str_of(e, "key")?)?, u64_of(e, "stake")?)))
            .collect::<Result<Vec<_>, String>>()?;
        let allocations = list("allocations")?
            .iter()
            .map(|e| Ok((hex::decode32(&str_of(e, "address")?)?, u64_of(e, "amount")?)))
            .collect::<Result<Vec<_>, String>>()?;
        let g = GenesisFile {
            chain_id: u64_of(v, "chain_id")?,
            name: str_of(v, "name")?,
            timestamp: u64_of(v, "timestamp").unwrap_or(0),
            gas_price: u64_of(v, "gas_price").unwrap_or(1),
            epoch_len: u64_of(v, "epoch_len").unwrap_or(600),
            min_stake: u64_of(v, "min_stake").unwrap_or(100_000 * UNITS_PER_BTB),
            validators,
            allocations,
        };
        g.validate()?;
        Ok(g)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read genesis {}: {e}", path.display()))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("genesis json: {e}"))?;
        Self::from_json(&v)
    }
}

/// Key file: `{"secret": "<64 hex digits>"}`. The same Ed25519 key signs consensus messages and
/// is the node's libp2p identity (so the peer id is derived from the validator key).
pub fn load_key(path: &Path) -> Result<SigningKey, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read key {}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("key json: {e}"))?;
    let secret = v
        .get("secret")
        .and_then(Value::as_str)
        .ok_or("key file needs a `secret` field")?;
    Ok(SigningKey::from_bytes(&hex::decode32(secret)?))
}

pub fn save_key(path: &Path, key: &SigningKey) -> Result<(), String> {
    let text = json!({"secret": hex::encode(&key.to_bytes()), "public": hex::encode(&key.verifying_key().to_bytes())});
    write_private(path, &serde_json::to_string_pretty(&text).expect("json"))
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// Timing knobs (milliseconds). The consensus timer is `tick_ms`; a round times out after
/// `round_timeout_ms`; an idle chain produces a block every `idle_block_ms` (crons need
/// blocks), which must stay well below the round timeout.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub tick_ms: u64,
    pub round_timeout_ms: u64,
    pub idle_block_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            tick_ms: 20,
            round_timeout_ms: 2_000,
            idle_block_ms: 250,
        }
    }
}

/// Everything a node needs to start.
#[derive(Debug, Clone)]
pub struct NodeParams {
    pub genesis: GenesisFile,
    pub key: SigningKey,
    /// libp2p listen address, for example `/ip4/127.0.0.1/udp/9000/quic-v1`.
    pub listen: String,
    /// Other validators: `/ip4/.../udp/.../quic-v1/p2p/<peer id>` (the peer id pins the key).
    pub peers: Vec<String>,
    /// JSON-RPC bind address (must be loopback). `None` disables the HTTP API.
    pub rpc: Option<SocketAddr>,
    /// Directory for consensus safety state and the execution log (and RocksDB with the
    /// `rocksdb` feature). `None`: everything in memory, lost on exit.
    pub data_dir: Option<PathBuf>,
    pub timing: Timing,
    /// Per-peer inbound limit: sustained cost per second and burst (a message costs
    /// `1 + bytes/4096`).
    pub rate_per_sec: f64,
    pub rate_burst: f64,
    /// Testing only: delay every inbound message by this many milliseconds to emulate a WAN.
    pub test_one_way_delay_ms: u64,
}

impl NodeParams {
    pub fn new(genesis: GenesisFile, key: SigningKey) -> Self {
        NodeParams {
            genesis,
            key,
            listen: "/ip4/127.0.0.1/udp/0/quic-v1".into(),
            peers: Vec::new(),
            rpc: None,
            data_dir: None,
            timing: Timing::default(),
            rate_per_sec: 2_000.0,
            rate_burst: 4_000.0,
            test_one_way_delay_ms: 0,
        }
    }

    /// Load `node.json`.
    pub fn load(config: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(config)
            .map_err(|e| format!("read {}: {e}", config.display()))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("config json: {e}"))?;
        let dir = config.parent().unwrap_or(Path::new("."));
        let path_of =
            |k: &str| -> Option<PathBuf> { v.get(k).and_then(Value::as_str).map(|p| dir.join(p)) };
        let genesis = GenesisFile::load(&path_of("genesis").ok_or("config needs `genesis`")?)?;
        let key = load_key(&path_of("key_file").ok_or("config needs `key_file`")?)?;
        let mut p = NodeParams::new(genesis, key);
        if let Some(l) = v.get("listen").and_then(Value::as_str) {
            p.listen = l.into();
        }
        if let Some(a) = v.get("peers").and_then(Value::as_array) {
            p.peers = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
        }
        if let Some(r) = v.get("rpc").and_then(Value::as_str) {
            let addr: SocketAddr = r.parse().map_err(|e| format!("rpc address: {e}"))?;
            p.rpc = Some(addr);
        }
        p.data_dir = path_of("data_dir");
        let num = |k: &str, d: u64| v.get(k).and_then(Value::as_u64).unwrap_or(d);
        p.timing = Timing {
            tick_ms: num("tick_ms", p.timing.tick_ms),
            round_timeout_ms: num("round_timeout_ms", p.timing.round_timeout_ms),
            idle_block_ms: num("idle_block_ms", p.timing.idle_block_ms),
        };
        Ok(p)
    }
}

/// What `init-testnet` writes. Returns the list of node directories.
pub fn init_testnet(
    validators: usize,
    dir: &Path,
    chain_id: u64,
    base_port: u16,
    rpc_base_port: u16,
) -> Result<Vec<PathBuf>, String> {
    use rand::RngCore;
    if validators == 0 || validators > 100 {
        return Err("--validators must be between 1 and 100".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut rng = rand::rngs::OsRng;
    let mut gen_key = || {
        let mut s = [0u8; 32];
        rng.fill_bytes(&mut s);
        SigningKey::from_bytes(&s)
    };
    let keys: Vec<SigningKey> = (0..validators).map(|_| gen_key()).collect();
    let faucet = gen_key();
    // Every validator account holds 1,000,000 BTB; the faucet holds the rest of the supply.
    let per_validator = 1_000_000 * UNITS_PER_BTB;
    let mut allocations: Vec<(Address, u64)> = keys
        .iter()
        .map(|k| {
            (
                account_address(&k.verifying_key().to_bytes()),
                per_validator,
            )
        })
        .collect();
    let faucet_amount = TOTAL_SUPPLY - per_validator * validators as u64;
    allocations.push((
        account_address(&faucet.verifying_key().to_bytes()),
        faucet_amount,
    ));
    let genesis = GenesisFile {
        chain_id,
        name: "btb-localnet".into(),
        timestamp: 0,
        gas_price: 1,
        validators: keys
            .iter()
            .map(|k| (k.verifying_key().to_bytes(), 100_000 * UNITS_PER_BTB))
            .collect(),
        allocations,
        epoch_len: 600,
        min_stake: 100_000 * UNITS_PER_BTB,
    };
    genesis.validate()?;
    let text = serde_json::to_string_pretty(&genesis.to_json()).expect("json");
    std::fs::write(dir.join("genesis.json"), text).map_err(|e| e.to_string())?;
    save_key(&dir.join("faucet.key"), &faucet)?;

    let peer_addr = |i: usize| {
        let id = crate::net::peer_id_of(&keys[i].verifying_key().to_bytes()).expect("valid key");
        format!(
            "/ip4/127.0.0.1/udp/{}/quic-v1/p2p/{}",
            base_port as usize + i,
            id
        )
    };
    let mut dirs = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        let nd = dir.join(format!("node{i}"));
        std::fs::create_dir_all(&nd).map_err(|e| e.to_string())?;
        save_key(&nd.join("validator.key"), key)?;
        let peers: Vec<String> = (0..validators).filter(|j| *j != i).map(peer_addr).collect();
        let mut cfg = json!({
            "key_file": "validator.key",
            "genesis": "../genesis.json",
            "listen": format!("/ip4/127.0.0.1/udp/{}/quic-v1", base_port as usize + i),
            "peers": peers,
            "rpc": format!("127.0.0.1:{}", rpc_base_port as usize + i),
        });
        if cfg!(feature = "rocksdb") {
            cfg["data_dir"] = json!("data");
        }
        std::fs::write(
            nd.join("node.json"),
            serde_json::to_string_pretty(&cfg).expect("json"),
        )
        .map_err(|e| e.to_string())?;
        dirs.push(nd);
    }
    Ok(dirs)
}
