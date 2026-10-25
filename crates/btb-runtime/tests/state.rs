use btb_runtime::layout::{self, IndexValue};
use btb_runtime::*;
use btb_vm::host::Host;
use std::ops::Bound;

const C: [u8; 32] = [7; 32];
const T: &[u8] = b"msgs";

/// Row = u64 LE "score" (i64 would work too) followed by a UTF-8 name.
fn row(score: u64, name: &str) -> Vec<u8> {
    let mut r = score.to_le_bytes().to_vec();
    r.extend_from_slice(name.as_bytes());
    r
}

fn tables() -> Tables {
    let mut t = Tables::new();
    t.register(
        C,
        T,
        vec![
            IndexDef::new("score", |r| {
                let b: [u8; 8] = r.get(..8).ok_or("short row")?.try_into().unwrap();
                Ok(Some(IndexValue::U64(u64::from_le_bytes(b))))
            }),
            IndexDef::new("name", |r| {
                let s = std::str::from_utf8(r.get(8..).ok_or("short row")?)
                    .map_err(|e| e.to_string())?;
                // Empty names stay out of the index.
                Ok((!s.is_empty()).then(|| IndexValue::Str(s.to_string())))
            }),
        ],
    )
    .unwrap();
    t
}

fn index_entries(kv: &MemKv) -> usize {
    kv.entries()
        .filter(|(k, _)| k[0] == layout::TAG_INDEX)
        .count()
}

fn ids(p: &Page) -> Vec<u64> {
    p.rows.iter().map(|r| r.id).collect()
}

fn opts(limit: usize, order: Order, cursor: Option<Cursor>) -> QueryOptions<'static> {
    QueryOptions {
        order,
        limit,
        cursor,
        filter: None,
    }
}

#[test]
fn encodings_preserve_order() {
    let sorted = |vals: Vec<IndexValue>| {
        let enc: Vec<_> = vals.iter().map(IndexValue::encode).collect();
        assert!(enc.windows(2).all(|w| w[0] < w[1]), "{vals:?}");
    };
    sorted(
        vec![0u64, 1, 255, 256, u64::MAX]
            .into_iter()
            .map(IndexValue::U64)
            .collect(),
    );
    sorted(
        vec![i64::MIN, -1, 0, 1, i64::MAX]
            .into_iter()
            .map(IndexValue::I64)
            .collect(),
    );
    sorted(
        vec![0u128, 1, 1 << 64, u128::MAX]
            .into_iter()
            .map(IndexValue::U128)
            .collect(),
    );
    sorted(
        [
            &b""[..],
            &[0],
            &[0, 0],
            &[0, 1],
            &[1],
            b"a",
            b"a\0",
            b"ab",
            b"b",
        ]
        .into_iter()
        .map(|b| IndexValue::Bytes(b.to_vec()))
        .collect(),
    );
    sorted(
        ["", "a", "aa", "b"]
            .into_iter()
            .map(|s| IndexValue::Str(s.into()))
            .collect(),
    );
}

#[test]
fn keys_are_ordered_and_separated() {
    let k = |id| layout::row_key(&C, T, id);
    assert!(k(1) < k(2) && k(255) < k(256) && k(u64::MAX - 1) < k(u64::MAX));
    // Table "a" rows never fall inside table "ab" rows' prefix range and vice versa.
    let pa = layout::row_prefix(&C, b"a");
    let pab = layout::row_prefix(&C, b"ab");
    assert!(!pab.starts_with(&pa) && !pa.starts_with(&pab));
    assert_eq!(layout::id_from_key(&k(42)), Some(42));
    assert_eq!(layout::prefix_successor(&[1, 0xFF]), Some(vec![2]));
    assert_eq!(layout::prefix_successor(&[0xFF]), None);
    // Namespaces never collide.
    let tags = [
        layout::storage_key(&C, b"")[0],
        k(0)[0],
        layout::counter_key(&C, T)[0],
        layout::index_prefix(&C, T, "i")[0],
        layout::balance_key(&C)[0],
    ];
    let mut s = tags.to_vec();
    s.sort();
    s.dedup();
    assert_eq!(s.len(), 5);
}

