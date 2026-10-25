//! Example contract: a per-user todo list, read through a secondary index.
//!
//! The `Todo` table is indexed by `owner` (declared on the host side for now, see
//! docs/DEV_SERVER.md), so `list_mine` reads only the caller's rows.
//!
//! Build: `./build.sh` (writes `todo.elf`).
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::prelude::*;

/// One row per todo item. The host indexes it by `owner` (the first 32 bytes of the row).
#[btb::table]
pub struct Todo {
    pub owner: Address,
    pub done: bool,
    pub text: String,
}

/// Add an item for the caller; returns its id.
#[btb::mutation]
pub fn add(text: String) -> u64 {
    if text.is_empty() || text.len() > 280 {
        abort("text must be 1..=280 bytes");
    }
    Todo {
        owner: caller(),
        done: false,
        text,
    }
    .insert()
}

/// Flip the `done` flag of one of the caller's items; returns the new value.
#[btb::mutation]
pub fn toggle(id: u64) -> bool {
    let Some(mut t) = Todo::get(id) else {
        abort("no such todo");
    };
    if t.owner != caller() {
        abort("not your todo");
    }
    t.done = !t.done;
    t.update(id);
    t.done
}

/// The caller's items as `(id, done, text)`, oldest first, read through the owner index.
#[btb::query]
pub fn list_mine() -> Vec<(u64, bool, String)> {
    let me = caller();
    let mut out = Vec::new();
    let mut cursor = Vec::new();
    loop {
        let page = Todo::index_eq("owner", host::Key::Bytes(&me.0), 64, &cursor);
        for id in page.ids {
            if let Some(t) = Todo::get(id) {
                out.push((id, t.done, t.text));
            }
        }
        match page.next_cursor {
            Some(c) => cursor = c,
            None => return out,
        }
    }
}
