//! Deterministic, gas-metered RISC-V RV64IM interpreter.
//!
//! No floats, no system time, no threads, no dependencies.

pub mod decode;
pub mod elf;
pub mod gas;
pub mod vm;

pub use decode::{decode, Instr, Trap};
pub use elf::{ElfError, Loaded};
pub use vm::{Exit, Vm};
