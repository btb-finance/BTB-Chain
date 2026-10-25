//! Adversarial tests for the RV64IM interpreter (public API only).

use btb_vm::{decode, Exit, Trap, Vm};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn i_type(op: u32, f3: u32, rd: u32, rs1: u32, imm: i32) -> u32 {
    op | (rd << 7) | (f3 << 12) | (rs1 << 15) | (((imm as u32) & 0xfff) << 20)
}
fn s_type(f3: u32, rs1: u32, rs2: u32, imm: i32) -> u32 {
    let i = imm as u32;
    0x23 | ((i & 0x1f) << 7) | (f3 << 12) | (rs1 << 15) | (rs2 << 20) | (((i >> 5) & 0x7f) << 25)
}
fn r_type(op: u32, f3: u32, f7: u32, rd: u32, rs1: u32, rs2: u32) -> u32 {
    op | (rd << 7) | (f3 << 12) | (rs1 << 15) | (rs2 << 20) | (f7 << 25)
}
const EBREAK: u32 = 0x0010_0073;

fn load_prog(vm: &mut Vm, prog: &[u32]) {
    for (i, w) in prog.iter().enumerate() {
        vm.write_mem(i as u64 * 4, &w.to_le_bytes()).unwrap();
    }
}

#[test]
fn decode_random_never_panics() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..1_000_000 {
        let _ = decode(rng.next() as u32);
    }
    for w in [0u32, u32::MAX, 0x0000_0013, 0x8000_0000, 0xffff_fffe] {
        let _ = decode(w);
    }
}

fn random_vm(seed: u64, gas: u64) -> Vm {
    let mut rng = Rng(seed | 1);
    let mut vm = Vm::new(256, gas);
    let bytes: Vec<u8> = (0..256).map(|_| rng.next() as u8).collect();
    vm.write_mem(0, &bytes).unwrap();
    for i in 1..32 {
        let v = match rng.next() % 4 {
            0 => rng.next() % 300,
            1 => u64::MAX - rng.next() % 16,
            _ => rng.next(),
        };
        vm.set_reg(i, v);
    }
    vm
}

#[test]
fn random_streams_terminate_within_gas() {
    for seed in 1..3000u64 {
        let limit = seed % 200;
        let mut vm = random_vm(seed, limit);
        let _ = vm.run();
        assert!(vm.gas_used() <= limit, "seed {seed}");
        assert_eq!(vm.reg(0), 0);
    }
}

#[test]
fn random_streams_deterministic() {
    for seed in 1..2000u64 {
        let mut a = random_vm(seed, 500);
        let mut b = random_vm(seed, 500);
        let (ra, rb) = (a.run(), b.run());
        assert_eq!(ra, rb);
        assert_eq!(a.gas_used(), b.gas_used());
        assert_eq!(a.pc(), b.pc());
        assert_eq!(a.memory(), b.memory());
        for i in 0..32 {
            assert_eq!(a.reg(i), b.reg(i));
        }
    }
}

#[test]
fn loads_stores_near_end_and_wrap() {
    let mut vm = Vm::new(1024, 1000);
    // sd x2, 0(x1); ld x3, 0(x1)
    load_prog(
        &mut vm,
        &[s_type(3, 1, 2, 0), i_type(0x03, 3, 3, 1, 0), EBREAK],
    );
    vm.set_reg(1, 1024 - 8);
    vm.set_reg(2, 0x1122_3344_5566_7788);
    assert_eq!(vm.run(), Ok(Exit::Halt));
    assert_eq!(vm.reg(3), 0x1122_3344_5566_7788);

    // One byte past the end must trap and not modify anything.
    for base in [1024 - 7, 1024, u64::MAX - 7, u64::MAX - 100, 1 << 63] {
        let mut vm = Vm::new(1024, 1000);
        load_prog(&mut vm, &[s_type(3, 1, 2, 0), EBREAK]);
        vm.set_reg(1, base);
        vm.set_reg(2, u64::MAX);
        let before = vm.memory().to_vec();
        assert_eq!(vm.run(), Err(Trap::MemoryOutOfBounds { addr: base }));
        assert_eq!(vm.memory(), &before[..]);
        assert_eq!(vm.pc(), 0);
    }

    // Ld at u64::MAX: address + len overflows, must trap.
    let mut vm = Vm::new(1024, 1000);
    load_prog(&mut vm, &[i_type(0x03, 3, 3, 1, 0), EBREAK]);
    vm.set_reg(1, u64::MAX);
    vm.set_reg(3, 7);
    assert!(matches!(vm.run(), Err(Trap::MemoryOutOfBounds { .. })));
    assert_eq!(vm.reg(3), 7);

    // Effective address wraps modulo 2^64: (u64::MAX - 7) + 24 == 16.
    let mut vm = Vm::new(1024, 1000);
    load_prog(&mut vm, &[i_type(0x03, 3, 3, 1, 24), EBREAK]);
    vm.write_mem(16, &0xdead_beef_u64.to_le_bytes()).unwrap();
    vm.set_reg(1, u64::MAX - 7);
    assert_eq!(vm.run(), Ok(Exit::Halt));
    assert_eq!(vm.reg(3), 0xdead_beef);

    // Negative immediate sign extension: lb at 1 + (-2) wraps to u64::MAX -> trap.
    let mut vm = Vm::new(1024, 1000);
    load_prog(&mut vm, &[i_type(0x03, 0, 3, 1, -2), EBREAK]);
    vm.set_reg(1, 1);
    assert!(matches!(vm.run(), Err(Trap::MemoryOutOfBounds { .. })));
}

