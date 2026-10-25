//! Host-call ABI: how a guest program talks to the chain.
//!
//! The guest executes `ecall` with the call number in a7 and arguments in a0..a5; the result (if
//! any) comes back in a0. `run_with_host` drives the VM and dispatches those calls to a `Host`.
//!
//! Every call charges gas BEFORE it runs (see `gas.rs`). If the charge fails the call has no
//! effect. Guest pointers and lengths are bounds-checked and trap, never panic. Addresses are 32
//! bytes; integers are little-endian u64 in registers.

use crate::decode::Trap;
use crate::gas;
use crate::vm::{Exit, Vm};

/// Stable host-call numbers (value of a7). Never renumber; only add.
pub mod abi {
    /// `storage_read(key_ptr, key_len, out_ptr, out_cap) -> value_len | NOT_FOUND`.
    /// Copies min(value_len, out_cap) bytes; returns the full length so the guest can retry.
    pub const STORAGE_READ: u64 = 1;
    /// `storage_write(key_ptr, key_len, val_ptr, val_len)`. Mutations only.
    pub const STORAGE_WRITE: u64 = 2;
    /// `storage_remove(key_ptr, key_len)`. Mutations only.
    pub const STORAGE_REMOVE: u64 = 3;
    /// `caller(out_ptr)`: writes the 32-byte caller address.
    pub const CALLER: u64 = 4;
    /// `block_height() -> u64`.
    pub const BLOCK_HEIGHT: u64 = 5;
    /// `token_balance(addr_ptr) -> u64` (balance of the 32-byte address).
    pub const TOKEN_BALANCE: u64 = 6;
    /// `token_transfer(to_ptr, amount) -> 0 ok | 1 insufficient`. Mutations only.
    pub const TOKEN_TRANSFER: u64 = 7;
    /// `return_data(ptr, len)`: ends execution with `Exit::Return`.
    pub const RETURN_DATA: u64 = 8;
    /// `abort(ptr, len)`: ends execution with `Trap::Abort(message)`.
    pub const ABORT: u64 = 9;
    /// `table_insert(table_ptr, table_len, row_ptr, row_len) -> row id`. Mutations only.
    /// Ids are monotonic per table and never reused, even after delete.
    pub const TABLE_INSERT: u64 = 10;
    /// `table_get(table_ptr, table_len, id, out_ptr, out_cap) -> row_len | NOT_FOUND`.
    /// Same copy semantics as `storage_read`.
    pub const TABLE_GET: u64 = 11;
    /// `table_update(table_ptr, table_len, id, row_ptr, row_len) -> 0 ok | 1 missing`. Mutations only.
    pub const TABLE_UPDATE: u64 = 12;
    /// `table_delete(table_ptr, table_len, id) -> 0 ok | 1 missing`. Mutations only.
    pub const TABLE_DELETE: u64 = 13;
    /// `table_scan(table_ptr, table_len, start_id, out_ptr, out_cap) -> count`. Writes up to
    /// out_cap/8 ascending row ids >= start_id as u64 LE. Page by passing last id + 1.
    pub const TABLE_SCAN: u64 = 14;
    /// `table_index_scan(req_ptr, req_len, out_ptr, out_cap) -> total_len`. Read-only; allowed in
    /// queries. Scans one secondary index of a table between two bounds and returns row ids.
    /// Request (all integers LE): `u32 table_len, table, u32 index_len, index (UTF-8),
    /// bound lo, bound hi, u8 flags (bit 0 = descending), u32 limit, u32 cursor_len, cursor`.
    /// A bound is `u8 kind (0 unbounded, 1 included, 2 excluded)` and, unless unbounded,
    /// `u8 value_tag (0 u64, 1 i64, 2 u128, 3 bytes, 4 string), u32 len, bytes` (integers use
    /// their little-endian width as `len`). Result: `u32 count, u32 cursor_len, count x u64 LE
    /// row ids ordered by (value, id), cursor`. A non-empty cursor means more rows exist; pass it
    /// back to resume. The call returns the full result length; the result is copied only if it
    /// fits in `out_cap`, so retry with a bigger buffer. A malformed request traps. `limit` is
    /// capped at `MAX_INDEX_SCAN_LIMIT`.
    pub const TABLE_INDEX_SCAN: u64 = 15;

    /// Largest `limit` honoured by `table_index_scan`.
    pub const MAX_INDEX_SCAN_LIMIT: u32 = 256;

