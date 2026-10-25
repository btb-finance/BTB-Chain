//! {{name}}: a BTB contract. A per-user note list, read through a secondary index.
//!
//! The `Note` table is indexed by `owner` (declared in Cargo.toml under
//! `[package.metadata.btb]` until contracts declare indexes themselves).
//!
//! `btb build` compiles it, `btb dev` serves it with hot reload. The `add`, `toggle` and
//! `list_mine` names are what the sample page in web/index.html calls.
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::host;
use btb_sdk::prelude::*;

/// One row per note. The host indexes it by `owner` (the first 32 bytes of the row).
#[btb::table]
pub struct Note {
    pub owner: Address,
    pub done: bool,
    pub text: String,
}

/// Add a note for the caller; returns its id.
#[btb::mutation]
pub fn add(text: String) -> u64 {
    if text.is_empty() || text.len() > 280 {
        abort("text must be 1..=280 bytes");
    }
    Note {
        owner: caller(),
        done: false,
        text,
    }
    .insert()
}

/// Flip the `done` flag of one of the caller's notes; returns the new value.
#[btb::mutation]
pub fn toggle(id: u64) -> bool {
    let Some(mut t) = Note::get(id) else {
        abort("no such note");
    };
    if t.owner != caller() {
        abort("not your note");
    }
    t.done = !t.done;
    t.update(id);
    t.done
}

/// The caller's notes as `(id, done, text)`, oldest first, read through the owner index.
#[btb::query]
pub fn list_mine() -> Vec<(u64, bool, String)> {
    let me = caller();
    let mut out = Vec::new();
    let mut cursor = Vec::new();
    loop {
        let page = Note::index_eq("owner", host::Key::Bytes(&me.0), 64, &cursor);
        for id in page.ids {
            if let Some(t) = Note::get(id) {
                out.push((id, t.done, t.text));
            }
        }
        match page.next_cursor {
            Some(c) => cursor = c,
            None => return out,
        }
    }
}
