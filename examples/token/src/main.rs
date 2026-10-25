//! Example contract: a fungible token stored in contract storage (it does not use the native
//! token host calls).
//!
//! Build: `./build.sh` (writes `token.elf`).
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::Address;

const SUPPLY: &[u8] = b"supply";

fn balance_key(who: &Address) -> [u8; 4 + 32] {
    let mut k = [0u8; 36];
    k[..4].copy_from_slice(b"bal:");
    k[4..].copy_from_slice(&who.0);
    k
}

fn load(key: &[u8]) -> Option<u128> {
    host::storage_get(key).map(|b| match <u128 as btb_sdk::Decode>::decode(&b) {
        Ok(v) => v,
        Err(_) => host::abort("corrupt storage"),
    })
}

fn store(key: &[u8], v: u128) {
    host::storage_set(key, &btb_sdk::Encode::encode(&v));
}

fn balance(who: &Address) -> u128 {
    load(&balance_key(who)).unwrap_or(0)
}

/// Create the token once: the caller receives `supply`. Aborts if already initialised.
#[btb::mutation]
pub fn init(supply: u128) {
    if load(SUPPLY).is_some() {
        host::abort("already initialised");
    }
    store(SUPPLY, supply);
    store(&balance_key(&host::caller()), supply);
}

/// Balance of `addr` (zero for unknown accounts).
#[btb::query]
pub fn balance_of(addr: Address) -> u128 {
    balance(&addr)
}

/// Total supply (zero before `init`).
#[btb::query]
pub fn total_supply() -> u128 {
    load(SUPPLY).unwrap_or(0)
}

/// Move `amount` from the caller to `to`. Aborts on insufficient funds or overflow.
#[btb::mutation]
pub fn transfer(to: Address, amount: u128) -> bool {
    let from = host::caller();
    let Some(rest) = balance(&from).checked_sub(amount) else {
        host::abort("insufficient balance");
    };
    if from == to {
        return true;
    }
    let Some(new_to) = balance(&to).checked_add(amount) else {
        host::abort("balance overflow");
    };
    store(&balance_key(&from), rest);
    store(&balance_key(&to), new_to);
    true
}

/// Deliberately wrong: a query that writes. A test fixture showing `WriteInQuery`.
#[btb::query]
pub fn transfer_in_query(to: Address, amount: u128) -> bool {
    store(&balance_key(&to), amount);
    true
}
