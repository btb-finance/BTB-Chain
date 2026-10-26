//! Tables with secondary indexes. Every write is computed as one `WriteBatch` (row, id
//! counter and all index entries) and committed atomically; index extractors run before
//! anything is written, so a failing extractor changes nothing.

use crate::kv::{KvError, KvStore, WriteBatch};
use crate::layout::{self, IndexValue};
use btb_vm::host::Address;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Maximum indexes per table (they are priced per write, so they are capped).
pub const MAX_INDEXES: usize = 8;

/// Pulls the indexed value out of raw row bytes. `Ok(None)` leaves the row out of the index;
/// `Err` (malformed row) aborts the write.
pub type Extractor = Arc<dyn Fn(&[u8]) -> Result<Option<IndexValue>, String> + Send + Sync>;

#[derive(Clone)]
pub struct IndexDef {
    pub name: String,
    pub extract: Extractor,
    /// At most one row may have a given value (a `#[primary]` key). Checked on insert and update.
    pub unique: bool,
}

impl IndexDef {
    pub fn new(
        name: &str,
        extract: impl Fn(&[u8]) -> Result<Option<IndexValue>, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.to_string(),
            extract: Arc::new(extract),
            unique: false,
        }
    }

    /// Make the index unique: writing a second row with the same value fails.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }
}

/// What [`Tables::rewrite`] did to the index: entries deleted and entries written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rewrite {
    pub deleted: u32,
    pub put: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    Kv(KvError),
    /// An extractor rejected the row.
    Extract {
        index: String,
        message: String,
    },
    /// The table's id counter is exhausted.
    IdOverflow,
    /// Bad schema registration.
    Schema(String),
    /// A write would give a second row the value of a unique (primary) index.
    Duplicate {
        index: String,
    },
}

impl From<KvError> for TableError {
    fn from(e: KvError) -> Self {
        TableError::Kv(e)
    }
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::Kv(e) => write!(f, "{e}"),
            TableError::Extract { index, message } => {
                write!(f, "index {index:?} rejected row: {message}")
            }
            TableError::IdOverflow => write!(f, "table id counter overflow"),
            TableError::Schema(m) => write!(f, "bad schema: {m}"),
            TableError::Duplicate { index } => {
                write!(f, "duplicate value for unique index {index:?}")
            }
        }
    }
}
impl std::error::Error for TableError {}

/// Registered schemas. A table with no registration works with no indexes.
#[derive(Clone, Default)]
pub struct Tables {
    schemas: BTreeMap<(Address, Vec<u8>), Vec<IndexDef>>,
}

type Extracted = Vec<(String, Option<IndexValue>)>;