#[test]
fn index_maintenance_has_no_stale_entries() {
    let (t, mut kv) = (tables(), MemKv::new());
    let a = t.insert(&mut kv, &C, T, &row(10, "alice")).unwrap();
    let b = t.insert(&mut kv, &C, T, &row(20, "")).unwrap();
    assert_eq!((a, b), (0, 1));
    assert_eq!(index_entries(&kv), 3); // b has no name entry
    assert_eq!(t.get(&kv, &C, T, a).unwrap(), Some(row(10, "alice")));

    assert!(t.update(&mut kv, &C, T, a, &row(30, "bob")).unwrap());
    assert_eq!(index_entries(&kv), 3);
    let p = by_index_eq(
        &kv,
        &C,
        T,
        "score",
        &IndexValue::U64(10),
        &opts(10, Order::Asc, None),
    )
    .unwrap();
    assert!(p.rows.is_empty());
    let p = by_index_eq(
        &kv,
        &C,
        T,
        "name",
        &IndexValue::Str("bob".into()),
        &opts(10, Order::Asc, None),
    )
    .unwrap();
    assert_eq!(ids(&p), [a]);

    // Entry appears when the field becomes indexable, and goes when it stops.
    assert!(t.update(&mut kv, &C, T, b, &row(20, "carol")).unwrap());
    assert_eq!(index_entries(&kv), 4);
    assert!(t.update(&mut kv, &C, T, b, &row(20, "")).unwrap());
    assert_eq!(index_entries(&kv), 3);

    assert!(t.delete(&mut kv, &C, T, a).unwrap());
    assert!(t.delete(&mut kv, &C, T, b).unwrap());
    assert!(!t.delete(&mut kv, &C, T, b).unwrap());
    assert!(!t.update(&mut kv, &C, T, 99, &row(1, "x")).unwrap());
    assert_eq!(index_entries(&kv), 0);
    // Only the id counter remains, and ids are never reused.
    assert_eq!(kv.len(), 1);
    assert_eq!(t.insert(&mut kv, &C, T, &row(1, "z")).unwrap(), 2);
}

#[test]
fn range_queries() {
    let (t, mut kv) = (tables(), MemKv::new());
    for s in [5u64, 1, 9, 3, 7, 3] {
        t.insert(&mut kv, &C, T, &row(s, "n")).unwrap();
    }
    let q = |lo, hi, order| {
        let p = by_index_range(&kv, &C, T, "score", lo, hi, &opts(100, order, None)).unwrap();
        p.rows
            .iter()
            .map(|r| u64::from_le_bytes(r.data[..8].try_into().unwrap()))
            .collect::<Vec<_>>()
    };
    let (three, seven) = (IndexValue::U64(3), IndexValue::U64(7));
    assert_eq!(
        q(Bound::Unbounded, Bound::Unbounded, Order::Asc),
        [1, 3, 3, 5, 7, 9]
    );
    assert_eq!(
        q(Bound::Included(&three), Bound::Included(&seven), Order::Asc),
        [3, 3, 5, 7]
    );
    assert_eq!(
        q(Bound::Excluded(&three), Bound::Excluded(&seven), Order::Asc),
        [5]
    );
    assert_eq!(
        q(Bound::Included(&three), Bound::Unbounded, Order::Desc),
        [9, 7, 5, 3, 3]
    );
    assert_eq!(
        q(Bound::Unbounded, Bound::Excluded(&three), Order::Asc),
        [1]
    );
    // Equality returns ties ordered by id.
    let p = by_index_eq(&kv, &C, T, "score", &three, &opts(10, Order::Asc, None)).unwrap();
    assert_eq!(ids(&p), [3, 5]);
    // Filter runs on row bytes.
    let f = |id: u64, _: &[u8]| id != 3;
    let o = QueryOptions {
        filter: Some(&f),
        ..opts(10, Order::Asc, None)
    };
    assert_eq!(
        ids(&by_index_eq(&kv, &C, T, "score", &three, &o).unwrap()),
        [5]
    );
    assert_eq!(by_id(&kv, &C, T, 2).unwrap().unwrap().data, row(9, "n"));
    assert!(by_id(&kv, &C, T, 50).unwrap().is_none());
}

