//! Test contract for contract upgrades and schema migrations. One source, several versions
//! chosen by Cargo feature (see Cargo.toml); the tests in `crates/btb-tests` deploy one and
//! upgrade to another.
#![no_std]
#![no_main]

use btb_sdk::btb;
use btb_sdk::prelude::*;

// ---------------------------------------------------------------- v1 and compatible changes
#[cfg(not(any(
    feature = "compat",
    feature = "migrate",
    feature = "fail",
    feature = "wrongprev",
    feature = "bad"
)))]
#[btb::table]
pub struct Todo {
    #[index]
    pub owner: Address,
    pub done: bool,
    pub text: String,
}

#[cfg(feature = "compat")]
#[btb::table]
pub struct Todo {
    #[index]
    pub owner: Address,
    #[index]
    pub done: bool,
    pub text: String,
    pub note: Option<String>,
}

#[cfg(feature = "bad")]
#[btb::table]
pub struct Todo {
    #[index]
    pub owner: Address,
    pub done: u8,
    pub text: String,
}

#[cfg(feature = "compat")]
#[btb::table]
pub struct Tag {
    #[index]
    pub name: String,
}

// ---------------------------------------------------------------- migrated layout (version 2)
#[cfg(any(feature = "migrate", feature = "fail"))]
#[btb::table(version = 1, of = "Todo")]
pub struct TodoV1 {
    pub owner: Address,
    pub done: bool,
    pub text: String,
}

#[cfg(feature = "wrongprev")]
#[btb::table(version = 1, of = "Todo")]
pub struct TodoV1 {
    pub owner: Address,
    pub done: u8,
    pub text: String,
}

#[cfg(any(feature = "migrate", feature = "fail", feature = "wrongprev"))]
#[btb::table(version = 2)]
pub struct Todo {
    #[index]
    pub owner: Address,
    #[index]
    pub status: u8,
    pub text: String,
}

#[cfg(any(feature = "migrate", feature = "fail", feature = "wrongprev"))]
#[btb::migration(from = 1)]
fn todo_v1_to_v2(row: TodoV1) -> Todo {
    #[cfg(feature = "fail")]
    if row.text == "bad" {
        abort("cannot migrate this row");
    }
    #[cfg(not(feature = "wrongprev"))]
    let status = row.done as u8;
    #[cfg(feature = "wrongprev")]
    let status = row.done;
    Todo {
        owner: row.owner,
        status: status + 1,
        text: row.text,
    }
}

// ---------------------------------------------------------------- functions
#[btb::mutation]
pub fn add(text: String) -> u64 {
    #[cfg(not(any(feature = "migrate", feature = "fail", feature = "wrongprev", feature = "bad")))]
    {
        #[cfg(not(feature = "compat"))]
        return Todo {
            owner: caller(),
            done: false,
            text,
        }
        .insert();
        #[cfg(feature = "compat")]
        return Todo {
            owner: caller(),
            done: false,
            text,
            note: None,
        }
        .insert();
    }
    #[cfg(feature = "bad")]
    return Todo {
        owner: caller(),
        done: 0,
        text,
    }
    .insert();
    #[cfg(any(feature = "migrate", feature = "fail", feature = "wrongprev"))]
    return Todo {
        owner: caller(),
        status: 0,
        text,
    }
    .insert();
}

#[btb::query]
pub fn texts(owner: Address) -> Vec<String> {
    Todo::by_owner(owner).map(|(_, t)| t.text).collect()
}

#[btb::query]
pub fn count(owner: Address) -> u64 {
    Todo::by_owner(owner).count() as u64
}

#[cfg(feature = "compat")]
#[btb::mutation]
pub fn set_note(id: u64, note: String) {
    let mut t = Todo::get(id).unwrap_or_else(|| abort("no such todo"));
    t.note = Some(note);
    t.update(id);
}

#[cfg(feature = "compat")]
#[btb::query]
pub fn note_of(id: u64) -> Option<String> {
    Todo::get(id).and_then(|t| t.note)
}

#[cfg(any(feature = "migrate", feature = "fail", feature = "wrongprev"))]
#[btb::query]
pub fn with_status(s: u8) -> Vec<String> {
    Todo::by_status(s).map(|(_, t)| t.text).collect()
}

#[cfg(feature = "compat")]
#[btb::query]
pub fn count_done(d: bool) -> u64 {
    Todo::by_done(d).count() as u64
}

#[cfg(feature = "compat")]
#[btb::mutation]
pub fn finish(id: u64) {
    let mut t = Todo::get(id).unwrap_or_else(|| abort("no such todo"));
    t.done = true;
    t.update(id);
}
