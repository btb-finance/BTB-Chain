//! Example contract: NFTs and a marketplace that sells them for a user token, with an escrow
//! ledger.
//!
//! **Why escrow.** A contract moves tokens only from its *own* balance (`Token::transfer`), and
//! there are no user-signed token transfers or attached funds yet. So a buyer cannot pay "from
//! their wallet" inside `buy`. Instead buyers first put tokens into this contract (`deposit`),
//! which credits a per-(token, owner) escrow balance; `buy` debits the buyer's escrow and sends
//! the price to the seller with a real `Token::transfer` from the contract; `withdraw` returns
//! unused escrow. Invariant per token: `contract balance >= total escrowed`, and the part above
//! it is "unclaimed" tokens that anyone may credit to themselves with `deposit`.
//!
//! **How `deposit` learns about incoming tokens.** The contract cannot see who sent tokens to
//! it, only its balance. `deposit(token, amount)` therefore credits the caller with `amount` out
//! of the unclaimed surplus (`balance - escrowed`) and aborts if the surplus is smaller. This is
//! front-runnable (whoever deposits first claims the surplus) and is a placeholder until user
//! transfers carry the sender. `create_currency` makes a demo token whose whole supply starts as
//! such a surplus, so the demo can fund users.
//!
//! Build: `./build.sh` (writes `nft.elf`).
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::prelude::*;
use btb_sdk::token::UnwrapOrAbort;
use btb_sdk::{Decode, Encode};

const MAX_URI: usize = 256;
const MAX_PAGE: u32 = 50;

/// One row per NFT. The host indexes it by `owner` (the first 32 bytes of the row).
#[btb::table]
pub struct Nft {
    pub owner: Address,
    pub uri: String,
}

/// One row per open listing; deleted on cancel and on sale.
#[btb::table]
pub struct Listing {
    pub nft: u64,
    pub seller: Address,
    pub token: Token,
    pub price: u128,
}

/// `(listing id, nft id, seller, token, price, uri)`.
pub type ListingRow = (u64, u64, Address, Token, u128, String);

/// `e:<token><owner>` -> `u128`: the escrow ledger.
fn escrow_key(t: &Token, who: &Address) -> [u8; 66] {
    let mut k = [0u8; 66];
    k[..2].copy_from_slice(b"e:");
    k[2..34].copy_from_slice(&t.0);
    k[34..].copy_from_slice(&who.0);
    k
}

/// `t:<token>` -> `u128`: the sum of all escrow balances of that token.
fn total_key(t: &Token) -> [u8; 34] {
    let mut k = [0u8; 34];
    k[..2].copy_from_slice(b"t:");
    k[2..].copy_from_slice(&t.0);
    k
}

/// `l:<nft id, big endian>` -> listing id: at most one open listing per NFT.
fn listed_key(nft: u64) -> [u8; 10] {
    let mut k = [0u8; 10];
    k[..2].copy_from_slice(b"l:");
    k[2..].copy_from_slice(&nft.to_be_bytes());
    k
}

fn load(key: &[u8]) -> u128 {
    match host::storage_get(key) {
        Some(b) => match u128::decode(&b) {
            Ok(v) => v,
            Err(_) => abort("corrupt storage"),
        },
        None => 0,
    }
}

fn store(key: &[u8], v: u128) {
    if v == 0 {
        host::storage_remove(key);
    } else {
        host::storage_set(key, &v.encode());
    }
}

fn credit(t: &Token, who: &Address, amount: u128) {
    let (Some(a), Some(total)) = (
        load(&escrow_key(t, who)).checked_add(amount),
        load(&total_key(t)).checked_add(amount),
    ) else {
        abort("overflow");
    };
    store(&escrow_key(t, who), a);
    store(&total_key(t), total);
}

fn debit(t: &Token, who: &Address, amount: u128) {
    let have = load(&escrow_key(t, who));
    if have < amount {
        abort("insufficient escrow");
    }
    store(&escrow_key(t, who), have - amount);
    store(&total_key(t), load(&total_key(t)) - amount);
}

/// Tokens the contract holds beyond what is credited to someone.
fn unclaimed_of(t: &Token) -> u128 {
    let bal = t.balance_of(&host::contract_address()).unwrap_or_abort();
    bal.saturating_sub(load(&total_key(t)))
}

fn nft_or_abort(id: u64) -> Nft {
    match Nft::get(id) {
        Some(n) => n,
        None => abort("no such nft"),
    }
}

fn listing_or_abort(id: u64) -> Listing {
    match Listing::get(id) {
        Some(l) => l,
        None => abort("no such listing"),
    }
}

// ---- NFTs ----

/// Mint a new NFT owned by the caller; returns its id.
#[btb::mutation]
pub fn mint(uri: String) -> u64 {
    mint_impl(uri)
}

fn mint_impl(uri: String) -> u64 {
    if uri.is_empty() || uri.len() > MAX_URI {
        abort("uri must be 1..=256 bytes");
    }
    Nft {
        owner: caller(),
        uri,
    }
    .insert()
}

/// Owner only: give the NFT to `to`. Aborts while it is listed (cancel first).
#[btb::mutation]
pub fn transfer(id: u64, to: Address) {
    let mut n = nft_or_abort(id);
    if n.owner != caller() {
        abort("not the owner");
    }
    if host::storage_get(&listed_key(id)).is_some() {
        abort("nft is listed");
    }
    n.owner = to;
    n.update(id);
}

// ---- Market ----

