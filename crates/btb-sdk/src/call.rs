//! Calling other contracts (`call_contract`, host call 50; `docs/LANGUAGE.md`, "Calling other
//! contracts").
//!
//! ```ignore
//! use btb_sdk::{call, CallError};
//!
//! // From a mutation or cron: run `transfer(to, amount)` of the token contract at `token`.
//! // The token sees `caller()` = this contract.
//! let moved: bool = call::<_, bool>(token, "transfer", &(to, amount), 200_000)?;
//!
//! // From anywhere (also from a query): run a query of another contract.
//! let bal: u128 = btb_sdk::query::<_, u128>(token, "balance_of", &(me,), 50_000)?;
//! ```
//!
//! * [`call`] runs a `mutation` of the target, [`query`] a `query`. A query function (and
//!   everything it calls) can only use [`query`]; [`call`] there traps with `WriteInQuery`.
//! * `gas` is forwarded to the callee, capped at what this call has left ([`GAS_ALL`] forwards
//!   everything). The callee's exact usage is charged to this call, the rest is not taken.
//! * An `Err` means the callee was reverted (only its own writes) and this call may go on, or
//!   `abort`. Naming a randomness-marked function or an HTTPS action callback traps instead.
//! * A contract cannot call one that is already on the call stack (itself included): that is
//!   [`CallError::Reentrant`]. Calls nest at most [`MAX_DEPTH`] deep.
//! * The result is read into a buffer of [`DEFAULT_OUTPUT_CAP`] bytes; use [`call_with_cap`] /
//!   [`query_with_cap`] for larger results (a result that does not fit fails the call with
//!   [`CallError::OutputTooLarge`] and reverts the callee, since a mutation cannot be retried).

use crate::host::ecall;
use alloc::vec;
use alloc::vec::Vec;
use btb_codec::{Address, Decode, Encode};

/// Host-call number and status codes; checked against `btb_vm::host::abi` by a test in
/// `btb-tests`.
pub mod num {
    pub const CALL_CONTRACT: u64 = 50;
    pub const CALL_ERR_MIN: u64 = u64::MAX - 15;
    pub const CALL_NO_CONTRACT: u64 = u64::MAX;
    pub const CALL_NO_FUNCTION: u64 = u64::MAX - 1;
    pub const CALL_ABORTED: u64 = u64::MAX - 2;
    pub const CALL_TRAPPED: u64 = u64::MAX - 3;
    pub const CALL_OUT_OF_GAS: u64 = u64::MAX - 4;
    pub const CALL_DEPTH: u64 = u64::MAX - 5;
    pub const CALL_REENTRANT: u64 = u64::MAX - 6;
    pub const CALL_OUTPUT_TOO_LARGE: u64 = u64::MAX - 7;
    pub const CALL_DEPOSIT: u64 = u64::MAX - 8;
    pub const CALL_UNSUPPORTED: u64 = u64::MAX - 9;
}

/// Deepest nesting of contract frames, the entry point included.
pub const MAX_DEPTH: usize = 8;

/// Forward all the gas this call has left.
pub const GAS_ALL: u64 = u64::MAX;

/// Size of the result buffer of [`call`] and [`query`]. (The allocator never frees, so every call
/// costs its buffer from the contract's heap.)
pub const DEFAULT_OUTPUT_CAP: usize = 2048;

/// Why a call to another contract failed. The callee's writes are reverted in every case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    /// No contract at the address.
    NoContract,
    /// The contract has no function of that name and kind.
    NoFunction,
    /// The callee called `abort`.
    Aborted,
    /// The callee trapped.
    Trapped,
    /// The callee used all the gas forwarded to it.
    OutOfGas,
    /// Nesting deeper than [`MAX_DEPTH`].
    Depth,
    /// The target is already on the call stack.
    Reentrant,
    /// The result did not fit the buffer.
    OutputTooLarge,
    /// The callee could not pay the storage deposit of what it wrote.
    Deposit,
    /// The host cannot run other contracts.
    Unsupported,
    /// The callee returned bytes that do not decode as the expected type.
    BadResult,
}

impl CallError {
    fn from_status(s: u64) -> Self {
        match s {
            num::CALL_NO_CONTRACT => Self::NoContract,
            num::CALL_NO_FUNCTION => Self::NoFunction,
            num::CALL_ABORTED => Self::Aborted,
            num::CALL_OUT_OF_GAS => Self::OutOfGas,
            num::CALL_DEPTH => Self::Depth,
            num::CALL_REENTRANT => Self::Reentrant,
            num::CALL_OUTPUT_TOO_LARGE => Self::OutputTooLarge,
            num::CALL_DEPOSIT => Self::Deposit,
            num::CALL_UNSUPPORTED => Self::Unsupported,
            _ => Self::Trapped,
        }
    }

