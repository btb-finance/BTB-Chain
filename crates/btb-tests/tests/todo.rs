//! `examples/todo` through the real state database: the owner index is read by the contract
//! with host call 15 (`table_index_scan`). The index is declared by the contract itself
//! (`#[index]` on a `#[btb::table]` field), not registered by the test.

use btb_codec::{Address, Decode, Encode};
use btb_runtime::{layout, IndexValue, KvStore, MemKv, RuntimeHost, Tables};
use btb_vm::{Contract, FunctionKind};

mod common;

const C: [u8; 32] = [8; 32];
const ALICE: [u8; 32] = [1; 32];
const BOB: [u8; 32] = [2; 32];

type Item = (u64, bool, String);

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
        let elf = common::build_example("todo");
        Env {
            c: Contract::from_elf(&elf).unwrap(),
            t: common::declared_tables(C, &elf),
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

type Page = (Vec<Item>, Vec<u8>);

impl Env {
    fn page(&mut self, who: [u8; 32], limit: u32, cursor: &[u8], newest_first: bool) -> Page {
        let (o, _) = self
            .run(
                FunctionKind::Query,
                "page_mine",
                who,
                &(limit, cursor.to_vec(), newest_first),
            )
            .unwrap();
        Page::decode(&o).unwrap()
    }

    fn at(&mut self, height: u64, who: [u8; 32], text: &str) -> u64 {
        let mut h = RuntimeHost::new(&self.kv, &self.t, C, who, height);
        let out = self
            .c
            .call(
                FunctionKind::Mutation,
                "add",
                &(text.to_string(),).encode(),
                &mut h,
                common::GAS,
                common::MEM,
            )
            .unwrap();
        let b = h.finish().unwrap();
        self.kv.commit(b).unwrap();
        u64::decode(&out.output).unwrap()
    }
}

#[test]
fn typed_lookup_pages_in_both_directions_with_cursors() {
    let mut e = Env::new();
    for i in 0..7 {
        e.add(ALICE, &format!("a{i}")); // ids 0, 2, 4, ... interleaved with Bob's
        e.add(BOB, &format!("b{i}"));
    }
    let ids = |rows: &[Item]| rows.iter().map(|r| r.0).collect::<Vec<_>>();

    // Ascending: 3 + 3 + 1 rows, then an empty cursor.
    let (r1, c1) = e.page(ALICE, 3, &[], false);
    assert_eq!(ids(&r1), vec![0, 2, 4]);
    assert!(!c1.is_empty());
    let (r2, c2) = e.page(ALICE, 3, &c1, false);
    assert_eq!(ids(&r2), vec![6, 8, 10]);
    let (r3, c3) = e.page(ALICE, 3, &c2, false);
    assert_eq!(ids(&r3), vec![12]);
    assert!(c3.is_empty());

    // Descending visits the same rows backwards.
    let (d1, dc1) = e.page(ALICE, 4, &[], true);
    assert_eq!(ids(&d1), vec![12, 10, 8, 6]);
    let (d2, dc2) = e.page(ALICE, 4, &dc1, true);
    assert_eq!(ids(&d2), vec![4, 2, 0]);
    assert!(dc2.is_empty());

    // The page is exactly the limit when more rows exist, and `recent` is newest first.
    let (o, _) = e.run(FunctionKind::Query, "recent", BOB, &(2u32,)).unwrap();
    let recent = Vec::<(Address, bool, u64, String)>::decode(&o).unwrap();
    assert_eq!(
        recent.iter().map(|r| r.3.as_str()).collect::<Vec<_>>(),
        vec!["b6", "b5"]
    );
}

#[test]
fn range_lookup_with_filter_uses_the_created_index() {
    let mut e = Env::new();
    e.at(5, ALICE, "early");
    e.at(10, BOB, "bob");
    let late = e.at(10, ALICE, "late");
    let done = e.at(12, ALICE, "done");
    e.at(20, ALICE, "latest");
    e.run(FunctionKind::Mutation, "toggle", ALICE, &(done,))
        .unwrap();
    let (o, _) = e
        .run(FunctionKind::Query, "open_since", ALICE, &(10u64,))
        .unwrap();
    // Not "early" (before 10), not Bob's, not the finished one; ordered by `created`, then id.
    assert_eq!(Vec::<u64>::decode(&o).unwrap(), vec![late, 4]);
}

#[test]
fn result_errors_abort_with_the_message_and_change_nothing() {
    let mut e = Env::new();
    e.add(ALICE, "milk");
    let before: Vec<_> =
        e.kv.entries()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
    let err = e
        .run(FunctionKind::Mutation, "add", ALICE, &("x".repeat(281),))
        .unwrap_err();
    assert_eq!(common::abort_msg(err), "text must be 1..=280 bytes");
    let err = e
        .run(FunctionKind::Mutation, "toggle", ALICE, &(41u64,))
        .unwrap_err();
    assert_eq!(common::abort_msg(err), "no such todo");
    let after: Vec<_> =
        e.kv.entries()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
    assert_eq!(before, after);
}
