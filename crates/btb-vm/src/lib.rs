//! Deterministic, gas-metered RISC-V RV64IM interpreter.
//!
//! No floats, no system time, no threads. Dependencies: `btb-codec` and `sha2` (token ids).

pub mod contract;
pub mod decode;
pub mod elf;
pub mod gas;
pub mod host;
pub mod testing;
pub mod token;
pub mod vm;

pub use contract::{CallError, CallOutcome, Contract, ContractError, EntryPoint, FunctionKind};
pub use decode::{decode, Instr, Trap};
pub use elf::{ElfError, Loaded};
pub use host::{run_with_host, Host};
pub use vm::{Exit, Vm};
