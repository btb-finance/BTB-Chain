//! Cross-language check for the TypeScript SDK (`sdk-ts`).
//!
//! `sdk-ts/test/fixtures/codec.json` maps case names to the hex that `btb-codec` produces. The
//! TypeScript tests (`sdk-ts/test/codec.test.ts`) encode the same values and compare. This test
//! fails when the checked-in fixture is stale; regenerate with
//! `BTB_UPDATE_FIXTURES=1 cargo test -p btb-tests --test ts_fixtures`.

use btb_codec::{Address, Encode};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn cases() -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    let mut add = |name: &'static str, v: &dyn Encode| {
        m.insert(name, hex(&v.encode()));
    };
    add("unit", &());
    add("bool_true", &true);
    add("bool_false", &false);
    add("u8_255", &255u8);
    add("u16_0102", &0x0102u16);
    add("u32_01020304", &0x0102_0304u32);
    add("u64_max", &u64::MAX);
    add("u64_0102030405060708", &0x0102_0304_0506_0708u64);
    add("u128_1", &1u128);
    add("u128_max", &u128::MAX);
    add("i8_neg2", &-2i8);
    add("i16_min", &i16::MIN);
    add("i32_neg1", &-1i32);
    add("i64_min", &i64::MIN);
    add("i128_neg1", &-1i128);
    add("i128_min", &i128::MIN);
    add("string_hi", &String::from("hi"));
    add("string_empty", &String::new());
    add(
        "string_unicode",
        &String::from("h\u{e9}llo \u{2713} \u{1F600}"),
    );
    add("bytes_deadbeef", &vec![0xde_u8, 0xad, 0xbe, 0xef]);
    add("option_some_5", &Some(5u8));
    add("option_none", &None::<u8>);
    add("vec_u16_1_2", &vec![1u16, 2]);
    add("vec_u8_7_8", &vec![7u8, 8]);
    add("array_u16_1_2", &[1u16, 2]);
    add("tuple_u8_bool", &(1u8, true));
    add("address_ab", &Address([0xab; 32]));
    add(
        "composite",
        &(7u64, String::from("x"), vec![Some(-1i32), None]),
    );
    m
}

#[test]
fn ts_codec_fixture_is_current() {
    let mut json = String::from("{\n");
    let all = cases();
    for (i, (k, v)) in all.iter().enumerate() {
        let sep = if i + 1 == all.len() { "" } else { "," };
        json.push_str(&format!("  \"{k}\": \"{v}\"{sep}\n"));
    }
    json.push_str("}\n");
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk-ts/test/fixtures/codec.json");
    if std::env::var_os("BTB_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, &json).unwrap();
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        on_disk, json,
        "sdk-ts/test/fixtures/codec.json is stale; run with BTB_UPDATE_FIXTURES=1"
    );
}