    /// A short description, for `abort`.
    pub fn message(self) -> &'static str {
        match self {
            Self::NoContract => "call: no such contract",
            Self::NoFunction => "call: no such function",
            Self::Aborted => "call: callee aborted",
            Self::Trapped => "call: callee trapped",
            Self::OutOfGas => "call: callee out of gas",
            Self::Depth => "call: too deep",
            Self::Reentrant => "call: reentrant",
            Self::OutputTooLarge => "call: result too large",
            Self::Deposit => "call: callee cannot pay its deposit",
            Self::Unsupported => "call: not supported by this host",
            Self::BadResult => "call: undecodable result",
        }
    }
}

/// Which kind of function to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Query = 0,
    Mutation = 1,
}

/// The `call_contract` request block (see `btb_vm::host::abi::CALL_CONTRACT`).
pub fn encode_request(
    target: &Address,
    kind: Kind,
    function: &str,
    args: &[u8],
    gas: u64,
) -> Vec<u8> {
    let mut req = Vec::with_capacity(32 + 1 + 8 + 4 + function.len() + args.len());
    req.extend_from_slice(&target.0);
    req.push(kind as u8);
    req.extend_from_slice(&gas.to_le_bytes());
    req.extend_from_slice(&(function.len() as u32).to_le_bytes());
    req.extend_from_slice(function.as_bytes());
    req.extend_from_slice(args);
    req
}

/// Run `function` of `target` with already encoded `args`; the raw result bytes.
pub fn call_raw(
    kind: Kind,
    target: &Address,
    function: &str,
    args: &[u8],
    gas: u64,
    output_cap: usize,
) -> Result<Vec<u8>, CallError> {
    let req = encode_request(target, kind, function, args, gas);
    let mut out = vec![0u8; output_cap];
    let r = ecall(
        num::CALL_CONTRACT,
        req.as_ptr() as u64,
        req.len() as u64,
        out.as_mut_ptr() as u64,
        out.len() as u64,
        0,
    );
    if r >= num::CALL_ERR_MIN {
        return Err(CallError::from_status(r));
    }
    out.truncate(r as usize);
    Ok(out)
}

fn typed<A: Encode, R: Decode>(
    kind: Kind,
    target: Address,
    function: &str,
    args: &A,
    gas: u64,
    cap: usize,
) -> Result<R, CallError> {
    let out = call_raw(kind, &target, function, &args.encode(), gas, cap)?;
    R::decode(&out).map_err(|_| CallError::BadResult)
}

/// Run the mutation `function` of `target` with `args` (encoded like a transaction's arguments)
/// and decode its result. Mutations and crons only. See the module docs.
pub fn call<A: Encode, R: Decode>(
    target: Address,
    function: &str,
    args: &A,
    gas: u64,
) -> Result<R, CallError> {
    typed(
        Kind::Mutation,
        target,
        function,
        args,
        gas,
        DEFAULT_OUTPUT_CAP,
    )
}

/// Like [`call`], with a result buffer of `output_cap` bytes.
pub fn call_with_cap<A: Encode, R: Decode>(
    target: Address,
    function: &str,
    args: &A,
    gas: u64,
    output_cap: usize,
) -> Result<R, CallError> {
    typed(Kind::Mutation, target, function, args, gas, output_cap)
}

/// Run the query `function` of `target`. Allowed everywhere, also inside queries.
pub fn query<A: Encode, R: Decode>(
    target: Address,
    function: &str,
    args: &A,
    gas: u64,
) -> Result<R, CallError> {
    typed(Kind::Query, target, function, args, gas, DEFAULT_OUTPUT_CAP)
}

/// Like [`query`], with a result buffer of `output_cap` bytes.
pub fn query_with_cap<A: Encode, R: Decode>(
    target: Address,
    function: &str,
    args: &A,
    gas: u64,
    output_cap: usize,
) -> Result<R, CallError> {
    typed(Kind::Query, target, function, args, gas, output_cap)
}

/// A handle on another contract, so call sites read `token.call("transfer", &(to, n), gas)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remote {
    pub address: Address,
}

impl Remote {
    pub const fn new(address: Address) -> Self {
        Remote { address }
    }

    /// Run a mutation (see [`call`]).
    pub fn call<A: Encode, R: Decode>(
        &self,
        function: &str,
        args: &A,
        gas: u64,
    ) -> Result<R, CallError> {
        call(self.address, function, args, gas)
    }

    /// Run a query (see [`query`]).
    pub fn query<A: Encode, R: Decode>(
        &self,
        function: &str,
        args: &A,
        gas: u64,
    ) -> Result<R, CallError> {
        query(self.address, function, args, gas)
    }
}
