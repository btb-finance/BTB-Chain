//! RV64IM instruction decoder.

/// Reasons execution stops abnormally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    IllegalInstruction,
    MisalignedFetch,
    MemoryOutOfBounds { addr: u64 },
    OutOfGas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Sub,
    Sll,
    Slt,
    Sltu,
    Xor,
    Srl,
    Sra,
    Or,
    And,
    Mul,
    Mulh,
    Mulhsu,
    Mulhu,
    Div,
    Divu,
    Rem,
    Remu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchOp {
    Beq,
    Bne,
    Blt,
    Bge,
    Bltu,
    Bgeu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOp {
    Lb,
    Lh,
    Lw,
    Ld,
    Lbu,
    Lhu,
    Lwu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    Sb,
    Sh,
    Sw,
    Sd,
}

/// A decoded instruction. Immediates are sign-extended.
/// `OpW`/`OpImmW` are the 32-bit (*W) forms whose result is sign-extended to 64 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instr {
    Lui {
        rd: u8,
        imm: i64,
    },
    Auipc {
        rd: u8,
        imm: i64,
    },
    Jal {
        rd: u8,
        imm: i64,
    },
    Jalr {
        rd: u8,
        rs1: u8,
        imm: i64,
    },
    Branch {
        op: BranchOp,
        rs1: u8,
        rs2: u8,
        imm: i64,
    },
    Load {
        op: LoadOp,
        rd: u8,
        rs1: u8,
        imm: i64,
    },
    Store {
        op: StoreOp,
        rs1: u8,
        rs2: u8,
        imm: i64,
    },
    OpImm {
        op: AluOp,
        rd: u8,
        rs1: u8,
        imm: i64,
    },
    OpImmW {
        op: AluOp,
        rd: u8,
        rs1: u8,
        imm: i64,
    },
    Op {
        op: AluOp,
        rd: u8,
        rs1: u8,
        rs2: u8,
    },
    OpW {
        op: AluOp,
        rd: u8,
        rs1: u8,
        rs2: u8,
    },
    Fence,
    Ecall,
    Ebreak,
}

fn sext(v: u32, bits: u32) -> i64 {
    let s = 64 - bits;
    ((v as u64) << s) as i64 >> s
}

