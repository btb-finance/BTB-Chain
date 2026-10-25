//! Typed access to rows, implemented for structs by `#[btb::table]`.

use crate::host::{self, Bound, IndexPage, Key};
use alloc::vec::Vec;
use btb_codec::{Decode, Encode};

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
