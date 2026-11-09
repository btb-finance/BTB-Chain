//! redb-backed `KvStore` (cargo feature `redb`): a pure-Rust alternative to RocksDB, under
//! evaluation in proposal 0003 (phase I).
//!
//! One table of byte keys and values; redb orders `&[u8]` keys bytewise, the same order as
//! `MemKv` and `RocksKv`. Each `commit` is one redb write transaction with
//! `Durability::Immediate` (fsync on commit, crash safe). redb has no compression of its own,
//! so values of [`COMPRESS_MIN`] bytes or more are stored zstd-compressed when that is smaller:
//! the first byte of every stored value says how (`0` raw, `1` zstd). Keys are not compressed
//! (they must keep their order).

use crate::kv::{BatchOp, Entry, KvError, KvStore, WriteBatch};
use redb::{Database, Durability, ReadableDatabase, TableDefinition};
use std::ops::Bound;
use std::path::Path;

type Guard = redb::AccessGuard<'static, &'static [u8]>;

const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");
/// Values shorter than this are stored raw (compression would not pay for its header).
pub const COMPRESS_MIN: usize = 64;
const ZSTD_LEVEL: i32 = 3;
const RAW: u8 = 0;
const ZSTD: u8 = 1;

pub struct RedbKv {
    db: Database,
}

fn err(e: impl std::fmt::Display) -> KvError {
    KvError(e.to_string())
}

fn encode_value(v: &[u8]) -> Vec<u8> {
    if v.len() >= COMPRESS_MIN {
        if let Ok(c) = zstd::bulk::compress(v, ZSTD_LEVEL) {
            if c.len() < v.len() {
                let mut out = Vec::with_capacity(c.len() + 1);
                out.push(ZSTD);
                out.extend_from_slice(&c);
                return out;
            }
        }
    }
    let mut out = Vec::with_capacity(v.len() + 1);
    out.push(RAW);
    out.extend_from_slice(v);
    out
}

fn decode_value(stored: &[u8]) -> Result<Vec<u8>, KvError> {
    match stored.split_first() {
        Some((&RAW, rest)) => Ok(rest.to_vec()),
        Some((&ZSTD, rest)) => {
            // Values are at most what a transaction can carry plus state records; 64 MiB caps
            // a corrupt length field.
            zstd::bulk::decompress(rest, 64 << 20).map_err(err)
        }
        _ => Err(KvError("corrupt stored value".into())),
    }
}

impl RedbKv {
    /// Open (creating if missing) the database file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, KvError> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(err)?;
        }
        let db = Database::create(path).map_err(err)?;
        // Make sure the table exists so reads never fail on a fresh file.
        let tx = db.begin_write().map_err(err)?;
        tx.open_table(TABLE).map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(Self { db })
    }
}

impl KvStore for RedbKv {
    fn as_sync(&self) -> Option<&(dyn KvStore + Sync)> {
        Some(self)
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        let tx = self.db.begin_read().map_err(err)?;
        let t = tx.open_table(TABLE).map_err(err)?;
        match t.get(key).map_err(err)? {
            None => Ok(None),
            Some(v) => decode_value(v.value()).map(Some),
        }
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
        let tx = self.db.begin_read().map_err(err)?;
        let t = tx.open_table(TABLE).map_err(err)?;
        let upper = match end {
            Some(e) => Bound::Excluded(e),
            None => Bound::Unbounded,
        };
        let range = t
            .range::<&[u8]>((Bound::Included(start), upper))
            .map_err(err)?;
        let mut out = Vec::new();
        let items: Box<dyn Iterator<Item = Result<(Guard, Guard), redb::StorageError>>> = if reverse
        {
            Box::new(range.rev())
        } else {
            Box::new(range)
        };
        for item in items.take(limit) {
            let (k, v) = item.map_err(err)?;
            out.push((k.value().to_vec(), decode_value(v.value())?));
        }
        Ok(out)
    }

    fn commit(&mut self, batch: WriteBatch) -> Result<(), KvError> {
        let mut tx = self.db.begin_write().map_err(err)?;
        tx.set_durability(Durability::Immediate).map_err(err)?;
        {
            let mut t = tx.open_table(TABLE).map_err(err)?;
            for op in batch.ops() {
                match op {
                    BatchOp::Put(k, v) => {
                        t.insert(k.as_slice(), encode_value(v).as_slice())
                            .map_err(err)?;
                    }
                    BatchOp::Delete(k) => {
                        t.remove(k.as_slice()).map_err(err)?;
                    }
                }
            }
        }
        tx.commit().map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_raw_and_compressed() {
        for v in [
            vec![],
            vec![7u8; 10],
            vec![0u8; 5000],
            (0..=255u8).collect::<Vec<_>>(),
        ] {
            assert_eq!(decode_value(&encode_value(&v)).unwrap(), v);
        }
        // compressible data is stored compressed
        assert_eq!(encode_value(&[0u8; 5000])[0], ZSTD);
        assert!(decode_value(&[9, 1, 2]).is_err());
        assert!(decode_value(&[]).is_err());
    }
}
