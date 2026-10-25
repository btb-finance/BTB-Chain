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
use crate::token::{self, CreateParams, TokenError, TokenId, TokenStore};
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
    /// `token_create(req_ptr, req_len, out_ptr) -> status`. Mutations only. `req` is the
    /// `btb-codec` tuple `(name: String, symbol: String, decimals: u8, initial_supply: u128,
    /// mint_authority: Option<Address>)` (see `token::CreateParams`). On `TOKEN_OK` writes the
    /// 32-byte token id (`SHA256("btb.token.v1" || creating contract || nonce:u64be)`) to
    /// `out_ptr`; the initial supply is credited to the creating contract. A malformed request
    /// traps. Never touches native BTB.
    pub const TOKEN_CREATE: u64 = 16;
    /// `token_transfer_of(id_ptr, to_ptr, amount_ptr) -> status`. Mutations only. Moves the
    /// `u128` LE amount at `amount_ptr` from the running contract to `to`.
    pub const TOKEN_TRANSFER_OF: u64 = 17;
    /// `token_balance_of(id_ptr, holder_ptr, out_ptr) -> status`. Read-only. On `TOKEN_OK`
    /// writes the balance as 16 bytes `u128` LE to `out_ptr`.
    pub const TOKEN_BALANCE_OF: u64 = 18;
    /// `token_info(id_ptr, out_ptr, out_cap) -> len | NOT_FOUND`. Read-only. Same copy rule as
    /// `storage_read`. The value is `token::TokenInfo` encoded with `btb-codec`.
    pub const TOKEN_INFO: u64 = 19;
    /// `token_holders(req_ptr, req_len, out_ptr, out_cap) -> total_len | NOT_FOUND`. Read-only.
    /// Request: `id[32] || limit:u32 LE || cursor (empty, or the 32-byte address returned by the
    /// previous page)`. Holders (non-zero balance) are paged in ascending ADDRESS order, at most
    /// `MAX_TOKEN_HOLDERS_LIMIT` per page. Result: `u32 count, u32 cursor_len (0 or 32), count x
    /// (address[32], balance u128 LE), cursor`. A non-empty cursor means more holders exist.
    /// Copied only if it fits `out_cap`; retry with a bigger buffer. A malformed request traps.
    pub const TOKEN_HOLDERS: u64 = 20;
    /// `token_mint(id_ptr, to_ptr, amount_ptr) -> status`. Mutations only. Only the token's mint
    /// authority (the running contract must be it); raises supply and `to`'s balance.
    pub const TOKEN_MINT: u64 = 21;
    /// `token_burn(id_ptr, amount_ptr) -> status`. Mutations only. Only the mint authority;
    /// burns from the running contract's own balance and lowers supply.
    pub const TOKEN_BURN: u64 = 22;

    /// `contract_address(out_ptr)`: writes the 32-byte address of the running contract (the
    /// address that owns its token balances and is matched against a token's mint authority).
    pub const CONTRACT_ADDRESS: u64 = 23;

    /// Oracle host calls use their own number range (40..=49) so they never collide with other
    /// modules' additions.
    ///
    /// `random_bytes(out_ptr, len) -> 0`. Writes `len` (at most `MAX_RANDOM_BYTES`) bytes derived
    /// from the chain's randomness beacon, the transaction id and a per-transaction call
    /// counter (see `docs/ORACLES.md`). Only allowed in a mutation or cron entry point marked
    /// `randomness`: anywhere else (queries, actions, unmarked functions) it traps with
    /// `Trap::RandomnessForbidden`; on a chain without a beacon it traps with
    /// `Trap::RandomnessUnavailable`. Gas `HOST_RANDOM_BYTES + RANDOM_BYTE * len`, charged first.
    pub const RANDOM_BYTES: u64 = 40;
    /// `price(pair_id, out_ptr) -> 0 ok | NOT_FOUND`. Read-only; allowed in queries, mutations
    /// and crons. On success writes 32 bytes to `out_ptr`: `price: u128 LE`, `height: u64 LE`
    /// (the block that last updated the price), `decimals: u8`, 7 zero bytes. `NOT_FOUND` if the
    /// pair is not configured or has no price yet. In mutations and crons the call costs
    /// `HOST_PRICE_MUTATION` gas (a read plus a per-read fee that goes to the validators); in
    /// queries `HOST_PRICE_QUERY`, and queries are not billed.
    pub const PRICE: u64 = 41;
    /// `report_request(data_ptr, data_len) -> report id | NOT_FOUND`. Mutations and crons only
    /// (a read-only host, so queries, actions and transforms, traps with `WriteInQuery`).
    /// Asks the chain's beacon committee to threshold-sign `data` (at most `MAX_REPORT_BYTES`)
    /// as a report for other chains; returns the report id (a counter, chain wide). The signed
    /// message binds the chain id, the calling contract, the id and the data, so a report cannot
    /// be passed off as another contract's. `NOT_FOUND`: the host has no report queue (a chain
    /// without an oracle committee). Gas `HOST_REPORT_REQUEST + REPORT_BYTE * len`, charged first.
    pub const REPORT_REQUEST: u64 = 42;

    /// Largest report accepted by `report_request`.
    pub const MAX_REPORT_BYTES: u64 = 1024;

    /// Largest `len` accepted by `random_bytes`.
    pub const MAX_RANDOM_BYTES: u64 = 1024;

    /// Largest `limit` honoured by `token_holders`.
    pub const MAX_TOKEN_HOLDERS_LIMIT: u32 = crate::token::MAX_HOLDERS_LIMIT;

    /// Status codes of the user-token calls (`TOKEN_NOT_FOUND`-style cases use these too).
    pub const TOKEN_OK: u64 = 0;
    /// Bad name, symbol or decimals.
    pub const TOKEN_INVALID: u64 = 1;
    /// Balance too low.
    pub const TOKEN_INSUFFICIENT: u64 = 2;
    /// Unknown token id.
    pub const TOKEN_NO_TOKEN: u64 = 3;
    /// Not the mint authority (or the token has none).
    pub const TOKEN_UNAUTHORIZED: u64 = 4;
    /// Supply, balance or nonce overflow.
    pub const TOKEN_OVERFLOW: u64 = 5;
    /// The host has no token ledger.
    pub const TOKEN_UNSUPPORTED: u64 = 6;

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

