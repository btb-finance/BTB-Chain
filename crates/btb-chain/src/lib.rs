//! The deterministic execution layer a consensus engine drives.
//!
//! `Executor::execute_block(state, block)` applies a block's transactions in order, then the
//! due cron jobs, against a `ChainState` (any `KvStore`), and returns the receipts and the new
//! Merkle state root. Nothing in here reads a clock, a random source or the network, and
//! nothing uses floats or hash-map iteration order. Queries and actions are never
//! transactions. See `docs/CHAIN.md`.

pub mod block;
pub mod exec;
pub mod mempool;
pub mod staking;
pub mod state;
pub mod tx;

pub use block::{Block, Header};
pub use exec::{BlockError, Executor, Invalid, Origin, Receipt, Revert, Status};
pub use mempool::{Mempool, MempoolConfig, MempoolError};
pub use staking::{
    ActiveValidator, EpochRecord, GenesisValidator, SlashOutcome, StakeError, StakingConfig,
    ValidatorInfo,
};
pub use state::{ChainConfig, ChainState, GenesisConfig, GenesisError, Head};
pub use tx::{
    account_address, contract_address, Address, Hash, Manifest, SigningKey, Transaction, TxBody,
    TxError, TxKind,
};

use sha2::{Digest, Sha256};

/// Decimals of BTB: 1 BTB = 10^9 base units. Balances, fees and deposits are `u64` base units.
pub const BTB_DECIMALS: u32 = 9;
pub const UNITS_PER_BTB: u64 = 1_000_000_000;
/// The fixed supply, 1,000,000,000 BTB, in base units (10^18, fits in `u64`).
pub const TOTAL_SUPPLY: u64 = 1_000_000_000 * UNITS_PER_BTB;

/// Share of a user transaction's fee that goes to the relay host pool, in basis points
/// (20%, "80/20 proposed" in TOKENOMICS.md). The rest goes to the executor (validator) pool.
pub const RELAY_SHARE_BPS: u64 = 2_000;

/// Fixed gas charged before the VM runs (all in the same unit as VM gas).
pub const TRANSFER_GAS: u64 = 2_000;
pub const CALL_BASE_GAS: u64 = 2_000;
pub const CALL_BYTE_GAS: u64 = 2;
pub const DEPLOY_BASE_GAS: u64 = 50_000;
pub const DEPLOY_BYTE_GAS: u64 = 100;
/// Staking transactions (see `docs/CHAIN.md`). Evidence verification checks two signatures.
pub const STAKE_GAS: u64 = 10_000;
pub const UNSTAKE_GAS: u64 = 10_000;
pub const REGISTER_VALIDATOR_GAS: u64 = 20_000;
pub const EVIDENCE_GAS: u64 = 100_000;

pub(crate) fn sha256(parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Reserved address that accumulates the executor (validator) share of fees.
pub fn validator_pool() -> Address {
    sha256(&[b"btb.pool.v1", b"validator"])
}

/// Reserved address that accumulates the relay host share of fees.
pub fn relay_pool() -> Address {
    sha256(&[b"btb.pool.v1", b"relay"])
}

/// Reserved address that holds slashed stake for good. Nothing can spend from it: no key and
/// no contract hashes to this address, and no code path moves its balance. It still counts in
/// the supply invariant, so slashing never changes the fixed supply.
pub fn slash_vault() -> Address {
    sha256(&[b"btb.pool.v1", b"slash-vault"])
}
