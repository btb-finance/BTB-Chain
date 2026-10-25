//! Example contract: a constant-product AMM (x * y = k) over two internal asset balances kept in
//! storage. It is self-contained: there are no cross-contract calls yet, so the reserves are
//! bookkeeping numbers, not real tokens. Swaps charge a 0.3% fee that stays in the pool.
//!
//! Build: `./build.sh` (writes `dex.elf`).
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::Address;

const RESERVE_A: &[u8] = b"ra";
const RESERVE_B: &[u8] = b"rb";
const TOTAL_SHARES: &[u8] = b"shares";

fn load(key: &[u8]) -> u128 {
    match host::storage_get(key) {
        Some(b) => match <u128 as btb_sdk::Decode>::decode(&b) {
            Ok(v) => v,
            Err(_) => host::abort("corrupt storage"),
        },
        None => 0,
    }
}

fn store(key: &[u8], v: u128) {
    host::storage_set(key, &btb_sdk::Encode::encode(&v));
}

fn share_key(who: &Address) -> [u8; 6 + 32] {
    let mut k = [0u8; 38];
    k[..6].copy_from_slice(b"share:");
    k[6..].copy_from_slice(&who.0);
    k
}

fn mul(a: u128, b: u128) -> u128 {
    match a.checked_mul(b) {
        Some(v) => v,
        None => host::abort("overflow"),
    }
}

fn add(a: u128, b: u128) -> u128 {
    match a.checked_add(b) {
        Some(v) => v,
        None => host::abort("overflow"),
    }
}

/// Integer square root (floor), Newton's method.
fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = x / 2 + 1;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Output for `amount_in` of A against reserves, after the 0.3% fee:
/// `in * 997 * rb / (ra * 1000 + in * 997)`, rounded down. Aborts when the pool is empty.
fn out_for(amount_in: u128, ra: u128, rb: u128) -> u128 {
    if ra == 0 || rb == 0 {
        host::abort("no liquidity");
    }
    let in_fee = mul(amount_in, 997);
    let den = add(mul(ra, 1000), in_fee);
    mul(in_fee, rb) / den
}

/// Deposit `a` of asset A and `b` of asset B. The first deposit mints `isqrt(a * b)` shares;
/// later ones mint `min(a * total / ra, b * total / rb)`. Returns the shares minted to the caller.
#[btb::mutation]
pub fn add_liquidity(a: u128, b: u128) -> u128 {
    if a == 0 || b == 0 {
        host::abort("zero amount");
    }
    let (ra, rb, total) = (load(RESERVE_A), load(RESERVE_B), load(TOTAL_SHARES));
    let minted = if total == 0 {
        isqrt(mul(a, b))
    } else {
        let sa = mul(a, total) / ra;
        let sb = mul(b, total) / rb;
        if sa < sb {
            sa
        } else {
            sb
        }
    };
    if minted == 0 {
        host::abort("zero shares");
    }
    let who = share_key(&host::caller());
    let mine = add(load(&who), minted);
    let (ra, rb, total) = (add(ra, a), add(rb, b), add(total, minted));
    store(&who, mine);
    store(RESERVE_A, ra);
    store(RESERVE_B, rb);
    store(TOTAL_SHARES, total);
    minted
}

/// Swap `amount_in` of A for B. Aborts if the output is below `min_out` or the pool is empty.
#[btb::mutation]
pub fn swap_a_for_b(amount_in: u128, min_out: u128) -> u128 {
    let (ra, rb) = (load(RESERVE_A), load(RESERVE_B));
    let out = out_for(amount_in, ra, rb);
    if out < min_out {
        host::abort("slippage");
    }
    store(RESERVE_A, add(ra, amount_in));
    store(RESERVE_B, rb - out);
    out
}

/// `(reserve_a, reserve_b)`.
#[btb::query]
pub fn reserves() -> (u128, u128) {
    (load(RESERVE_A), load(RESERVE_B))
}

/// What `swap_a_for_b(amount_in, _)` would return now.
#[btb::query]
pub fn quote(amount_in: u128) -> u128 {
    out_for(amount_in, load(RESERVE_A), load(RESERVE_B))
}

/// LP shares held by `addr`.
#[btb::query]
pub fn shares_of(addr: Address) -> u128 {
    load(&share_key(&addr))
}

/// Deliberately wrong: a query that writes. A test fixture showing `WriteInQuery`.
#[btb::query]
pub fn swap_in_query(amount_in: u128) -> u128 {
    store(RESERVE_A, amount_in);
    0
}
