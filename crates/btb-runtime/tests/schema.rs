//! Declared schema: type grammar, index extraction from encoded rows (including rows with
//! variable-size fields before the indexed one), and unique (primary) indexes.

use btb_runtime::layout::{self, IndexValue};
use btb_runtime::schema::{FnSchema, IndexKind, Schema, TableSchema, Ty};
use btb_runtime::{Fault, KvStore, MemKv, RuntimeHost, TableError, Tables};
use btb_vm::host::Host;

const C: [u8; 32] = [7; 32];

fn table() -> TableSchema {
    // tags: vec<string>, note: opt<string>, owner: address (indexed), n: i32 (indexed),
    // name: string (primary), blob: bytes
    TableSchema {
        name: "Doc".into(),
        fields: vec![
            ("tags".into(), Ty::parse("vec<string>").unwrap()),
            ("note".into(), Ty::parse("opt<string>").unwrap()),
            ("owner".into(), Ty::Address),
            ("n".into(), Ty::I32),
            ("name".into(), Ty::Str),
            ("blob".into(), Ty::Bytes),
        ],
        indexes: vec!["owner".into(), "n".into(), "name".into()],
        primary: Some("name".into()),
        version: 1,
    }
}

fn schema() -> Schema {
    Schema {
        tables: vec![table()],
        functions: vec![],
        ..Schema::default()
    }
}

/// Encode a row by hand following docs/ENCODING.md.
fn row(tags: &[&str], note: Option<&str>, owner: u8, n: i32, name: &str, blob: &[u8]) -> Vec<u8> {
    fn s(out: &mut Vec<u8>, v: &[u8]) {
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        out.extend_from_slice(v);
    }
    let mut r = Vec::new();
    r.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for t in tags {
        s(&mut r, t.as_bytes());
    }
    match note {
        None => r.push(0),
        Some(n) => {
            r.push(1);
            s(&mut r, n.as_bytes());
        }
    }
    r.extend_from_slice(&[owner; 32]);
    r.extend_from_slice(&n.to_le_bytes());
    s(&mut r, name.as_bytes());
    s(&mut r, blob);
    r
}

fn tables() -> Tables {
    let mut t = Tables::new();
    for (name, defs) in schema().index_defs().unwrap() {
        t.register(C, name.as_bytes(), defs).unwrap();
    }
    t
}

fn snapshot(kv: &MemKv) -> Vec<(Vec<u8>, Vec<u8>)> {
    kv.entries()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect()
}

fn has(kv: &MemKv, index: &str, v: IndexValue, id: u64) -> bool {
    kv.get(&layout::index_key(&C, b"Doc", index, &v, id))
        .unwrap()
        .is_some()
}

