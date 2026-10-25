//! Query subscriptions: re-run a query when data it read changes.
//!
//! A subscription stores the query (contract, function, args), the read set of its last run and
//! a fingerprint of its last result. For each committed `WriteBatch`, `affected` returns the
//! subscriptions whose read set contains a written key. `refresh` re-runs one through a
//! caller-supplied closure (so this crate needs no ELF) and reports a change only when the
//! result differs.
//!
//! Complexity, with p point keys and r ranges registered in total, a batch of w keys, and m
//! matches: points are a `BTreeMap` (O(w log p + m)). Ranges are a list sorted by start with a
//! prefix maximum of ends, rebuilt lazily after registrations (O(r log r)), then each written
//! key costs O(log r + m + s), where s is the number of ranges skipped before the prefix
//! maximum rules out the rest (small when ranges are narrow or disjoint, r in the worst case).

use crate::readset::ReadSet;
use std::collections::{BTreeMap, BTreeSet};

pub type SubId = u64;

/// 128-bit non-cryptographic fingerprint of a result, used only to tell "changed" from
/// "unchanged" locally. Two FNV-1a passes with different offsets, plus the length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultHash(u64, u64, u64);

impl ResultHash {
    pub fn of(data: &[u8]) -> Self {
        let pass = |mut h: u64| {
            for b in data {
                h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        };
        Self(
            pass(0xcbf2_9ce4_8422_2325),
            pass(0x9e37_79b9_7f4a_7c15),
            data.len() as u64,
        )
    }
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub contract: [u8; 32],
    pub function: String,
    pub args: Vec<u8>,
    pub reads: ReadSet,
    pub last_hash: ResultHash,
}

/// A re-run that produced a different result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub id: SubId,
    pub result: Vec<u8>,
}

#[derive(Default)]
pub struct SubscriptionManager {
    next_id: SubId,
    subs: BTreeMap<SubId, Subscription>,
    points: BTreeMap<Vec<u8>, BTreeSet<SubId>>,
    /// `(start, end, id)` sorted by start; rebuilt when `dirty`.
    ranges: Vec<(Vec<u8>, Option<Vec<u8>>, SubId)>,
    /// `prefix_max[i]` = largest end among `ranges[..=i]` (`None` = unbounded).
    prefix_max: Vec<Option<Vec<u8>>>,
    dirty: bool,
}

fn end_after(end: &Option<Vec<u8>>, key: &[u8]) -> bool {
    end.as_deref().is_none_or(|e| e > key)
}

