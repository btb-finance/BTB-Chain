//! Example contract: a router that calls other contracts (`call_contract`, see
//! `docs/LANGUAGE.md`, "Calling other contracts").
//!
//! Build: `./build.sh` (writes `caller.elf`).
//!
//! The router is a thin front for another contract, here the `token` example:
//!
//! * `forward_transfer(token, to, amount)` moves tokens that the router itself holds. The token
//!   sees `caller()` = the router's address, so the router's balance is debited.
//! * `try_transfer` does the same but keeps going when the token refuses, and returns a status.
//! * `balance_via(token, who)` is a query that asks the token's query.
//!
//! The rest are fixtures for the call rules (revert scope, gas, depth, re-entrancy, randomness,
//! identity, deposits, native transfers); the tests in `crates/btb-tests/tests/calls.rs` and
//! `crates/btb-chain/tests/calls.rs` drive them.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use btb_sdk::btb;
use btb_sdk::call::GAS_ALL;
use btb_sdk::host;
use btb_sdk::oracle::{action, action::HttpRequest, random};
use btb_sdk::prelude::String;
use btb_sdk::{call, query, Address, CallError};

/// A small number for each way a call can fail (0 is success).
fn code(e: CallError) -> u8 {
    match e {
        CallError::NoContract => 1,
        CallError::NoFunction => 2,
        CallError::Aborted => 3,
        CallError::Trapped => 4,
        CallError::OutOfGas => 5,
        CallError::Depth => 6,
        CallError::Reentrant => 7,
        CallError::OutputTooLarge => 8,
        CallError::Deposit => 9,
        CallError::Unsupported => 10,
        CallError::BadResult => 11,
    }
}

fn unwrap<T>(r: Result<T, CallError>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => host::abort(e.message()),
    }
}

// ---- the router ------------------------------------------------------------------------------

/// Move `amount` of `token` from this router to `to`. Aborts if the token refuses.
#[btb::mutation]
pub fn forward_transfer(token: Address, to: Address, amount: u128) -> bool {
    unwrap(call::<_, bool>(token, "transfer", &(to, amount), GAS_ALL))
}

/// Like `forward_transfer` but never aborts: 0 on success, 100 if the token returned false,
/// else the failure code. Counts every attempt in the router's own storage.
#[btb::mutation]
pub fn try_transfer(token: Address, to: Address, amount: u128) -> u8 {
    let n = host::storage_get(b"attempts")
        .map_or(0u64, |b| <u64 as btb_sdk::Decode>::decode(&b).unwrap_or(0));
    host::storage_set(b"attempts", &btb_sdk::Encode::encode(&(n + 1)));
    match call::<_, bool>(token, "transfer", &(to, amount), GAS_ALL) {
        Ok(true) => 0,
        Ok(false) => 100,
        Err(e) => code(e),
    }
}

/// Number of `try_transfer` attempts.
#[btb::query]
pub fn attempts() -> u64 {
    host::storage_get(b"attempts")
        .map_or(0u64, |b| <u64 as btb_sdk::Decode>::decode(&b).unwrap_or(0))
}

/// Balance of `who` in `token`, asked from a query.
#[btb::query]
pub fn balance_via(token: Address, who: Address) -> u128 {
    unwrap(query::<_, u128>(token, "balance_of", &(who,), GAS_ALL))
}

// ---- fixtures --------------------------------------------------------------------------------

/// Store `val` under `key` in this contract.
#[btb::mutation]
pub fn put(key: Vec<u8>, val: Vec<u8>) {
    host::storage_set(&key, &val);
}

/// The value under `key` (empty if missing).
#[btb::query]
pub fn get(key: Vec<u8>) -> Vec<u8> {
    host::storage_get(&key).unwrap_or_default()
}

