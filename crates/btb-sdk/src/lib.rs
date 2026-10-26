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

pub mod call;
pub mod error;
pub mod files;
pub mod host;
pub mod oracle;
#[cfg(all(feature = "runtime", target_os = "none"))]
pub mod runtime;
pub mod table;
pub mod token;

pub use btb_codec as codec;
pub use btb_codec::{Address, Decode, DecodeError, Encode};
/// The attribute macros, used as `#[btb::query]` after `use btb_sdk::btb;`.
pub use btb_sdk_macros as btb;
pub use call::{call, query, CallError, Remote};
pub use error::{Error, Result};

/// Items the generated code refers to. Not part of the public API.
#[doc(hidden)]
pub mod __private {
    pub use alloc::format;
    pub use alloc::string::String;
    pub use alloc::vec::Vec;
    pub use core::slice::from_raw_parts;

    /// Abort the call with the message of an `Err` returned by a `Result` entry point.
    pub fn fail<E: Into<crate::Error>>(e: E) -> ! {
        let e: crate::Error = e.into();
        crate::host::abort(e.message())
    }

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

/// Who is calling: the account, and whether it signed through a session key.
pub mod auth {
    use btb_codec::Address;

    /// The account that called this function (same as `host::caller()`).
    pub fn caller() -> Address {
        crate::host::caller()
    }

    /// Whether the call came from a session key of `caller()` instead of the account's own key.
    /// A session is scoped by the account's grant (contract, functions, expiry, fee budget), so
    /// most functions need not care; check it for actions that deserve the account's own
    /// signature (the chain already refuses transfers, staking and new sessions from sessions).
    /// Always false in a nested contract-to-contract call. Costs 5 gas.
    pub fn via_session() -> bool {
        crate::host::caller_via_session()
    }

    /// `(account, via_session)`.
    pub fn who() -> (Address, bool) {
        (caller(), via_session())
    }

    /// Abort unless the account signed this call itself, not through a session key.
    pub fn require_direct() {
        if via_session() {
            crate::host::abort("this action needs the account's own signature");
        }
    }
}

/// Everything a contract usually needs.
pub mod prelude {
    pub use crate::btb;
    pub use crate::host::{
        abort, block_height, caller, contract_address, token_balance, token_transfer, Key,
    };
    pub use crate::oracle::{price, price_checked, price_twap, random, Price, PriceError, Twap};
    pub use crate::table::Table;
    pub use crate::token::Token;
    pub use crate::{bail, ensure};
    pub use alloc::string::String;
    pub use alloc::vec::Vec;
    pub use btb_codec::Address;
}
