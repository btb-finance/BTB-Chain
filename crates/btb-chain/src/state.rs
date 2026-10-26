//! Chain state: the committed key-value store plus the registries rebuilt from it.
//!
//! Everything consensus depends on lives in the one ordered keyspace and is committed by the
//! Merkle root. The chain adds these tags to the runtime's layout (`0x01`..`0x0C`, nodes
//! `0x06`/`0x07`):
//!
//! | Tag | Key | Value |
//! | --- | --- | --- |
//! | `0x20` nonce | `20 A` | next expected nonce of account `A`, `u64` LE |
//! | `0x21` contract | `21 C` | `(code, manifest, deployer, deploy_height)` |
//! | `0x24` cron | `24 due:u64be C function` | `every_blocks`, `u64` LE |
//! | `0x25` meta | `25 01` / `25 02` | config / head `(height, hash, timestamp)` |
//! | `0x32` site file | `32 C path` | `SHA-256(content) \|\| content` (`site.rs`) |
//!
//! Hosting nodes add `0x60`..`0x62` (see `host.rs`). Storage nodes add `0x40`..`0x47` (see `storage.rs`). Payment channels add `0x30`/`0x31` (see `channel.rs`), the DEX `0x34`..`0x38` (see `dex.rs`). Staking adds `0x26`..`0x2C` (see `staking.rs`). Native balances are the runtime's `0x05`
//! entries (accounts, contracts, the fee pools and the slash vault share them); storage
//! deposits are its `0x09` entries; bonded stake is `0x26` and unbonding stake `0x27`.

use crate::staking::{self, GenesisValidator, StakingConfig};
use crate::tx::{Address, Hash, IndexSpec, Manifest};
use crate::{sha256, TOTAL_SUPPLY};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore, MemKv};
use btb_runtime::layout;
use btb_runtime::merkle::{self, MerkleError};
use btb_runtime::tables::{IndexDef, Tables};
use btb_runtime::IndexValue;
use btb_vm::Contract;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

pub const TAG_NONCE: u8 = 0x20;
pub const TAG_CONTRACT: u8 = 0x21;
pub const TAG_CRON: u8 = 0x24;
pub const TAG_META: u8 = 0x25;

/// A storage or Merkle failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "state error: {}", self.0)
    }
}
impl std::error::Error for StateError {}
impl From<KvError> for StateError {
    fn from(e: KvError) -> Self {
        StateError(e.0)
    }
}
impl From<MerkleError> for StateError {
    fn from(e: MerkleError) -> Self {
        StateError(format!("{e:?}"))
    }
}
impl From<DecodeError> for StateError {
    fn from(e: DecodeError) -> Self {
        StateError(format!("corrupt record: {e:?}"))
    }
}

/// Protocol parameters fixed at genesis (and committed in state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainConfig {
    pub chain_id: u64,
    /// Base units charged per unit of gas.
    pub gas_price: u64,
    /// Largest `gas_limit` a call may declare.
    pub max_tx_gas: u64,
    /// Largest sum of `gas_limit`s in one block.
    pub block_gas_limit: u64,
    pub max_block_txs: u32,
    /// Gas limit given to each cron run (and the balance it must be able to reserve).
    pub cron_gas_limit: u64,
    /// Most cron gas (sum of gas used) per block; later crons wait for the next block.
    pub cron_gas_block_cap: u64,
    pub max_crons_per_block: u32,
    /// VM memory per call, bytes.
    pub vm_memory: u32,
    /// Gas limit for local queries (`Executor::query`).
    pub query_gas: u64,
    /// Validator staking, epochs, slashing.
    pub staking: StakingConfig,
    /// DEX batch window in blocks: orders are cleared together at every height that is a
    /// multiple of this. 1 = every block.
    pub dex_batch_blocks: u64,
    /// Clearing fee in basis points of what each side receives (see `dex.rs`).
    pub dex_fee_bps: u64,
    /// Storage nodes: challenge periods, prices, deposits (`storage.rs`).
    pub storage: crate::storage::StorageConfig,
    /// BTB locked from a hosting node's balance while it is registered (`host.rs`).
    pub host_bond: u64,
    /// Blocks a host's bond stays locked (and counted as money) after `Unregister`.
    pub host_unbonding_blocks: u64,
}

