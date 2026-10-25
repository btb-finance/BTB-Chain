//! State database for BTB Chain (Phase 2): an ordered key-value store, a deterministic key
//! layout, tables with secondary indexes, a query engine, and a `btb_vm::Host` over it.
//!
//! Deterministic: no floats, no system time, no hash-map iteration order in anything that
//! reaches state.

pub mod host;
pub mod kv;
pub mod layout;
pub mod query;
pub mod tables;

pub use host::{Fault, RuntimeHost};
pub use kv::{KvError, KvStore, MemKv, Overlay, WriteBatch};
pub use layout::IndexValue;
pub use query::{
    by_id, by_index_eq, by_index_range, scan_table, Cursor, Order, Page, QueryOptions, Row,
};
pub use tables::{IndexDef, TableError, Tables};
