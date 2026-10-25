//! End to end: build `examples/dex` for riscv64 and run it in `btb-vm`.

use btb_codec::{Address, Decode};
use btb_vm::decode::Trap;
use btb_vm::testing::MemHost;
use btb_vm::{CallError, Contract, FunctionKind};
use std::sync::OnceLock;

mod common;
use common::{abort_msg, call};

const ALICE: Address = Address([1; 32]);
const BOB: Address = Address([2; 32]);

fn elf() -> &'static [u8] {
    static ELF: OnceLock<Vec<u8>> = OnceLock::new();
    ELF.get_or_init(|| common::build_example("dex"))
}

fn contract() -> Contract {
    Contract::from_elf(elf()).expect("valid contract")
}

fn add_liquidity(
    c: &Contract,
    h: &mut MemHost,
    who: Address,
    a: u128,
    b: u128,
) -> Result<(u128, u64), CallError> {
    h.caller = who.0;
    let (out, gas) = call(c, FunctionKind::Mutation, "add_liquidity", &(a, b), h)?;
    Ok((u128::decode(&out).unwrap(), gas))
}

fn swap(
    c: &Contract,
    h: &mut MemHost,
    amount_in: u128,
    min_out: u128,
) -> Result<(u128, u64), CallError> {
    let (out, gas) = call(
        c,
        FunctionKind::Mutation,
        "swap_a_for_b",
        &(amount_in, min_out),
        h,
    )?;
    Ok((u128::decode(&out).unwrap(), gas))
}

fn reserves(c: &Contract, h: &mut MemHost) -> (u128, u128) {
    let (out, _) = call(c, FunctionKind::Query, "reserves", &(), h).unwrap();
    <(u128, u128)>::decode(&out).unwrap()
}

fn quote(c: &Contract, h: &mut MemHost, amount_in: u128) -> Result<u128, CallError> {
    let (out, _) = call(c, FunctionKind::Query, "quote", &(amount_in,), h)?;
    Ok(u128::decode(&out).unwrap())
}

fn shares(c: &Contract, h: &mut MemHost, who: Address) -> u128 {
    let (out, _) = call(c, FunctionKind::Query, "shares_of", &(who,), h).unwrap();
    u128::decode(&out).unwrap()
}

/// Scripted session for the determinism check; returns gas per call.
fn session(c: &Contract, h: &mut MemHost) -> Vec<u64> {
    let g0 = add_liquidity(c, h, ALICE, 1_000_000, 4_000_000).unwrap().1;
    let g1 = swap(c, h, 1_000, 3_900).unwrap().1;
    let g2 = call(c, FunctionKind::Query, "quote", &(10_000u128,), h)
        .unwrap()
        .1;
    vec![g0, g1, g2]
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
        (FunctionKind::Mutation, "add_liquidity"),
        (FunctionKind::Mutation, "swap_a_for_b"),
        (FunctionKind::Query, "reserves"),
        (FunctionKind::Query, "quote"),
        (FunctionKind::Query, "shares_of"),
    ] {
        assert!(names.contains(&want), "missing {want:?} in {names:?}");
    }
}

#[test]
fn only_rv64im_instructions_are_emitted() {
    common::assert_only_rv64im(elf());
}

#[test]
fn liquidity_swap_and_quote_with_exact_numbers() {
    let c = contract();
    let mut h = MemHost::default();
    assert_eq!(reserves(&c, &mut h), (0, 0));

    // First deposit mints isqrt(1_000_000 * 4_000_000) = 2_000_000 shares.
    assert_eq!(
        add_liquidity(&c, &mut h, ALICE, 1_000_000, 4_000_000)
            .unwrap()
            .0,
        2_000_000
    );
    assert_eq!(reserves(&c, &mut h), (1_000_000, 4_000_000));
    assert_eq!(shares(&c, &mut h, ALICE), 2_000_000);

    // 1000 * 997 * 4_000_000 / (1_000_000 * 1000 + 1000 * 997) = 3984 (rounded down).
    assert_eq!(quote(&c, &mut h, 1_000).unwrap(), 3_984);
    assert_eq!(swap(&c, &mut h, 1_000, 3_984).unwrap().0, 3_984);
    assert_eq!(reserves(&c, &mut h), (1_001_000, 3_996_016));
    // The fee stays in the pool, so k grows.
    let (ra, rb) = reserves(&c, &mut h);
    assert!(ra * rb > 1_000_000 * 4_000_000);
    assert_eq!(quote(&c, &mut h, 10_000).unwrap(), 39_407);

    // Second provider: min(500 * 2_000_000 / 1_001_000, 2000 * 2_000_000 / 3_996_016) = 999.
    assert_eq!(add_liquidity(&c, &mut h, BOB, 500, 2_000).unwrap().0, 999);
    assert_eq!(reserves(&c, &mut h), (1_001_500, 3_998_016));
    assert_eq!(shares(&c, &mut h, BOB), 999);
    assert_eq!(shares(&c, &mut h, ALICE), 2_000_000);
}

#[test]
fn slippage_aborts_without_writing() {
    let c = contract();
    let mut h = MemHost::default();
    add_liquidity(&c, &mut h, ALICE, 1_000_000, 4_000_000).unwrap();
    let before = h.storage.clone();
    let e = swap(&c, &mut h, 1_000, 3_985).unwrap_err();
    assert_eq!(abort_msg(e), "slippage");
    assert_eq!(h.storage, before);
}

#[test]
fn empty_pool_and_zero_amounts_abort() {
    let c = contract();
    let mut h = MemHost::default();
    assert_eq!(
        abort_msg(swap(&c, &mut h, 10, 0).unwrap_err()),
        "no liquidity"
    );
    assert_eq!(
        abort_msg(quote(&c, &mut h, 10).unwrap_err()),
        "no liquidity"
    );
    for (a, b) in [(0, 5), (5, 0), (0, 0)] {
        let e = add_liquidity(&c, &mut h, ALICE, a, b).unwrap_err();
        assert_eq!(abort_msg(e), "zero amount");
    }
    assert!(h.storage.is_empty());

    // A deposit too small to mint a share aborts.
    add_liquidity(&c, &mut h, ALICE, 1_000_000, 4_000_000).unwrap();
    let before = h.storage.clone();
    let e = add_liquidity(&c, &mut h, BOB, 1, 1).unwrap_err();
    assert_eq!(abort_msg(e), "zero shares");
    assert_eq!(h.storage, before);
}

#[test]
fn overflow_aborts() {
    let c = contract();
    let mut h = MemHost::default();
    // a * b does not fit in u128.
    let e = add_liquidity(&c, &mut h, ALICE, u128::MAX, 2).unwrap_err();
    assert_eq!(abort_msg(e), "overflow");
    assert!(h.storage.is_empty());
    add_liquidity(&c, &mut h, ALICE, 1_000_000, 4_000_000).unwrap();
    let before = h.storage.clone();
    // amount_in * 997 does not fit in u128.
    let e = swap(&c, &mut h, u128::MAX, 0).unwrap_err();
    assert_eq!(abort_msg(e), "overflow");
    assert_eq!(h.storage, before);
}

#[test]
fn query_that_writes_traps() {
    let c = contract();
    let mut h = MemHost::default();
    let e = call(&c, FunctionKind::Query, "swap_in_query", &(5u128,), &mut h).unwrap_err();
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
        "gas used: add_liquidity={} swap_a_for_b={} quote={} (identical across two runs)",
        g1[0], g1[1], g1[2]
    );
    assert!(g1.iter().all(|g| *g > 0));
}