impl ChainConfig {
    pub fn new(chain_id: u64) -> Self {
        ChainConfig {
            chain_id,
            gas_price: 1,
            max_tx_gas: 50_000_000,
            block_gas_limit: 500_000_000,
            max_block_txs: 10_000,
            cron_gas_limit: 5_000_000,
            cron_gas_block_cap: 50_000_000,
            max_crons_per_block: 256,
            vm_memory: 1 << 20,
            query_gas: 50_000_000,
            staking: StakingConfig::default(),
            dex_batch_blocks: 1,
            dex_fee_bps: 10,
            storage: crate::storage::StorageConfig::default(),
            host_bond: 10_000 * crate::UNITS_PER_BTB,
            host_unbonding_blocks: StakingConfig::default().unbonding_blocks,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.gas_price == 0 {
            return Err("gas_price must be at least 1".into());
        }
        if self.max_tx_gas == 0 || self.cron_gas_limit == 0 {
            return Err("gas limits must be non-zero".into());
        }
        if self.cron_gas_block_cap < self.cron_gas_limit {
            return Err("cron_gas_block_cap below cron_gas_limit: crons could never run".into());
        }
        if self.max_tx_gas.checked_mul(self.gas_price).is_none()
            || self.cron_gas_limit.checked_mul(self.gas_price).is_none()
        {
            return Err("gas limit times gas_price overflows u64".into());
        }
        if (self.vm_memory as u64) < 2 * btb_vm::contract::MIN_STACK {
            return Err("vm_memory too small".into());
        }
        self.staking.validate()?;
        if !(1..=1_000_000).contains(&self.dex_batch_blocks) {
            return Err("dex_batch_blocks must be 1..=1000000".into());
        }
        if self.dex_fee_bps > crate::dex::MAX_FEE_BPS {
            return Err("dex_fee_bps above the maximum".into());
        }
        self.storage.validate()?;
        if self.host_bond > TOTAL_SUPPLY / 1_000 {
            return Err("host_bond above 0.1% of the supply".into());
        }
        if self.host_unbonding_blocks == 0 {
            return Err("host_unbonding_blocks must be at least 1".into());
        }
        Ok(())
    }

    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        for v in [
            self.chain_id,
            self.gas_price,
            self.max_tx_gas,
            self.block_gas_limit,
            u64::from(self.max_block_txs),
            self.cron_gas_limit,
            self.cron_gas_block_cap,
            u64::from(self.max_crons_per_block),
            u64::from(self.vm_memory),
            self.query_gas,
        ]
        .into_iter()
        .chain(self.staking.fields())
        .chain([self.dex_batch_blocks, self.dex_fee_bps])
        .chain(self.storage.fields())
        .chain([self.host_bond, self.host_unbonding_blocks])
        {
            v.encode_to(&mut o);
        }
        o
    }

    fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b, 1 << 20);
        let mut n = || u64::decode_from(&mut r);
        let cfg = ChainConfig {
            chain_id: n()?,
            gas_price: n()?,
            max_tx_gas: n()?,
            block_gas_limit: n()?,
            max_block_txs: n()? as u32,
            cron_gas_limit: n()?,
            cron_gas_block_cap: n()?,
            max_crons_per_block: n()? as u32,
            vm_memory: n()? as u32,
            query_gas: n()?,
            staking: StakingConfig::from_fields([n()?, n()?, n()?, n()?, n()?, n()?]),
            dex_batch_blocks: n()?,
            dex_fee_bps: n()?,
            storage: crate::storage::StorageConfig::from_fields([
                n()?,
                n()?,
                n()?,
                n()?,
                n()?,
                n()?,
                n()?,
            ]),
            host_bond: n()?,
            host_unbonding_blocks: n()?,
        };
        Ok(cfg)
    }
}

