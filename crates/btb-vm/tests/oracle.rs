//! Randomness and price host calls: gating by entry-point marker, gas, determinism.

use btb_vm::gas;
use btb_vm::host::{abi, PriceRecord};
use btb_vm::rand::{stream, tx_seed, RandomSource};
use btb_vm::testing::{build_elf, MemHost};
use btb_vm::{run_with_host, CallError, Contract, ContractError, Exit, FunctionKind, Trap, Vm};

const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;
const RET: u32 = 0x0000_8067;
const MEM: usize = 1 << 18;

fn addi(rd: u32, rs1: u32, imm: u32) -> u32 {
    (imm << 20) | (rs1 << 15) | (rd << 7) | 0x13
}

fn source() -> RandomSource {
    RandomSource::new(&[5; 32], &[6; 32])
}

fn vm_with(call: u64, args: [u64; 4], allowed: bool) -> Vm {
    let mut vm = Vm::new(4096, 100_000);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.write_mem(4, &EBREAK.to_le_bytes()).unwrap();
    vm.set_reg(17, call);
    for (i, a) in args.iter().enumerate() {
        vm.set_reg(10 + i, *a);
    }
    vm.set_randomness_allowed(allowed);
    vm
}

#[test]
fn derivation_is_deterministic_and_separated() {
    let a = tx_seed(&[1; 32], &[2; 32]);
    assert_eq!(a, tx_seed(&[1; 32], &[2; 32]));
    assert_ne!(a, tx_seed(&[1; 32], &[3; 32]), "per transaction");
    assert_ne!(a, tx_seed(&[9; 32], &[2; 32]), "per beacon");
    assert_eq!(stream(&a, 0, 100), stream(&a, 0, 100));
    assert_ne!(stream(&a, 0, 32), stream(&a, 1, 32), "per call index");
    // a longer request extends the shorter one
    assert_eq!(stream(&a, 0, 32), stream(&a, 0, 80)[..32]);
    assert_eq!(stream(&a, 0, 80).len(), 80);
    assert!(stream(&a, 0, 0).is_empty());
    // the source counts calls
    let mut s = RandomSource::new(&[1; 32], &[2; 32]);
    assert_eq!(s.next(16), stream(&a, 0, 16));
    assert_eq!(s.next(16), stream(&a, 1, 16));
}

#[test]
fn random_bytes_charges_gas_and_fills_memory() {
    let mut h = MemHost {
        random: Some(source()),
        ..Default::default()
    };
    let mut vm = vm_with(abi::RANDOM_BYTES, [512, 40, 0, 0], true);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(
        vm.gas_used(),
        2 + gas::HOST_RANDOM_BYTES + gas::RANDOM_BYTE * 40
    );
    assert_eq!(vm.reg(10), 0);
    let expect = stream(&btb_vm::rand::tx_seed(&[5; 32], &[6; 32]), 0, 40);
    assert_eq!(vm.read_mem(512, 40).unwrap(), expect);
    // bytes after the window are untouched
    assert_eq!(vm.read_mem(552, 8).unwrap(), [0u8; 8]);
    // a second call gets different bytes
    let mut vm2 = vm_with(abi::RANDOM_BYTES, [512, 40, 0, 0], true);
    assert_eq!(run_with_host(&mut vm2, &mut h), Ok(Exit::Halt));
    assert_ne!(vm2.read_mem(512, 40).unwrap(), expect);
}

#[test]
fn random_bytes_is_refused_without_the_marker() {
    let mut h = MemHost {
        random: Some(source()),
        ..Default::default()
    };
    let mut vm = vm_with(abi::RANDOM_BYTES, [512, 8, 0, 0], false);
    assert_eq!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::RandomnessForbidden)
    );
    // the beacon was not consumed by the refused call
    let mut vm = vm_with(abi::RANDOM_BYTES, [512, 8, 0, 0], true);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(
        vm.read_mem(512, 8).unwrap(),
        stream(&tx_seed(&[5; 32], &[6; 32]), 0, 8)
    );
}

#[test]
fn random_bytes_is_refused_in_a_read_only_host_even_if_flagged() {
    let mut h = MemHost {
        random: Some(source()),
        read_only: true,
        ..Default::default()
    };
    let mut vm = vm_with(abi::RANDOM_BYTES, [512, 8, 0, 0], true);
    assert_eq!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::RandomnessForbidden)
    );
}

