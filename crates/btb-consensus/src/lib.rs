//! Deterministic, I/O-free BFT consensus for BTB Chain: a Jolteon-style (2-chain, pipelined
//! HotStuff-2 family) state machine with timeout certificates, stake-weighted quorums,
//! equivocation evidence and crash-safe persistence hooks. See `docs/CONSENSUS.md`.

mod codec;
mod replica;
mod storage;
mod types;

#[cfg(test)]
mod epoch_sim;
#[cfg(test)]
mod exec_tests;
#[cfg(test)]
mod hooks_tests;
#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;

pub use replica::{
    Anchor, Config, ExecCheck, ExecProvider, ExecVerdict, LeaderPolicy, PayloadCheck, Replica,
    Stats,
};
pub use storage::{MemoryStorage, SafetyState, SafetyStorage};
pub use types::*;