/// The genesis configuration: parameters and who holds the fixed supply.
#[derive(Debug, Clone)]
pub struct GenesisConfig {
    pub config: ChainConfig,
    /// Unix milliseconds of the genesis block (informational).
    pub timestamp: u64,
    /// Initial balances in base units. Must sum to exactly `TOTAL_SUPPLY`, with no address
    /// listed twice. This is the only place BTB is ever created.
    pub allocations: Vec<(Address, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenesisError {
    Config(String),
    /// The allocations do not add up to the fixed supply.
    WrongSupply {
        sum: u128,
    },
    DuplicateAddress(Address),
    /// The store already holds a chain.
    NotEmpty,
    State(StateError),
}

impl From<StateError> for GenesisError {
    fn from(e: StateError) -> Self {
        GenesisError::State(e)
    }
}
impl From<KvError> for GenesisError {
    fn from(e: KvError) -> Self {
        GenesisError::State(e.into())
    }
}
impl From<MerkleError> for GenesisError {
    fn from(e: MerkleError) -> Self {
        GenesisError::State(e.into())
    }
}

impl GenesisConfig {
    /// Hash that identifies the genesis block: chain parameters and sorted allocations.
    pub fn hash(&self) -> Hash {
        self.hash_with_validators(&[])
    }

    /// `hash()`, also committing to the genesis validators (identical when there are none).
    pub fn hash_with_validators(&self, validators: &[GenesisValidator]) -> Hash {
        self.hash_full(validators, None)
    }

    /// `hash_with_validators`, also committing to the oracle config (identical when `None`).
    pub fn hash_full(
        &self,
        validators: &[GenesisValidator],
        oracle: Option<&crate::oracle::OracleGenesis>,
    ) -> Hash {
        let mut allocs = self.allocations.clone();
        allocs.sort();
        let mut b = self.config.encode();
        self.timestamp.encode_to(&mut b);
        for (a, v) in &allocs {
            b.extend_from_slice(a);
            v.encode_to(&mut b);
        }
        let mut vals: Vec<&GenesisValidator> = validators.iter().collect();
        vals.sort_by_key(|v| v.operator);
        for v in vals {
            b.extend_from_slice(&v.operator);
            b.extend_from_slice(&v.consensus_key);
            v.network_addr.encode_to(&mut b);
            v.stake.encode_to(&mut b);
        }
        if let Some(o) = oracle {
            b.extend_from_slice(b"btb.oracle.genesis.v1");
            o.encode_to(&mut b);
        }
        sha256(&[b"btb.genesis.v1", &b])
    }
}

/// The last executed block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub height: u64,
    pub hash: Hash,
    pub timestamp: u64,
}

impl Head {
    pub(crate) fn encode(&self) -> Vec<u8> {
        (self.height, self.hash, self.timestamp).encode()
    }
    pub(crate) fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let (height, hash, timestamp) = <(u64, [u8; 32], u64)>::decode(b)?;
        Ok(Head {
            height,
            hash,
            timestamp,
        })
    }
}

/// A deployed contract as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractRecord {
    pub code: Vec<u8>,
    pub manifest: Manifest,
    pub deployer: Address,
    pub height: u64,
}

impl ContractRecord {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.code.encode_to(&mut o);
        self.manifest.encode_to(&mut o);
        self.deployer.encode_to(&mut o);
        self.height.encode_to(&mut o);
        o
    }
    pub(crate) fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b, 1 << 24);
        let rec = ContractRecord {
            code: Vec::decode_from(&mut r)?,
            manifest: Manifest::decode_from(&mut r)?,
            deployer: <[u8; 32]>::decode_from(&mut r)?,
            height: u64::decode_from(&mut r)?,
        };
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(rec)
    }
}

