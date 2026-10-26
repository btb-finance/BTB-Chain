//! Typed access to rows, implemented for structs by `#[btb::table]`.

use crate::host::{self, Bound, IndexPage, Key};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use btb_codec::{Decode, Encode};
use core::marker::PhantomData;

/// A table of rows. The table name is the struct name.
pub trait Table: Encode + Decode {
    const NAME: &'static str;

    /// Insert as a new row and return its id.
    fn insert(&self) -> u64 {
        host::table_insert(Self::NAME.as_bytes(), &self.encode())
    }

    /// Load row `id`; aborts if the stored bytes do not decode (corrupt row).
    fn get(id: u64) -> Option<Self> {
        let bytes = host::table_get(Self::NAME.as_bytes(), id)?;
        match Self::decode(&bytes) {
            Ok(v) => Some(v),
            Err(_) => host::abort("corrupt table row"),
        }
    }

    /// Overwrite row `id`; false if it does not exist.
    fn update(&self, id: u64) -> bool {
        host::table_update(Self::NAME.as_bytes(), id, &self.encode())
    }

    fn delete(id: u64) -> bool {
        host::table_delete(Self::NAME.as_bytes(), id)
    }

    /// Up to `max` ascending row ids >= `start_id`.
    fn scan(start_id: u64, max: usize) -> Vec<u64> {
        host::table_scan(Self::NAME.as_bytes(), start_id, max)
    }

    /// Every row (in id order) for which `pred` is true, with its id. This reads the whole
    /// table, so it costs gas per row: prefer a lookup on an index (`Todo::by_owner(..)`) and
    /// filter its rows if only part of them matter.
    fn filter<F: FnMut(&Self) -> bool>(pred: F) -> Filter<Self, F> {
        Filter {
            next_id: 0,
            buf: VecDeque::new(),
            done: false,
            pred,
            _row: PhantomData,
        }
    }

    /// Rows whose index `index` equals `key`, ordered by id, at most `limit` per page. Pass the
    /// previous page's `next_cursor` (or empty) as `cursor`. The index must be declared on the
    /// host side.
    fn index_eq(index: &str, key: Key<'_>, limit: u32, cursor: &[u8]) -> IndexPage {
        host::table_index_scan(
            Self::NAME.as_bytes(),
            index,
            Bound::Included(key),
            Bound::Included(key),
            false,
            limit,
            cursor,
        )
    }
}

/// Rows of [`Table::filter`].
pub struct Filter<T: Table, F> {
    next_id: u64,
    buf: VecDeque<u64>,
    done: bool,
    pred: F,
    _row: PhantomData<T>,
}

impl<T: Table, F: FnMut(&T) -> bool> Iterator for Filter<T, F> {
    type Item = (u64, T);

    fn next(&mut self) -> Option<(u64, T)> {
        loop {
            if let Some(id) = self.buf.pop_front() {
                if let Some(row) = T::get(id) {
                    if (self.pred)(&row) {
                        return Some((id, row));
                    }
                }
                continue;
            }
            if self.done {
                return None;
            }
            let ids = T::scan(self.next_id, PAGE);
            match ids.last() {
                Some(&last) => match last.checked_add(1) {
                    Some(n) => self.next_id = n,
                    None => self.done = true,
                },
                None => self.done = true,
            }
            if ids.len() < PAGE {
                self.done = true;
            }
            self.buf.extend(ids);
        }
    }
}

/// Rows fetched per host call while iterating a [`Lookup`] or [`Table::filter`].
const PAGE: usize = 64;

/// An owned index value (a lookup is built from borrowed `Key`s but outlives them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnedKey {
    U64(u64),
    I64(i64),
    U128(u128),
    Bytes(Vec<u8>),
    Str(String),
}

impl OwnedKey {
    pub fn of(k: Key<'_>) -> Self {
        match k {
            Key::U64(v) => OwnedKey::U64(v),
            Key::I64(v) => OwnedKey::I64(v),
            Key::U128(v) => OwnedKey::U128(v),
            Key::Bytes(v) => OwnedKey::Bytes(v.to_vec()),
            Key::Str(v) => OwnedKey::Str(String::from(v)),
        }
    }

    fn key(&self) -> Key<'_> {
        match self {
            OwnedKey::U64(v) => Key::U64(*v),
            OwnedKey::I64(v) => Key::I64(*v),
            OwnedKey::U128(v) => Key::U128(*v),
            OwnedKey::Bytes(v) => Key::Bytes(v),
            OwnedKey::Str(v) => Key::Str(v),
        }
    }
}

/// One end of a lookup range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnedBound {
    Unbounded,
    Included(OwnedKey),
    Excluded(OwnedKey),
}