#[test]
fn random_bytes_limits_and_missing_beacon() {
    let mut h = MemHost {
        random: Some(source()),
        ..Default::default()
    };
    // too long
    let mut vm = vm_with(
        abi::RANDOM_BYTES,
        [0, abi::MAX_RANDOM_BYTES + 1, 0, 0],
        true,
    );
    assert!(matches!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::Abort(_))
    ));
    // output window outside memory
    let mut vm = vm_with(abi::RANDOM_BYTES, [4090, 16, 0, 0], true);
    assert!(matches!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::MemoryOutOfBounds { .. })
    ));
    // out of gas is charged first, before anything else
    let mut vm = Vm::new(4096, 100);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.set_reg(17, abi::RANDOM_BYTES);
    vm.set_reg(10, 512);
    vm.set_reg(11, 8);
    vm.set_randomness_allowed(true);
    assert_eq!(run_with_host(&mut vm, &mut h), Err(Trap::OutOfGas));
    // no beacon on this host
    let mut none = MemHost::default();
    let mut vm = vm_with(abi::RANDOM_BYTES, [512, 8, 0, 0], true);
    assert_eq!(
        run_with_host(&mut vm, &mut none),
        Err(Trap::RandomnessUnavailable)
    );
}

#[test]
fn price_call_reads_gas_and_not_found() {
    let mut h = MemHost::default();
    h.prices.insert(
        3,
        PriceRecord {
            price: 6_500_012_345_678,
            height: 77,
            decimals: 8,
        },
    );
    let mut vm = vm_with(abi::PRICE, [3, 512, 0, 0], false);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), 0);
    assert_eq!(vm.gas_used(), 2 + gas::HOST_PRICE_MUTATION);
    let out = vm.read_mem(512, 32).unwrap();
    assert_eq!(
        u128::from_le_bytes(out[..16].try_into().unwrap()),
        6_500_012_345_678
    );
    assert_eq!(u64::from_le_bytes(out[16..24].try_into().unwrap()), 77);
    assert_eq!(out[24], 8);
    assert_eq!(&out[25..], &[0u8; 7]);
    // unknown pair, and a pair id that does not fit in u32
    for pair in [4u64, 1 << 40] {
        let mut vm = vm_with(abi::PRICE, [pair, 512, 0, 0], false);
        assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
        assert_eq!(vm.reg(10), abi::NOT_FOUND);
    }
    // read-only (query) hosts pay the cheap, fee-free rate
    h.read_only = true;
    let mut vm = vm_with(abi::PRICE, [3, 512, 0, 0], false);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.gas_used(), 2 + gas::HOST_PRICE_QUERY);
    const { assert!(gas::HOST_PRICE_MUTATION > gas::HOST_PRICE_QUERY) };
    // output window outside memory traps
    let mut vm = vm_with(abi::PRICE, [3, 4090, 0, 0], false);
    assert!(matches!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::MemoryOutOfBounds { .. })
    ));
}

// ---------------------------------------------------------------------- whole contracts

/// `roll`: random_bytes(input, 16); return_data(input, 16). Input must be 16 bytes.
fn roll_code() -> Vec<u8> {
    [
        addi(5, 10, 0),  // t0 = a0 (input pointer)
        addi(11, 0, 16), // a1 = 16
        addi(17, 0, abi::RANDOM_BYTES as u32),
        ECALL,
        addi(10, 5, 0), // a0 = t0
        addi(11, 0, 16),
        addi(17, 0, abi::RETURN_DATA as u32),
        ECALL,
        RET,
    ]
    .iter()
    .flat_map(|w| w.to_le_bytes())
    .collect()
}

fn elf(symbols: &[&str]) -> Vec<u8> {
    let syms: Vec<(&str, u64)> = symbols.iter().map(|s| (*s, 0)).collect();
    build_elf(&roll_code(), &syms)
}

fn run(
    c: &Contract,
    kind: FunctionKind,
    name: &str,
    host: &mut MemHost,
) -> Result<Vec<u8>, CallError> {
    c.call(kind, name, &[0u8; 16], host, 100_000, MEM)
        .map(|o| o.output)
}

fn mem_host() -> MemHost {
    MemHost {
        random: Some(source()),
        ..Default::default()
    }
}