fn list_impl(id: u64, token: Token, price: u128) -> u64 {
    let n = nft_or_abort(id);
    if n.owner != caller() {
        abort("not the owner");
    }
    if price == 0 {
        abort("price must be positive");
    }
    if host::storage_get(&listed_key(id)).is_some() {
        abort("already listed");
    }
    if token.info().is_none() {
        abort("unknown token");
    }
    let lid = Listing {
        nft: id,
        seller: n.owner,
        token,
        price,
    }
    .insert();
    host::storage_set(&listed_key(id), &lid.encode());
    lid
}

/// Owner only: offer the NFT for `price` of `token`. Returns the listing id.
#[btb::mutation]
pub fn list(id: u64, token: Token, price: u128) -> u64 {
    list_impl(id, token, price)
}

/// Mint and list in one call. Atomic: if the listing is rejected (unknown token, zero price)
/// the new NFT does not exist either.
#[btb::mutation]
pub fn mint_and_list(uri: String, token: Token, price: u128) -> (u64, u64) {
    let id = mint_impl(uri);
    (id, list_impl(id, token, price))
}

/// Seller only: withdraw the listing.
#[btb::mutation]
pub fn cancel(listing: u64) {
    let l = listing_or_abort(listing);
    if l.seller != caller() {
        abort("not the seller");
    }
    Listing::delete(listing);
    host::storage_remove(&listed_key(l.nft));
}

/// Buy a listing: debit the buyer's escrow, pay the seller `price` with a token transfer from
/// this contract, hand over the NFT and close the listing. All or nothing.
#[btb::mutation]
pub fn buy(listing: u64) {
    let l = listing_or_abort(listing);
    let buyer = caller();
    if buyer == l.seller {
        abort("cannot buy your own listing");
    }
    let mut n = nft_or_abort(l.nft);
    if n.owner != l.seller {
        abort("seller no longer owns the nft");
    }
    debit(&l.token, &buyer, l.price);
    l.token.transfer(&l.seller, l.price).unwrap_or_abort();
    n.owner = buyer;
    n.update(l.nft);
    Listing::delete(listing);
    host::storage_remove(&listed_key(l.nft));
}

// ---- Escrow ----

/// Credit the caller with `amount` of `token` out of the unclaimed surplus held by this contract.
#[btb::mutation]
pub fn deposit(token: Token, amount: u128) {
    if amount == 0 {
        abort("zero amount");
    }
    if amount > unclaimed_of(&token) {
        abort("not enough unclaimed tokens");
    }
    credit(&token, &caller(), amount);
}

/// Send `amount` of the caller's escrow back to the caller.
#[btb::mutation]
pub fn withdraw(token: Token, amount: u128) {
    if amount == 0 {
        abort("zero amount");
    }
    let who = caller();
    debit(&token, &who, amount);
    token.transfer(&who, amount).unwrap_or_abort();
}

/// Demo helper: create a token whose whole supply starts as unclaimed surplus in this contract
/// (fund yourself with `deposit`). Returns the token id.
#[btb::mutation]
pub fn create_currency(name: String, symbol: String, decimals: u8, supply: u128) -> Token {
    Token::create(&name, &symbol, decimals, supply, None).unwrap_or_abort()
}

// ---- Queries ----

/// The caller's NFTs as `(id, uri)`, oldest first, read through the owner index.
#[btb::query]
pub fn my_nfts() -> Vec<(u64, String)> {
    let me = caller();
    let mut out = Vec::new();
    let mut cursor = Vec::new();
    loop {
        let page = Nft::index_eq("owner", host::Key::Bytes(&me.0), 64, &cursor);
        for id in page.ids {
            if let Some(n) = Nft::get(id) {
                out.push((id, n.uri));
            }
        }
        match page.next_cursor {
            Some(c) => cursor = c,
            None => return out,
        }
    }
}

/// `(owner, uri)` of an NFT.
#[btb::query]
pub fn nft(id: u64) -> (Address, String) {
    let n = nft_or_abort(id);
    (n.owner, n.uri)
}

/// One page of open listings, ascending listing id, starting at `cursor` (0 for the first page),
/// at most `limit` (1..=50) rows: `(listing id, nft id, seller, token, price, uri)`. The second
/// value is the cursor of the next page, `None` at the end.
#[btb::query]
pub fn listings(cursor: u64, limit: u32) -> (Vec<ListingRow>, Option<u64>) {
    let limit = limit.clamp(1, MAX_PAGE) as usize;
    let mut ids = Listing::scan(cursor, limit + 1);
    let next = if ids.len() > limit { ids.pop() } else { None };
    let mut out = Vec::new();
    for id in ids {
        if let Some(l) = Listing::get(id) {
            let uri = Nft::get(l.nft).map(|n| n.uri).unwrap_or_default();
            out.push((id, l.nft, l.seller, l.token, l.price, uri));
        }
    }
    (out, next)
}

/// Escrow balance of `who` in `token`.
#[btb::query]
pub fn escrow_of(token: Token, who: Address) -> u128 {
    load(&escrow_key(&token, &who))
}

/// Tokens held by this contract that nobody has been credited for yet (what `deposit` can claim).
#[btb::query]
pub fn unclaimed(token: Token) -> u128 {
    unclaimed_of(&token)
}

/// Sum of all escrow balances of `token`.
#[btb::query]
pub fn escrowed(token: Token) -> u128 {
    load(&total_key(&token))
}

/// Token balance of `who` (what a seller has received, or a withdrawn escrow).
#[btb::query]
pub fn balance_of(token: Token, who: Address) -> u128 {
    token.balance_of(&who).unwrap_or_abort()
}

/// `(name, symbol, decimals, supply)`; aborts for an unknown token.
#[btb::query]
pub fn info(token: Token) -> (String, String, u8, u128) {
    match token.info() {
        Some(i) => (i.name, i.symbol, i.decimals, i.supply),
        None => abort("unknown token"),
    }
}