/// Decode one 32-bit instruction word.
pub fn decode(w: u32) -> Result<Instr, Trap> {
    use AluOp::*;
    let ill = Err(Trap::IllegalInstruction);
    if w & 3 != 3 {
        return ill; // compressed not supported
    }
    let rd = ((w >> 7) & 31) as u8;
    let rs1 = ((w >> 15) & 31) as u8;
    let rs2 = ((w >> 20) & 31) as u8;
    let f3 = (w >> 12) & 7;
    let f7 = w >> 25;
    let imm_i = sext(w >> 20, 12);
    match w & 0x7f {
        0x37 => Ok(Instr::Lui {
            rd,
            imm: (w & 0xffff_f000) as i32 as i64,
        }),
        0x17 => Ok(Instr::Auipc {
            rd,
            imm: (w & 0xffff_f000) as i32 as i64,
        }),
        0x6f => {
            let v = ((w >> 31) << 20)
                | (((w >> 12) & 0xff) << 12)
                | (((w >> 20) & 1) << 11)
                | (((w >> 21) & 0x3ff) << 1);
            Ok(Instr::Jal {
                rd,
                imm: sext(v, 21),
            })
        }
        0x67 if f3 == 0 => Ok(Instr::Jalr {
            rd,
            rs1,
            imm: imm_i,
        }),
        0x63 => {
            let op = match f3 {
                0 => BranchOp::Beq,
                1 => BranchOp::Bne,
                4 => BranchOp::Blt,
                5 => BranchOp::Bge,
                6 => BranchOp::Bltu,
                7 => BranchOp::Bgeu,
                _ => return ill,
            };
            let v = ((w >> 31) << 12)
                | (((w >> 7) & 1) << 11)
                | (((w >> 25) & 0x3f) << 5)
                | (((w >> 8) & 0xf) << 1);
            Ok(Instr::Branch {
                op,
                rs1,
                rs2,
                imm: sext(v, 13),
            })
        }
        0x03 => {
            let op = match f3 {
                0 => LoadOp::Lb,
                1 => LoadOp::Lh,
                2 => LoadOp::Lw,
                3 => LoadOp::Ld,
                4 => LoadOp::Lbu,
                5 => LoadOp::Lhu,
                6 => LoadOp::Lwu,
                _ => return ill,
            };
            Ok(Instr::Load {
                op,
                rd,
                rs1,
                imm: imm_i,
            })
        }
        0x23 => {
            let op = match f3 {
                0 => StoreOp::Sb,
                1 => StoreOp::Sh,
                2 => StoreOp::Sw,
                3 => StoreOp::Sd,
                _ => return ill,
            };
            let v = ((w >> 25) << 5) | ((w >> 7) & 0x1f);
            Ok(Instr::Store {
                op,
                rs1,
                rs2,
                imm: sext(v, 12),
            })
        }
        0x13 => {
            let hi6 = w >> 26;
            let shamt = ((w >> 20) & 0x3f) as i64;
            let (op, imm) = match f3 {
                0 => (Add, imm_i),
                2 => (Slt, imm_i),
                3 => (Sltu, imm_i),
                4 => (Xor, imm_i),
                6 => (Or, imm_i),
                7 => (And, imm_i),
                1 if hi6 == 0 => (Sll, shamt),
                5 if hi6 == 0 => (Srl, shamt),
                5 if hi6 == 0x10 => (Sra, shamt),
                _ => return ill,
            };
            Ok(Instr::OpImm { op, rd, rs1, imm })
        }
        0x1b => {
            let shamt = rs2 as i64;
            let (op, imm) = match (f3, f7) {
                (0, _) => (Add, imm_i),
                (1, 0) => (Sll, shamt),
                (5, 0) => (Srl, shamt),
                (5, 0x20) => (Sra, shamt),
                _ => return ill,
            };
            Ok(Instr::OpImmW { op, rd, rs1, imm })
        }
        0x33 => {
            let op = match (f7, f3) {
                (0, 0) => Add,
                (0x20, 0) => Sub,
                (0, 1) => Sll,
                (0, 2) => Slt,
                (0, 3) => Sltu,
                (0, 4) => Xor,
                (0, 5) => Srl,
                (0x20, 5) => Sra,
                (0, 6) => Or,
                (0, 7) => And,
                (1, 0) => Mul,
                (1, 1) => Mulh,
                (1, 2) => Mulhsu,
                (1, 3) => Mulhu,
                (1, 4) => Div,
                (1, 5) => Divu,
                (1, 6) => Rem,
                (1, 7) => Remu,
                _ => return ill,
            };
            Ok(Instr::Op { op, rd, rs1, rs2 })
        }
        0x3b => {
            let op = match (f7, f3) {
                (0, 0) => Add,
                (0x20, 0) => Sub,
                (0, 1) => Sll,
                (0, 5) => Srl,
                (0x20, 5) => Sra,
                (1, 0) => Mul,
                (1, 4) => Div,
                (1, 5) => Divu,
                (1, 6) => Rem,
                (1, 7) => Remu,
                _ => return ill,
            };
            Ok(Instr::OpW { op, rd, rs1, rs2 })
        }
        0x0f if f3 == 0 => Ok(Instr::Fence),
        0x73 => match w {
            0x0000_0073 => Ok(Instr::Ecall),
            0x0010_0073 => Ok(Instr::Ebreak),
            _ => ill, // CSRs and everything else
        },
        _ => ill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_basics() {
        assert_eq!(decode(0x0000_0073), Ok(Instr::Ecall));
        assert_eq!(decode(0x0010_0073), Ok(Instr::Ebreak));
        assert_eq!(decode(0x0ff0_000f), Ok(Instr::Fence));
        // addi x1, x0, -1
        assert_eq!(
            decode(0xfff0_0093),
            Ok(Instr::OpImm {
                op: AluOp::Add,
                rd: 1,
                rs1: 0,
                imm: -1
            })
        );
    }

    #[test]
    fn rejects_illegal() {
        assert_eq!(decode(0), Err(Trap::IllegalInstruction));
        assert_eq!(decode(0xffff_ffff), Err(Trap::IllegalInstruction));
        // compressed encoding (low bits != 11)
        assert_eq!(decode(0x0000_4501), Err(Trap::IllegalInstruction));
        // csrrw x1, mstatus, x2
        assert_eq!(decode(0x3001_10f3), Err(Trap::IllegalInstruction));
        // fence.i
        assert_eq!(decode(0x0000_100f), Err(Trap::IllegalInstruction));
        // float load flw
        assert_eq!(decode(0x0000_2007), Err(Trap::IllegalInstruction));
        // slli with bad upper bits
        assert_eq!(decode(0x4000_9093), Err(Trap::IllegalInstruction));
        // add with bad funct7
        assert_eq!(decode(0x4200_80b3), Err(Trap::IllegalInstruction));
    }
}
