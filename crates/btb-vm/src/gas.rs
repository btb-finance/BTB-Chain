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
