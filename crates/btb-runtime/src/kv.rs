//! Ordered byte-key store with atomic batch commits.
//!
//! A RocksDB backend can implement `KvStore` later behind a cargo feature; nothing here
//! assumes more than ordered keys, point reads and an atomic batch write.

use crate::readset::ReadSet;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Bound;

/// Backend failure (I/O, corruption). `MemKv` never fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvError(pub String);

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "kv error: {}", self.0)
    }
}
impl std::error::Error for KvError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOp {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

/// Writes applied together or not at all, in order (a later op on a key wins).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteBatch {
    ops: Vec<BatchOp>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.ops.push(BatchOp::Put(key, value));
    }
    pub fn delete(&mut self, key: Vec<u8>) {
        self.ops.push(BatchOp::Delete(key));
    }
    pub fn ops(&self) -> &[BatchOp] {
        &self.ops
    }
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

/// Key-value pair returned by scans.
pub type Entry = (Vec<u8>, Vec<u8>);

pub trait KvStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError>;

    /// Entries with `start <= key < end` (`end = None`: no upper bound), ascending, or
    /// descending when `reverse`, at most `limit` of them.
    fn scan(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<Entry>, KvError>;

    /// Apply every op atomically.
    fn commit(&mut self, batch: WriteBatch) -> Result<(), KvError>;

    /// This store as a shareable read-only view, if it can be read from several threads at
    /// once (`MemKv`, `RocksKv`). The parallel block executor needs it; stores that return
    /// `None` (the default, e.g. `Overlay`) are executed sequentially.
    fn as_sync(&self) -> Option<&(dyn KvStore + Sync)> {
        None
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvError> {
        let mut b = WriteBatch::new();
        b.put(key.to_vec(), value.to_vec());
        self.commit(b)
    }
    fn delete(&mut self, key: &[u8]) -> Result<(), KvError> {
        let mut b = WriteBatch::new();
        b.delete(key.to_vec());
        self.commit(b)
    }
}

/// In-memory store (BTreeMap).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemKv {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl MemKv {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// All entries in key order (for tests and snapshots).
    pub fn entries(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.map.iter()
    }
}

fn range_scan<'a>(
    iter: impl DoubleEndedIterator<Item = (&'a Vec<u8>, &'a Vec<u8>)>,
    reverse: bool,
    limit: usize,
) -> Vec<Entry> {
    let f = |(k, v): (&Vec<u8>, &Vec<u8>)| (k.clone(), v.clone());
    if reverse {
        iter.rev().take(limit).map(f).collect()
    } else {
        iter.take(limit).map(f).collect()
    }
}

impl KvStore for MemKv {
    fn as_sync(&self) -> Option<&(dyn KvStore + Sync)> {
        Some(self)
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        Ok(self.map.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<Entry>, KvError> {
        if end.is_some_and(|e| e <= start) {
            return Ok(Vec::new());
        }
        let hi = end.map_or(Bound::Unbounded, Bound::Excluded);
        Ok(range_scan(
            self.map.range::<[u8], _>((Bound::Included(start), hi)),
            reverse,
            limit,
        ))
    }

    fn commit(&mut self, batch: WriteBatch) -> Result<(), KvError> {
        for op in batch.ops {
            match op {
                BatchOp::Put(k, v) => {
                    self.map.insert(k, v);
                }
                BatchOp::Delete(k) => {
                    self.map.remove(&k);
                }
            }
        }
        Ok(())
    }
}

/// Uncommitted writes on top of a shared base store. Reads see the writes; the base is
/// untouched until the pending writes are taken with `into_batch` and committed (or dropped
/// to roll back). This is how one contract call stays atomic.
pub struct Overlay<'a, S: KvStore> {
    base: &'a S,
    pending: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    reads: RefCell<ReadSet>,
    /// Previous pending value of every key written while a savepoint is open (see `savepoint`).
    undo: Vec<UndoEntry>,
    open: usize,
}

/// A key and its pending value before a write (`None`: it had no pending entry).
type UndoEntry = (Vec<u8>, Option<Option<Vec<u8>>>);

/// A point in an overlay's write history that [`Overlay::rollback`] can return to.
#[derive(Debug)]
pub struct Savepoint(usize);

impl<'a, S: KvStore> Overlay<'a, S> {
    pub fn new(base: &'a S) -> Self {
        Self {
            base,
            pending: BTreeMap::new(),
            reads: RefCell::new(ReadSet::new()),
            undo: Vec::new(),
            open: 0,
        }
    }

