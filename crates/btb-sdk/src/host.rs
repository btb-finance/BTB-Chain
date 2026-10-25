//! Safe wrappers over every host call of the `btb-vm` ABI (`btb_vm::host::abi`).
//!
//! Gas for each call is charged by the VM; see the table in `crates/btb-vm/README.md`.
//! Inside a `#[btb::query]` the state-changing calls trap with `WriteInQuery`.

use alloc::vec;
use alloc::vec::Vec;
use btb_codec::Address;

/// Host-call numbers; must match `btb_vm::host::abi` (checked by a test in `btb-tests`).
pub mod num {
    pub const STORAGE_READ: u64 = 1;
    pub const STORAGE_WRITE: u64 = 2;
    pub const STORAGE_REMOVE: u64 = 3;
    pub const CALLER: u64 = 4;
    pub const BLOCK_HEIGHT: u64 = 5;
    pub const TOKEN_BALANCE: u64 = 6;
    pub const TOKEN_TRANSFER: u64 = 7;
    pub const RETURN_DATA: u64 = 8;
    pub const ABORT: u64 = 9;
    pub const TABLE_INSERT: u64 = 10;
    pub const TABLE_GET: u64 = 11;
    pub const TABLE_UPDATE: u64 = 12;
    pub const TABLE_DELETE: u64 = 13;
    pub const TABLE_SCAN: u64 = 14;
    pub const TABLE_INDEX_SCAN: u64 = 15;
    pub const TOKEN_CREATE: u64 = 16;
    pub const TOKEN_TRANSFER_OF: u64 = 17;
    pub const TOKEN_BALANCE_OF: u64 = 18;
    pub const TOKEN_INFO: u64 = 19;
    pub const TOKEN_HOLDERS: u64 = 20;
    pub const TOKEN_MINT: u64 = 21;
    pub const TOKEN_BURN: u64 = 22;
    pub const CONTRACT_ADDRESS: u64 = 23;
    pub const NOT_FOUND: u64 = u64::MAX;
}

/// Raw `ecall`: call number in a7, arguments in a0..a4, result in a0.
#[cfg(target_arch = "riscv64")]
#[inline(always)]
fn ecall(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    // SAFETY: the VM defines `ecall` as a host call that only touches a0 and guest memory the
    // caller passed in.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") n,
            inlateout("a0") a0 => ret,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
            in("a4") a4,
        );
    }
    ret
}

