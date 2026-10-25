//! `RuntimeHost::table_index_scan`: equality, ranges, order, cursors, overlay visibility and
//! read-set coverage (so subscriptions on index queries fire).

use btb_runtime::{IndexDef, IndexValue, MemKv, RuntimeHost, Tables};
use btb_vm::host::{Host, IndexBound, IndexKey, IndexScan};

const C: [u8; 32] = [7; 32];

fn tables() -> Tables {
    let mut t = Tables::new();
    t.register(
        C,
        b"T",
        vec![IndexDef::new("owner", |row| {
            Ok(Some(IndexValue::Bytes(row[..2].to_vec())))
        })],
    )
    .unwrap();
    t
}

fn scan(lo: IndexBound, hi: IndexBound, desc: bool, limit: u32, cursor: &[u8]) -> IndexScan {
    IndexScan {
        table: b"T".to_vec(),
        index: "owner".into(),
        lo,
        hi,
        descending: desc,
        limit,
        cursor: cursor.to_vec(),
    }
}

fn eq(who: &[u8]) -> (IndexBound, IndexBound) {
    let k = IndexKey::Bytes(who.to_vec());
    (IndexBound::Included(k.clone()), IndexBound::Included(k))
}

#[test]
fn equality_order_paging_and_overlay() {
    let (store, t) = (MemKv::new(), tables());
    let mut h = RuntimeHost::new(&store, &t, C, [0; 32], 1);
    for r in [b"aa1", b"bb2", b"aa3", b"aa4", b"cc5"] {
        h.table_insert(b"T", r);
    }
    // Rows written in this very call are visible.
    let (lo, hi) = eq(b"aa");
    let p = h.table_index_scan(&scan(lo.clone(), hi.clone(), false, 10, b""));
    assert_eq!(p.ids, vec![0, 2, 3]);
    assert!(p.next_cursor.is_empty());

    let p = h.table_index_scan(&scan(lo.clone(), hi.clone(), false, 2, b""));
    assert_eq!(p.ids, vec![0, 2]);
    assert!(!p.next_cursor.is_empty());
    let p2 = h.table_index_scan(&scan(lo.clone(), hi.clone(), false, 2, &p.next_cursor));
    assert_eq!(p2.ids, vec![3]);
    assert!(p2.next_cursor.is_empty());

    let d = h.table_index_scan(&scan(lo, hi, true, 10, b""));
    assert_eq!(d.ids, vec![3, 2, 0]);

    // Range [bb, cc] excluding cc; unknown index and table are empty, not errors.
    let r = h.table_index_scan(&scan(
        IndexBound::Included(IndexKey::Bytes(b"bb".to_vec())),
        IndexBound::Excluded(IndexKey::Bytes(b"cc".to_vec())),
        false,
        10,
        b"",
    ));
    assert_eq!(r.ids, vec![1]);
    let mut s = scan(IndexBound::Unbounded, IndexBound::Unbounded, false, 10, b"");
    s.index = "nope".into();
    assert!(h.table_index_scan(&s).ids.is_empty());
    s.index = "owner".into();
    s.table = b"X".to_vec();
    assert!(h.table_index_scan(&s).ids.is_empty());
}

#[test]
fn scan_is_covered_by_the_read_set() {
    let (mut store, t) = (MemKv::new(), tables());
    // The contract pays a storage deposit.
    btb_runtime::KvStore::put(
        &mut store,
        &btb_runtime::layout::balance_key(&C),
        &1_000_000u64.to_le_bytes(),
    )
    .unwrap();
    // Seed one row for "aa" through a committed call.
    let mut h = RuntimeHost::new(&store, &t, C, [0; 32], 1);
    h.table_insert(b"T", b"aa1");
    let batch = h.finish().unwrap();
    btb_runtime::KvStore::commit(&mut store, batch).unwrap();

    let mut q = RuntimeHost::new(&store, &t, C, [0; 32], 1);
    let (lo, hi) = eq(b"aa");
    assert_eq!(
        q.table_index_scan(&scan(lo, hi, false, 10, b"")).ids,
        vec![0]
    );
    let (_, reads) = q.finish_tracked().unwrap();
    // A new "aa" index entry lands inside the scanned range; a "bb" one does not.
    let inside =
        btb_runtime::layout::index_key(&C, b"T", "owner", &IndexValue::Bytes(b"aa".to_vec()), 9);
    let outside =
        btb_runtime::layout::index_key(&C, b"T", "owner", &IndexValue::Bytes(b"bb".to_vec()), 9);
    assert!(reads.contains_key(&inside));
    assert!(!reads.contains_key(&outside));
}
