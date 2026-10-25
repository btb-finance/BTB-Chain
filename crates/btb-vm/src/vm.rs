//! The RV64IM interpreter.

use crate::decode::{decode, AluOp, BranchOp, Instr, LoadOp, StoreOp, Trap};
use crate::gas;

/// Why `run` returned without a trap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// ECALL executed; the host reads the call number from a7 (x17) and arguments from a0..a6,
    /// then calls `run` again to resume after the ECALL.
    Ecall,
    /// EBREAK executed; the program halted normally.
    Halt,
}

/// A deterministic RV64IM machine with flat little-endian memory starting at address 0.
///
/// Misaligned data accesses are allowed (little-endian, byte-exact). Instruction fetches must be
/// 4-byte aligned. Gas is charged before execution; if an instruction then traps (e.g. out of
/// bounds access), its gas stays charged but no register, memory or pc change is made.
#[derive(Debug, Clone)]
pub struct Vm {
    regs: [u64; 32],
    pc: u64,
    mem: Vec<u8>,
    gas_limit: u64,
    gas_used: u64,
}

impl Vm {
    pub fn new(mem_size: usize, gas_limit: u64) -> Self {
        Vm {
            regs: [0; 32],
            pc: 0,
            mem: vec![0; mem_size],
            gas_limit,
            gas_used: 0,
        }
    }

