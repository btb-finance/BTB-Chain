//! End to end: build `examples/counter` for riscv64 with the real toolchain, then run it in
//! `btb-vm` through `Contract::call`.

use btb_codec::{Decode, Encode};
use btb_vm::decode::Trap;
use btb_vm::host::abi;
use btb_vm::testing::MemHost;
use btb_vm::{CallError, Contract, FunctionKind};
use std::sync::OnceLock;

mod common;
use common::{call, GAS, MEM};

/// Build the counter once per test binary and return the ELF bytes.
fn elf() -> &'static [u8] {
    static ELF: OnceLock<Vec<u8>> = OnceLock::new();
    ELF.get_or_init(|| common::build_example("counter"))
}

fn contract() -> Contract {
    Contract::from_elf(elf()).expect("valid contract")
}

fn u64_of(out: &[u8]) -> u64 {
    u64::decode(out).expect("u64 result")
}

/// The scripted session used for the determinism check; returns (results, gas per call).
fn session(c: &Contract, h: &mut MemHost) -> (Vec<u64>, Vec<u64>) {
    let mut results = Vec::new();
    let mut gas = Vec::new();
    for (kind, name, arg) in [
        (FunctionKind::Mutation, "increment", Some(5u64)),
        (FunctionKind::Mutation, "increment", Some(7)),
        (FunctionKind::Query, "get", None),
    ] {
        let (out, g) = match arg {
            Some(a) => call(c, kind, name, &(a,), h).unwrap(),
            None => call(c, kind, name, &(), h).unwrap(),
        };
        results.push(u64_of(&out));
        gas.push(g);
    }
    (results, gas)
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
        (FunctionKind::Query, "get"),
        (FunctionKind::Query, "change"),
        (FunctionKind::Mutation, "increment"),
        (FunctionKind::Cron, "tick"),
    ] {
        assert!(names.contains(&want), "missing {want:?} in {names:?}");
    }
}

#[test]
fn only_rv64im_instructions_are_emitted() {
    common::assert_only_rv64im(elf());
}

#[test]
fn increment_twice_then_get() {
    let c = contract();
    let mut h = MemHost::default();
    let (results, _) = session(&c, &mut h);
    assert_eq!(results, [5, 12, 12]);

    // Cron adds one; the second increment left a table row with its block height.
    h.height = 99;
    call(&c, FunctionKind::Cron, "tick", &(), &mut h).unwrap();
    let (out, _) = call(&c, FunctionKind::Query, "get", &(), &mut h).unwrap();
    assert_eq!(u64_of(&out), 13);
    call(&c, FunctionKind::Mutation, "increment", &(1u64,), &mut h).unwrap();
    let (out, _) = call(&c, FunctionKind::Query, "change", &(2u64,), &mut h).unwrap();
    assert_eq!(
        Option::<(u64, u64, u64)>::decode(&out),
        Ok(Some((1, 14, 99)))
    );
    let (out, _) = call(&c, FunctionKind::Query, "change", &(9u64,), &mut h).unwrap();
    assert_eq!(Option::<(u64, u64, u64)>::decode(&out), Ok(None));
}

