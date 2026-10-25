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
