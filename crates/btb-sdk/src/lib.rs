//! Rust SDK for BTB Chain contracts.
//!
//! `no_std` (uses `alloc`). Write contracts with the attribute macros and the safe host-call
//! wrappers in [`host`]:
//!
//! ```ignore
//! use btb_sdk::btb;
//!
//! #[btb::query]
//! pub fn get() -> u64 { /* ... */ 0 }
//! ```
//!
//! On `riscv64` the wrappers execute `ecall` following the ABI in `btb-vm`. On every other target
//! they panic, so the workspace still builds and unit-tests on a developer machine.
#![no_std]

extern crate alloc;

extern crate self as btb_sdk;

pub mod host;
#[cfg(all(feature = "runtime", target_os = "none"))]
pub mod runtime;
pub mod table;

pub use btb_codec as codec;
pub use btb_codec::{Address, Decode, DecodeError, Encode};
/// The attribute macros, used as `#[btb::query]` after `use btb_sdk::btb;`.
pub use btb_sdk_macros as btb;

/// Items the generated code refers to. Not part of the public API.
#[doc(hidden)]
pub mod __private {
    pub use alloc::vec::Vec;
    pub use core::slice::from_raw_parts;

    /// Decode the call input as `T`, aborting the call on malformed input.
    ///
    /// # Safety
    /// `ptr` must point to `len` readable bytes (or `len` must be 0).
    pub unsafe fn decode_args<T: btb_codec::Decode>(ptr: *const u8, len: usize) -> T {
        let input: &[u8] = if len == 0 {
            &[]
        } else {
            from_raw_parts(ptr, len)
        };
        match T::decode(input) {
            Ok(v) => v,
            Err(_) => crate::host::abort("invalid arguments"),
        }
    }

    pub fn return_value<T: btb_codec::Encode>(v: &T) {
        crate::host::return_data(&v.encode());
    }
}

/// Everything a contract usually needs.
pub mod prelude {
    pub use crate::btb;
    pub use crate::host::{abort, block_height, caller, token_balance, token_transfer, Key};
    pub use crate::table::Table;
    pub use alloc::string::String;
    pub use alloc::vec::Vec;
    pub use btb_codec::Address;
}