/// Length of `target.get(key)` read with the default result buffer; `10000 + code` if the call
/// failed (8 = the value did not fit).
#[btb::mutation]
pub fn get_len_via(target: Address, key: Vec<u8>) -> u32 {
    match query::<_, Vec<u8>>(target, "get", &(key,), GAS_ALL) {
        Ok(v) => v.len() as u32,
        Err(e) => 10_000 + code(e) as u32,
    }
}

/// Write, then abort: everything this call wrote must be gone.
#[btb::mutation]
pub fn put_then_abort(key: Vec<u8>, val: Vec<u8>) {
    host::storage_set(&key, &val);
    host::abort("put_then_abort");
}

/// Write `mine` here, then call `target.put_then_abort(key, val)` and carry on. Returns the
/// failure code (3 = aborted is what the test expects).
#[btb::mutation]
pub fn try_put_then_abort(target: Address, key: Vec<u8>, val: Vec<u8>) -> u8 {
    host::storage_set(b"mine", b"kept");
    match call::<_, ()>(target, "put_then_abort", &(key, val), GAS_ALL) {
        Ok(()) => 0,
        Err(e) => code(e),
    }
}

/// Like `try_put_then_abort`, but aborts itself after the callee failed.
#[btb::mutation]
pub fn put_then_abort_via(target: Address, key: Vec<u8>, val: Vec<u8>) {
    host::storage_set(b"mine", b"lost");
    let _ = call::<_, ()>(target, "put_then_abort", &(key, val), GAS_ALL);
    host::abort("outer abort");
}

/// Call `path[0].chain(path[1..])`. Returns the number of hops that succeeded below this frame
/// plus one; a failed call is stored as `err` (see `last_err`) and ends the chain. Used for the
/// depth limit and for re-entrancy.
#[btb::mutation]
pub fn chain(path: Vec<Address>) -> u64 {
    let Some((next, rest)) = path.split_first() else {
        return 1;
    };
    match call::<_, u64>(*next, "chain", &(rest.to_vec(),), GAS_ALL) {
        Ok(n) => n + 1,
        Err(e) => {
            host::storage_set(b"err", &[code(e)]);
            1
        }
    }
}

/// The code of the last failed call made by `chain` (0 = none).
#[btb::query]
pub fn last_err() -> u8 {
    host::storage_get(b"err").map_or(0, |b| b.first().copied().unwrap_or(0))
}

/// Who called this and who is running: `(caller(), contract_address())`.
#[btb::query]
pub fn ident() -> (Address, Address) {
    (host::caller(), host::contract_address())
}

/// The same as a mutation.
#[btb::mutation]
pub fn ident_m() -> (Address, Address) {
    (host::caller(), host::contract_address())
}

/// `target.ident()` as seen from here (a query call).
#[btb::mutation]
pub fn ident_via(target: Address) -> (Address, Address) {
    unwrap(query::<_, (Address, Address)>(
        target,
        "ident",
        &(),
        GAS_ALL,
    ))
}

/// `target.ident_m()` as seen from here (a mutation call).
#[btb::mutation]
pub fn ident_m_via(target: Address) -> (Address, Address) {
    unwrap(call::<_, (Address, Address)>(
        target,
        "ident_m",
        &(),
        GAS_ALL,
    ))
}

/// Who is calling and whether through a session key: `(btb_sdk::auth::who())`.
#[btb::mutation]
pub fn auth_m() -> (Address, bool) {
    btb_sdk::auth::who()
}

/// `target.auth_m()` as seen from here: a nested frame is never a session.
#[btb::mutation]
pub fn auth_m_via(target: Address) -> (Address, bool) {
    unwrap(call::<_, (Address, bool)>(target, "auth_m", &(), GAS_ALL))
}

/// Only the account's own signature may do this: aborts when a session key signed.
#[btb::mutation]
pub fn direct_only() -> bool {
    btb_sdk::auth::require_direct();
    true
}