/// The agreed price of a pair, as `price` returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceRecord {
    pub price: u128,
    /// Block height of the update.
    pub height: u64,
    pub decimals: u8,
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
    /// Address of the running contract. Hosts that do not track one report the zero address.
    fn contract_address(&self) -> Address {
        [0; 32]
    }
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
    /// The user-token ledger (`token_*` calls other than native BTB). `None`: unsupported, the
    /// calls then return `abi::TOKEN_UNSUPPORTED` (`NOT_FOUND` for the info and holder reads).
    fn tokens(&mut self) -> Option<&mut dyn TokenStore> {
        None
    }
    /// `len` pseudo-random bytes for the next `random_bytes` call of this execution, derived from
    /// the beacon; `None` if the host has no beacon. The VM has already checked that the entry
    /// point is allowed to ask. Must be deterministic: same beacon, same transaction, same call
    /// index, same bytes.
    fn random_bytes(&mut self, _len: usize) -> Option<Vec<u8>> {
        None
    }
    /// The agreed price of `pair`, if any.
    fn price(&mut self, _pair: u32) -> Option<PriceRecord> {
        None
    }
    /// Queue `data` to be threshold-signed as a report for other chains; returns its id. `None`:
    /// the host has no report queue.
    fn report_request(&mut self, _data: &[u8]) -> Option<u64> {
        None
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

fn token_id(vm: &Vm, ptr: u64) -> Result<TokenId, Trap> {
    addr(vm, ptr)
}

/// A `u128` LE amount stored in guest memory.
fn amount(vm: &Vm, ptr: u64) -> Result<u128, Trap> {
    let mut a = [0u8; 16];
    a.copy_from_slice(vm.read_mem(ptr, 16)?);
    Ok(u128::from_le_bytes(a))
}

/// Status code of a token operation (`None`: the host has no ledger).
fn status(r: Option<Result<(), TokenError>>) -> u64 {
    match r {
        None => abi::TOKEN_UNSUPPORTED,
        Some(Ok(())) => abi::TOKEN_OK,
        Some(Err(e)) => e.code(),
    }
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
            abi::CONTRACT_ADDRESS => {
                vm.charge_gas(gas::HOST_CONTRACT_ADDRESS)?;
                vm.write_mem(a0, &host.contract_address())?;
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
            abi::TOKEN_CREATE => {
                check_write(host)?;
                vm.charge_gas(cost(gas::HOST_TOKEN_CREATE, gas::TOKEN_BYTE, a1))?;
                let req = buf(vm, a0, a1)?;
                // Validate the output window before doing anything.
                buf(vm, a2, 32)?;
                let Some(params) = CreateParams::decode(&req) else {
                    return Err(Trap::Abort("malformed token_create request".into()));
                };
                match host.tokens() {
                    None => abi::TOKEN_UNSUPPORTED,
                    Some(t) => match token::create(t, &params) {
                        Ok(id) => {
                            vm.write_mem(a2, &id)?;
                            abi::TOKEN_OK
                        }
                        Err(e) => e.code(),
                    },
                }
            }
            abi::TOKEN_TRANSFER_OF => {
                check_write(host)?;
                vm.charge_gas(gas::HOST_TOKEN_TRANSFER_OF)?;
                let (id, to, amount) = (token_id(vm, a0)?, addr(vm, a1)?, amount(vm, a2)?);
                status(host.tokens().map(|t| token::transfer(t, &id, &to, amount)))
            }
            abi::TOKEN_BALANCE_OF => {
                vm.charge_gas(gas::HOST_TOKEN_BALANCE_OF)?;
                let (id, holder) = (token_id(vm, a0)?, addr(vm, a1)?);
                // Validate the output window before doing anything.
                buf(vm, a2, 16)?;
                match host.tokens().map(|t| token::balance_of(t, &id, &holder)) {
                    None => abi::TOKEN_UNSUPPORTED,
                    Some(Err(e)) => e.code(),
                    Some(Ok(b)) => {
                        vm.write_mem(a2, &b.to_le_bytes())?;
                        abi::TOKEN_OK
                    }
                }
            }
            abi::TOKEN_INFO => {
                vm.charge_gas(gas::HOST_TOKEN_INFO)?;
                let id = token_id(vm, a0)?;
                // Validate the output window before doing anything.
                buf(vm, a1, a2)?;
                match host.tokens().and_then(|t| t.info(&id)) {
                    None => abi::NOT_FOUND,
                    Some(info) => {
                        let v = info.encode();
                        let n = (v.len() as u64).min(a2);
                        vm.charge_gas(n.saturating_mul(gas::TOKEN_BYTE))?;
                        vm.write_mem(a1, &v[..n as usize])?;
                        v.len() as u64
                    }
                }
            }
            abi::TOKEN_HOLDERS => {
                vm.charge_gas(cost(gas::HOST_TOKEN_HOLDERS, gas::TOKEN_BYTE, a1))?;
                let req = buf(vm, a0, a1)?;
                // Validate the output window before doing anything.
                buf(vm, a2, a3)?;
                let Some((id, limit, cursor)) = token::parse_holders_req(&req) else {
                    return Err(Trap::Abort("malformed token_holders request".into()));
                };
                let page = host
                    .tokens()
                    .and_then(|t| token::holders(t, &id, cursor.as_ref(), limit).ok());
                match page {
                    None => abi::NOT_FOUND,
                    Some((rows, next)) => {
                        // The scan work is done either way, so it is always charged.
                        vm.charge_gas(
                            (rows.len() as u64)
                                .saturating_mul(gas::TOKEN_HOLDER_ENTRY)
                                .saturating_add(if next.is_some() {
                                    32 * gas::TOKEN_BYTE
                                } else {
                                    0
                                }),
                        )?;
                        let mut out = Vec::with_capacity(8 + rows.len() * 48 + 32);
                        out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
                        out.extend_from_slice(
                            &(if next.is_some() { 32u32 } else { 0 }).to_le_bytes(),
                        );
                        for (a, b) in &rows {
                            out.extend_from_slice(a);
                            out.extend_from_slice(&b.to_le_bytes());
                        }
                        if let Some(n) = next {
                            out.extend_from_slice(&n);
                        }
                        if out.len() as u64 <= a3 {
                            vm.write_mem(a2, &out)?;
                        }
                        out.len() as u64
                    }
                }
            }
            abi::TOKEN_MINT => {
                check_write(host)?;
                vm.charge_gas(gas::HOST_TOKEN_MINT)?;
                let (id, to, amount) = (token_id(vm, a0)?, addr(vm, a1)?, amount(vm, a2)?);
                status(host.tokens().map(|t| token::mint(t, &id, &to, amount)))
            }
            abi::TOKEN_BURN => {
                check_write(host)?;
                vm.charge_gas(gas::HOST_TOKEN_BURN)?;
                let (id, amount) = (token_id(vm, a0)?, amount(vm, a1)?);
                status(host.tokens().map(|t| token::burn(t, &id, amount)))
            }
            abi::RANDOM_BYTES => {
                vm.charge_gas(cost(gas::HOST_RANDOM_BYTES, gas::RANDOM_BYTE, a1))?;
                if !vm.randomness_allowed() || host.read_only() {
                    return Err(Trap::RandomnessForbidden);
                }
                if a1 > abi::MAX_RANDOM_BYTES {
                    return Err(Trap::Abort("random_bytes: len above the maximum".into()));
                }
                // Validate the output window before doing anything.
                buf(vm, a0, a1)?;
                let bytes = host.random_bytes(a1 as usize);
                match bytes {
                    Some(b) if b.len() as u64 == a1 => vm.write_mem(a0, &b)?,
                    _ => return Err(Trap::RandomnessUnavailable),
                }
                0
            }
            abi::PRICE => {
                vm.charge_gas(if host.read_only() {
                    gas::HOST_PRICE_QUERY
                } else {
                    gas::HOST_PRICE_MUTATION
                })?;
                // Validate the output window before doing anything.
                buf(vm, a1, 32)?;
                match u32::try_from(a0).ok().and_then(|pair| host.price(pair)) {
                    None => abi::NOT_FOUND,
                    Some(p) => {
                        let mut out = [0u8; 32];
                        out[..16].copy_from_slice(&p.price.to_le_bytes());
                        out[16..24].copy_from_slice(&p.height.to_le_bytes());
                        out[24] = p.decimals;
                        vm.write_mem(a1, &out)?;
                        0
                    }
                }
            }
            abi::REPORT_REQUEST => {
                check_write(host)?;
                vm.charge_gas(cost(gas::HOST_REPORT_REQUEST, gas::REPORT_BYTE, a1))?;
                if a1 > abi::MAX_REPORT_BYTES {
                    return Err(Trap::Abort("report_request: data above the maximum".into()));
                }
                let data = buf(vm, a0, a1)?;
                host.report_request(&data).unwrap_or(abi::NOT_FOUND)
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
