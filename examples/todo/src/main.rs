//! Example contract: a per-user todo list with declared indexes and typed lookups.
//!
//! The `Todo` table declares two indexes with `#[index]`; the host and the chain read them from
//! the schema in the ELF (no `Cargo.toml` metadata). `Todo::by_owner(addr)` and
//! `Todo::by_created_range(a..)` are generated lookups, and `btb codegen --ts` turns the
//! signatures below into a typed TypeScript client (see `web/`).
//!
//! Build: `./build.sh` (writes `todo.elf`).
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::ensure;
use btb_sdk::prelude::*;
use btb_sdk::table::Order;

/// One row per todo item.
#[btb::table]
pub struct Todo {
    /// Whose item this is. Indexed: `Todo::by_owner(addr)`.
    #[index]
    pub owner: Address,
    pub done: bool,
    /// Block height when it was added. Indexed: `Todo::by_created_range(from..)`.
    #[index]
    pub created: u64,
    pub text: String,
}

/// Add an item for the caller; returns its id.
#[btb::mutation]
pub fn add(text: String) -> btb_sdk::Result<u64> {
    ensure!(
        !text.is_empty() && text.len() <= 280,
        "text must be 1..=280 bytes"
    );
    Ok(Todo {
        owner: caller(),
        done: false,
        created: block_height(),
        text,
    }
    .insert())
}

/// Flip the `done` flag of one of the caller's items; returns the new value.
#[btb::mutation]
pub fn toggle(id: u64) -> btb_sdk::Result<bool> {
    let Some(mut t) = Todo::get(id) else {
        btb_sdk::bail!("no such todo");
    };
    ensure!(t.owner == caller(), "not your todo");
    t.done = !t.done;
    t.update(id);
    Ok(t.done)
}

/// The caller's items as `(id, done, text)`, oldest first, read through the owner index.
#[btb::query]
pub fn list_mine() -> Vec<(u64, bool, String)> {
    Todo::by_owner(caller())
        .map(|(id, t)| (id, t.done, t.text))
        .collect()
}

/// The caller's newest `limit` items (at most 100), newest first, as full rows.
#[btb::query]
pub fn recent(limit: u32) -> Vec<Todo> {
    Todo::by_owner(caller())
        .order(Order::Desc)
        .limit(limit.min(100) as usize)
        .map(|(_, t)| t)
        .collect()
}

/// One page of the caller's items, `(rows, cursor)`; pass the returned cursor back for the next
/// page (empty cursor = first page; an empty cursor in the answer means no more).
#[btb::query]
pub fn page_mine(
    limit: u32,
    cursor: Vec<u8>,
    newest_first: bool,
) -> (Vec<(u64, bool, String)>, Vec<u8>) {
    let order = if newest_first {
        Order::Desc
    } else {
        Order::Asc
    };
    let page = Todo::by_owner(caller())
        .order(order)
        .limit(limit.clamp(1, 100) as usize)
        .after(cursor)
        .page();
    let rows = page
        .rows
        .into_iter()
        .map(|(id, t)| (id, t.done, t.text))
        .collect();
    (rows, page.next.unwrap_or_default())
}

/// The caller's unfinished items added at block `from` or later: a range lookup on `created`,
/// then a filter on the owner and the flag.
#[btb::query]
pub fn open_since(from: u64) -> Vec<u64> {
    let me = caller();
    Todo::by_created_range(from..)
        .filter(|(_, t)| t.owner == me && !t.done)
        .map(|(id, _)| id)
        .collect()
}