fn collect_pages(
    kv: &MemKv,
    order: Order,
    limit: usize,
    f: Option<btb_runtime::query::Filter>,
) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let o = QueryOptions {
            order,
            limit,
            cursor: cursor.clone(),
            filter: f,
        };
        let p = scan_table(kv, &C, T, &o).unwrap();
        assert!(p.rows.len() <= limit);
        out.extend(ids(&p));
        match p.next_cursor {
            Some(c) => {
                assert_eq!(p.rows.len(), limit, "cursor only when more rows exist");
                cursor = Some(c);
            }
            None => return out,
        }
    }
}

#[test]
fn pagination_has_no_duplicates_or_gaps() {
    let (t, mut kv) = (tables(), MemKv::new());
    for i in 0..200u64 {
        t.insert(&mut kv, &C, T, &row(i % 7, "n")).unwrap();
    }
    // A second table must not leak in.
    t.insert(&mut kv, &C, b"other", b"x").unwrap();
    let all: Vec<u64> = (0..200).collect();
    for limit in [1, 7, 64, 100, 199, 200, 500] {
        assert_eq!(
            collect_pages(&kv, Order::Asc, limit, None),
            all,
            "asc {limit}"
        );
        let mut rev = all.clone();
        rev.reverse();
        assert_eq!(
            collect_pages(&kv, Order::Desc, limit, None),
            rev,
            "desc {limit}"
        );
    }
    // Sparse filter across chunk boundaries.
    let f = |id: u64, _: &[u8]| id % 50 == 49;
    assert_eq!(
        collect_pages(&kv, Order::Asc, 2, Some(&f)),
        [49, 99, 149, 199]
    );
    assert_eq!(
        collect_pages(&kv, Order::Desc, 3, Some(&f)),
        [199, 149, 99, 49]
    );

    // Paged index query: score 3 rows, descending, two at a time.
    let mut got = Vec::new();
    let mut cursor = None;
    loop {
        let o = opts(2, Order::Desc, cursor);
        let p = by_index_eq(&kv, &C, T, "score", &IndexValue::U64(3), &o).unwrap();
        got.extend(ids(&p));
        cursor = p.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let want: Vec<u64> = (0..200).filter(|i| i % 7 == 3).rev().collect();
    assert_eq!(got, want);

    // Empty results and limit 0.
    assert!(scan_table(&kv, &C, b"nope", &opts(5, Order::Asc, None))
        .unwrap()
        .rows
        .is_empty());
    let p = scan_table(&kv, &C, T, &opts(0, Order::Asc, None)).unwrap();
    assert!(p.rows.is_empty() && p.next_cursor.is_none());
}

#[test]
fn cursor_survives_concurrent_changes() {
    let (t, mut kv) = (tables(), MemKv::new());
    for _ in 0..10 {
        t.insert(&mut kv, &C, T, &row(1, "n")).unwrap();
    }
    let p = scan_table(&kv, &C, T, &opts(4, Order::Asc, None)).unwrap();
    t.delete(&mut kv, &C, T, 3).unwrap(); // the last row of page 1, i.e. the cursor row
    let p2 = scan_table(&kv, &C, T, &opts(4, Order::Asc, p.next_cursor)).unwrap();
    assert_eq!(ids(&p2), [4, 5, 6, 7]);
}

#[test]
fn failed_writes_leave_nothing_behind() {
    let (t, mut kv) = (tables(), MemKv::new());
    t.insert(&mut kv, &C, T, &row(1, "ok")).unwrap();
    let before = kv.clone();
    // Short row: the first extractor fails.
    assert!(matches!(
        t.insert(&mut kv, &C, T, b"abc"),
        Err(TableError::Extract { .. })
    ));
    // Invalid UTF-8: the first index would succeed, the second fails.
    let mut bad = row(5, "");
    bad.push(0xFF);
    assert!(matches!(
        t.insert(&mut kv, &C, T, &bad),
        Err(TableError::Extract { ref index, .. }) if index == "name"
    ));
    assert!(t.update(&mut kv, &C, T, 0, &bad).is_err());
    assert_eq!(kv, before);
    // Counter was not advanced by the failures.
    assert_eq!(t.insert(&mut kv, &C, T, &row(2, "next")).unwrap(), 1);
}

#[test]
fn overlay_isolates_and_merges() {
    let mut base = MemKv::new();
    for k in [1u8, 3, 5, 7] {
        base.put(&[k], &[k]).unwrap();
    }
    let snapshot = base.clone();
    let mut o = Overlay::new(&base);
    o.put(&[4], &[4]).unwrap();
    o.delete(&[5]).unwrap();
    o.put(&[1], &[9]).unwrap();
    assert_eq!(o.get(&[1]).unwrap(), Some(vec![9]));
    assert_eq!(o.get(&[5]).unwrap(), None);
    let keys = |v: Vec<(Vec<u8>, Vec<u8>)>| v.into_iter().map(|(k, _)| k[0]).collect::<Vec<_>>();
    assert_eq!(keys(o.scan(&[0], None, false, 10).unwrap()), [1, 3, 4, 7]);
    assert_eq!(keys(o.scan(&[0], None, true, 10).unwrap()), [7, 4, 3, 1]);
    assert_eq!(keys(o.scan(&[0], None, false, 2).unwrap()), [1, 3]);
    assert_eq!(keys(o.scan(&[0], None, true, 2).unwrap()), [7, 4]);
    assert_eq!(keys(o.scan(&[2], Some(&[7]), false, 10).unwrap()), [3, 4]);
    let batch = o.into_batch();
    assert_eq!(base, snapshot); // untouched until committed
    base.commit(batch).unwrap();
    assert_eq!(
        keys(base.scan(&[0], None, false, 10).unwrap()),
        [1, 3, 4, 7]
    );
}

#[test]
fn host_is_atomic_and_reports_faults() {
    let (t, mut kv) = (tables(), MemKv::new());
    kv.put(&layout::balance_key(&C), &100u64.to_le_bytes())
        .unwrap();
    let before = kv.clone();

    let mut h = RuntimeHost::new(&kv, &t, C, [1; 32], 5);
    h.storage_write(b"k", b"v");
    assert_eq!(h.table_insert(T, &row(1, "a")), 0);
    assert!(h.table_update(T, 0, &row(2, "b")));
    assert_eq!(h.table_get(T, 0), Some(row(2, "b")));
    assert_eq!(h.table_scan(T, 0, 10), [0]);
    assert!(h.token_transfer(&[9; 32], 40));
    assert!(!h.token_transfer(&[9; 32], 61));
    assert_eq!(h.token_balance(&[9; 32]), 40);
    assert_eq!((h.caller(), h.block_height()), ([1; 32], 5));
    let batch = h.finish().unwrap();
    assert_eq!(kv, before); // nothing committed yet
    kv.commit(batch).unwrap();
    assert_eq!(
        kv.get(&layout::storage_key(&C, b"k")).unwrap(),
        Some(b"v".to_vec())
    );
    assert_eq!(index_entries(&kv), 2);

    // A bad row sets a fault; finish refuses to hand out a batch.
    let mut h = RuntimeHost::new(&kv, &t, C, [1; 32], 6);
    h.storage_write(b"k2", b"v");
    h.table_insert(T, b"abc");
    assert!(matches!(
        h.finish(),
        Err(Fault::Table(TableError::Extract { .. }))
    ));
}
