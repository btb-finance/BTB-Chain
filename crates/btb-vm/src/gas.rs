//! Per-instruction gas costs. Charged before an instruction executes.

use crate::decode::{AluOp, Instr};

/// Simple ALU ops, LUI, AUIPC, FENCE, EBREAK, ECALL (host-call costs are charged by the host).
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
