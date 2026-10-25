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