#[test]
fn query_that_writes_traps() {
    let c = contract();
    // Even with a writable host, the VM forces queries read-only.
    let mut h = MemHost::default();
    assert!(!h.read_only);
    let e = call(
        &c,
        FunctionKind::Query,
        "increment_in_query",
        &(1u64,),
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
    assert!(h.storage.is_empty() && h.tables.is_empty());
    // The same code as a mutation works.
    call(&c, FunctionKind::Mutation, "increment", &(1u64,), &mut h).unwrap();
}

#[test]
fn bad_arguments_abort() {
    let c = contract();
    let mut h = MemHost::default();
    for input in [vec![], vec![1, 2, 3], vec![0; 9]] {
        let e = c
            .call(
                FunctionKind::Mutation,
                "increment",
                &input,
                &mut h,
                GAS,
                MEM,
            )
            .unwrap_err();
        match e {
            CallError::Trap {
                trap: Trap::Abort(m),
                ..
            } => assert_eq!(m, "invalid arguments"),
            other => panic!("{other:?}"),
        }
    }
    assert!(h.storage.is_empty());
}

#[test]
fn overflow_aborts_without_writing() {
    let c = contract();
    let mut h = MemHost::default();
    call(
        &c,
        FunctionKind::Mutation,
        "increment",
        &(u64::MAX,),
        &mut h,
    )
    .unwrap();
    let before = h.clone().storage;
    let e = call(&c, FunctionKind::Mutation, "increment", &(1u64,), &mut h).unwrap_err();
    assert!(matches!(
        e,
        CallError::Trap { trap: Trap::Abort(ref m), .. } if m == "counter overflow"
    ));
    assert_eq!(h.storage, before);
}

#[test]
fn gas_is_deterministic() {
    let c = contract();
    let (r1, g1) = session(&c, &mut MemHost::default());
    let (r2, g2) = session(&c, &mut MemHost::default());
    assert_eq!((r1, &g1), (r2, &g2));
    println!(
        "gas used: increment(5)={} increment(7)={} get={} (identical across two runs)",
        g1[0], g1[1], g1[2]
    );
    assert!(g1.iter().all(|g| *g > 0));
}

#[test]
fn out_of_gas_is_a_trap() {
    let c = contract();
    let mut h = MemHost::default();
    let e = c
        .call(
            FunctionKind::Mutation,
            "increment",
            &(1u64,).encode(),
            &mut h,
            100,
            MEM,
        )
        .unwrap_err();
    assert!(matches!(
        e,
        CallError::Trap {
            trap: Trap::OutOfGas,
            ..
        }
    ));
}

#[test]
fn sdk_host_call_numbers_match_the_vm() {
    use btb_sdk::host::num;
    let pairs = [
        (num::STORAGE_READ, abi::STORAGE_READ),
        (num::STORAGE_WRITE, abi::STORAGE_WRITE),
        (num::STORAGE_REMOVE, abi::STORAGE_REMOVE),
        (num::CALLER, abi::CALLER),
        (num::BLOCK_HEIGHT, abi::BLOCK_HEIGHT),
        (num::TOKEN_BALANCE, abi::TOKEN_BALANCE),
        (num::TOKEN_TRANSFER, abi::TOKEN_TRANSFER),
        (num::RETURN_DATA, abi::RETURN_DATA),
        (num::ABORT, abi::ABORT),
        (num::TABLE_INSERT, abi::TABLE_INSERT),
        (num::TABLE_GET, abi::TABLE_GET),
        (num::TABLE_UPDATE, abi::TABLE_UPDATE),
        (num::TABLE_DELETE, abi::TABLE_DELETE),
        (num::TABLE_SCAN, abi::TABLE_SCAN),
        (num::TABLE_INDEX_SCAN, abi::TABLE_INDEX_SCAN),
        (num::TOKEN_CREATE, abi::TOKEN_CREATE),
        (num::TOKEN_TRANSFER_OF, abi::TOKEN_TRANSFER_OF),
        (num::TOKEN_BALANCE_OF, abi::TOKEN_BALANCE_OF),
        (num::TOKEN_INFO, abi::TOKEN_INFO),
        (num::TOKEN_HOLDERS, abi::TOKEN_HOLDERS),
        (num::TOKEN_MINT, abi::TOKEN_MINT),
        (num::TOKEN_BURN, abi::TOKEN_BURN),
        (num::CONTRACT_ADDRESS, abi::CONTRACT_ADDRESS),
        (num::NOT_FOUND, abi::NOT_FOUND),
    ];
    for (a, b) in pairs {
        assert_eq!(a, b);
    }
}

#[test]
fn sdk_oracle_host_call_numbers_match_the_vm() {
    use btb_sdk::oracle::num;
    assert_eq!(num::RANDOM_BYTES, abi::RANDOM_BYTES);
    assert_eq!(num::PRICE, abi::PRICE);
    assert_eq!(num::REPORT_REQUEST, abi::REPORT_REQUEST);
    assert_eq!(num::PRICE_TWAP, abi::PRICE_TWAP);
    assert_eq!(num::PRICE_PAUSED, abi::PRICE_PAUSED);
    assert_eq!(num::PRICE_BAD_WINDOW, abi::PRICE_BAD_WINDOW);
    assert_eq!(btb_sdk::oracle::MAX_TWAP_WINDOW, abi::MAX_TWAP_WINDOW);
    assert_eq!(
        btb_sdk::oracle::MAX_REPORT_BYTES as u64,
        abi::MAX_REPORT_BYTES
    );
    assert_eq!(
        btb_sdk::oracle::MAX_RANDOM_BYTES as u64,
        abi::MAX_RANDOM_BYTES
    );
}