/// Burn gas in a loop; the result keeps the loop alive.
#[btb::mutation]
pub fn burn(n: u64) -> u64 {
    let mut acc = 0u64;
    let mut i = 0u64;
    while i < n {
        acc = acc.wrapping_mul(31).wrapping_add(i);
        i += 1;
        core::hint::black_box(&acc);
    }
    acc
}

/// Call `target.burn(n)` with `gas` forwarded: 0 on success, else the failure code.
#[btb::mutation]
pub fn call_burn(target: Address, n: u64, gas: u64) -> u8 {
    match call::<_, u64>(target, "burn", &(n,), gas) {
        Ok(_) => 0,
        Err(e) => code(e),
    }
}

/// A marked function: may use randomness itself, and must never be callable by contracts.
#[btb::mutation(randomness)]
pub fn roll() -> u8 {
    random::range(1, 7) as u8
}

/// Unmarked: asking for randomness here traps.
#[btb::mutation]
pub fn peek_random() -> u64 {
    random::u64()
}

/// Call the marked `target.roll()`: the call itself traps.
#[btb::mutation]
pub fn call_roll(target: Address) -> u8 {
    match call::<_, u8>(target, "roll", &(), GAS_ALL) {
        Ok(_) => 0,
        Err(e) => code(e),
    }
}

/// A marked function that calls an unmarked `target.peek_random()`: randomness inside the nested
/// frame traps, which the callee reports as a failed call.
#[btb::mutation(randomness)]
pub fn marked_calls_peek(target: Address) -> u8 {
    match call::<_, u64>(target, "peek_random", &(), GAS_ALL) {
        Ok(_) => 0,
        Err(e) => code(e),
    }
}

/// A query that tries to run a mutation: traps with `WriteInQuery`.
#[btb::query]
pub fn bad_query(target: Address) {
    let _ = call::<_, ()>(
        target,
        "put",
        &(Vec::<u8>::new(), Vec::<u8>::new()),
        GAS_ALL,
    );
}

/// Store `n` bytes under `blob` (grows this contract's storage deposit).
#[btb::mutation]
pub fn grow(n: u32) {
    host::storage_set(b"blob", &alloc::vec![7u8; n as usize]);
}

/// Call `target.grow(n)`: 0 on success, else the failure code (9 = the target cannot pay).
#[btb::mutation]
pub fn grow_via(target: Address, n: u32) -> u8 {
    match call::<_, ()>(target, "grow", &(n,), GAS_ALL) {
        Ok(()) => 0,
        Err(e) => code(e),
    }
}

/// Pay `amount` native BTB from this contract to `to`.
#[btb::mutation]
pub fn pay(to: Address, amount: u64) -> bool {
    host::token_transfer(&to, amount)
}

/// Call `target.pay(to, amount)`: the target pays, not this contract. 0 paid, 100 refused.
#[btb::mutation]
pub fn pay_via(target: Address, to: Address, amount: u64) -> u8 {
    match call::<_, bool>(target, "pay", &(to, amount), GAS_ALL) {
        Ok(true) => 0,
        Ok(false) => 100,
        Err(e) => code(e),
    }
}

/// An HTTPS action and its callback: the callback must not be callable by contracts.
#[btb::action]
pub fn fetch(url: String) -> HttpRequest {
    HttpRequest::get(url).limits(4096, 2000)
}

#[btb::mutation]
pub fn fetch_callback(id: Address, args: Vec<u8>, status: u8, data: Vec<u8>) {
    action::require_callback();
    host::storage_set(b"fetched", &[status]);
    let _ = (id, args, data);
}

/// Try to call `target.fetch_callback(...)`: the call traps.
#[btb::mutation]
pub fn call_callback(target: Address) -> u8 {
    match call::<_, ()>(
        target,
        "fetch_callback",
        &(Address([0; 32]), Vec::<u8>::new(), 0u8, Vec::<u8>::new()),
        GAS_ALL,
    ) {
        Ok(()) => 0,
        Err(e) => code(e),
    }
}