impl Tables {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) the indexes of `table` in `contract`. Replacing a schema does not
    /// rebuild existing entries.
    pub fn register(
        &mut self,
        contract: Address,
        table: &[u8],
        indexes: Vec<IndexDef>,
    ) -> Result<(), TableError> {
        if indexes.len() > MAX_INDEXES {
            return Err(TableError::Schema(format!(
                "more than {MAX_INDEXES} indexes"
            )));
        }
        for (i, d) in indexes.iter().enumerate() {
            if d.name.is_empty() {
                return Err(TableError::Schema("empty index name".into()));
            }
            if indexes[..i].iter().any(|o| o.name == d.name) {
                return Err(TableError::Schema(format!("duplicate index {:?}", d.name)));
            }
        }
        self.schemas.insert((contract, table.to_vec()), indexes);
        Ok(())
    }

    /// Add every schema of `other` (replacing same-named ones).
    pub fn absorb(&mut self, other: &Tables) {
        for (k, v) in &other.schemas {
            self.schemas.insert(k.clone(), v.clone());
        }
    }

    fn defs(&self, contract: &Address, table: &[u8]) -> &[IndexDef] {
        self.schemas
            .get(&(*contract, table.to_vec()))
            .map_or(&[], Vec::as_slice)
    }

    fn extract(
        &self,
        contract: &Address,
        table: &[u8],
        row: &[u8],
    ) -> Result<Extracted, TableError> {
        self.defs(contract, table)
            .iter()
            .map(|d| {
                (d.extract)(row)
                    .map(|v| (d.name.clone(), v))
                    .map_err(|message| TableError::Extract {
                        index: d.name.clone(),
                        message,
                    })
            })
            .collect()
    }

    /// Fail if another row than `id` already holds a unique index value in `idx`.
    fn check_unique(
        &self,
        kv: &impl KvStore,
        contract: &Address,
        table: &[u8],
        idx: &Extracted,
        id: u64,
        old: Option<&Extracted>,
    ) -> Result<(), TableError> {
        for (i, d) in self.defs(contract, table).iter().enumerate() {
            let Some((_, Some(v))) = idx.get(i) else {
                continue;
            };
            if !d.unique {
                continue;
            }
            if old.is_some_and(|o| o.get(i).map(|x| &x.1) == Some(&Some(v.clone()))) {
                continue;
            }
            let mut prefix = layout::index_prefix(contract, table, &d.name);
            v.encode_into(&mut prefix);
            let Some(end) = layout::prefix_successor(&prefix) else {
                continue;
            };
            // Entries are `prefix || id (8 bytes BE)`; the value encoding is prefix-free.
            let found = kv.scan(&prefix, Some(&end), false, 2)?;
            if found
                .iter()
                .any(|(k, _)| k.len() != prefix.len() + 8 || !k.ends_with(&id.to_be_bytes()))
            {
                return Err(TableError::Duplicate {
                    index: d.name.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn get(
        &self,
        kv: &impl KvStore,
        contract: &Address,
        table: &[u8],
        id: u64,
    ) -> Result<Option<Vec<u8>>, TableError> {
        Ok(kv.get(&layout::row_key(contract, table, id))?)
    }

    /// Insert a row; returns its id (monotonic per table, never reused).
    pub fn insert(
        &self,
        kv: &mut impl KvStore,
        contract: &Address,
        table: &[u8],
        row: &[u8],
    ) -> Result<u64, TableError> {
        let idx = self.extract(contract, table, row)?;
        let counter = layout::counter_key(contract, table);
        let id = match kv.get(&counter)? {
            Some(b) => u64::from_le_bytes(
                b.try_into()
                    .map_err(|_| TableError::Kv(KvError("corrupt table counter".into())))?,
            ),
            None => 0,
        };
        let next = id.checked_add(1).ok_or(TableError::IdOverflow)?;
        self.check_unique(kv, contract, table, &idx, id, None)?;
        let mut batch = WriteBatch::new();
        batch.put(counter, next.to_le_bytes().to_vec());
        batch.put(layout::row_key(contract, table, id), row.to_vec());
        for (name, v) in idx {
            if let Some(v) = v {
                batch.put(
                    layout::index_key(contract, table, &name, &v, id),
                    Vec::new(),
                );
            }
        }
        kv.commit(batch)?;
        Ok(id)
    }

    /// Replace a row, moving its index entries. `Ok(false)` if the id does not exist.
    pub fn update(
        &self,
        kv: &mut impl KvStore,
        contract: &Address,
        table: &[u8],
        id: u64,
        row: &[u8],
    ) -> Result<bool, TableError> {
        let key = layout::row_key(contract, table, id);
        let Some(old) = kv.get(&key)? else {
            return Ok(false);
        };
        let old_idx = self.extract(contract, table, &old)?;
        let new_idx = self.extract(contract, table, row)?;
        self.check_unique(kv, contract, table, &new_idx, id, Some(&old_idx))?;
        let mut batch = WriteBatch::new();
        for ((name, o), (_, n)) in old_idx.into_iter().zip(new_idx) {
            if o == n {
                continue;
            }
            if let Some(o) = o {
                batch.delete(layout::index_key(contract, table, &name, &o, id));
            }
            if let Some(n) = n {
                batch.put(
                    layout::index_key(contract, table, &name, &n, id),
                    Vec::new(),
                );
            }
        }
        batch.put(key, row.to_vec());
        kv.commit(batch)?;
        Ok(true)
    }

    /// Delete a row and its index entries. `Ok(false)` if the id does not exist.
    pub fn delete(
        &self,
        kv: &mut impl KvStore,
        contract: &Address,
        table: &[u8],
        id: u64,
    ) -> Result<bool, TableError> {
        let key = layout::row_key(contract, table, id);
        let Some(old) = kv.get(&key)? else {
            return Ok(false);
        };
        let mut batch = WriteBatch::new();
        for (name, v) in self.extract(contract, table, &old)? {
            if let Some(v) = v {
                batch.delete(layout::index_key(contract, table, &name, &v, id));
            }
        }
        batch.delete(key);
        kv.commit(batch)?;
        Ok(true)
    }

    /// Replace the stored row `id` (bytes `old_row`, whose index entries follow `from`'s
    /// definitions) by `new_row`, whose entries follow `self`'s. Entries of indexes both define
    /// with the same value are left alone; the rest are deleted or written. This is what a
    /// contract upgrade does to each row (`from` = the registry of the old code, `self` = the
    /// new one); with `old_row == new_row` it only adds and removes index entries (backfill).
    ///
    /// Unique indexes are checked against rows with a lower id only: an upgrade rewrites rows in
    /// id order, so the rows above `id` still hold their old entries, and each of them is
    /// checked against this one when its turn comes.
    #[allow(clippy::too_many_arguments)]
    pub fn rewrite(
        &self,
        from: &Tables,
        kv: &mut impl KvStore,
        contract: &Address,
        table: &[u8],
        id: u64,
        old_row: &[u8],
        new_row: &[u8],
    ) -> Result<Rewrite, TableError> {
        let old_idx = from.extract(contract, table, old_row)?;
        let new_idx = self.extract(contract, table, new_row)?;
        for (i, d) in self.defs(contract, table).iter().enumerate() {
            let Some((_, Some(v))) = new_idx.get(i) else {
                continue;
            };
            if !d.unique {
                continue;
            }
            let same = old_idx
                .iter()
                .find(|(n, _)| *n == d.name)
                .is_some_and(|(_, o)| o.as_ref() == Some(v));
            if same {
                continue;
            }
            let mut prefix = layout::index_prefix(contract, table, &d.name);
            v.encode_into(&mut prefix);
            let Some(end) = layout::prefix_successor(&prefix) else {
                continue;
            };
            let found = kv.scan(&prefix, Some(&end), false, 2)?;
            if found.iter().any(|(k, _)| {
                k.len() == prefix.len() + 8 && layout::id_from_key(k).is_some_and(|o| o < id)
            }) {
                return Err(TableError::Duplicate {
                    index: d.name.clone(),
                });
            }
        }
        let mut batch = WriteBatch::new();
        let mut work = Rewrite::default();
        for (name, o) in &old_idx {
            let kept = new_idx.iter().find(|(n, _)| n == name);
            if kept.is_some_and(|(_, n)| n == o) {
                continue;
            }
            if let Some(o) = o {
                batch.delete(layout::index_key(contract, table, name, o, id));
                work.deleted += 1;
            }
        }
        for (name, n) in &new_idx {
            let had = old_idx.iter().find(|(o, _)| o == name);
            if had.is_some_and(|(_, o)| o == n) {
                continue;
            }
            if let Some(n) = n {
                batch.put(layout::index_key(contract, table, name, n, id), Vec::new());
                work.put += 1;
            }
        }
        if old_row != new_row {
            batch.put(layout::row_key(contract, table, id), new_row.to_vec());
        }
        kv.commit(batch)?;
        Ok(work)
    }

    /// Index definitions of a table (names, in registration order).
    pub fn index_names(&self, contract: &Address, table: &[u8]) -> Vec<String> {
        self.defs(contract, table)
            .iter()
            .map(|d| d.name.clone())
            .collect()
    }
}