    /// Returned by `storage_read` and `table_get` when the key is missing.
    pub const NOT_FOUND: u64 = u64::MAX;
    /// `token_transfer` results.
    pub const TRANSFER_OK: u64 = 0;
    pub const TRANSFER_INSUFFICIENT: u64 = 1;
    /// `table_update` / `table_delete` results.
    pub const TABLE_OK: u64 = 0;
    pub const TABLE_MISSING: u64 = 1;
}

/// A 32-byte account address.
pub type Address = [u8; 32];

/// A typed index value in an index scan request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexKey {
    U64(u64),
    I64(i64),
    U128(u128),
    Bytes(Vec<u8>),
    Str(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexBound {
    Unbounded,
    Included(IndexKey),
    Excluded(IndexKey),
}

/// A decoded `table_index_scan` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexScan {
    pub table: Vec<u8>,
    pub index: String,
    pub lo: IndexBound,
    pub hi: IndexBound,
    pub descending: bool,
    /// Already capped at `abi::MAX_INDEX_SCAN_LIMIT`.
    pub limit: u32,
    /// Empty to start from the beginning.
    pub cursor: Vec<u8>,
}

/// Result of an index scan: ids in (value, id) order and a resume cursor (empty = no more).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexScanPage {
    pub ids: Vec<u64>,
    pub next_cursor: Vec<u8>,
}

/// Cursor for decoding a guest-supplied request without panicking.
struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn bound(&mut self) -> Option<IndexBound> {
        let kind = self.u8()?;
        if kind == 0 {
            return Some(IndexBound::Unbounded);
        }
        let tag = self.u8()?;
        let b = self.bytes()?;
        let key = match (tag, b.len()) {
            (0, 8) => IndexKey::U64(u64::from_le_bytes(b.try_into().ok()?)),
            (1, 8) => IndexKey::I64(i64::from_le_bytes(b.try_into().ok()?)),
            (2, 16) => IndexKey::U128(u128::from_le_bytes(b.try_into().ok()?)),
            (3, _) => IndexKey::Bytes(b.to_vec()),
            (4, _) => IndexKey::Str(String::from_utf8(b.to_vec()).ok()?),
            _ => return None,
        };
        match kind {
            1 => Some(IndexBound::Included(key)),
            2 => Some(IndexBound::Excluded(key)),
            _ => None,
        }
    }
}

/// Decode a `table_index_scan` request; `None` if malformed (including trailing bytes).
pub fn parse_index_scan(req: &[u8]) -> Option<IndexScan> {
    let mut r = Rd(req);
    let table = r.bytes()?.to_vec();
    let index = String::from_utf8(r.bytes()?.to_vec()).ok()?;
    let lo = r.bound()?;
    let hi = r.bound()?;
    let flags = r.u8()?;
    if flags > 1 {
        return None;
    }
    let limit = r.u32()?.min(abi::MAX_INDEX_SCAN_LIMIT);
    let cursor = r.bytes()?.to_vec();
    if !r.0.is_empty() {
        return None;
    }
    Some(IndexScan {
        table,
        index,
        lo,
        hi,
        descending: flags == 1,
        limit,
        cursor,
    })
}

/// What the embedder (node runtime) provides to a running contract.
pub trait Host {
    /// True for queries: state-changing calls then trap with `Trap::WriteInQuery`.
    fn read_only(&self) -> bool {
        false
    }
    fn storage_read(&mut self, key: &[u8]) -> Option<Vec<u8>>;
    fn storage_write(&mut self, key: &[u8], value: &[u8]);
    fn storage_remove(&mut self, key: &[u8]);
    fn caller(&self) -> Address;
    fn block_height(&self) -> u64;
    fn token_balance(&self, addr: &Address) -> u64;
    /// Move `amount` from the contract to `to`; false (no change) if the balance is too low.
    fn token_transfer(&mut self, to: &Address, amount: u64) -> bool;
    /// Append an opaque row to `table`; returns its new id (monotonic, never reused).
    fn table_insert(&mut self, table: &[u8], row: &[u8]) -> u64;
    fn table_get(&mut self, table: &[u8], id: u64) -> Option<Vec<u8>>;
    /// False (no change) if the id does not exist.
    fn table_update(&mut self, table: &[u8], id: u64, row: &[u8]) -> bool;
    /// False (no change) if the id does not exist.
    fn table_delete(&mut self, table: &[u8], id: u64) -> bool;
    /// Up to `max` ascending row ids >= `start_id`.
    fn table_scan(&mut self, table: &[u8], start_id: u64, max: u64) -> Vec<u64>;
    /// Scan a secondary index. The default has no indexes and returns nothing; hosts with
    /// index storage (`btb-runtime`) override it.
    fn table_index_scan(&mut self, _scan: &IndexScan) -> IndexScanPage {
        IndexScanPage::default()
    }
}

