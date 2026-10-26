//! Per-instruction gas costs. Charged before an instruction executes.

use crate::decode::{AluOp, Instr};

/// Simple ALU ops, LUI, AUIPC, FENCE, EBREAK, ECALL (host-call costs are charged by `host::run_with_host`, see below).
pub const BASE: u64 = 1;
/// Branches and jumps.
pub const BRANCH: u64 = 1;
/// MUL, MULH, MULHSU, MULHU, MULW.
pub const MUL: u64 = 3;
/// DIV, DIVU, REM, REMU and *W variants.
pub const DIV: u64 = 5;
/// Loads.
pub const LOAD: u64 = 2;
/// Stores.
pub const STORE: u64 = 2;

fn alu(op: AluOp) -> u64 {
    match op {
        AluOp::Mul | AluOp::Mulh | AluOp::Mulhsu | AluOp::Mulhu => MUL,
        AluOp::Div | AluOp::Divu | AluOp::Rem | AluOp::Remu => DIV,
        _ => BASE,
    }
}

/// Gas cost of one instruction.
pub fn cost(i: &Instr) -> u64 {
    match *i {
        Instr::Jal { .. } | Instr::Jalr { .. } | Instr::Branch { .. } => BRANCH,
        Instr::Load { .. } => LOAD,
        Instr::Store { .. } => STORE,
        Instr::Op { op, .. } | Instr::OpW { op, .. } => alu(op),
        Instr::OpImm { op, .. } | Instr::OpImmW { op, .. } => alu(op),
        _ => BASE,
    }
}

// ---- Host calls -------------------------------------------------------------------------------
//
// Charged by `host::run_with_host` on top of the ECALL instruction itself (BASE), BEFORE the call
// runs. cost = base + per_byte * bytes_moved, saturating (so absurd lengths just run out of gas).
// Unknown call numbers charge nothing extra and trap.

/// `caller(out_ptr)`: flat, writes 32 bytes.
pub const HOST_CALLER: u64 = 10;
/// `caller_via_session()`: flat.
pub const HOST_CALLER_VIA_SESSION: u64 = 5;
/// `contract_address(out_ptr)`: flat, writes 32 bytes.
pub const HOST_CONTRACT_ADDRESS: u64 = 10;
/// `block_height()`: flat.
pub const HOST_BLOCK_HEIGHT: u64 = 5;
/// `token_balance(addr_ptr)`: flat, reads a 32-byte address.
pub const HOST_TOKEN_BALANCE: u64 = 50;
/// `token_transfer(to_ptr, amount)`: flat, reads a 32-byte address.
pub const HOST_TOKEN_TRANSFER: u64 = 300;
/// `storage_read`: base, plus `STORAGE_READ_BYTE` per key byte up front, plus the same per value
/// byte actually copied to the guest (min(value_len, out_cap)), charged before the copy.
pub const HOST_STORAGE_READ: u64 = 200;
pub const STORAGE_READ_BYTE: u64 = 1;
/// `storage_write`: base plus `STORAGE_WRITE_BYTE` per (key_len + val_len).
pub const HOST_STORAGE_WRITE: u64 = 500;
pub const STORAGE_WRITE_BYTE: u64 = 4;
/// `storage_remove`: base plus `STORAGE_REMOVE_BYTE` per key byte.
pub const HOST_STORAGE_REMOVE: u64 = 300;
pub const STORAGE_REMOVE_BYTE: u64 = 1;
/// `return_data`: base plus `RETURN_BYTE` per byte returned.
pub const HOST_RETURN_DATA: u64 = 10;
pub const RETURN_BYTE: u64 = 1;
/// `abort`: base plus `ABORT_BYTE` per message byte.
pub const HOST_ABORT: u64 = 10;
pub const ABORT_BYTE: u64 = 1;
/// `table_insert`: base plus `TABLE_WRITE_BYTE` per (table_len + row_len).
pub const HOST_TABLE_INSERT: u64 = 600;
/// `table_update`: base plus `TABLE_WRITE_BYTE` per (table_len + row_len).
pub const HOST_TABLE_UPDATE: u64 = 500;
pub const TABLE_WRITE_BYTE: u64 = 4;
/// `table_get`: base plus `TABLE_READ_BYTE` per table-name byte up front, plus the same per row
/// byte actually copied (min(row_len, out_cap)), charged before the copy.
pub const HOST_TABLE_GET: u64 = 250;
/// `table_delete`: base plus `TABLE_READ_BYTE` per table-name byte.
pub const HOST_TABLE_DELETE: u64 = 300;
/// `table_scan`: base plus `TABLE_READ_BYTE` per table-name byte up front, plus `TABLE_SCAN_ID`
/// per id returned (at most out_cap/8), charged before the write.
pub const HOST_TABLE_SCAN: u64 = 250;
pub const TABLE_READ_BYTE: u64 = 1;
pub const TABLE_SCAN_ID: u64 = 10;
/// `table_index_scan`: base plus `TABLE_READ_BYTE` per request byte up front, plus
/// `TABLE_INDEX_SCAN_ID` per id returned and `TABLE_READ_BYTE` per cursor byte returned, charged
/// before the (optional) copy. Each id costs an index entry read plus a row existence check.
pub const HOST_TABLE_INDEX_SCAN: u64 = 400;
pub const TABLE_INDEX_SCAN_ID: u64 = 40;

