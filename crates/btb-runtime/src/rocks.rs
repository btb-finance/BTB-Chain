//! RocksDB-backed `KvStore` (cargo feature `rocksdb`).
//!
//! Keys use RocksDB's default bytewise comparator, which is the same order as `MemKv`.
//!
//! Storage is compressed (proposal 0003, phase A): zstd on every level and on the write-ahead
//! log, 32 KiB blocks (small blocks compress poorly), and a stronger zstd level with a trained
//! dictionary on the bottom level, where old data settles. Compression is local to the node: it
//! changes nothing in state roots, blocks or consensus, and old uncompressed files stay readable.

use crate::kv::{BatchOp, Entry, KvError, KvStore, WriteBatch};
use rocksdb::{
    BlockBasedOptions, DBCompressionType, Direction, IteratorMode, Options, ReadOptions,
    WriteBatch as RocksBatch, DB,
};
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
        opts.set_compression_type(DBCompressionType::Zstd);
        // window_bits, level, strategy, max_dict_bytes (RocksDB's zstd defaults otherwise)
        opts.set_compression_options(-14, 3, 0, 0);
        opts.set_bottommost_compression_type(DBCompressionType::Zstd);
        opts.set_bottommost_compression_options(-14, 9, 0, 64 << 10, true);
        opts.set_bottommost_zstd_max_train_bytes(4 << 20, true);
        opts.set_wal_compression_type(DBCompressionType::Zstd);
        let mut table = BlockBasedOptions::default();
        table.set_block_size(32 << 10);
        opts.set_block_based_table_factory(&table);
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
