//! The deterministic execution layer a consensus engine drives.
//!
//! `Executor::execute_block(state, block)` applies a block's transactions in order, then the
//! due cron jobs, against a `ChainState` (any `KvStore`), and returns the receipts and the new
//! Merkle state root. Nothing in here reads a clock, a random source or the network, and
//! nothing uses floats or hash-map iteration order. Queries and actions are never
//! transactions. See `docs/CHAIN.md`.

pub mod actions;
pub mod block;
pub mod channel;
pub mod cross;
pub mod dex;
pub mod dkg;
pub mod dkg_agent;
pub mod exec;
pub mod host;
pub mod mempool;
pub mod oracle;
pub(crate) mod parallel;
pub mod reports;
pub mod session;
pub mod site;
pub mod staking;
pub mod state;
pub mod storage;
pub mod tx;
pub mod upgrade;
pub mod vault;
pub mod vault_plan;
pub mod xdkg;
pub mod xdkg_agent;
pub mod xsign;

pub use actions::{ActionError, ActionOp, ActionVote};
pub use block::{Block, Header};
pub use channel::{channel_id, Channel, ChannelError, ChannelOp};
pub use cross::{CrossError, CrossOp};
pub use dex::{dex_fees, dex_vault, Asset, DexError, DexOp};
pub use dkg::{DkgError, DkgOp};
pub use exec::{BlockError, ExecMode, Executor, Invalid, Origin, Receipt, Revert, Status};
pub use host::{HostError, HostOp};
pub use mempool::{Mempool, MempoolConfig, MempoolError};
pub use oracle::{OracleGenesis, OracleInput, PairConfig, PriceRules};
pub use parallel::ParStats;
pub use session::{Grant, Scope, SessionError, SessionOp};
pub use staking::{
    ActiveValidator, EpochRecord, GenesisValidator, SlashOutcome, StakeError, StakingConfig,
    ValidatorInfo,
};
pub use state::{ChainConfig, ChainState, GenesisConfig, GenesisError, Head};
pub use storage::{StorageConfig, StorageError, StorageOp};
pub use tx::{
    account_address, contract_address, Address, Hash, Manifest, SigningKey, Transaction, TxBody,
    TxError, TxKind,
};

pub use tx::UpgradePolicy;
pub use upgrade::{upgrade_pool, UpgradeError, UpgradeStatus};

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
/// Payment channels: open, top-up and finalize cost `CHANNEL_GAS`; a voucher check adds a signature verification.
pub const CHANNEL_GAS: u64 = 10_000;
pub const CHANNEL_VOUCHER_GAS: u64 = 30_000;
/// DEX: placing an order escrows funds and writes the book; cancelling refunds. Clearing work
/// is paid for by the clearing fee (`ChainConfig::dex_fee_bps`), not per transaction.
pub const DEX_PLACE_GAS: u64 = 30_000;
pub const DEX_CANCEL_GAS: u64 = 15_000;
/// Blocks the payee has to submit a higher voucher after the payer starts a timeout close.
pub const CHANNEL_CHALLENGE_BLOCKS: u64 = 600;

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