#[test]
fn type_text_round_trips() {
    for s in [
        "u64",
        "vec<(u64,bool,string)>",
        "opt<address>",
        "arr<u8,4>",
        "()",
        "(vec<opt<bytes>>,Todo)",
        "Todo",
    ] {
        assert_eq!(Ty::parse(s).unwrap().to_string(), s);
    }
    for bad in ["", "vec<", "arr<u8>", "u64 x", "(u8", "vec<u8>>"] {
        assert!(Ty::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn indexes_are_extracted_after_variable_size_fields() {
    let t = tables();
    let mut kv = MemKv::new();
    let r1 = row(&["a", "bcd", ""], Some("hello"), 5, -3, "first", b"xyz");
    let r2 = row(&[], None, 6, 10, "second", b"");
    let i1 = t.insert(&mut kv, &C, b"Doc", &r1).unwrap();
    let i2 = t.insert(&mut kv, &C, b"Doc", &r2).unwrap();
    assert!(has(&kv, "owner", IndexValue::Bytes(vec![5; 32]), i1));
    assert!(has(&kv, "owner", IndexValue::Bytes(vec![6; 32]), i2));
    assert!(has(&kv, "n", IndexValue::I64(-3), i1));
    assert!(has(&kv, "n", IndexValue::I64(10), i2));
    assert!(has(&kv, "name", IndexValue::Str("first".into()), i1));
    assert!(has(&kv, "name", IndexValue::Str("second".into()), i2));
    // Updating moves entries; deleting removes them.
    let r1b = row(&["a"], None, 9, -3, "first", b"");
    assert!(t.update(&mut kv, &C, b"Doc", i1, &r1b).unwrap());
    assert!(!has(&kv, "owner", IndexValue::Bytes(vec![5; 32]), i1));
    assert!(has(&kv, "owner", IndexValue::Bytes(vec![9; 32]), i1));
    assert!(t.delete(&mut kv, &C, b"Doc", i2).unwrap());
    assert!(!has(&kv, "name", IndexValue::Str("second".into()), i2));
}

#[test]
fn malformed_rows_are_rejected_deterministically() {
    let t = tables();
    let mut kv = MemKv::new();
    let good = row(&["a"], None, 1, 1, "n", b"");
    // Truncated before the indexed field, and a vector count larger than the row.
    for bad in [
        good[..10].to_vec(),
        {
            let mut b = good.clone();
            b[..4].copy_from_slice(&u32::MAX.to_le_bytes());
            b
        },
        {
            // Bad option tag.
            let mut b = row(&[], None, 1, 1, "n", b"");
            b[4] = 7;
            b
        },
    ] {
        assert!(matches!(
            t.insert(&mut kv, &C, b"Doc", &bad),
            Err(TableError::Extract { .. })
        ));
    }
    assert_eq!(kv.entries().count(), 0);
}

#[test]
fn primary_is_unique_on_insert_and_update_and_failure_is_atomic() {
    let t = tables();
    let mut kv = MemKv::new();
    let a = t
        .insert(&mut kv, &C, b"Doc", &row(&[], None, 1, 1, "alpha", b""))
        .unwrap();
    let b = t
        .insert(&mut kv, &C, b"Doc", &row(&[], None, 1, 1, "beta", b""))
        .unwrap();
    let before = snapshot(&kv);
    // Same owner/n are fine (not unique); the same name is not.
    let dup = row(&[], None, 2, 2, "alpha", b"");
    assert_eq!(
        t.insert(&mut kv, &C, b"Doc", &dup),
        Err(TableError::Duplicate {
            index: "name".into()
        })
    );
    assert_eq!(
        t.update(&mut kv, &C, b"Doc", b, &dup),
        Err(TableError::Duplicate {
            index: "name".into()
        })
    );
    assert_eq!(snapshot(&kv), before);
    // Rewriting a row with its own key is fine, and a freed key can be reused.
    assert!(t
        .update(&mut kv, &C, b"Doc", a, &row(&[], None, 3, 3, "alpha", b"x"))
        .unwrap());
    assert!(t.delete(&mut kv, &C, b"Doc", a).unwrap());
    t.insert(&mut kv, &C, b"Doc", &dup).unwrap();
    // A prefix of an existing key is a different key.
    t.insert(&mut kv, &C, b"Doc", &row(&[], None, 1, 1, "alph", b""))
        .unwrap();
}

#[test]
fn duplicate_primary_aborts_the_whole_call() {
    let t = tables();
    let mut kv = MemKv::new();
    t.insert(&mut kv, &C, b"Doc", &row(&[], None, 1, 1, "k", b""))
        .unwrap();
    let before = snapshot(&kv);
    let mut h = RuntimeHost::new(&kv, &t, C, [1; 32], 1);
    h.storage_write(b"side", b"effect");
    h.table_insert(b"Doc", &row(&[], None, 1, 1, "other", b""));
    h.table_insert(b"Doc", &row(&[], None, 2, 2, "k", b"")); // duplicate
    assert!(matches!(
        h.finish(),
        Err(Fault::Table(TableError::Duplicate { .. }))
    ));
    assert_eq!(snapshot(&kv), before);
}

#[test]
fn schema_validation_rejects_bad_declarations() {
    let mut s = schema();
    s.tables[0].indexes.push("blob".into()); // bytes can be indexed...
    assert!(s.validate().is_ok());
    s.tables[0].indexes.push("tags".into()); // ...vectors cannot
    assert!(s.validate().unwrap_err().contains("cannot be indexed"));

    let mut s = schema();
    s.tables[0].indexes.push("nope".into());
    assert!(s.validate().unwrap_err().contains("unknown field"));

    let mut s = schema();
    s.tables[0].primary = Some("owner".into()); // primary must be listed once as index; ok
    assert!(s.validate().is_ok());
    s.tables[0].primary = Some("tags".into());
    assert!(s.validate().is_err());

    // Zero-size elements would make a vector unbounded for the walker.
    let mut s = schema();
    s.tables[0].fields[0].1 = Ty::Vec(Box::new(Ty::Tuple(vec![])));
    assert!(s.validate().unwrap_err().contains("no size"));

    // Unknown and recursive named types.
    let mut s = schema();
    s.tables[0].fields[0].1 = Ty::Named("Missing".into());
    assert!(s.validate().is_err());
    let mut s = schema();
    s.tables[0].fields[0].1 = Ty::Vec(Box::new(Ty::Named("Doc".into())));
    assert!(s.validate().is_err());

    let mut s = schema();
    s.functions = vec![
        FnSchema {
            kind: "query".into(),
            name: "f".into(),
            args: vec![],
            ret: None,
            fallible: false,
        };
        2
    ];
    assert!(s.validate().unwrap_err().contains("twice"));
}

#[test]
fn index_kinds_follow_the_field_type() {
    use IndexKind::*;
    let k = |s: &str| Ty::parse(s).unwrap().index_kind();
    assert_eq!(k("u8"), Some(U64));
    assert_eq!(k("bool"), Some(U64));
    assert_eq!(k("i16"), Some(I64));
    assert_eq!(k("u128"), Some(U128));
    assert_eq!(k("i128"), None);
    assert_eq!(k("string"), Some(Str));
    assert_eq!(k("address"), Some(Bytes));
    assert_eq!(k("arr<u8,8>"), Some(Bytes));
    assert_eq!(k("opt<u64>"), Some(U64));
    assert_eq!(k("vec<u8>"), None);
}