    pub fn reg(&self, i: usize) -> u64 {
        self.regs.get(i).copied().unwrap_or(0)
    }
    pub fn set_reg(&mut self, i: usize, v: u64) {
        if i != 0 && i < 32 {
            self.regs[i] = v;
        }
    }
    pub fn pc(&self) -> u64 {
        self.pc
    }
    pub fn set_pc(&mut self, pc: u64) {
        self.pc = pc;
    }
    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }
    pub fn gas_limit(&self) -> u64 {
        self.gas_limit
    }
    pub fn memory(&self) -> &[u8] {
        &self.mem
    }

    fn range(&self, addr: u64, len: u64) -> Result<usize, Trap> {
        match addr.checked_add(len) {
            Some(end) if end <= self.mem.len() as u64 => Ok(addr as usize),
            _ => Err(Trap::MemoryOutOfBounds { addr }),
        }
    }

    pub fn read_mem(&self, addr: u64, len: usize) -> Result<&[u8], Trap> {
        let a = self.range(addr, len as u64)?;
        Ok(&self.mem[a..a + len])
    }

    pub fn write_mem(&mut self, addr: u64, data: &[u8]) -> Result<(), Trap> {
        let a = self.range(addr, data.len() as u64)?;
        self.mem[a..a + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn load(&self, addr: u64, n: usize) -> Result<u64, Trap> {
        let b = self.read_mem(addr, n)?;
        let mut v = [0u8; 8];
        v[..n].copy_from_slice(b);
        Ok(u64::from_le_bytes(v))
    }

    fn wr(&mut self, rd: u8, v: u64) {
        if rd != 0 {
            self.regs[(rd & 31) as usize] = v;
        }
    }

    fn jump_target(t: u64) -> Result<u64, Trap> {
        if t & 3 != 0 {
            Err(Trap::MisalignedFetch)
        } else {
            Ok(t)
        }
    }

    /// Execute one instruction. Returns `Some(exit)` for ECALL/EBREAK.
    pub fn step(&mut self) -> Result<Option<Exit>, Trap> {
        if self.pc & 3 != 0 {
            return Err(Trap::MisalignedFetch);
        }
        let w = u32::from_le_bytes(
            self.load(self.pc, 4)?.to_le_bytes()[..4]
                .try_into()
                .unwrap(),
        );
        let instr = decode(w)?;
        let cost = gas::cost(&instr);
        match self.gas_used.checked_add(cost) {
            Some(g) if g <= self.gas_limit => self.gas_used = g,
            _ => return Err(Trap::OutOfGas),
        }
        let pc = self.pc;
        let next = pc.wrapping_add(4);
        let r = |s: &Self, i: u8| s.regs[(i & 31) as usize];
        let mut new_pc = next;
        match instr {
            Instr::Lui { rd, imm } => self.wr(rd, imm as u64),
            Instr::Auipc { rd, imm } => self.wr(rd, pc.wrapping_add(imm as u64)),
            Instr::Jal { rd, imm } => {
                new_pc = Self::jump_target(pc.wrapping_add(imm as u64))?;
                self.wr(rd, next);
            }
            Instr::Jalr { rd, rs1, imm } => {
                new_pc = Self::jump_target(r(self, rs1).wrapping_add(imm as u64) & !1)?;
                self.wr(rd, next);
            }
            Instr::Branch { op, rs1, rs2, imm } => {
                let (a, b) = (r(self, rs1), r(self, rs2));
                let taken = match op {
                    BranchOp::Beq => a == b,
                    BranchOp::Bne => a != b,
                    BranchOp::Blt => (a as i64) < (b as i64),
                    BranchOp::Bge => (a as i64) >= (b as i64),
                    BranchOp::Bltu => a < b,
                    BranchOp::Bgeu => a >= b,
                };
                if taken {
                    new_pc = Self::jump_target(pc.wrapping_add(imm as u64))?;
                }
            }
            Instr::Load { op, rd, rs1, imm } => {
                let addr = r(self, rs1).wrapping_add(imm as u64);
                let v = match op {
                    LoadOp::Lb => self.load(addr, 1)? as u8 as i8 as u64,
                    LoadOp::Lh => self.load(addr, 2)? as u16 as i16 as u64,
                    LoadOp::Lw => self.load(addr, 4)? as u32 as i32 as u64,
                    LoadOp::Ld => self.load(addr, 8)?,
                    LoadOp::Lbu => self.load(addr, 1)?,
                    LoadOp::Lhu => self.load(addr, 2)?,
                    LoadOp::Lwu => self.load(addr, 4)?,
                };
                self.wr(rd, v);
            }
            Instr::Store { op, rs1, rs2, imm } => {
                let addr = r(self, rs1).wrapping_add(imm as u64);
                let v = r(self, rs2).to_le_bytes();
                let n = match op {
                    StoreOp::Sb => 1,
                    StoreOp::Sh => 2,
                    StoreOp::Sw => 4,
                    StoreOp::Sd => 8,
                };
                self.write_mem(addr, &v[..n])?;
            }
            Instr::OpImm { op, rd, rs1, imm } => self.wr(rd, alu(op, r(self, rs1), imm as u64)),
            Instr::OpImmW { op, rd, rs1, imm } => self.wr(rd, alu_w(op, r(self, rs1), imm as u64)),
            Instr::Op { op, rd, rs1, rs2 } => self.wr(rd, alu(op, r(self, rs1), r(self, rs2))),
            Instr::OpW { op, rd, rs1, rs2 } => self.wr(rd, alu_w(op, r(self, rs1), r(self, rs2))),
            Instr::Fence => {}
            Instr::Ecall => {
                self.pc = next;
                return Ok(Some(Exit::Ecall));
            }
            Instr::Ebreak => {
                self.pc = next;
                return Ok(Some(Exit::Halt));
            }
        }
        self.pc = new_pc;
        Ok(None)
    }

    /// Run until ECALL, EBREAK or a trap.
    pub fn run(&mut self) -> Result<Exit, Trap> {
        loop {
            if let Some(e) = self.step()? {
                return Ok(e);
            }
        }
    }
}

fn alu(op: AluOp, a: u64, b: u64) -> u64 {
    let (sa, sb) = (a as i64, b as i64);
    match op {
        AluOp::Add => a.wrapping_add(b),
        AluOp::Sub => a.wrapping_sub(b),
        AluOp::Sll => a << (b & 63),
        AluOp::Slt => (sa < sb) as u64,
        AluOp::Sltu => (a < b) as u64,
        AluOp::Xor => a ^ b,
        AluOp::Srl => a >> (b & 63),
        AluOp::Sra => (sa >> (b & 63)) as u64,
        AluOp::Or => a | b,
        AluOp::And => a & b,
        AluOp::Mul => a.wrapping_mul(b),
        AluOp::Mulh => ((sa as i128).wrapping_mul(sb as i128) >> 64) as u64,
        AluOp::Mulhsu => ((sa as i128).wrapping_mul(b as i128) >> 64) as u64,
        AluOp::Mulhu => ((a as u128 * b as u128) >> 64) as u64,
        AluOp::Div => {
            if b == 0 {
                u64::MAX
            } else {
                sa.wrapping_div(sb) as u64
            }
        }
        AluOp::Divu => a.checked_div(b).unwrap_or(u64::MAX),
        AluOp::Rem => {
            if b == 0 {
                a
            } else {
                sa.wrapping_rem(sb) as u64
            }
        }
        AluOp::Remu => a.checked_rem(b).unwrap_or(a),
    }
}

/// 32-bit operation on the low halves; result sign-extended to 64 bits.
fn alu_w(op: AluOp, a: u64, b: u64) -> u64 {
    let (ua, ub) = (a as u32, b as u32);
    let (sa, sb) = (ua as i32, ub as i32);
    let r: u32 = match op {
        AluOp::Add => ua.wrapping_add(ub),
        AluOp::Sub => ua.wrapping_sub(ub),
        AluOp::Sll => ua << (ub & 31),
        AluOp::Srl => ua >> (ub & 31),
        AluOp::Sra => (sa >> (ub & 31)) as u32,
        AluOp::Mul => ua.wrapping_mul(ub),
        AluOp::Div => {
            if sb == 0 {
                u32::MAX
            } else {
                sa.wrapping_div(sb) as u32
            }
        }
        AluOp::Divu => ua.checked_div(ub).unwrap_or(u32::MAX),
        AluOp::Rem => {
            if sb == 0 {
                ua
            } else {
                sa.wrapping_rem(sb) as u32
            }
        }
        AluOp::Remu => ua.checked_rem(ub).unwrap_or(ua),
        // Not encodable as *W instructions; the decoder never produces them.
        _ => 0,
    };
    r as i32 as i64 as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- tiny assembler ---
    fn r(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
        (f7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
    }
    fn i(imm: i32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
        (((imm as u32) & 0xfff) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
    }
    fn s(imm: i32, rs2: u32, rs1: u32, f3: u32) -> u32 {
        let v = imm as u32;
        (((v >> 5) & 0x7f) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (f3 << 12)
            | ((v & 0x1f) << 7)
            | 0x23
    }
    fn b(imm: i32, rs2: u32, rs1: u32, f3: u32) -> u32 {
        let v = imm as u32;
        (((v >> 12) & 1) << 31)
            | (((v >> 5) & 0x3f) << 25)
            | (rs2 << 20)
            | (rs1 << 15)
            | (f3 << 12)
            | (((v >> 1) & 0xf) << 8)
            | (((v >> 11) & 1) << 7)
            | 0x63
    }
    fn jal(imm: i32, rd: u32) -> u32 {
        let v = imm as u32;
        (((v >> 20) & 1) << 31)
            | (((v >> 1) & 0x3ff) << 21)
            | (((v >> 11) & 1) << 20)
            | (((v >> 12) & 0xff) << 12)
            | (rd << 7)
            | 0x6f
    }
    fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 0, rd, 0x13)
    }
    fn lui(rd: u32, imm20: u32) -> u32 {
        (imm20 << 12) | (rd << 7) | 0x37
    }
    const EBREAK: u32 = 0x0010_0073;

    fn vm_with(prog: &[u32], gas: u64) -> Vm {
        let mut vm = Vm::new(4096, gas);
        let bytes: Vec<u8> = prog.iter().flat_map(|w| w.to_le_bytes()).collect();
        vm.write_mem(0, &bytes).unwrap();
        vm
    }
    fn run(prog: &[u32]) -> Vm {
        let mut p = prog.to_vec();
        p.push(EBREAK);
        let mut vm = vm_with(&p, 1_000_000);
        assert_eq!(vm.run(), Ok(Exit::Halt));
        vm
    }
    /// Run `op x3, x1, x2` with given inputs (op is a prebuilt R-type with rd=3, rs1=1, rs2=2).
    fn bin(inst: u32, a: u64, bv: u64) -> u64 {
        let mut vm = vm_with(&[inst, EBREAK], 100);
        vm.set_reg(1, a);
        vm.set_reg(2, bv);
        assert_eq!(vm.run(), Ok(Exit::Halt));
        vm.reg(3)
    }

    #[test]
    fn alu_immediates() {
        let vm = run(&[
            addi(1, 0, -5),
            i(7, 1, 2, 2, 0x13),    // slti x2, x1, 7 -> 1
            i(-6, 1, 3, 3, 0x13),   // sltiu x3, x1, -6: (2^64-5) < (2^64-6) -> 0
            i(0xff, 1, 4, 4, 0x13), // xori x4, x1, 0xff
            i(0xf0, 1, 6, 5, 0x13), // ori x5
            i(0xf0, 1, 7, 6, 0x13), // andi x6
            addi(7, 0, 1),
            i(63, 7, 1, 7, 0x13), // slli x7, x7, 63
        ]);
        assert_eq!(vm.reg(1) as i64, -5);
        assert_eq!(vm.reg(2), 1);
        assert_eq!(vm.reg(3), 0);
        assert_eq!(vm.reg(4), (-5i64 as u64) ^ 0xff);
        assert_eq!(vm.reg(5), (-5i64 as u64) | 0xf0);
        assert_eq!(vm.reg(6), 0xf0);
        assert_eq!(vm.reg(7), 1 << 63);
    }

    #[test]
    fn alu_register_ops() {
        let add = r(0, 2, 1, 0, 3, 0x33);
        assert_eq!(bin(add, u64::MAX, 2), 1);
        assert_eq!(bin(r(0x20, 2, 1, 0, 3, 0x33), 0, 1), u64::MAX);
        assert_eq!(bin(r(0, 2, 1, 1, 3, 0x33), 1, 65), 2); // sll uses low 6 bits
        assert_eq!(bin(r(0, 2, 1, 2, 3, 0x33), u64::MAX, 0), 1); // slt
        assert_eq!(bin(r(0, 2, 1, 3, 3, 0x33), u64::MAX, 0), 0); // sltu
        assert_eq!(bin(r(0, 2, 1, 4, 3, 0x33), 0b1100, 0b1010), 0b0110);
        assert_eq!(bin(r(0, 2, 1, 5, 3, 0x33), 1 << 63, 63), 1);
        assert_eq!(bin(r(0x20, 2, 1, 5, 3, 0x33), 1 << 63, 63), u64::MAX);
        assert_eq!(bin(r(0, 2, 1, 6, 3, 0x33), 0b1100, 0b1010), 0b1110);
        assert_eq!(bin(r(0, 2, 1, 7, 3, 0x33), 0b1100, 0b1010), 0b1000);
    }

    #[test]
    fn mul_group() {
        let m = |f3| r(1, 2, 1, f3, 3, 0x33);
        assert_eq!(bin(m(0), 1 << 63, 2), 0);
        assert_eq!(bin(m(1), u64::MAX, u64::MAX), 0); // (-1)*(-1)=1, high 0
        assert_eq!(bin(m(1), 1 << 63, 2), u64::MAX); // MIN*2 high = -1
        assert_eq!(bin(m(2), u64::MAX, u64::MAX), u64::MAX); // -1 * (2^64-1) high = -1
        assert_eq!(bin(m(3), u64::MAX, u64::MAX), u64::MAX - 1);
        // mulw sign-extends
        assert_eq!(
            bin(r(1, 2, 1, 0, 3, 0x3b), 0x4000_0000, 2),
            0xffff_ffff_8000_0000
        );
    }

    #[test]
    fn div_edge_cases() {
        let d = |f3| r(1, 2, 1, f3, 3, 0x33);
        let min = i64::MIN as u64;
        assert_eq!(bin(d(4), 7, 0), u64::MAX); // div x/0
        assert_eq!(bin(d(5), 7, 0), u64::MAX); // divu
        assert_eq!(bin(d(6), 7, 0), 7); // rem x/0
        assert_eq!(bin(d(7), 7, 0), 7); // remu
        assert_eq!(bin(d(4), min, u64::MAX), min); // MIN / -1
        assert_eq!(bin(d(6), min, u64::MAX), 0); // MIN % -1
        assert_eq!(bin(d(4), -7i64 as u64, 2), -3i64 as u64);
        assert_eq!(bin(d(6), -7i64 as u64, 2), -1i64 as u64);
        let w = |f3| r(1, 2, 1, f3, 3, 0x3b);
        let min32 = 0x8000_0000u64;
        assert_eq!(bin(w(4), 7, 0), u64::MAX);
        assert_eq!(bin(w(5), 7, 0), u64::MAX); // divuw x/0 = 2^32-1 sign-extended
        assert_eq!(bin(w(6), 0x1_8000_0000, 0), 0xffff_ffff_8000_0000); // remw x/0 = sext(low32)
        assert_eq!(bin(w(7), 5, 0), 5);
        assert_eq!(bin(w(4), min32, 0xffff_ffff), 0xffff_ffff_8000_0000); // MIN32 / -1
        assert_eq!(bin(w(6), min32, 0xffff_ffff), 0);
    }

    #[test]
    fn w_ops_sign_extend() {
        // addw 0x7fffffff + 1 = 0xffffffff80000000
        assert_eq!(
            bin(r(0, 2, 1, 0, 3, 0x3b), 0x7fff_ffff, 1),
            0xffff_ffff_8000_0000
        );
        assert_eq!(bin(r(0x20, 2, 1, 0, 3, 0x3b), 0, 1), u64::MAX); // subw
        assert_eq!(bin(r(0, 2, 1, 1, 3, 0x3b), 1, 31), 0xffff_ffff_8000_0000); // sllw
        assert_eq!(bin(r(0, 2, 1, 5, 3, 0x3b), 0xffff_ffff, 4), 0x0fff_ffff); // srlw
        assert_eq!(
            bin(r(0x20, 2, 1, 5, 3, 0x3b), 0x8000_0000, 4),
            0xffff_ffff_f800_0000
        ); // sraw
           // addiw x3, x1, 1
        assert_eq!(
            bin(i(1, 1, 0, 3, 0x1b), 0x7fff_ffff, 0),
            0xffff_ffff_8000_0000
        );
        // slliw x3, x1, 1
        assert_eq!(
            bin(i(1, 1, 1, 3, 0x1b), 0x4000_0000, 0),
            0xffff_ffff_8000_0000
        );
        // sraiw x3, x1, 4
        assert_eq!(
            bin(i(4 | 0x400, 1, 5, 3, 0x1b), 0x8000_0000, 0),
            0xffff_ffff_f800_0000
        );
    }

    #[test]
    fn shifts_immediate_and_lui_auipc() {
        let vm = run(&[
            lui(1, 0xfffff),             // x1 = 0xffffffff_fffff000
            i(0x400 | 4, 1, 5, 2, 0x13), // srai x2, x1, 4
            i(4, 1, 5, 3, 0x13),         // srli x3, x1, 4
            (1 << 12) | (4 << 7) | 0x17, // auipc x4, 1 at pc=12
        ]);
        assert_eq!(vm.reg(1), 0xffff_ffff_ffff_f000);
        assert_eq!(vm.reg(2), 0xffff_ffff_ffff_ff00);
        assert_eq!(vm.reg(3), 0x0fff_ffff_ffff_ff00);
        assert_eq!(vm.reg(4), 12 + 0x1000);
    }

    #[test]
    fn x0_writes_ignored() {
        let vm = run(&[addi(0, 0, 123), lui(0, 5), r(0, 0, 0, 0, 0, 0x33)]);
        assert_eq!(vm.reg(0), 0);
        let mut vm = Vm::new(16, 1);
        vm.set_reg(0, 9);
        assert_eq!(vm.reg(0), 0);
    }

    #[test]
    fn loads_and_stores() {
        let vm = run(&[
            addi(1, 0, -2),        // x1 = -2
            addi(5, 0, 256),       // base
            s(0, 1, 5, 3),         // sd x1, 0(x5)
            i(0, 5, 0, 2, 0x03),   // lb
            i(0, 5, 4, 3, 0x03),   // lbu
            i(0, 5, 1, 4, 0x03),   // lh
            i(0, 5, 5, 6, 0x03),   // lhu
            i(0, 5, 2, 7, 0x03),   // lw
            i(0, 5, 6, 8, 0x03),   // lwu
            i(0, 5, 3, 9, 0x03),   // ld
            s(8, 1, 5, 0),         // sb x1, 8(x5)
            s(10, 1, 5, 1),        // sh x1, 10(x5)
            s(12, 1, 5, 2),        // sw x1, 12(x5)
            i(8, 5, 3, 10, 0x03),  // ld x10, 8(x5)
            i(12, 5, 3, 11, 0x03), // ld x11, 12(x5)
        ]);
        assert_eq!(vm.reg(2), u64::MAX - 1);
        assert_eq!(vm.reg(3), 0xfe);
        assert_eq!(vm.reg(4), u64::MAX - 1);
        assert_eq!(vm.reg(6), 0xfffe);
        assert_eq!(vm.reg(7), u64::MAX - 1);
        assert_eq!(vm.reg(8), 0xffff_fffe);
        assert_eq!(vm.reg(9), u64::MAX - 1);
        assert_eq!(vm.reg(10), 0xffff_fffe_fffe_00fe);
        assert_eq!(vm.reg(11), 0xffff_fffe);
    }

    #[test]
    fn memory_out_of_bounds() {
        let mut vm = vm_with(&[i(0, 1, 3, 2, 0x03), EBREAK], 100); // ld x2, 0(x1)
        vm.set_reg(1, 4090); // 8 bytes from 4090 overflows 4096
        assert_eq!(vm.run(), Err(Trap::MemoryOutOfBounds { addr: 4090 }));
        assert_eq!(vm.reg(2), 0);
        assert_eq!(vm.pc(), 0);
        let mut vm = vm_with(&[s(0, 2, 1, 0), EBREAK], 100);
        vm.set_reg(1, u64::MAX); // wraps / overflows
        assert_eq!(vm.run(), Err(Trap::MemoryOutOfBounds { addr: u64::MAX }));
        // last valid byte works
        let mut vm = vm_with(&[s(0, 2, 1, 0), EBREAK], 100);
        vm.set_reg(1, 4095);
        assert_eq!(vm.run(), Ok(Exit::Halt));
        // fetch out of bounds
        let mut vm = Vm::new(64, 10);
        vm.set_pc(64);
        assert_eq!(vm.run(), Err(Trap::MemoryOutOfBounds { addr: 64 }));
    }

    #[test]
    fn illegal_and_misaligned() {
        let mut vm = vm_with(&[0, EBREAK], 100);
        assert_eq!(vm.run(), Err(Trap::IllegalInstruction));
        assert_eq!(vm.gas_used(), 0);
        let mut vm = vm_with(&[EBREAK], 100);
        vm.set_pc(2);
        assert_eq!(vm.run(), Err(Trap::MisalignedFetch));
        // jal to pc+2
        let mut vm = vm_with(&[jal(2, 1), EBREAK], 100);
        assert_eq!(vm.run(), Err(Trap::MisalignedFetch));
        assert_eq!(vm.reg(1), 0);
    }

    #[test]
    fn jumps_and_branches() {
        let vm = run(&[
            jal(8, 1),            // 0: x1 = 4, jump to 8
            addi(2, 0, 99),       // 4: skipped
            addi(3, 0, 1),        // 8
            i(20, 0, 0, 4, 0x67), // 12: jalr x4, 20(x0) -> x4=16, pc=20
            addi(5, 0, 99),       // 16: skipped
            addi(6, 0, 1),        // 20
            addi(7, 0, -1),       // 24
            b(8, 0, 7, 4),        // 28: blt x7, x0 -> taken to 36
            addi(8, 0, 99),       // 32: skipped
            addi(9, 0, 1),        // 36
            b(8, 0, 7, 6),        // 40: bltu x7, x0 -> not taken
            addi(10, 0, 1),       // 44
        ]);
        assert_eq!((vm.reg(1), vm.reg(4)), (4, 16));
        assert_eq!((vm.reg(2), vm.reg(5), vm.reg(8)), (0, 0, 0));
        assert_eq!((vm.reg(3), vm.reg(6), vm.reg(9), vm.reg(10)), (1, 1, 1, 1));
    }

    #[test]
    fn ecall_resume_and_fence() {
        let mut vm = vm_with(
            &[
                addi(17, 0, 7),
                0x0ff0_000f,
                0x0000_0073,
                addi(10, 0, 5),
                EBREAK,
            ],
            100,
        );
        assert_eq!(vm.run(), Ok(Exit::Ecall));
        assert_eq!(vm.reg(17), 7);
        assert_eq!(vm.pc(), 12);
        assert_eq!(vm.run(), Ok(Exit::Halt));
        assert_eq!(vm.reg(10), 5);
    }

    #[test]
    fn out_of_gas_exact_boundary() {
        // addi, addi, ebreak = 3 gas
        let prog = [addi(1, 0, 1), addi(2, 0, 2), EBREAK];
        let mut vm = vm_with(&prog, 3);
        assert_eq!(vm.run(), Ok(Exit::Halt));
        assert_eq!(vm.gas_used(), 3);

        let mut vm = vm_with(&prog, 2);
        assert_eq!(vm.run(), Err(Trap::OutOfGas));
        assert_eq!(vm.gas_used(), 2);
        assert_eq!(vm.pc(), 8); // ebreak did not execute
                                // a mul needing 3 with only 2 left: no state change
        let mut vm = vm_with(&[r(1, 2, 1, 0, 3, 0x33)], 2);
        vm.set_reg(1, 3);
        vm.set_reg(2, 4);
        assert_eq!(vm.run(), Err(Trap::OutOfGas));
        assert_eq!((vm.reg(3), vm.pc(), vm.gas_used()), (0, 0, 0));
    }

    #[test]
    fn gas_table_costs() {
        let c = |p: &[u32]| {
            let mut vm = vm_with(p, 100);
            vm.set_reg(1, 256);
            vm.set_reg(2, 3);
            vm.step().unwrap();
            vm.gas_used()
        };
        assert_eq!(c(&[r(0, 2, 1, 0, 3, 0x33)]), gas::BASE);
        assert_eq!(c(&[r(1, 2, 1, 0, 3, 0x33)]), gas::MUL);
        assert_eq!(c(&[r(1, 2, 1, 4, 3, 0x33)]), gas::DIV);
        assert_eq!(c(&[r(1, 2, 1, 7, 3, 0x3b)]), gas::DIV);
        assert_eq!(c(&[i(0, 1, 3, 3, 0x03)]), gas::LOAD);
        assert_eq!(c(&[s(0, 2, 1, 3)]), gas::STORE);
        assert_eq!(c(&[jal(4, 0)]), gas::BRANCH);
    }

    #[test]
    fn sum_loop_exact_gas() {
        // sum 1..=10
        let prog = [
            addi(1, 0, 10),         // counter
            addi(2, 0, 0),          // sum
            r(0, 1, 2, 0, 2, 0x33), // loop: add x2, x2, x1
            addi(1, 1, -1),
            b(-8, 0, 1, 1), // bne x1, x0, loop
            EBREAK,
        ];
        let mut vm = vm_with(&prog, 1000);
        assert_eq!(vm.run(), Ok(Exit::Halt));
        assert_eq!(vm.reg(2), 55);
        assert_eq!(vm.gas_used(), 2 + 10 * 3 + 1);
        // deterministic: identical rerun
        let mut vm2 = vm_with(&prog, 1000);
        vm2.run().unwrap();
        assert_eq!(vm2.gas_used(), vm.gas_used());
        // one gas short fails at the final ebreak
        let mut vm3 = vm_with(&prog, 32);
        assert_eq!(vm3.run(), Err(Trap::OutOfGas));
    }
}