pub(crate) fn tagged(tag: u8, a: &Address) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(a);
    k
}
pub(crate) fn nonce_key(a: &Address) -> Vec<u8> {
    tagged(TAG_NONCE, a)
}
pub(crate) fn contract_key(a: &Address) -> Vec<u8> {
    tagged(TAG_CONTRACT, a)
}
pub(crate) fn meta_config_key() -> Vec<u8> {
    vec![TAG_META, 1]
}
pub(crate) fn meta_head_key() -> Vec<u8> {
    vec![TAG_META, 2]
}
pub(crate) fn cron_key(due: u64, contract: &Address, function: &str) -> Vec<u8> {
    let mut k = vec![TAG_CRON];
    k.extend_from_slice(&due.to_be_bytes());
    k.extend_from_slice(contract);
    k.extend_from_slice(function.as_bytes());
    k
}

pub(crate) fn read_u64(kv: &impl KvStore, key: &[u8]) -> Result<u64, KvError> {
    Ok(kv
        .get(key)?
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map_or(0, u64::from_le_bytes))
}

/// Sum of the `u64` LE values under one tag, paged so memory stays bounded.
pub(crate) fn sum_tag(kv: &impl KvStore, tag: u8) -> Result<u128, KvError> {
    let end = [tag + 1];
    let mut from = vec![tag];
    let mut total = 0u128;
    loop {
        let page = kv.scan(&from, Some(&end), false, 4096)?;
        for (_, v) in &page {
            total += u128::from(<[u8; 8]>::try_from(v.as_slice()).map_or(0, u64::from_le_bytes));
        }
        match page.last() {
            Some((k, _)) if page.len() == 4096 => {
                from = k.clone();
                from.push(0);
            }
            _ => return Ok(total),
        }
    }
}

/// Everything that is "money": balances (accounts, contracts, fee pools, the slash vault),
/// storage deposits, bonded stake and unbonding stake.
pub(crate) fn money(kv: &impl KvStore) -> Result<u128, KvError> {
    Ok(sum_tag(kv, layout::TAG_BALANCE)?
        + sum_tag(kv, layout::TAG_DEPOSIT)?
        + sum_tag(kv, staking::TAG_STAKE)?
        + sum_tag(kv, staking::TAG_UNBOND)?
        + sum_tag(kv, crate::channel::TAG_LOCKED)?
        + sum_tag(kv, crate::storage::TAG_NODE_DEPOSIT)?
        + sum_tag(kv, crate::storage::TAG_ESCROW)?
        + sum_tag(kv, crate::host::TAG_HOST_BOND)?
        + sum_tag(kv, crate::host::TAG_HOST_UNBOND)?)
}

