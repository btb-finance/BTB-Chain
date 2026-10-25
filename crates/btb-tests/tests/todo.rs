//! `examples/todo` through the real state database: the owner index is read by the contract
//! with host call 15 (`table_index_scan`).

use btb_codec::{Decode, Encode};
use btb_runtime::{layout, IndexDef, IndexValue, KvStore, MemKv, RuntimeHost, Tables};
use btb_vm::{Contract, FunctionKind};

mod common;

const C: [u8; 32] = [8; 32];
const ALICE: [u8; 32] = [1; 32];
const BOB: [u8; 32] = [2; 32];

type Item = (u64, bool, String);

fn tables() -> Tables {
    let mut t = Tables::new();
    t.register(
        C,
        b"Todo",
        vec![IndexDef::new("owner", |row| {
            Ok(Some(IndexValue::Bytes(row[..32].to_vec())))
        })],
    )
    .unwrap();
    t
}

struct Env {
    c: Contract,
    t: Tables,
    kv: MemKv,
}

impl Env {
    fn new() -> Self {
        let mut kv = MemKv::new();
        kv.put(&layout::balance_key(&C), &1_000_000u64.to_le_bytes())
            .unwrap();
        Env {
            c: Contract::from_elf(&common::build_example("todo")).unwrap(),
            t: tables(),
            kv,
        }
    }

    /// Run and, for mutations, commit. Returns (output, gas).
    fn run(
        &mut self,
        kind: FunctionKind,
        f: &str,
        who: [u8; 32],
        args: &impl Encode,
    ) -> Result<(Vec<u8>, u64), btb_vm::CallError> {
        let mut h = RuntimeHost::new(&self.kv, &self.t, C, who, 1);
        let out = self
            .c
            .call(kind, f, &args.encode(), &mut h, common::GAS, common::MEM)?;
        if kind == FunctionKind::Mutation {
            let b = h.finish().unwrap();
            self.kv.commit(b).unwrap();
        }
        Ok((out.output, out.gas_used))
    }

    fn list(&mut self, who: [u8; 32]) -> (Vec<Item>, u64) {
        let (o, g) = self
            .run(FunctionKind::Query, "list_mine", who, &())
            .unwrap();
        (Vec::<Item>::decode(&o).unwrap(), g)
    }

    fn add(&mut self, who: [u8; 32], text: &str) -> u64 {
        let (o, _) = self
            .run(FunctionKind::Mutation, "add", who, &(text.to_string(),))
            .unwrap();
        u64::decode(&o).unwrap()
    }
}

#[test]
fn list_mine_reads_only_the_callers_rows_through_the_index() {
    let mut e = Env::new();
    assert_eq!(e.list(ALICE).0, vec![]);
    assert_eq!(e.add(ALICE, "milk"), 0);
    assert_eq!(e.add(BOB, "bob's"), 1);
    assert_eq!(e.add(ALICE, "eggs"), 2);
    assert_eq!(
        e.list(ALICE).0,
        vec![(0, false, "milk".into()), (2, false, "eggs".into())]
    );
    assert_eq!(e.list(BOB).0, vec![(1, false, "bob's".into())]);

    let (o, _) = e
        .run(FunctionKind::Mutation, "toggle", ALICE, &(0u64,))
        .unwrap();
    assert_eq!(bool::decode(&o), Ok(true));
    assert_eq!(e.list(ALICE).0[0], (0, true, "milk".into()));
    assert_eq!(e.list(BOB).0, vec![(1, false, "bob's".into())]);
}

#[test]
fn toggle_rejects_other_users_and_missing_ids() {
    let mut e = Env::new();
    e.add(ALICE, "milk");
    let err = e
        .run(FunctionKind::Mutation, "toggle", BOB, &(0u64,))
        .unwrap_err();
    assert_eq!(common::abort_msg(err), "not your todo");
    let err = e
        .run(FunctionKind::Mutation, "toggle", ALICE, &(9u64,))
        .unwrap_err();
    assert_eq!(common::abort_msg(err), "no such todo");
    let err = e
        .run(FunctionKind::Mutation, "add", ALICE, &(String::new(),))
        .unwrap_err();
    assert_eq!(common::abort_msg(err), "text must be 1..=280 bytes");
}

#[test]
fn list_mine_is_a_query_and_gas_is_exact_and_repeatable() {
    let mut e = Env::new();
    e.add(ALICE, "milk");
    let (_, g1) = e.list(ALICE);
    let (_, g2) = e.list(ALICE);
    assert_eq!(g1, g2);
    assert!(g1 > 0);
    // More rows cost more (one index id + one row read each).
    e.add(ALICE, "eggs");
    assert!(e.list(ALICE).1 > g1);
}

#[test]
fn pages_through_more_than_one_page() {
    let mut e = Env::new();
    for i in 0..150 {
        e.add(ALICE, &format!("t{i}"));
    }
    e.add(BOB, "x");
    let (items, _) = e.list(ALICE); // contract pages 64 at a time
    assert_eq!(items.len(), 150);
    assert!(items.windows(2).all(|w| w[0].0 < w[1].0));
}

/// The index must be a real range read: a new row for the caller invalidates it, another
/// user's row does not.
#[test]
fn list_mine_read_set_covers_only_the_callers_index_range() {
    let mut e = Env::new();
    e.add(ALICE, "milk");
    let mut h = RuntimeHost::new(&e.kv, &e.t, C, ALICE, 1);
    e.c.call(
        FunctionKind::Query,
        "list_mine",
        &().encode(),
        &mut h,
        common::GAS,
        common::MEM,
    )
    .unwrap();
    let (_, reads) = h.finish_tracked().unwrap();
    let key = |who: [u8; 32], id| {
        layout::index_key(&C, b"Todo", "owner", &IndexValue::Bytes(who.to_vec()), id)
    };
    assert!(reads.contains_key(&key(ALICE, 77)));
    assert!(!reads.contains_key(&key(BOB, 77)));
}
