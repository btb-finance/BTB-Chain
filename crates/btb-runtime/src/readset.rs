//! Read sets: the exact keys a call read and the key ranges it scanned.
//!
//! Recorded by `Overlay` (every `get` and `scan`). A scan that stopped at its limit only
//! covers the keys up to the last entry it returned, so a write beyond that point does not
//! invalidate it.

use std::collections::BTreeSet;

/// Half-open key range `[start, end)`; `end = None` means no upper bound.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct KeyRange {
    pub start: Vec<u8>,
    pub end: Option<Vec<u8>>,
}

impl KeyRange {
    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start.as_slice() && self.end.as_deref().is_none_or(|e| key < e)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadSet {
    points: BTreeSet<Vec<u8>>,
    ranges: BTreeSet<KeyRange>,
}

impl ReadSet {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add_point(&mut self, key: &[u8]) {
        if !self.points.contains(key) {
            self.points.insert(key.to_vec());
        }
    }
    /// Empty ranges are ignored.
    pub fn add_range(&mut self, start: &[u8], end: Option<&[u8]>) {
        if end.is_some_and(|e| e <= start) {
            return;
        }
        self.ranges.insert(KeyRange {
            start: start.to_vec(),
            end: end.map(<[u8]>::to_vec),
        });
    }
    pub fn points(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.points.iter()
    }
    pub fn ranges(&self) -> impl Iterator<Item = &KeyRange> {
        self.ranges.iter()
    }
    pub fn is_empty(&self) -> bool {
        self.points.is_empty() && self.ranges.is_empty()
    }
    pub fn merge(&mut self, other: &ReadSet) {
        self.points.extend(other.points.iter().cloned());
        self.ranges.extend(other.ranges.iter().cloned());
    }

    /// Whether a write to `key` would change what this call saw.
    /// O(log p + r) for p points and r ranges.
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.points.contains(key) || self.ranges.iter().any(|r| r.contains(key))
    }
}
