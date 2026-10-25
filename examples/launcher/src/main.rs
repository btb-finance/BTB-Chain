//! Example contract: a token launcher with a faucet, built on the native user-token module
//! (`btb_sdk::token::Token`). Native BTB is never touched.
//!
//! `launch` creates a token whose whole supply is held by this contract and registers a faucet;
//! `claim` hands each caller `faucet_amount` of it once. A mintable launch makes this contract
//! the mint authority, and only the launcher (the caller of `launch`) can mint or burn through it.
//!
//! Build: `./build.sh` (writes `launcher.elf`).
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::prelude::{String, Vec};
use btb_sdk::token::{HolderPage, Token, UnwrapOrAbort};
use btb_sdk::{Address, Decode, Encode};

/// Storage key of a faucet record: `f:<token id>` -> `(launcher, faucet_amount)`.
fn faucet_key(t: &Token) -> [u8; 34] {
    let mut k = [0u8; 34];
    k[..2].copy_from_slice(b"f:");
    k[2..].copy_from_slice(&t.0);
    k
}

/// `c:<token id><claimer>` -> marker.
fn claimed_key(t: &Token, who: &Address) -> [u8; 66] {
    let mut k = [0u8; 66];
    k[..2].copy_from_slice(b"c:");
    k[2..34].copy_from_slice(&t.0);
    k[34..].copy_from_slice(&who.0);
    k
}

fn faucet(t: &Token) -> (Address, u128) {
    match host::storage_get(&faucet_key(t)) {
        Some(b) => match <(Address, u128)>::decode(&b) {
            Ok(v) => v,
            Err(_) => host::abort("corrupt storage"),
        },
        None => host::abort("no such launch"),
    }
}

fn only_launcher(t: &Token) {
    if faucet(t).0 != host::caller() {
        host::abort("only the launcher");
    }
}

/// Create a token; this contract receives `supply` and the faucet pays `faucet_amount` per
/// claimer. If `mintable`, this contract is the mint authority. Returns the token id.
#[btb::mutation]
pub fn launch(
    name: String,
    symbol: String,
    decimals: u8,
    supply: u128,
    faucet_amount: u128,
    mintable: bool,
) -> Token {
    let authority = mintable.then(host::contract_address);
    let t = Token::create(&name, &symbol, decimals, supply, authority).unwrap_or_abort();
    host::storage_set(&faucet_key(&t), &(host::caller(), faucet_amount).encode());
    t
}

/// Once per caller and token: send the caller `faucet_amount`. Aborts when the faucet is empty.
#[btb::mutation]
pub fn claim(token: Token) -> u128 {
    let (_, amount) = faucet(&token);
    let who = host::caller();
    let key = claimed_key(&token, &who);
    if host::storage_get(&key).is_some() {
        host::abort("already claimed");
    }
    host::storage_set(&key, &[1]);
    token.transfer(&who, amount).unwrap_or_abort();
    amount
}

/// Launcher only: mint `amount` to `to` (aborts unless the token was launched mintable).
#[btb::mutation]
pub fn mint_more(token: Token, to: Address, amount: u128) {
    only_launcher(&token);
    token.mint(&to, amount).unwrap_or_abort();
}

/// Launcher only: burn `amount` from the contract's own (faucet) balance.
#[btb::mutation]
pub fn burn_faucet(token: Token, amount: u128) {
    only_launcher(&token);
    token.burn(amount).unwrap_or_abort();
}

/// Balance of `who` in `token`.
#[btb::query]
pub fn balance_of(token: Token, who: Address) -> u128 {
    token.balance_of(&who).unwrap_or_abort()
}

/// `(name, symbol, decimals, supply)`; aborts for an unknown token.
#[btb::query]
pub fn info(token: Token) -> (String, String, u8, u128) {
    match token.info() {
        Some(i) => (i.name, i.symbol, i.decimals, i.supply),
        None => host::abort("unknown token"),
    }
}

/// One page of holders (ascending address) and the cursor for the next page.
#[btb::query]
pub fn holders(
    token: Token,
    cursor: Option<Address>,
    limit: u32,
) -> (Vec<(Address, u128)>, Option<Address>) {
    let HolderPage {
        holders,
        next_cursor,
    } = token.holders(cursor.as_ref(), limit).unwrap_or_abort();
    (holders, next_cursor)
}

/// Deliberately wrong: a query that moves tokens. A test fixture showing `WriteInQuery`.
#[btb::query]
pub fn claim_in_query(token: Token) -> u128 {
    let (_, amount) = faucet(&token);
    token.transfer(&host::caller(), amount).unwrap_or_abort();
    amount
}