#[cfg(not(target_arch = "riscv64"))]
fn ecall(n: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> u64 {
    panic!("btb host call {n} is only available when running inside the BTB VM (riscv64)");
}

fn p(b: &[u8]) -> u64 {
    b.as_ptr() as u64
}

fn l(b: &[u8]) -> u64 {
    b.len() as u64
}

/// Run a "copy into buffer, returns full length" host call, retrying once with an exact buffer.
fn read_with(mut call: impl FnMut(*mut u8, u64) -> u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    let n = call(buf.as_mut_ptr(), buf.len() as u64);
    if n == num::NOT_FOUND {
        return None;
    }
    let n = n as usize;
    if n > buf.len() {
        buf = vec![0u8; n];
        // State cannot change between the two calls inside one function execution.
        let m = call(buf.as_mut_ptr(), n as u64);
        if m != n as u64 {
            abort("host read changed size");
        }
    }
    buf.truncate(n);
    Some(buf)
}

/// Read the value stored under `key`.
pub fn storage_get(key: &[u8]) -> Option<Vec<u8>> {
    read_with(|out, cap| ecall(num::STORAGE_READ, p(key), l(key), out as u64, cap, 0))
}

/// Store `value` under `key`. Mutations and crons only.
pub fn storage_set(key: &[u8], value: &[u8]) {
    ecall(num::STORAGE_WRITE, p(key), l(key), p(value), l(value), 0);
}

/// Remove `key`. Mutations and crons only.
pub fn storage_remove(key: &[u8]) {
    ecall(num::STORAGE_REMOVE, p(key), l(key), 0, 0, 0);
}

/// Address of the account that called this function.
pub fn caller() -> Address {
    let mut a = [0u8; 32];
    ecall(num::CALLER, a.as_mut_ptr() as u64, 0, 0, 0, 0);
    Address(a)
}

/// Address of this contract (its token balances live here; match it against a mint authority).
pub fn contract_address() -> Address {
    let mut a = [0u8; 32];
    ecall(num::CONTRACT_ADDRESS, a.as_mut_ptr() as u64, 0, 0, 0, 0);
    Address(a)
}

/// Height of the block this call executes in.
pub fn block_height() -> u64 {
    ecall(num::BLOCK_HEIGHT, 0, 0, 0, 0, 0)
}

/// Token balance of `addr`.
pub fn token_balance(addr: &Address) -> u64 {
    ecall(num::TOKEN_BALANCE, addr.0.as_ptr() as u64, 0, 0, 0, 0)
}

/// Move `amount` from this contract to `to`. Returns false (no change) on insufficient balance.
pub fn token_transfer(to: &Address, amount: u64) -> bool {
    ecall(num::TOKEN_TRANSFER, to.0.as_ptr() as u64, amount, 0, 0, 0) == 0
}

/// End the call successfully with `data` as the result.
pub fn return_data(data: &[u8]) -> ! {
    ecall(num::RETURN_DATA, p(data), l(data), 0, 0, 0);
    unreachable_after_exit()
}

/// End the call with an error; all state changes of the call are discarded.
pub fn abort(msg: &str) -> ! {
    ecall(num::ABORT, p(msg.as_bytes()), l(msg.as_bytes()), 0, 0, 0);
    unreachable_after_exit()
}

/// `return_data` and `abort` never return to the guest; spin if a host ever did.
fn unreachable_after_exit() -> ! {
    #[allow(clippy::empty_loop)]
    loop {}
}

/// Append `row` to `table`; returns the new row id (monotonic, never reused).
pub fn table_insert(table: &[u8], row: &[u8]) -> u64 {
    ecall(num::TABLE_INSERT, p(table), l(table), p(row), l(row), 0)
}

/// Read row `id` of `table`.
pub fn table_get(table: &[u8], id: u64) -> Option<Vec<u8>> {
    read_with(|out, cap| ecall(num::TABLE_GET, p(table), l(table), id, out as u64, cap))
}

/// Replace row `id`. Returns false if it does not exist.
pub fn table_update(table: &[u8], id: u64, row: &[u8]) -> bool {
    ecall(num::TABLE_UPDATE, p(table), l(table), id, p(row), l(row)) == 0
}

/// Delete row `id`. Returns false if it does not exist.
pub fn table_delete(table: &[u8], id: u64) -> bool {
    ecall(num::TABLE_DELETE, p(table), l(table), id, 0, 0) == 0
}

/// Up to `max` ascending row ids >= `start_id`. Page by passing the last id + 1.
pub fn table_scan(table: &[u8], start_id: u64, max: usize) -> Vec<u64> {
    let mut raw = vec![0u8; max.saturating_mul(8)];
    let n = ecall(
        num::TABLE_SCAN,
        p(table),
        l(table),
        start_id,
        raw.as_mut_ptr() as u64,
        raw.len() as u64,
    ) as usize;
    raw.chunks_exact(8)
        .take(n)
        .map(|c| {
            let mut a = [0u8; 8];
            a.copy_from_slice(c);
            u64::from_le_bytes(a)
        })
        .collect()
}

/// A typed index value, ordered like the runtime's order-preserving encoding.
#[derive(Clone, Copy, Debug)]
pub enum Key<'a> {
    U64(u64),
    I64(i64),
    U128(u128),
    Bytes(&'a [u8]),
    Str(&'a str),
}

/// One end of an index range.
#[derive(Clone, Copy, Debug)]
pub enum Bound<'a> {
    Unbounded,
    Included(Key<'a>),
    Excluded(Key<'a>),
}

/// A page of an index scan: row ids in (index value, id) order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexPage {
    pub ids: Vec<u64>,
    /// Pass back as `cursor` to get the next page; `None` when there are no more rows.
    pub next_cursor: Option<Vec<u8>>,
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn put_bound(out: &mut Vec<u8>, b: Bound<'_>) {
    let (kind, key) = match b {
        Bound::Unbounded => return out.push(0),
        Bound::Included(k) => (1, k),
        Bound::Excluded(k) => (2, k),
    };
    out.push(kind);
    match key {
        Key::U64(v) => {
            out.push(0);
            put_bytes(out, &v.to_le_bytes());
        }
        Key::I64(v) => {
            out.push(1);
            put_bytes(out, &v.to_le_bytes());
        }
        Key::U128(v) => {
            out.push(2);
            put_bytes(out, &v.to_le_bytes());
        }
        Key::Bytes(v) => {
            out.push(3);
            put_bytes(out, v);
        }
        Key::Str(v) => {
            out.push(4);
            put_bytes(out, v.as_bytes());
        }
    }
}

/// Scan the secondary index `index` of `table` between `lo` and `hi`, at most `limit` ids
/// (the VM caps it at 256). Declared indexes are registered on the host side for now (see
/// docs/DEV_SERVER.md). Allowed in queries.
pub fn table_index_scan(
    table: &[u8],
    index: &str,
    lo: Bound<'_>,
    hi: Bound<'_>,
    descending: bool,
    limit: u32,
    cursor: &[u8],
) -> IndexPage {
    let mut req = Vec::new();
    put_bytes(&mut req, table);
    put_bytes(&mut req, index.as_bytes());
    put_bound(&mut req, lo);
    put_bound(&mut req, hi);
    req.push(u8::from(descending));
    req.extend_from_slice(&limit.min(256).to_le_bytes());
    put_bytes(&mut req, cursor);
    let want = 8 + 8 * limit.min(256) as usize + 256;
    let mut out = vec![0u8; want];
    let mut total = ecall(
        num::TABLE_INDEX_SCAN,
        p(&req),
        l(&req),
        out.as_mut_ptr() as u64,
        out.len() as u64,
        0,
    ) as usize;
    if total > out.len() {
        out = vec![0u8; total];
        // State cannot change between the two calls inside one function execution.
        total = ecall(
            num::TABLE_INDEX_SCAN,
            p(&req),
            l(&req),
            out.as_mut_ptr() as u64,
            out.len() as u64,
            0,
        ) as usize;
    }
    let u32_at = |o: usize| u32::from_le_bytes([out[o], out[o + 1], out[o + 2], out[o + 3]]);
    let (count, clen) = (u32_at(0) as usize, u32_at(4) as usize);
    if total != 8 + count * 8 + clen {
        abort("bad index scan result");
    }
    let ids = out[8..8 + count * 8]
        .chunks_exact(8)
        .map(|c| {
            let mut a = [0u8; 8];
            a.copy_from_slice(c);
            u64::from_le_bytes(a)
        })
        .collect();
    let cur = &out[8 + count * 8..total];
    IndexPage {
        ids,
        next_cursor: (!cur.is_empty()).then(|| cur.to_vec()),
    }
}

// ---- User tokens: raw wrappers. Prefer the typed `crate::token::Token`. ---------------------------

/// `token_create` with an encoded `(name, symbol, decimals, supply, authority)` request; returns
/// the new token id or the status code. Mutations only.
pub fn token_create_raw(req: &[u8]) -> Result<[u8; 32], u64> {
    let mut id = [0u8; 32];
    match ecall(
        num::TOKEN_CREATE,
        p(req),
        l(req),
        id.as_mut_ptr() as u64,
        0,
        0,
    ) {
        0 => Ok(id),
        e => Err(e),
    }
}

/// Status code of `token_transfer_of` (0 ok). Mutations only.
pub fn token_transfer_of_raw(id: &[u8; 32], to: &Address, amount: u128) -> u64 {
    let a = amount.to_le_bytes();
    ecall(num::TOKEN_TRANSFER_OF, p(id), p(&to.0), p(&a), 0, 0)
}

/// Balance of `holder` in token `id`, or the status code.
pub fn token_balance_of_raw(id: &[u8; 32], holder: &Address) -> Result<u128, u64> {
    let mut out = [0u8; 16];
    match ecall(
        num::TOKEN_BALANCE_OF,
        p(id),
        p(&holder.0),
        out.as_mut_ptr() as u64,
        0,
        0,
    ) {
        0 => Ok(u128::from_le_bytes(out)),
        e => Err(e),
    }
}

/// Encoded token info (see `crate::token::TokenInfo`), or `None` for an unknown token.
pub fn token_info_raw(id: &[u8; 32]) -> Option<Vec<u8>> {
    read_with(|out, cap| ecall(num::TOKEN_INFO, p(id), out as u64, cap, 0, 0))
}

/// Holders `(address, balance)` of one page and the cursor for the next.
pub type HolderRows = (Vec<(Address, u128)>, Option<Address>);

/// One page of `token_holders`: `(address, balance)` ordered by address, and the cursor for the
/// next page. `None` for an unknown token.
pub fn token_holders_raw(
    id: &[u8; 32],
    cursor: Option<&Address>,
    limit: u32,
) -> Option<HolderRows> {
    let mut req = Vec::with_capacity(68);
    req.extend_from_slice(id);
    req.extend_from_slice(&limit.min(256).to_le_bytes());
    if let Some(c) = cursor {
        req.extend_from_slice(&c.0);
    }
    let mut out = vec![0u8; 8 + 48 * limit.min(256) as usize + 32];
    let mut total = ecall(
        num::TOKEN_HOLDERS,
        p(&req),
        l(&req),
        out.as_mut_ptr() as u64,
        out.len() as u64,
        0,
    );
    if total == num::NOT_FOUND {
        return None;
    }
    if total as usize > out.len() {
        out = vec![0u8; total as usize];
        // State cannot change between the two calls inside one function execution.
        total = ecall(
            num::TOKEN_HOLDERS,
            p(&req),
            l(&req),
            out.as_mut_ptr() as u64,
            out.len() as u64,
            0,
        );
    }
    let total = total as usize;
    let u32_at = |o: usize| u32::from_le_bytes([out[o], out[o + 1], out[o + 2], out[o + 3]]);
    let (count, clen) = (u32_at(0) as usize, u32_at(4) as usize);
    if total != 8 + count * 48 + clen || clen > 32 {
        abort("bad token holders result");
    }
    let rows = out[8..8 + count * 48]
        .chunks_exact(48)
        .map(|c| {
            let mut a = [0u8; 32];
            a.copy_from_slice(&c[..32]);
            let mut b = [0u8; 16];
            b.copy_from_slice(&c[32..]);
            (Address(a), u128::from_le_bytes(b))
        })
        .collect();
    let next = (clen == 32).then(|| {
        let mut a = [0u8; 32];
        a.copy_from_slice(&out[8 + count * 48..total]);
        Address(a)
    });
    Some((rows, next))
}

/// Status code of `token_mint` (0 ok). Mutations only; the running contract must be the authority.
pub fn token_mint_raw(id: &[u8; 32], to: &Address, amount: u128) -> u64 {
    let a = amount.to_le_bytes();
    ecall(num::TOKEN_MINT, p(id), p(&to.0), p(&a), 0, 0)
}

/// Status code of `token_burn` (0 ok). Mutations only; burns from this contract's balance.
pub fn token_burn_raw(id: &[u8; 32], amount: u128) -> u64 {
    let a = amount.to_le_bytes();
    ecall(num::TOKEN_BURN, p(id), p(&a), 0, 0, 0)
}