impl OwnedBound {
    fn bound(&self) -> Bound<'_> {
        match self {
            OwnedBound::Unbounded => Bound::Unbounded,
            OwnedBound::Included(k) => Bound::Included(k.key()),
            OwnedBound::Excluded(k) => Bound::Excluded(k.key()),
        }
    }
}

/// Direction of a lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// A page of rows and where the next page starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    /// `(id, row)`.
    pub rows: Vec<(u64, T)>,
    /// Pass to [`Lookup::after`] for the next page; `None` after the last page. Plain bytes, so a
    /// query can return it to the client.
    pub next: Option<Vec<u8>>,
}

/// A read of one index, generated as `Todo::by_owner(addr)` and `Todo::by_created_range(a..b)`.
///
/// Configure with [`order`](Lookup::order), [`limit`](Lookup::limit) and
/// [`after`](Lookup::after), then iterate (rows are fetched 64 at a time) or take one
/// [`page`](Lookup::page). The item is `(row id, row)`. Filter with `Iterator::filter`.
///
/// ```ignore
/// let page = Todo::by_owner(caller()).order(Order::Desc).limit(20).after(cursor).page();
/// ```
pub struct Lookup<T: Table> {
    index: &'static str,
    lo: OwnedBound,
    hi: OwnedBound,
    order: Order,
    limit: Option<usize>,
    cursor: Vec<u8>,
    buf: VecDeque<u64>,
    /// Cursor returned by the last host page; `None` once the index is exhausted.
    next: Option<Vec<u8>>,
    started: bool,
    yielded: usize,
    _row: PhantomData<T>,
}

impl<T: Table> Lookup<T> {
    /// Rows whose index value equals `key`.
    pub fn eq(index: &'static str, key: Key<'_>) -> Self {
        let k = OwnedKey::of(key);
        Self::range(
            index,
            OwnedBound::Included(k.clone()),
            OwnedBound::Included(k),
        )
    }

    /// Rows whose index value is between `lo` and `hi`.
    pub fn range(index: &'static str, lo: OwnedBound, hi: OwnedBound) -> Self {
        Lookup {
            index,
            lo,
            hi,
            order: Order::Asc,
            limit: None,
            cursor: Vec::new(),
            buf: VecDeque::new(),
            next: None,
            started: false,
            yielded: 0,
            _row: PhantomData,
        }
    }

    /// Ascending (default) or descending by index value, then by id.
    pub fn order(mut self, order: Order) -> Self {
        self.order = order;
        self
    }

    /// At most `n` rows in total.
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Continue after a cursor from [`Page::next`] or [`Lookup::cursor`] of an earlier lookup
    /// with the same bounds and order.
    pub fn after(mut self, cursor: impl AsRef<[u8]>) -> Self {
        self.cursor = cursor.as_ref().to_vec();
        self
    }

    /// Where to continue after the rows read so far: exact once the iterator ended (limit
    /// reached or index exhausted); `None` if the index has no more rows.
    pub fn cursor(&self) -> Option<Vec<u8>> {
        if self.started && self.next.is_none() {
            None
        } else if self.started {
            self.next.clone()
        } else {
            Some(self.cursor.clone())
        }
    }

    fn fetch(&mut self, want: u32) -> IndexPage {
        host::table_index_scan(
            T::NAME.as_bytes(),
            self.index,
            self.lo.bound(),
            self.hi.bound(),
            self.order == Order::Desc,
            want,
            &self.cursor,
        )
    }

    /// One page of at most `limit` rows (default 64, at most 256) in one host call.
    pub fn page(mut self) -> Page<T> {
        let want = self.limit.unwrap_or(PAGE).min(256) as u32;
        let page = self.fetch(want);
        let rows = page
            .ids
            .into_iter()
            .filter_map(|id| T::get(id).map(|r| (id, r)))
            .collect();
        Page {
            rows,
            next: page.next_cursor,
        }
    }
}

impl<T: Table> Iterator for Lookup<T> {
    type Item = (u64, T);

    fn next(&mut self) -> Option<(u64, T)> {
        loop {
            if self.limit.is_some_and(|l| self.yielded >= l) {
                return None;
            }
            if let Some(id) = self.buf.pop_front() {
                if let Some(row) = T::get(id) {
                    self.yielded += 1;
                    return Some((id, row));
                }
                continue;
            }
            if self.started && self.next.is_none() {
                return None;
            }
            // Never fetch more than the rows still wanted, so the cursor lands right after the
            // last row the caller can see.
            let left = self.limit.map_or(PAGE, |l| l - self.yielded);
            let page = self.fetch(left.min(PAGE) as u32);
            self.started = true;
            if let Some(c) = &page.next_cursor {
                self.cursor = c.clone();
            }
            self.next = page.next_cursor;
            self.buf.extend(page.ids);
            if self.buf.is_empty() && self.next.is_none() {
                return None;
            }
        }
    }
}