impl SubscriptionManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.subs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }
    pub fn get(&self, id: SubId) -> Option<&Subscription> {
        self.subs.get(&id)
    }

    /// Register a query with the read set and result of its first run.
    pub fn subscribe(
        &mut self,
        contract: [u8; 32],
        function: &str,
        args: &[u8],
        reads: ReadSet,
        result: &[u8],
    ) -> SubId {
        let id = self.next_id;
        self.next_id += 1;
        self.index(id, &reads);
        self.subs.insert(
            id,
            Subscription {
                contract,
                function: function.to_string(),
                args: args.to_vec(),
                reads,
                last_hash: ResultHash::of(result),
            },
        );
        id
    }

    pub fn unsubscribe(&mut self, id: SubId) -> bool {
        let Some(sub) = self.subs.remove(&id) else {
            return false;
        };
        self.unindex(id, &sub.reads);
        true
    }

    fn index(&mut self, id: SubId, reads: &ReadSet) {
        for p in reads.points() {
            self.points.entry(p.clone()).or_default().insert(id);
        }
        for r in reads.ranges() {
            self.ranges.push((r.start.clone(), r.end.clone(), id));
            self.dirty = true;
        }
    }

    fn unindex(&mut self, id: SubId, reads: &ReadSet) {
        for p in reads.points() {
            if let Some(set) = self.points.get_mut(p) {
                set.remove(&id);
                if set.is_empty() {
                    self.points.remove(p);
                }
            }
        }
        if reads.ranges().next().is_some() {
            self.ranges.retain(|(_, _, i)| *i != id);
            self.dirty = true;
        }
    }

    fn rebuild(&mut self) {
        if !self.dirty {
            return;
        }
        self.ranges.sort();
        self.prefix_max.clear();
        let mut cur: Option<Vec<u8>> = Some(Vec::new());
        for (i, (_, end, _)) in self.ranges.iter().enumerate() {
            cur = match (&cur, end) {
                (_, None) => None,
                (None, _) => None,
                (Some(c), Some(e)) => Some(if i == 0 || e > c {
                    e.clone()
                } else {
                    c.clone()
                }),
            };
            self.prefix_max.push(cur.clone());
        }
        self.dirty = false;
    }

    fn collect_ranges_for(&self, key: &[u8], out: &mut BTreeSet<SubId>) {
        let n = self.ranges.partition_point(|(s, _, _)| s.as_slice() <= key);
        for i in (0..n).rev() {
            if !end_after(&self.prefix_max[i], key) {
                break;
            }
            if end_after(&self.ranges[i].1, key) {
                out.insert(self.ranges[i].2);
            }
        }
    }

    /// Subscriptions whose read set contains any of `keys`, in id order.
    pub fn affected_by_keys<'k>(&mut self, keys: impl IntoIterator<Item = &'k [u8]>) -> Vec<SubId> {
        self.rebuild();
        let mut out = BTreeSet::new();
        for key in keys {
            if let Some(ids) = self.points.get(key) {
                out.extend(ids.iter().copied());
            }
            self.collect_ranges_for(key, &mut out);
        }
        out.into_iter().collect()
    }

    /// Subscriptions to re-run after `batch` was committed (puts and deletes both count).
    pub fn affected(&mut self, batch: &crate::kv::WriteBatch) -> Vec<SubId> {
        use crate::kv::BatchOp;
        self.affected_by_keys(batch.ops().iter().map(|op| match op {
            BatchOp::Put(k, _) | BatchOp::Delete(k) => k.as_slice(),
        }))
    }

    /// Re-run one subscription. `run` gets the contract, function and args and returns the new
    /// result bytes and read set (or an error, which leaves the subscription untouched).
    /// Returns the new result only if it differs from the last one; the read set is always
    /// updated, since the query may now read other keys.
    pub fn refresh<E>(
        &mut self,
        id: SubId,
        run: impl FnOnce(&[u8; 32], &str, &[u8]) -> Result<(Vec<u8>, ReadSet), E>,
    ) -> Result<Option<Change>, E> {
        let Some(sub) = self.subs.get(&id) else {
            return Ok(None);
        };
        let (result, reads) = run(&sub.contract, &sub.function, &sub.args)?;
        let hash = ResultHash::of(&result);
        let old_reads = std::mem::take(&mut self.subs.get_mut(&id).expect("exists").reads);
        self.unindex(id, &old_reads);
        self.index(id, &reads);
        let sub = self.subs.get_mut(&id).expect("exists");
        sub.reads = reads;
        let changed = sub.last_hash != hash;
        sub.last_hash = hash;
        Ok(changed.then_some(Change { id, result }))
    }

    /// Find the affected subscriptions for a committed batch and re-run each, returning only
    /// those whose result changed. Stops at the first error from `run`.
    pub fn process_commit<E>(
        &mut self,
        batch: &crate::kv::WriteBatch,
        mut run: impl FnMut(&[u8; 32], &str, &[u8]) -> Result<(Vec<u8>, ReadSet), E>,
    ) -> Result<Vec<Change>, E> {
        let mut changes = Vec::new();
        for id in self.affected(batch) {
            if let Some(c) = self.refresh(id, &mut run)? {
                changes.push(c);
            }
        }
        Ok(changes)
    }
}
