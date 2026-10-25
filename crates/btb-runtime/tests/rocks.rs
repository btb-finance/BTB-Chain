#![cfg(feature = "rocksdb")]

use btb_runtime::rocks::RocksKv;
use btb_runtime::{KvStore, MemKv, WriteBatch};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static N: AtomicU32 = AtomicU32::new(0);

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("btb-rocks-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn key(&mut self) -> Vec<u8> {
        // Short keys over a tiny alphabet so prefixes, empty keys and 0x00/0xff collide.
        let len = self.below(4) as usize;
        (0..len)
            .map(|_| [0u8, 1, 0x7f, 0xff][self.below(4) as usize])
            .collect()
    }
}

fn assert_same(m: &MemKv, r: &RocksKv, rng: &mut Rng) {
    for _ in 0..20 {
        let k = rng.key();
        assert_eq!(m.get(&k).unwrap(), r.get(&k).unwrap(), "get {k:?}");
        let s = rng.key();
        let e = if rng.below(4) == 0 {
            None
        } else {
            Some(rng.key())
        };
        let limit = rng.below(8) as usize;
        for rev in [false, true] {
            assert_eq!(
                m.scan(&s, e.as_deref(), rev, limit).unwrap(),
                r.scan(&s, e.as_deref(), rev, limit).unwrap(),
                "scan {s:?}..{e:?} rev={rev} limit={limit}"
            );
        }
    }
}

#[test]
fn matches_memkv_on_random_ops() {
    let dir = TempDir::new();
    let mut r = RocksKv::open(&dir.0).unwrap();
    let mut m = MemKv::new();
    let mut rng = Rng(0x9e3779b97f4a7c15);
    for _ in 0..300 {
        let mut b = WriteBatch::new();
        for _ in 0..rng.below(6) {
            let k = rng.key();
            if rng.below(3) == 0 {
                b.delete(k);
            } else {
                b.put(k, rng.next().to_le_bytes().to_vec());
            }
        }
        m.commit(b.clone()).unwrap();
        r.commit(b).unwrap();
        assert_same(&m, &r, &mut rng);
    }
}

