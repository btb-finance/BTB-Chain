//! Query engine: by id, full table scan, index equality and index range, with a filter,
//! a limit and cursor pagination in either direction.
//!
//! A cursor is the last key returned (opaque to callers). Passing it back resumes strictly
//! after that key in the scan direction, so pages never overlap or skip rows, even if rows
//! are inserted or deleted between pages. A cursor outside the queried range is clamped.
//! `next_cursor` is `Some` only when at least one more matching row exists.

use crate::kv::{KvError, KvStore};
use crate::layout::{self, IndexValue};
use btb_vm::host::Address;
use std::ops::Bound;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(pub Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub rows: Vec<Row>,
    pub next_cursor: Option<Cursor>,
}

/// Row predicate: id and raw row bytes.
pub type Filter<'a> = &'a dyn Fn(u64, &[u8]) -> bool;

pub struct QueryOptions<'a> {
    pub order: Order,
    pub limit: usize,
    pub cursor: Option<Cursor>,
    /// Keep only rows for which this returns true (runs on the id and raw row bytes).
    pub filter: Option<Filter<'a>>,
}

impl Default for QueryOptions<'_> {
    fn default() -> Self {
        Self {
            order: Order::Asc,
            limit: 100,
            cursor: None,
            filter: None,
        }
    }
}

/// Rows fetched from the store per round trip while scanning.
const CHUNK: usize = 64;

pub fn by_id(
    kv: &impl KvStore,
    contract: &Address,
    table: &[u8],
    id: u64,
) -> Result<Option<Row>, KvError> {
    Ok(kv
        .get(&layout::row_key(contract, table, id))?
        .map(|data| Row { id, data }))
}

/// Every row of the table, ordered by id.
pub fn scan_table(
    kv: &impl KvStore,
    contract: &Address,
    table: &[u8],
    opts: &QueryOptions,
) -> Result<Page, KvError> {
    let prefix = layout::row_prefix(contract, table);
    let end = layout::prefix_successor(&prefix);
    paged(kv, prefix, end, opts, None)
}

/// Rows whose `index` value equals `value`, ordered by row id.
pub fn by_index_eq(
    kv: &impl KvStore,
    contract: &Address,
    table: &[u8],
    index: &str,
    value: &IndexValue,
    opts: &QueryOptions,
) -> Result<Page, KvError> {
    by_index_range(
        kv,
        contract,
        table,
        index,
        Bound::Included(value),
        Bound::Included(value),
        opts,
    )
}

/// Rows whose `index` value lies between `lo` and `hi`, ordered by (value, id).
pub fn by_index_range(
    kv: &impl KvStore,
    contract: &Address,
    table: &[u8],
    index: &str,
    lo: Bound<&IndexValue>,
    hi: Bound<&IndexValue>,
    opts: &QueryOptions,
) -> Result<Page, KvError> {
    let prefix = layout::index_prefix(contract, table, index);
    let with = |v: &IndexValue| {
        let mut k = prefix.clone();
        v.encode_into(&mut k);
        k
    };
    // Entry keys are `enc(value) id`; every key starting with `enc(value)` has that value.
    let start = match lo {
        Bound::Unbounded => prefix.clone(),
        Bound::Included(v) => with(v),
        Bound::Excluded(v) => layout::prefix_successor(&with(v)).unwrap_or_default(),
    };
    let end = match hi {
        Bound::Unbounded => layout::prefix_successor(&prefix),
        Bound::Included(v) => layout::prefix_successor(&with(v)),
        Bound::Excluded(v) => Some(with(v)),
    };
    // An excluded lower bound with no successor means nothing can match.
    if matches!(lo, Bound::Excluded(_)) && start.is_empty() {
        return Ok(Page {
            rows: Vec::new(),
            next_cursor: None,
        });
    }
    paged(kv, start, end, opts, Some((contract, table)))
}

/// Scan `[start, end)` and map entries to rows. For index scans the entry value is empty and
/// the row is loaded by the id in the key.
fn paged(
    kv: &impl KvStore,
    start: Vec<u8>,
    end: Option<Vec<u8>>,
    opts: &QueryOptions,
    via_index: Option<(&Address, &[u8])>,
) -> Result<Page, KvError> {
    let desc = opts.order == Order::Desc;
    let (mut lo, mut hi) = (start, end);
    if let Some(Cursor(c)) = &opts.cursor {
        if desc {
            // Strictly below the cursor.
            if hi.as_ref().is_none_or(|h| c < h) {
                hi = Some(c.clone());
            }
        } else {
            // Strictly above the cursor.
            let mut after = c.clone();
            after.push(0);
            if after > lo {
                lo = after;
            }
        }
    }
    let mut rows = Vec::new();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let want = opts.limit.saturating_add(1);
    'outer: while rows.len() < want && opts.limit > 0 {
        let chunk = kv.scan(&lo, hi.as_deref(), desc, CHUNK)?;
        let full = chunk.len() == CHUNK;
        let Some((last, _)) = chunk.last() else {
            break;
        };
        let last = last.clone();
        for (key, val) in chunk {
            let Some(id) = layout::id_from_key(&key) else {
                continue;
            };
            let data = match via_index {
                Some((contract, table)) => match kv.get(&layout::row_key(contract, table, id))? {
                    Some(d) => d,
                    None => continue,
                },
                None => val,
            };
            if opts.filter.is_some_and(|f| !f(id, &data)) {
                continue;
            }
            rows.push(Row { id, data });
            keys.push(key);
            if rows.len() == want {
                break 'outer;
            }
        }
        if !full {
            break;
        }
        if desc {
            hi = Some(last);
        } else {
            let mut next = last;
            next.push(0);
            lo = next;
        }
    }
    let next_cursor = if rows.len() > opts.limit {
        rows.truncate(opts.limit);
        keys.truncate(opts.limit);
        keys.last().cloned().map(Cursor)
    } else {
        None
    };
    Ok(Page { rows, next_cursor })
}
