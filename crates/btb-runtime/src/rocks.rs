//! RocksDB-backed `KvStore` (cargo feature `rocksdb`).
//!
//! Keys use RocksDB's default bytewise comparator, which is the same order as `MemKv`.

use crate::kv::{BatchOp, Entry, KvError, KvStore, WriteBatch};
use rocksdb::{Direction, IteratorMode, Options, ReadOptions, WriteBatch as RocksBatch, DB};
use std::path::Path;

pub struct RocksKv {
    db: DB,
}

fn err(e: rocksdb::Error) -> KvError {
    KvError(e.to_string())
}

impl RocksKv {
    /// Open (creating if missing) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, KvError> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        Ok(Self {
            db: DB::open(&opts, path).map_err(err)?,
        })
    }
}

impl KvStore for RocksKv {
    fn as_sync(&self) -> Option<&(dyn KvStore + Sync)> {
        Some(self)
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        self.db.get(key).map_err(err)
    }

    fn scan(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<Entry>, KvError> {
        if end.is_some_and(|e| e <= start) || limit == 0 {
            return Ok(Vec::new());
        }
        let mut ro = ReadOptions::default();
        ro.set_iterate_lower_bound(start.to_vec());
        if let Some(e) = end {
            ro.set_iterate_upper_bound(e.to_vec());
        }
        let mode = if reverse {
            IteratorMode::End
        } else {
            IteratorMode::From(start, Direction::Forward)
        };
        let mut out = Vec::new();
        for item in self.db.iterator_opt(mode, ro).take(limit) {
            let (k, v) = item.map_err(err)?;
            out.push((k.into_vec(), v.into_vec()));
        }
        Ok(out)
    }

    fn commit(&mut self, batch: WriteBatch) -> Result<(), KvError> {
        let mut wb = RocksBatch::default();
        for op in batch.ops() {
            match op {
                BatchOp::Put(k, v) => wb.put(k, v),
                BatchOp::Delete(k) => wb.delete(k),
            }
        }
        self.db.write(wb).map_err(err)
    }
}