// ---- User tokens (native multi-token module) ----------------------------------------------------
//
// Charged before the call runs, like the calls above. These are separate from native BTB
// (`HOST_TOKEN_BALANCE` / `HOST_TOKEN_TRANSFER`).

/// `token_create`: base plus `TOKEN_BYTE` per request byte (writes the info record, the creator's
/// balance and the creator's nonce).
pub const HOST_TOKEN_CREATE: u64 = 2000;
/// `token_transfer_of`: flat (info read, two balance reads, up to two balance writes).
pub const HOST_TOKEN_TRANSFER_OF: u64 = 400;
/// `token_balance_of`: flat (info read and balance read).
pub const HOST_TOKEN_BALANCE_OF: u64 = 100;
/// `token_info`: base plus `TOKEN_BYTE` per info byte actually copied, charged before the copy.
pub const HOST_TOKEN_INFO: u64 = 150;
/// `token_holders`: base plus `TOKEN_BYTE` per request byte up front, plus `TOKEN_HOLDER_ENTRY`
/// per holder returned and `TOKEN_BYTE` per cursor byte returned, charged before the optional copy.
pub const HOST_TOKEN_HOLDERS: u64 = 300;
pub const TOKEN_HOLDER_ENTRY: u64 = 30;
/// `token_mint`: flat (info read and write, balance read and write).
pub const HOST_TOKEN_MINT: u64 = 500;
/// `token_burn`: flat (info read and write, balance read and write).
pub const HOST_TOKEN_BURN: u64 = 400;
pub const TOKEN_BYTE: u64 = 1;

// ---- Oracles (randomness beacon and price feeds) ------------------------------------------------

/// `random_bytes(out_ptr, len)`: base plus `RANDOM_BYTE` per byte requested (one SHA-256 per 32
/// bytes of output). Charged before the call runs, whether or not the call is then refused.
pub const HOST_RANDOM_BYTES: u64 = 300;
pub const RANDOM_BYTE: u64 = 2;
/// `price(pair, out_ptr)` in a query: flat (a state read). Queries are not billed, so a price
/// read there is free to the caller.
pub const HOST_PRICE_QUERY: u64 = 200;
/// `price(pair, out_ptr)` in a mutation or cron: the same read plus a per-read fee, in gas, that
/// is paid like all gas to the validator pool.
pub const PRICE_READ_FEE: u64 = 10_000;
pub const HOST_PRICE_MUTATION: u64 = HOST_PRICE_QUERY + PRICE_READ_FEE;

/// `price_twap(pair, window, out_ptr)`: two checkpoint reads plus arithmetic; in a query it is
/// free to the caller, in a mutation or cron it adds a per-read fee (see `docs/ORACLES.md`).
pub const HOST_TWAP_QUERY: u64 = 500;
pub const TWAP_READ_FEE: u64 = 20_000;
pub const HOST_TWAP_MUTATION: u64 = HOST_TWAP_QUERY + TWAP_READ_FEE;

/// `report_request(data_ptr, len)`: base plus `REPORT_BYTE` per byte. The committee signs every
/// report, which costs the validators a pairing-free partial signature each and a block's worth
/// of gossip, so it is priced well above a storage write.
pub const HOST_REPORT_REQUEST: u64 = 5_000;
pub const REPORT_BYTE: u64 = 10;

// ---- Cross-contract calls -------------------------------------------------------------------

/// `call_contract(req_ptr, req_len, out_ptr, out_cap)`: base plus `CALL_BYTE` per request byte,
/// charged first. It covers loading the callee's code and a fresh VM memory image. On top of it
/// the caller pays, exactly, the gas the callee used (at most the forwarded gas), then
/// `CALL_RETURN_BYTE` per output byte copied back, charged before the copy. Gas the callee did
/// not use stays with the caller. A refused call (trap) costs the base only.
pub const HOST_CALL_CONTRACT: u64 = 3_000;
pub const CALL_BYTE: u64 = 2;
pub const CALL_RETURN_BYTE: u64 = 1;

// ---- File storage ---------------------------------------------------------------------------

/// `file_info(root_ptr, out_ptr)`: two state reads (the file record and its expiry marker). In a
/// query it is free to the caller like every read there.
pub const HOST_FILE_INFO: u64 = 400;
/// `file_pin(req_ptr, req_len)`: the placement work of a `Store` transaction (`STORE_GAS` in
/// `btb-chain`), charged first, also when the pin is refused. The escrow is paid from the
/// contract's balance and is not gas.
pub const HOST_FILE_PIN: u64 = 30_000;