/// Register a manifest's indexes into `tables` for `contract`.
pub(crate) fn register_manifest(
    tables: &mut Tables,
    contract: Address,
    manifest: &Manifest,
) -> Result<(), String> {
    let specs = manifest.validate()?;
    let mut by_table: BTreeMap<&str, Vec<&IndexSpec>> = BTreeMap::new();
    for s in &specs {
        by_table.entry(&s.table).or_default().push(s);
    }
    for (table, specs) in by_table {
        let mut names = BTreeSet::new();
        let mut defs = Vec::new();
        for s in specs {
            if !names.insert(s.index.clone()) {
                return Err(format!("index {:?} declared twice on {table}", s.index));
            }
            let (addr, off) = (s.addr, s.offset);
            defs.push(IndexDef::new(&s.index, move |row| {
                let n = if addr { 32 } else { 8 };
                let b = off
                    .checked_add(n)
                    .and_then(|end| row.get(off..end))
                    .ok_or_else(|| "row too short for index".to_string())?;
                Ok(Some(if addr {
                    IndexValue::Bytes(b.to_vec())
                } else {
                    IndexValue::U64(u64::from_le_bytes(b.try_into().expect("8 bytes")))
                }))
            }));
        }
        tables
            .register(contract, table.as_bytes(), defs)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The chain's committed state over any `KvStore`.
pub struct ChainState<S: KvStore = MemKv> {
    pub(crate) kv: S,
    pub(crate) tables: Tables,
    pub(crate) code_cache: BTreeMap<Hash, Arc<Contract>>,
    pub(crate) config: ChainConfig,
}

impl<S: KvStore> ChainState<S> {
    /// Create the chain in an empty store: write the allocations, config and head, and commit
    /// the genesis Merkle root (version 0).
    pub fn from_genesis(kv: S, genesis: &GenesisConfig) -> Result<Self, GenesisError> {
        Self::from_genesis_with_validators(kv, genesis, &[])
    }

    /// `from_genesis` with the epoch-0 validators. Each validator's stake is bonded out of its
    /// operator's allocation (the allocations still sum to `TOTAL_SUPPLY`). The genesis hash
    /// commits to the validators.
    pub fn from_genesis_with_validators(
        kv: S,
        genesis: &GenesisConfig,
        validators: &[GenesisValidator],
    ) -> Result<Self, GenesisError> {
        Self::from_genesis_full(kv, genesis, validators, None)
    }

    /// `from_genesis_with_validators` plus the oracle config (beacon key setup and price
    /// feeds, `crate::oracle`). The genesis hash commits to it. With an oracle config every
    /// block of an epoch that has a beacon key must carry a valid beacon; the validators
    /// generate the keys on chain (`crate::dkg`).
    pub fn from_genesis_full(
        mut kv: S,
        genesis: &GenesisConfig,
        validators: &[GenesisValidator],
        oracle: Option<&crate::oracle::OracleGenesis>,
    ) -> Result<Self, GenesisError> {
        genesis.config.validate().map_err(GenesisError::Config)?;
        let mut sum = 0u128;
        let mut seen = BTreeSet::new();
        for (a, v) in &genesis.allocations {
            if !seen.insert(*a) {
                return Err(GenesisError::DuplicateAddress(*a));
            }
            sum += u128::from(*v);
        }
        if sum != u128::from(TOTAL_SUPPLY) {
            return Err(GenesisError::WrongSupply { sum });
        }
        if !kv.scan(&[0], None, false, 1)?.is_empty() {
            return Err(GenesisError::NotEmpty);
        }
        let mut batch = btb_runtime::WriteBatch::new();
        for (a, v) in &genesis.allocations {
            let staked: u128 = validators
                .iter()
                .filter(|g| g.operator == *a)
                .map(|g| u128::from(g.stake))
                .sum();
            let left = u128::from(*v).checked_sub(staked).ok_or_else(|| {
                GenesisError::Config("genesis validator stakes more than its allocation".into())
            })?;
            if left > 0 {
                batch.put(layout::balance_key(a), (left as u64).to_le_bytes().to_vec());
            }
        }
        for g in validators {
            if !genesis.allocations.iter().any(|(a, _)| *a == g.operator) {
                return Err(GenesisError::Config(
                    "genesis validator has no allocation".into(),
                ));
            }
        }
        let genesis_hash = genesis.hash_full(validators, oracle);
        staking::genesis_ops(
            &genesis.config.staking,
            validators,
            genesis_hash,
            &mut batch,
        )
        .map_err(GenesisError::Config)?;
        if let Some(og) = oracle {
            og.validate_for(genesis.config.staking.epoch_len)
                .map_err(GenesisError::Config)?;
            crate::oracle::genesis_ops(og, &mut batch).map_err(GenesisError::Config)?;
        }
        batch.put(meta_config_key(), genesis.config.encode());
        let head = Head {
            height: 0,
            hash: genesis_hash,
            timestamp: genesis.timestamp,
        };
        batch.put(meta_head_key(), head.encode());
        kv.commit(batch.clone())?;
        merkle::apply(&mut kv, 0, &batch)?;
        let st = ChainState {
            kv,
            tables: Tables::new(),
            code_cache: BTreeMap::new(),
            config: genesis.config,
        };
        debug_assert_eq!(st.total_supply().ok(), Some(u128::from(TOTAL_SUPPLY)));
        Ok(st)
    }

    /// Reopen a chain from a store that already holds one (rebuilds table registrations).
    pub fn open(kv: S) -> Result<Self, StateError> {
        let cfg = kv
            .get(&meta_config_key())?
            .ok_or_else(|| StateError("no chain in store".into()))?;
        let config = ChainConfig::decode(&cfg)?;
        let mut tables = Tables::new();
        let end = [TAG_CONTRACT + 1];
        let mut from = vec![TAG_CONTRACT];
        loop {
            let page = kv.scan(&from, Some(&end), false, 256)?;
            for (k, v) in &page {
                let addr: Address = k[1..]
                    .try_into()
                    .map_err(|_| StateError("bad contract key".into()))?;
                let rec = ContractRecord::decode(v)?;
                register_manifest(&mut tables, addr, &rec.manifest).map_err(StateError)?;
            }
            match page.last() {
                Some((k, _)) if page.len() == 256 => {
                    from = k.clone();
                    from.push(0);
                }
                _ => break,
            }
        }
        Ok(ChainState {
            kv,
            tables,
            code_cache: BTreeMap::new(),
            config,
        })
    }

    pub fn config(&self) -> &ChainConfig {
        &self.config
    }

    pub fn store(&self) -> &S {
        &self.kv
    }

    pub fn into_store(self) -> S {
        self.kv
    }

    pub fn head(&self) -> Result<Head, StateError> {
        let b = self
            .kv
            .get(&meta_head_key())?
            .ok_or_else(|| StateError("no head".into()))?;
        Ok(Head::decode(&b)?)
    }

    /// Merkle state root after the head block.
    pub fn root(&self) -> Result<Hash, StateError> {
        let h = self.head()?.height;
        merkle::root(&self.kv, h)?.ok_or_else(|| StateError("missing root".into()))
    }

    pub fn balance(&self, a: &Address) -> Result<u64, KvError> {
        read_u64(&self.kv, &layout::balance_key(a))
    }

    pub fn nonce(&self, a: &Address) -> Result<u64, KvError> {
        read_u64(&self.kv, &nonce_key(a))
    }

    /// BTB locked as storage deposit by a contract.
    pub fn deposit(&self, a: &Address) -> Result<u64, KvError> {
        read_u64(&self.kv, &layout::deposit_key(a))
    }

    pub fn validator_pool_balance(&self) -> Result<u64, KvError> {
        self.balance(&crate::validator_pool())
    }

    pub fn relay_pool_balance(&self) -> Result<u64, KvError> {
        self.balance(&crate::relay_pool())
    }

    pub fn contract(&self, a: &Address) -> Result<Option<ContractRecord>, StateError> {
        match self.kv.get(&contract_key(a))? {
            None => Ok(None),
            Some(b) => Ok(Some(ContractRecord::decode(&b)?)),
        }
    }

    /// The bond locked by a registered hosting node (0 if none).
    pub fn host_bond(&self, a: &Address) -> Result<u64, KvError> {
        crate::host::bond(&self.kv, a)
    }

    /// The URL a hosting node registered, if it is registered.
    pub fn host_url(&self, a: &Address) -> Result<Option<Vec<u8>>, KvError> {
        crate::host::url(&self.kv, a)
    }

    /// A hosting node's unbonding bond: `(maturity height, amount)`.
    pub fn host_unbonding(&self, a: &Address) -> Result<Vec<(u64, u64)>, KvError> {
        crate::host::unbonding(&self.kv, a)
    }

    /// Balances plus storage deposits. Always equals `TOTAL_SUPPLY`.
    pub fn total_supply(&self) -> Result<u128, KvError> {
        money(&self.kv)
    }
}