#[test]
fn only_marked_mutations_and_crons_get_randomness() {
    let c = Contract::from_elf(&elf(&[
        "btb_mutation_roll",
        "btb_randomness_mutation_roll",
        "btb_mutation_plain",
        "btb_query_peek",
        "btb_cron_tick",
        "btb_randomness_cron_tick",
        "btb_cron_idle",
        "btb_action_fetch",
    ]))
    .unwrap();
    assert!(c.is_randomness(FunctionKind::Mutation, "roll"));
    assert!(c.is_randomness(FunctionKind::Cron, "tick"));
    assert!(!c.is_randomness(FunctionKind::Mutation, "plain"));
    assert!(!c.is_randomness(FunctionKind::Query, "peek"));
    assert!(!c.is_randomness(FunctionKind::Cron, "idle"));

    // marked mutation and cron work and are deterministic
    let want = stream(&tx_seed(&[5; 32], &[6; 32]), 0, 16);
    assert_eq!(
        run(&c, FunctionKind::Mutation, "roll", &mut mem_host()).unwrap(),
        want
    );
    assert_eq!(
        run(&c, FunctionKind::Cron, "tick", &mut mem_host()).unwrap(),
        want
    );
    // unmarked mutation, unmarked cron, query and action all trap
    for (kind, name) in [
        (FunctionKind::Mutation, "plain"),
        (FunctionKind::Cron, "idle"),
        (FunctionKind::Query, "peek"),
        (FunctionKind::Action, "fetch"),
    ] {
        match run(&c, kind, name, &mut mem_host()) {
            Err(CallError::Trap {
                trap: Trap::RandomnessForbidden,
                ..
            }) => {}
            other => panic!("{kind:?} {name}: {other:?}"),
        }
    }
    // a marked mutation on a chain without a beacon traps differently
    assert!(matches!(
        run(&c, FunctionKind::Mutation, "roll", &mut MemHost::default()),
        Err(CallError::Trap {
            trap: Trap::RandomnessUnavailable,
            ..
        })
    ));
}

#[test]
fn a_query_cannot_use_a_marker_of_a_mutation_with_the_same_name() {
    // `roll` exists as a marked mutation and as a query; the marker belongs to the mutation only.
    let c = Contract::from_elf(&elf(&[
        "btb_mutation_roll",
        "btb_randomness_mutation_roll",
        "btb_query_roll",
    ]))
    .unwrap();
    assert!(c.is_randomness(FunctionKind::Mutation, "roll"));
    assert!(!c.is_randomness(FunctionKind::Query, "roll"));
    assert!(matches!(
        run(&c, FunctionKind::Query, "roll", &mut mem_host()),
        Err(CallError::Trap {
            trap: Trap::RandomnessForbidden,
            ..
        })
    ));
    assert!(run(&c, FunctionKind::Mutation, "roll", &mut mem_host()).is_ok());
}

#[test]
fn bad_markers_make_the_contract_invalid() {
    let bad = |syms: &[&str]| Contract::from_elf(&elf(syms)).unwrap_err();
    // markers on a query or an action
    assert!(matches!(
        bad(&[
            "btb_query_peek",
            "btb_randomness_query_peek",
            "btb_mutation_x"
        ]),
        ContractError::BadRandomnessMarker(_)
    ));
    assert!(matches!(
        bad(&["btb_action_a", "btb_randomness_action_a", "btb_mutation_x"]),
        ContractError::BadRandomnessMarker(_)
    ));
    // a marker without an entry point of that kind
    assert!(matches!(
        bad(&["btb_mutation_x", "btb_randomness_mutation_y"]),
        ContractError::BadRandomnessMarker(_)
    ));
    assert!(matches!(
        bad(&["btb_query_x", "btb_randomness_mutation_x"]),
        ContractError::BadRandomnessMarker(_)
    ));
    // a bad name, a missing kind
    assert!(matches!(
        bad(&["btb_mutation_x", "btb_randomness_mutation_Bad"]),
        ContractError::BadRandomnessMarker(_)
    ));
    assert!(matches!(
        bad(&["btb_mutation_x", "btb_randomness_x"]),
        ContractError::BadRandomnessMarker(_)
    ));
    // duplicates cannot be exported twice by a linker, but two spellings of one marker are caught
    // by the entry-point duplicate rule; the marker itself is fine once
    assert!(Contract::from_elf(&elf(&["btb_mutation_x", "btb_randomness_mutation_x"])).is_ok());
}