    /// Open a savepoint: writes from now on can be undone with `rollback`, or kept with
    /// `release`. Savepoints nest and must be closed in reverse order. The read set is never
    /// rolled back (a rolled-back call still depended on what it read).
    pub fn savepoint(&mut self) -> Savepoint {
        self.open += 1;
        Savepoint(self.undo.len())
    }

    /// Keep the writes since `sp`.
    pub fn release(&mut self, sp: Savepoint) {
        debug_assert!(self.open > 0 && sp.0 <= self.undo.len());
        self.open = self.open.saturating_sub(1);
        if self.open == 0 {
            self.undo.clear();
        }
    }

    /// Discard every write since `sp`.
    pub fn rollback(&mut self, sp: Savepoint) {
        debug_assert!(self.open > 0 && sp.0 <= self.undo.len());
        while self.undo.len() > sp.0 {
            let Some((k, old)) = self.undo.pop() else {
                break;
            };
            match old {
                None => {
                    self.pending.remove(&k);
                }
                Some(v) => {
                    self.pending.insert(k, v);
                }
            }
        }
        self.open = self.open.saturating_sub(1);
        if self.open == 0 {
            self.undo.clear();
        }
    }

    /// Everything read through this overlay so far (point reads and scanned ranges).
    pub fn read_set(&self) -> ReadSet {
        self.reads.borrow().clone()
    }

    /// Pending writes in key order (`None` = delete), not recorded in the read set.
    pub fn pending(&self) -> impl Iterator<Item = (&Vec<u8>, &Option<Vec<u8>>)> {
        self.pending.iter()
    }

    /// Value of `key` in the base store, ignoring pending writes and the read set.
    pub fn base_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        self.base.get(key)
    }

    /// The pending writes and the read set.
    pub fn into_parts(self) -> (WriteBatch, ReadSet) {
        let reads = self.reads.take();
        (self.into_batch(), reads)
    }

    /// The pending writes as a batch (one op per touched key, in key order).
    pub fn into_batch(self) -> WriteBatch {
        let mut b = WriteBatch::new();
        for (k, v) in self.pending {
            match v {
                Some(v) => b.put(k, v),
                None => b.delete(k),
            }
        }
        b
    }
}

impl<S: KvStore> KvStore for Overlay<'_, S> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        self.reads.borrow_mut().add_point(key);
        match self.pending.get(key) {
            Some(v) => Ok(v.clone()),
            None => self.base.get(key),
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
        let result = self.scan_inner(start, end, reverse, limit)?;
        // A scan cut off by its limit only depends on keys up to its last entry.
        let mut reads = self.reads.borrow_mut();
        match (result.len() == limit, result.last()) {
            (true, Some((last, _))) if reverse => reads.add_range(last, end),
            (true, Some((last, _))) => {
                let mut after = last.clone();
                after.push(0);
                reads.add_range(start, Some(&after));
            }
            _ => reads.add_range(start, end),
        }
        Ok(result)
    }

    fn commit(&mut self, batch: WriteBatch) -> Result<(), KvError> {
        for op in batch.ops {
            let (k, v) = match op {
                BatchOp::Put(k, v) => (k, Some(v)),
                BatchOp::Delete(k) => (k, None),
            };
            if self.open > 0 {
                self.undo.push((k.clone(), self.pending.get(&k).cloned()));
            }
            self.pending.insert(k, v);
        }
        Ok(())
    }
}

impl<S: KvStore> Overlay<'_, S> {
    fn scan_inner(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<Entry>, KvError> {
        let hi = end.map_or(Bound::Unbounded, Bound::Excluded);
        let local: Vec<(&Vec<u8>, &Option<Vec<u8>>)> = self
            .pending
            .range::<[u8], _>((Bound::Included(start), hi))
            .collect();
        // Each pending key can shadow at most one base entry, so this many extra suffices.
        let base = self
            .base
            .scan(start, end, reverse, limit.saturating_add(local.len()))?;
        let mut merged: BTreeMap<Vec<u8>, Vec<u8>> = base.into_iter().collect();
        // A base scan truncated at its limit may miss keys beyond its last entry; pending keys
        // beyond that point must not be merged ahead of unseen base keys.
        let cutoff = if merged.len() >= limit.saturating_add(local.len()) {
            if reverse {
                merged.keys().next().cloned()
            } else {
                merged.keys().next_back().cloned()
            }
        } else {
            None
        };
        for (k, v) in local {
            if let Some(c) = &cutoff {
                let beyond = if reverse { k < c } else { k > c };
                if beyond {
                    continue;
                }
            }
            match v {
                Some(v) => {
                    merged.insert(k.clone(), v.clone());
                }
                None => {
                    merged.remove(k);
                }
            }
        }
        Ok(range_scan(merged.iter(), reverse, limit))
    }
}
