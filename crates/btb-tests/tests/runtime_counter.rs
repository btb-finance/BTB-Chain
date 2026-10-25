//! `examples/counter` run through `Contract::call` with `RuntimeHost` over `MemKv`.

use btb_codec::{Decode, Encode};
use btb_runtime::{layout, KvStore, MemKv, RuntimeHost, Tables};
use btb_vm::{Contract, FunctionKind};

mod common;

const ME: [u8; 32] = [3; 32];
const CONTRACT: [u8; 32] = [8; 32];

/// Run one call in its own transaction: commit on success, discard on error.
fn run(
    c: &Contract,
    kv: &mut MemKv,
    kind: FunctionKind,
    name: &str,
    input: &impl Encode,
    height: u64,
) -> Result<Vec<u8>, btb_vm::CallError> {
    let tables = Tables::new();
    let mut h = RuntimeHost::new(kv, &tables, CONTRACT, ME, height);
    let out = c.call(
        kind,
        name,
        &input.encode(),
        &mut h,
        common::GAS,
        common::MEM,
    )?;
    let batch = h.finish().expect("no faults");
    kv.commit(batch).unwrap();
    Ok(out.output)
}

#[test]
fn counter_persists_in_memkv() {
    let c = Contract::from_elf(&common::build_example("counter")).unwrap();
    let mut kv = MemKv::new();
    for (n, want) in [(5u64, 5u64), (7, 12)] {
        let out = run(&c, &mut kv, FunctionKind::Mutation, "increment", &(n,), 42).unwrap();
        assert_eq!(u64::decode(&out), Ok(want));
    }
    let out = run(&c, &mut kv, FunctionKind::Query, "get", &(), 42).unwrap();
    assert_eq!(u64::decode(&out), Ok(12));

    // State lives in the shared keyspace under this contract's prefix, and a table row exists.
    let storage = kv
        .entries()
        .filter(|(k, _)| k[0] == layout::TAG_STORAGE && k[1..33] == CONTRACT)
        .count();
    let rows = kv
        .entries()
        .filter(|(k, _)| k[0] == layout::TAG_ROW)
        .count();
    assert!(storage >= 1 && rows >= 1, "storage={storage} rows={rows}");
    let out = run(&c, &mut kv, FunctionKind::Query, "change", &(1u64,), 42).unwrap();
    assert_eq!(
        Option::<(u64, u64, u64)>::decode(&out),
        Ok(Some((7, 12, 42)))
    );

    // A failing mutation (overflow) leaves the store unchanged.
    let snapshot = kv.clone();
    run(
        &c,
        &mut kv,
        FunctionKind::Mutation,
        "increment",
        &(u64::MAX,),
        43,
    )
    .unwrap_err();
    assert_eq!(kv, snapshot);
}