#[test]
fn read_write_mem_api_edges() {
    let mut vm = Vm::new(16, 10);
    assert!(vm.read_mem(16, 0).is_ok());
    assert!(vm.read_mem(17, 0).is_err());
    assert!(vm.read_mem(u64::MAX, 1).is_err());
    assert!(vm.read_mem(u64::MAX, 0).is_err());
    assert!(vm.write_mem(15, &[1, 2]).is_err());
    assert!(vm.write_mem(u64::MAX, &[1, 2]).is_err());
    assert_eq!(vm.memory(), &[0u8; 16]);
}

#[test]
fn jumps_to_bad_targets() {
    // jalr x5, 0(x1) with assorted targets.
    for (t, ok) in [
        (u64::MAX, false),
        (u64::MAX - 3, false), // aligned but out of bounds fetch
        (2, false),
        (3, false),
        (1 << 63, false),
        (1024, false),
        (1020, false), // in-bounds word is zero: illegal instruction
    ] {
        let mut vm = Vm::new(1024, 1000);
        load_prog(&mut vm, &[i_type(0x67, 0, 5, 1, 0)]);
        vm.set_reg(1, t);
        vm.set_reg(5, 99);
        let r = vm.run();
        assert_eq!(r.is_ok(), ok, "target {t:#x}: {r:?}");
        if t & 2 != 0 {
            // Misaligned jump traps atomically: link register untouched.
            assert_eq!(vm.reg(5), 99, "target {t:#x}");
        }
    }
    // Misaligned target is reported as MisalignedFetch with pc unchanged.
    let mut vm = Vm::new(1024, 1000);
    load_prog(&mut vm, &[i_type(0x67, 0, 5, 1, 0)]);
    vm.set_reg(1, 6);
    assert_eq!(vm.run(), Err(Trap::MisalignedFetch));
    assert_eq!(vm.pc(), 0);
    // JALR clears bit 0 (target 5 -> 4, aligned) and works.
    let mut vm = Vm::new(1024, 1000);
    load_prog(&mut vm, &[i_type(0x67, 0, 5, 1, 0), EBREAK]);
    vm.set_reg(1, 5);
    assert_eq!(vm.run(), Ok(Exit::Halt));
    assert_eq!(vm.reg(5), 4);
    // set_pc to huge/misaligned values.
    for pc in [u64::MAX, u64::MAX - 3, 1, 1 << 63, 1024] {
        let mut vm = Vm::new(1024, 10);
        vm.set_pc(pc);
        assert!(vm.run().is_err());
        assert_eq!(vm.gas_used(), 0);
    }
}

#[test]
fn out_of_gas_leaves_state_unchanged() {
    // addi x1, x0, 5 costs 1; mul x2,x1,x1 costs 3. Limit 2 -> second fails.
    let mut vm = Vm::new(64, 2);
    load_prog(
        &mut vm,
        &[
            i_type(0x13, 0, 1, 0, 5),
            r_type(0x33, 0, 1, 2, 1, 1),
            EBREAK,
        ],
    );
    assert_eq!(vm.run(), Err(Trap::OutOfGas));
    assert_eq!(vm.reg(1), 5);
    assert_eq!(vm.reg(2), 0);
    assert_eq!(vm.pc(), 4);
    assert_eq!(vm.gas_used(), 1);
    // Zero gas: nothing executes.
    let mut vm = Vm::new(64, 0);
    load_prog(&mut vm, &[i_type(0x13, 0, 1, 0, 5)]);
    assert_eq!(vm.run(), Err(Trap::OutOfGas));
    assert_eq!(vm.reg(1), 0);
    assert_eq!(vm.pc(), 0);
    assert_eq!(vm.gas_used(), 0);
    // u64::MAX limit with gas near overflow must not wrap.
    let mut vm = Vm::new(64, u64::MAX);
    load_prog(&mut vm, &[EBREAK]);
    assert_eq!(vm.run(), Ok(Exit::Halt));
}