#[test]
fn persists_across_reopen() {
    let dir = TempDir::new();
    {
        let mut r = RocksKv::open(&dir.0).unwrap();
        r.put(b"a", b"1").unwrap();
        r.put(b"b", b"2").unwrap();
        r.delete(b"a").unwrap();
    }
    let r = RocksKv::open(&dir.0).unwrap();
    assert_eq!(r.get(b"a").unwrap(), None);
    assert_eq!(r.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn batch_is_ordered_and_applied_together() {
    let dir = TempDir::new();
    let mut r = RocksKv::open(&dir.0).unwrap();
    r.put(b"x", b"old").unwrap();
    let mut b = WriteBatch::new();
    b.put(b"x".to_vec(), b"new".to_vec());
    b.put(b"y".to_vec(), b"1".to_vec());
    b.delete(b"y".to_vec());
    b.put(b"z".to_vec(), b"3".to_vec());
    r.commit(b).unwrap();
    assert_eq!(r.get(b"x").unwrap(), Some(b"new".to_vec()));
    assert_eq!(r.get(b"y").unwrap(), None);
    assert_eq!(
        r.scan(b"", None, false, 10).unwrap(),
        vec![
            (b"x".to_vec(), b"new".to_vec()),
            (b"z".to_vec(), b"3".to_vec())
        ]
    );
}

// ---- Tables, indexes, queries and deposits: RocksKv must equal MemKv ----

mod tables {
    use super::TempDir;
    use btb_runtime::layout::{self, IndexValue};
    use btb_runtime::rocks::RocksKv;
    use btb_runtime::*;
    use btb_vm::host::Host;
    use std::ops::Bound;

    const C: [u8; 32] = [7; 32];
    const T: &[u8] = b"t";

    /// Row = one score byte followed by a name.
    fn tables() -> Tables {
        let mut t = Tables::new();
        t.register(
            C,
            T,
            vec![
                IndexDef::new("score", |r| {
                    Ok(Some(IndexValue::U64(
                        r.first().copied().unwrap_or(0).into(),
                    )))
                }),
                IndexDef::new("name", |r| {
                    let s = String::from_utf8_lossy(r.get(1..).unwrap_or(&[])).into_owned();
                    Ok((!s.is_empty()).then_some(IndexValue::Str(s)))
                }),
            ],
        )
        .unwrap();
        t
    }

    fn row(score: u8, name: &str) -> Vec<u8> {
        let mut r = vec![score];
        r.extend_from_slice(name.as_bytes());
        r
    }

    fn mutate<K: KvStore>(kv: &mut K, t: &Tables, f: impl FnOnce(&mut RuntimeHost<K>)) {
        let mut h = RuntimeHost::new(kv, t, C, [1; 32], 1);
        f(&mut h);
        let b = h.finish().unwrap();
        kv.commit(b).unwrap();
    }

    fn page(p: &Page) -> String {
        format!(
            "{:?} next={:?}",
            p.rows
                .iter()
                .map(|r| (r.id, r.data.clone()))
                .collect::<Vec<_>>(),
            p.next_cursor
        )
    }

    /// Run a fixed op sequence and return a transcript of every observable result.
    fn run<K: KvStore>(kv: &mut K) -> Vec<String> {
        let t = tables();
        let mut out = Vec::new();
        kv.put(&layout::balance_key(&C), &1_000_000u64.to_le_bytes())
            .unwrap();
        mutate(kv, &t, |h| {
            for (s, n) in [(3, "c"), (1, "a"), (3, "b"), (9, "z"), (5, "")] {
                out.push(format!("insert {}", h.table_insert(T, &row(s, n))));
            }
            h.storage_write(b"k", b"vvvv");
        });
        mutate(kv, &t, |h| {
            out.push(format!("update {}", h.table_update(T, 1, &row(4, "a2"))));
            out.push(format!(
                "update missing {}",
                h.table_update(T, 99, &row(1, "q"))
            ));
            out.push(format!("delete {}", h.table_delete(T, 3)));
            out.push(format!("delete again {}", h.table_delete(T, 3)));
        });
        let get = |k: &[u8], kv: &K| format!("{:?}", kv.get(k).unwrap());
        for key in [
            layout::balance_key(&C),
            layout::usage_key(&C),
            layout::deposit_key(&C),
        ] {
            out.push(get(&key, kv));
        }
        for order in [Order::Asc, Order::Desc] {
            for limit in [1usize, 2, 10] {
                // Walk every page of each query by following the cursor.
                let walk = |f: &dyn Fn(Option<Cursor>) -> Page, out: &mut Vec<String>| {
                    let mut cur = None;
                    loop {
                        let p = f(cur.take());
                        out.push(page(&p));
                        match p.next_cursor {
                            Some(c) => cur = Some(c),
                            None => break,
                        }
                    }
                };
                let o = |cursor| QueryOptions {
                    order,
                    limit,
                    cursor,
                    filter: None,
                };
                walk(&|c| scan_table(kv, &C, T, &o(c)).unwrap(), &mut out);
                walk(
                    &|c| by_index_eq(kv, &C, T, "score", &IndexValue::U64(3), &o(c)).unwrap(),
                    &mut out,
                );
                walk(
                    &|c| {
                        by_index_range(
                            kv,
                            &C,
                            T,
                            "score",
                            Bound::Included(&IndexValue::U64(1)),
                            Bound::Excluded(&IndexValue::U64(9)),
                            &o(c),
                        )
                        .unwrap()
                    },
                    &mut out,
                );
                walk(
                    &|c| {
                        by_index_range(kv, &C, T, "name", Bound::Unbounded, Bound::Unbounded, &o(c))
                            .unwrap()
                    },
                    &mut out,
                );
            }
        }
        out
    }

    #[test]
    fn tables_indexes_queries_and_deposits_match_memkv() {
        let dir = TempDir::new();
        let mut r = RocksKv::open(&dir.0).unwrap();
        let mut m = MemKv::new();
        let (a, b) = (run(&mut m), run(&mut r));
        assert_eq!(a, b);
        assert!(a.len() > 40);
        // Raw keyspace is identical too (rows, index entries, counters, deposit accounting).
        assert_eq!(
            m.scan(b"", None, false, usize::MAX).unwrap(),
            r.scan(b"", None, false, usize::MAX).unwrap()
        );
    }

    #[test]
    fn rows_and_index_entries_persist_across_reopen() {
        let dir = TempDir::new();
        let expected;
        {
            let mut r = RocksKv::open(&dir.0).unwrap();
            run(&mut r);
            expected = r.scan(b"", None, false, usize::MAX).unwrap();
        }
        let r = RocksKv::open(&dir.0).unwrap();
        assert_eq!(r.scan(b"", None, false, usize::MAX).unwrap(), expected);
        assert!(expected.iter().any(|(k, _)| k[0] == layout::TAG_INDEX));
        let o = QueryOptions {
            order: Order::Asc,
            limit: 10,
            cursor: None,
            filter: None,
        };
        let p = by_index_eq(&r, &C, T, "name", &IndexValue::Str("a2".into()), &o).unwrap();
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].data, row(4, "a2"));
        // The old index entry of the updated row and the deleted row are gone.
        let p = by_index_eq(&r, &C, T, "name", &IndexValue::Str("a".into()), &o).unwrap();
        assert!(p.rows.is_empty());
        let p = by_index_eq(&r, &C, T, "score", &IndexValue::U64(3), &o).unwrap();
        assert_eq!(p.rows.len(), 2);
        let p = by_index_eq(&r, &C, T, "score", &IndexValue::U64(9), &o).unwrap();
        assert!(p.rows.is_empty());
    }
}
