//! {{name}}: a BTB contract. A per-user note list, read through a declared index.
//!
//! The `Note` table declares an index on `owner` with `#[index]`: `btb dev` and the chain read it
//! from the contract, and `Note::by_owner(addr)` is the generated lookup. `btb codegen --ts out/`
//! turns the function signatures below into a typed TypeScript client.
//!
//! `btb build` compiles it, `btb dev` serves it with hot reload. The `add`, `toggle` and
//! `list_mine` names are what the sample page in web/index.html calls.
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::ensure;
use btb_sdk::prelude::*;

/// One row per note.
#[btb::table]
pub struct Note {
    #[index]
    pub owner: Address,
    pub done: bool,
    pub text: String,
}

/// Add a note for the caller; returns its id. `Err` aborts with its message.
#[btb::mutation]
pub fn add(text: String) -> btb_sdk::Result<u64> {
    ensure!(
        !text.is_empty() && text.len() <= 280,
        "text must be 1..=280 bytes"
    );
    Ok(Note {
        owner: caller(),
        done: false,
        text,
    }
    .insert())
}

/// Flip the `done` flag of one of the caller's notes; returns the new value.
#[btb::mutation]
pub fn toggle(id: u64) -> btb_sdk::Result<bool> {
    let Some(mut t) = Note::get(id) else {
        btb_sdk::bail!("no such note");
    };
    ensure!(t.owner == caller(), "not your note");
    t.done = !t.done;
    t.update(id);
    Ok(t.done)
}

/// The caller's notes as `(id, done, text)`, oldest first, read through the owner index.
#[btb::query]
pub fn list_mine() -> Vec<(u64, bool, String)> {
    Note::by_owner(caller())
        .map(|(id, t)| (id, t.done, t.text))
        .collect()
}