fn cost(base: u64, per_byte: u64, bytes: u64) -> u64 {
    base.saturating_add(per_byte.saturating_mul(bytes))
}

fn check_write(host: &impl Host) -> Result<(), Trap> {
    if host.read_only() {
        Err(Trap::WriteInQuery)
    } else {
        Ok(())
    }
}

fn addr(vm: &Vm, ptr: u64) -> Result<Address, Trap> {
    let mut a = [0u8; 32];
    a.copy_from_slice(vm.read_mem(ptr, 32)?);
    Ok(a)
}

/// Read a guest buffer; `len` is checked against memory size before any allocation.
fn buf(vm: &Vm, ptr: u64, len: u64) -> Result<Vec<u8>, Trap> {
    let len = usize::try_from(len).map_err(|_| Trap::MemoryOutOfBounds { addr: ptr })?;
    Ok(vm.read_mem(ptr, len)?.to_vec())
}

/// Run until the guest halts, returns data, or traps, serving host calls from `host`.
pub fn run_with_host(vm: &mut Vm, host: &mut impl Host) -> Result<Exit, Trap> {
    loop {
        match vm.run()? {
            Exit::Ecall => {}
            other => return Ok(other),
        }
        let [a0, a1, a2, a3, a4] = [10, 11, 12, 13, 14].map(|i| vm.reg(i));
        let ret = match vm.reg(17) {
            abi::STORAGE_READ => {
                vm.charge_gas(cost(gas::HOST_STORAGE_READ, gas::STORAGE_READ_BYTE, a1))?;
                let key = buf(vm, a0, a1)?;
                // Validate the output window before doing anything.
                buf(vm, a2, a3)?;
                match host.storage_read(&key) {
                    None => abi::NOT_FOUND,
                    Some(v) => {
                        let n = (v.len() as u64).min(a3);
                        vm.charge_gas(n.saturating_mul(gas::STORAGE_READ_BYTE))?;
                        vm.write_mem(a2, &v[..n as usize])?;
                        v.len() as u64
                    }
                }
            }
            abi::STORAGE_WRITE => {
                check_write(host)?;
                vm.charge_gas(cost(
                    gas::HOST_STORAGE_WRITE,
                    gas::STORAGE_WRITE_BYTE,
                    a1.saturating_add(a3),
                ))?;
                let (key, val) = (buf(vm, a0, a1)?, buf(vm, a2, a3)?);
                host.storage_write(&key, &val);
                0
            }
            abi::STORAGE_REMOVE => {
                check_write(host)?;
                vm.charge_gas(cost(gas::HOST_STORAGE_REMOVE, gas::STORAGE_REMOVE_BYTE, a1))?;
                let key = buf(vm, a0, a1)?;
                host.storage_remove(&key);
                0
            }
            abi::CALLER => {
                vm.charge_gas(gas::HOST_CALLER)?;
                vm.write_mem(a0, &host.caller())?;
                0
            }
            abi::BLOCK_HEIGHT => {
                vm.charge_gas(gas::HOST_BLOCK_HEIGHT)?;
                host.block_height()
            }
            abi::TOKEN_BALANCE => {
                vm.charge_gas(gas::HOST_TOKEN_BALANCE)?;
                host.token_balance(&addr(vm, a0)?)
            }
            abi::TOKEN_TRANSFER => {
                check_write(host)?;
                vm.charge_gas(gas::HOST_TOKEN_TRANSFER)?;
                let to = addr(vm, a0)?;
                if host.token_transfer(&to, a1) {
                    abi::TRANSFER_OK
                } else {
                    abi::TRANSFER_INSUFFICIENT
                }
            }
            abi::TABLE_INSERT => {
                check_write(host)?;
                vm.charge_gas(cost(
                    gas::HOST_TABLE_INSERT,
                    gas::TABLE_WRITE_BYTE,
                    a1.saturating_add(a3),
                ))?;
                let (table, row) = (buf(vm, a0, a1)?, buf(vm, a2, a3)?);
                host.table_insert(&table, &row)
            }
            abi::TABLE_GET => {
                vm.charge_gas(cost(gas::HOST_TABLE_GET, gas::TABLE_READ_BYTE, a1))?;
                let table = buf(vm, a0, a1)?;
                // Validate the output window before doing anything.
                buf(vm, a3, a4)?;
                match host.table_get(&table, a2) {
                    None => abi::NOT_FOUND,
                    Some(v) => {
                        let n = (v.len() as u64).min(a4);
                        vm.charge_gas(n.saturating_mul(gas::TABLE_READ_BYTE))?;
                        vm.write_mem(a3, &v[..n as usize])?;
                        v.len() as u64
                    }
                }
            }
            abi::TABLE_UPDATE => {
                check_write(host)?;
                vm.charge_gas(cost(
                    gas::HOST_TABLE_UPDATE,
                    gas::TABLE_WRITE_BYTE,
                    a1.saturating_add(a4),
                ))?;
                let (table, row) = (buf(vm, a0, a1)?, buf(vm, a3, a4)?);
                if host.table_update(&table, a2, &row) {
                    abi::TABLE_OK
                } else {
                    abi::TABLE_MISSING
                }
            }
            abi::TABLE_DELETE => {
                check_write(host)?;
                vm.charge_gas(cost(gas::HOST_TABLE_DELETE, gas::TABLE_READ_BYTE, a1))?;
                let table = buf(vm, a0, a1)?;
                if host.table_delete(&table, a2) {
                    abi::TABLE_OK
                } else {
                    abi::TABLE_MISSING
                }
            }
            abi::TABLE_SCAN => {
                vm.charge_gas(cost(gas::HOST_TABLE_SCAN, gas::TABLE_READ_BYTE, a1))?;
                let table = buf(vm, a0, a1)?;
                // Validate the output window before doing anything.
                buf(vm, a3, a4)?;
                let ids = host.table_scan(&table, a2, a4 / 8);
                let n = (ids.len() as u64).min(a4 / 8);
                vm.charge_gas(n.saturating_mul(gas::TABLE_SCAN_ID))?;
                let out: Vec<u8> = ids[..n as usize]
                    .iter()
                    .flat_map(|id| id.to_le_bytes())
                    .collect();
                vm.write_mem(a3, &out)?;
                n
            }
            abi::TABLE_INDEX_SCAN => {
                vm.charge_gas(cost(gas::HOST_TABLE_INDEX_SCAN, gas::TABLE_READ_BYTE, a1))?;
                let req = buf(vm, a0, a1)?;
                // Validate the output window before doing anything.
                buf(vm, a2, a3)?;
                let Some(scan) = parse_index_scan(&req) else {
                    return Err(Trap::Abort("malformed table_index_scan request".into()));
                };
                let page = host.table_index_scan(&scan);
                // The scan work is done either way, so it is always charged.
                vm.charge_gas(
                    (page.ids.len() as u64)
                        .saturating_mul(gas::TABLE_INDEX_SCAN_ID)
                        .saturating_add((page.next_cursor.len() as u64) * gas::TABLE_READ_BYTE),
                )?;
                let mut out = Vec::with_capacity(8 + page.ids.len() * 8 + page.next_cursor.len());
                out.extend_from_slice(&(page.ids.len() as u32).to_le_bytes());
                out.extend_from_slice(&(page.next_cursor.len() as u32).to_le_bytes());
                for id in &page.ids {
                    out.extend_from_slice(&id.to_le_bytes());
                }
                out.extend_from_slice(&page.next_cursor);
                if out.len() as u64 <= a3 {
                    vm.write_mem(a2, &out)?;
                }
                out.len() as u64
            }
            abi::RETURN_DATA => {
                vm.charge_gas(cost(gas::HOST_RETURN_DATA, gas::RETURN_BYTE, a1))?;
                return Ok(Exit::Return(buf(vm, a0, a1)?));
            }
            abi::ABORT => {
                vm.charge_gas(cost(gas::HOST_ABORT, gas::ABORT_BYTE, a1))?;
                let msg = buf(vm, a0, a1)?;
                return Err(Trap::Abort(String::from_utf8_lossy(&msg).into_owned()));
            }
            n => return Err(Trap::UnknownHostCall(n)),
        };
        vm.set_reg(10, ret);
    }
}
