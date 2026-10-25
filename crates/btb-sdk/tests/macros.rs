//! The macros expand and the generated codecs follow `docs/ENCODING.md`. Host calls themselves
//! need the VM; they are exercised end to end in `crates/btb-tests`.

use btb_sdk::btb;
use btb_sdk::table::Table;
use btb_sdk::{Address, Decode, DecodeError, Encode};

#[btb::table]
#[derive(Debug, PartialEq)]
pub struct Pool {
    pub id: u64,
    pub owner: Address,
    pub name: String,
    pub open: bool,
    pub fee: Option<u16>,
}

#[btb::table]
#[derive(Debug, PartialEq)]
pub struct Pair(u8, u32);

#[btb::table]
#[derive(Debug, PartialEq)]
pub struct Marker;

fn sample() -> Pool {
    Pool {
        id: 7,
        owner: Address([3; 32]),
        name: "ab".into(),
        open: true,
        fee: Some(5),
    }
}

#[test]
fn struct_encodes_as_tuple_of_fields() {
    let p = sample();
    let tuple = (7u64, Address([3; 32]), String::from("ab"), true, Some(5u16));
    assert_eq!(p.encode(), tuple.encode());
    assert_eq!(Pool::decode(&p.encode()), Ok(p));
    assert_eq!(Pair(1, 2).encode(), [1, 2, 0, 0, 0]);
    assert_eq!(Pair::decode(&[1, 2, 0, 0, 0]), Ok(Pair(1, 2)));
    assert!(Marker.encode().is_empty());
    assert_eq!(Marker::decode(&[]), Ok(Marker));
}

#[test]
fn decode_rejects_bad_rows() {
    let mut bytes = sample().encode();
    bytes.push(0);
    assert_eq!(Pool::decode(&bytes), Err(DecodeError::TrailingBytes));
    let bytes = sample().encode();
    assert!(Pool::decode(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn table_name_is_struct_name() {
    assert_eq!(Pool::NAME, "Pool");
    assert_eq!(Pair::NAME, "Pair");
}

#[btb::query]
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}

#[btb::mutation]
pub fn nothing() {}

#[btb::cron(every = "3600 blocks")]
pub fn hourly() {}

#[btb::action]
pub fn fetch(url: String) -> Option<Vec<u8>> {
    let _ = url;
    None
}

#[test]
fn annotated_functions_stay_callable() {
    assert_eq!(add(1, 2), 3);
    nothing();
    hourly();
    assert_eq!(fetch(String::new()), None);
}

#[test]
fn cron_every_exports_a_schedule_marker() {
    // The marker is a plain symbol `btb_every_<blocks>_<name>` that `btb deploy` reads.
    let _: extern "C" fn() = btb_every_3600_hourly;
}

#[btb::mutation(randomness)]
pub fn dice() -> u8 {
    0
}

#[btb::cron(randomness, every = "10 blocks")]
pub fn lottery() {}

#[btb::cron(every = "7 blocks", randomness)]
pub fn lottery2() {}

#[btb::cron(randomness)]
pub fn unscheduled_lottery() {}

#[test]
fn randomness_exports_a_marker_symbol() {
    // `btb_randomness_<kind>_<name>` is what the VM reads from the ELF; it is independent of
    // the schedule marker.
    let _: extern "C" fn() = btb_randomness_mutation_dice;
    let _: extern "C" fn() = btb_randomness_cron_lottery;
    let _: extern "C" fn() = btb_every_10_lottery;
    let _: extern "C" fn() = btb_randomness_cron_lottery2;
    let _: extern "C" fn() = btb_every_7_lottery2;
    let _: extern "C" fn() = btb_randomness_cron_unscheduled_lottery;
    assert_eq!(dice(), 0);
    lottery();
    lottery2();
    unscheduled_lottery();
}
