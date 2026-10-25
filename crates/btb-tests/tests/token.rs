//! End to end: build `examples/token` for riscv64 and run it in `btb-vm`.

use btb_codec::{Address, Decode};
use btb_vm::decode::Trap;
use btb_vm::testing::MemHost;
use btb_vm::{CallError, Contract, FunctionKind};
use std::sync::OnceLock;

mod common;
use common::{abort_msg, call};

const ALICE: Address = Address([1; 32]);
const BOB: Address = Address([2; 32]);
const CAROL: Address = Address([3; 32]);

fn elf() -> &'static [u8] {
    static ELF: OnceLock<Vec<u8>> = OnceLock::new();
    ELF.get_or_init(|| common::build_example("token"))
}

fn contract() -> Contract {
    Contract::from_elf(elf()).expect("valid contract")
}

fn as_caller(h: &mut MemHost, who: Address) {
    h.caller = who.0;
}

fn balance(c: &Contract, h: &mut MemHost, who: Address) -> u128 {
    let (out, _) = call(c, FunctionKind::Query, "balance_of", &(who,), h).unwrap();
    u128::decode(&out).unwrap()
}

fn supply(c: &Contract, h: &mut MemHost) -> u128 {
    let (out, _) = call(c, FunctionKind::Query, "total_supply", &(), h).unwrap();
    u128::decode(&out).unwrap()
}

fn transfer(c: &Contract, h: &mut MemHost, to: Address, amount: u128) -> Result<u64, CallError> {
    let (out, gas) = call(c, FunctionKind::Mutation, "transfer", &(to, amount), h)?;
    assert_eq!(bool::decode(&out), Ok(true));
    Ok(gas)
}

/// Scripted session for the determinism check; returns gas per call.
fn session(c: &Contract, h: &mut MemHost) -> Vec<u64> {
    as_caller(h, ALICE);
    let mut gas = vec![
        call(c, FunctionKind::Mutation, "init", &(1_000u128,), h)
            .unwrap()
            .1,
    ];
    gas.push(transfer(c, h, BOB, 250).unwrap());
    gas.push(
        call(c, FunctionKind::Query, "balance_of", &(BOB,), h)
            .unwrap()
            .1,
    );
    gas
}

#[test]
fn entry_points_are_exported() {
    let c = contract();
    let names: Vec<_> = c
        .entry_points()
        .iter()
        .map(|e| (e.kind, e.name.as_str()))
        .collect();
    for want in [
        (FunctionKind::Mutation, "init"),
        (FunctionKind::Mutation, "transfer"),
        (FunctionKind::Query, "balance_of"),
        (FunctionKind::Query, "total_supply"),
    ] {
        assert!(names.contains(&want), "missing {want:?} in {names:?}");
    }
}

#[test]
fn only_rv64im_instructions_are_emitted() {
    common::assert_only_rv64im(elf());
}

#[test]
fn init_and_transfers() {
    let c = contract();
    let mut h = MemHost::default();
    assert_eq!(supply(&c, &mut h), 0);
    as_caller(&mut h, ALICE);
    call(&c, FunctionKind::Mutation, "init", &(1_000u128,), &mut h).unwrap();
    assert_eq!(supply(&c, &mut h), 1_000);
    assert_eq!(balance(&c, &mut h, ALICE), 1_000);
    assert_eq!(balance(&c, &mut h, BOB), 0);

    transfer(&c, &mut h, BOB, 300).unwrap();
    as_caller(&mut h, BOB);
    transfer(&c, &mut h, CAROL, 100).unwrap();
    // Transfer of zero and to oneself succeed and change nothing.
    transfer(&c, &mut h, CAROL, 0).unwrap();
    transfer(&c, &mut h, BOB, 200).unwrap();

    assert_eq!(balance(&c, &mut h, ALICE), 700);
    assert_eq!(balance(&c, &mut h, BOB), 200);
    assert_eq!(balance(&c, &mut h, CAROL), 100);
    assert_eq!(supply(&c, &mut h), 1_000);
}

#[test]
fn insufficient_balance_aborts_without_writing() {
    let c = contract();
    let mut h = MemHost::default();
    as_caller(&mut h, ALICE);
    call(&c, FunctionKind::Mutation, "init", &(100u128,), &mut h).unwrap();
    let before = h.storage.clone();
    let e = transfer(&c, &mut h, BOB, 101).unwrap_err();
    assert_eq!(abort_msg(e), "insufficient balance");
    // A stranger with no balance cannot send even one unit, not even to oneself.
    as_caller(&mut h, CAROL);
    let e = transfer(&c, &mut h, CAROL, 1).unwrap_err();
    assert_eq!(abort_msg(e), "insufficient balance");
    assert_eq!(h.storage, before);
}

#[test]
fn double_init_aborts() {
    let c = contract();
    let mut h = MemHost::default();
    as_caller(&mut h, ALICE);
    call(&c, FunctionKind::Mutation, "init", &(100u128,), &mut h).unwrap();
    let before = h.storage.clone();
    for who in [ALICE, BOB] {
        as_caller(&mut h, who);
        let e = call(&c, FunctionKind::Mutation, "init", &(5u128,), &mut h).unwrap_err();
        assert_eq!(abort_msg(e), "already initialised");
    }
    assert_eq!(h.storage, before);
    assert_eq!(supply(&c, &mut h), 100);
}

#[test]
fn recipient_overflow_aborts() {
    let c = contract();
    let mut h = MemHost::default();
    as_caller(&mut h, ALICE);
    call(&c, FunctionKind::Mutation, "init", &(10u128,), &mut h).unwrap();
    // Force a recipient balance of u128::MAX (unreachable through the API, since the sum of all
    // balances equals the supply) to prove the checked addition.
    let mut key = b"bal:".to_vec();
    key.extend_from_slice(&BOB.0);
    h.storage.insert(key, u128::MAX.to_le_bytes().to_vec());
    let before = h.storage.clone();
    let e = transfer(&c, &mut h, BOB, 1).unwrap_err();
    assert_eq!(abort_msg(e), "balance overflow");
    assert_eq!(h.storage, before);
    // The largest possible supply is fine.
    let mut h2 = MemHost::default();
    call(&c, FunctionKind::Mutation, "init", &(u128::MAX,), &mut h2).unwrap();
    assert_eq!(supply(&c, &mut h2), u128::MAX);
}

#[test]
fn query_that_writes_traps() {
    let c = contract();
    let mut h = MemHost::default();
    let e = call(
        &c,
        FunctionKind::Query,
        "transfer_in_query",
        &(BOB, 5u128),
        &mut h,
    )
    .unwrap_err();
    assert!(matches!(
        e,
        CallError::Trap {
            trap: Trap::WriteInQuery,
            ..
        }
    ));
    assert!(h.storage.is_empty());
}

#[test]
fn gas_is_deterministic() {
    let c = contract();
    let g1 = session(&c, &mut MemHost::default());
    let g2 = session(&c, &mut MemHost::default());
    assert_eq!(g1, g2);
    println!(
        "gas used: init={} transfer={} balance_of={} (identical across two runs)",
        g1[0], g1[1], g1[2]
    );
    assert!(g1.iter().all(|g| *g > 0));
}