#[test]
fn mulh_family_matches_i128() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let edge = [0u64, 1, u64::MAX, 1 << 63, (1 << 63) - 1, 2];
    let mut cases: Vec<(u64, u64)> = Vec::new();
    for &a in &edge {
        for &b in &edge {
            cases.push((a, b));
        }
    }
    for _ in 0..3000 {
        cases.push((rng.next(), rng.next()));
    }
    for (a, b) in cases {
        let h = ((a as i64 as i128 * b as i64 as i128) >> 64) as u64;
        let hsu = ((a as i64 as i128 * b as i128) >> 64) as u64;
        let hu = ((a as u128 * b as u128) >> 64) as u64;
        for (f3, want) in [(1u32, h), (2, hsu), (3, hu)] {
            let mut vm = Vm::new(64, 100);
            load_prog(&mut vm, &[r_type(0x33, f3, 1, 3, 1, 2), EBREAK]);
            vm.set_reg(1, a);
            vm.set_reg(2, b);
            assert_eq!(vm.run(), Ok(Exit::Halt));
            assert_eq!(vm.reg(3), want, "f3={f3} a={a:#x} b={b:#x}");
        }
    }
}

#[test]
fn divrem_w_match_reference() {
    let mut rng = Rng(0xfeed_beef_1234_5671);
    let edge = [
        0u64,
        1,
        u64::MAX,
        0x8000_0000,
        0xffff_ffff_8000_0000,
        0x7fff_ffff,
        1 << 63,
    ];
    let mut cases: Vec<(u64, u64)> = Vec::new();
    for &a in &edge {
        for &b in &edge {
            cases.push((a, b));
        }
    }
    for _ in 0..3000 {
        cases.push((rng.next(), rng.next()));
    }
    let sx = |v: u32| v as i32 as i64 as u64;
    for (a, b) in cases {
        let (ua, ub) = (a as u32, b as u32);
        let (sa, sb) = (ua as i32, ub as i32);
        let div = if sb == 0 {
            u64::MAX
        } else if sa == i32::MIN && sb == -1 {
            sx(i32::MIN as u32)
        } else {
            sx((sa / sb) as u32)
        };
        let rem = if sb == 0 {
            sx(ua)
        } else if sa == i32::MIN && sb == -1 {
            0
        } else {
            sx((sa % sb) as u32)
        };
        let divu = ua.checked_div(ub).map_or(u64::MAX, sx);
        let remu = if ub == 0 { sx(ua) } else { sx(ua % ub) };
        for (f3, want, name) in [
            (4u32, div, "divw"),
            (5, divu, "divuw"),
            (6, rem, "remw"),
            (7, remu, "remuw"),
        ] {
            let mut vm = Vm::new(64, 100);
            load_prog(&mut vm, &[r_type(0x3b, f3, 1, 3, 1, 2), EBREAK]);
            vm.set_reg(1, a);
            vm.set_reg(2, b);
            assert_eq!(vm.run(), Ok(Exit::Halt));
            assert_eq!(vm.reg(3), want, "{name} a={a:#x} b={b:#x}");
        }
    }
}

#[test]
fn div_rem_64_edge_cases() {
    let cases = [
        (4u32, i64::MIN as u64, u64::MAX, i64::MIN as u64),
        (6, i64::MIN as u64, u64::MAX, 0),
        (4, 7, 0, u64::MAX),
        (5, 7, 0, u64::MAX),
        (6, 7, 0, 7),
        (7, 7, 0, 7),
    ];
    for (f3, a, b, want) in cases {
        let mut vm = Vm::new(64, 100);
        load_prog(&mut vm, &[r_type(0x33, f3, 1, 3, 1, 2), EBREAK]);
        vm.set_reg(1, a);
        vm.set_reg(2, b);
        assert_eq!(vm.run(), Ok(Exit::Halt));
        assert_eq!(vm.reg(3), want, "f3={f3}");
    }
}
