//! A subscription on `examples/counter`'s `get` fires when `increment` commits.

use btb_codec::{Decode, Encode};
use btb_runtime::{MemKv, Occ, ReadSet, RuntimeHost, SubscriptionManager, Tables, WriteBatch};
use btb_vm::{Contract, FunctionKind};

mod common;

const ME: [u8; 32] = [3; 32];
const CONTRACT: [u8; 32] = [8; 32];

fn call(
    c: &Contract,
    kv: &MemKv,
    kind: FunctionKind,
    name: &str,
    args: &[u8],
) -> (Vec<u8>, WriteBatch, ReadSet) {
    let tables = Tables::new();
    let mut h = RuntimeHost::new(kv, &tables, CONTRACT, ME, 1);
    let out = c
        .call(kind, name, args, &mut h, common::GAS, common::MEM)
        .unwrap();
    let (batch, reads) = h.finish_tracked().unwrap();
    (out.output, batch, reads)
}

#[test]
fn increment_fires_get_subscription() {
    let c = Contract::from_elf(&common::build_example("counter")).unwrap();
    let mut occ = Occ::new(MemKv::new());
    let mut subs = SubscriptionManager::new();

    let (out, _, reads) = call(&c, occ.store(), FunctionKind::Query, "get", &[]);
    assert_eq!(u64::decode(&out), Ok(0));
    assert!(!reads.is_empty(), "get must record what it read");
    let id = subs.subscribe(CONTRACT, "get", &[], reads, &out);

    let committed = occ
        .execute_with_retry::<_, ()>(3, |s, _| {
            let (o, b, r) = call(
                &c,
                s,
                FunctionKind::Mutation,
                "increment",
                &(5u64,).encode(),
            );
            Ok((o, b, r))
        })
        .unwrap();
    assert_eq!(u64::decode(&committed.output), Ok(5));

    let changes = subs
        .process_commit::<()>(&committed.batch, |_, f, args| {
            let (o, _, r) = call(&c, occ.store(), FunctionKind::Query, f, args);
            Ok((o, r))
        })
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].id, id);
    assert_eq!(u64::decode(&changes[0].result), Ok(5));
}
