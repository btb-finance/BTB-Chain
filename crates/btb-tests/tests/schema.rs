//! The schema markers `#[btb::table]` and the entry-point macros export in the ELF, decoded
//! with the same code the host and the chain use.

use btb_runtime::schema::{Schema, Ty};

mod common;

fn names(elf: &[u8]) -> Vec<String> {
    btb_vm::elf::export_list(elf)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .filter(|n| n.starts_with("btb_schema_"))
        .collect()
}

#[test]
fn todo_exports_table_and_function_markers() {
    let elf = common::build_example("todo");
    let syms = names(&elf);
    assert!(syms.iter().any(|s| s.starts_with("btb_schema_table_Todo_")));
    assert!(syms
        .iter()
        .any(|s| s.starts_with("btb_schema_fn_mutation_add_")));
    // The payload is the hex of readable text.
    let t = syms
        .iter()
        .find(|s| s.starts_with("btb_schema_table_Todo_"))
        .unwrap();
    let hex = t.rsplit_once('_').unwrap().1;
    let text: String = hex
        .as_bytes()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap() as char)
        .collect();
    assert_eq!(
        text,
        "f=owner:address|done:bool|created:u64|text:string;i=owner,created;p="
    );

    let s = Schema::from_elf(&elf).unwrap();
    assert_eq!(s.tables.len(), 1);
    let todo = s.table("Todo").unwrap();
    assert_eq!(
        todo.fields,
        vec![
            ("owner".to_string(), Ty::Address),
            ("done".to_string(), Ty::Bool),
            ("created".to_string(), Ty::U64),
            ("text".to_string(), Ty::Str),
        ]
    );
    assert_eq!(todo.indexes, vec!["owner", "created"]);
    assert_eq!(todo.primary, None);

    let f = |kind: &str, name: &str| {
        s.functions
            .iter()
            .find(|f| f.kind == kind && f.name == name)
            .unwrap_or_else(|| panic!("no {kind} {name}"))
    };
    let add = f("mutation", "add");
    assert_eq!(add.args, vec![("text".to_string(), Ty::Str)]);
    assert_eq!(add.ret, Some(Ty::U64));
    assert!(add.fallible);
    let list = f("query", "list_mine");
    assert!(list.args.is_empty() && !list.fallible);
    assert_eq!(
        list.ret.as_ref().unwrap().to_string(),
        "vec<(u64,bool,string)>"
    );
    assert_eq!(
        f("query", "recent").ret.as_ref().unwrap().to_string(),
        "vec<Todo>"
    );
    assert_eq!(
        f("query", "page_mine")
            .args
            .iter()
            .map(|(n, t)| format!("{n}:{t}"))
            .collect::<Vec<_>>(),
        vec!["limit:u32", "cursor:bytes", "newest_first:bool"]
    );
}

#[test]
fn nft_declares_a_primary_key() {
    let s = Schema::from_elf(&common::build_example("nft")).unwrap();
    let nft = s.table("Nft").unwrap();
    assert_eq!(nft.indexes, vec!["owner"]);
    assert_eq!(nft.primary, None);
    let l = s.table("Listing").unwrap();
    assert_eq!(l.primary.as_deref(), Some("nft"));
    assert_eq!(l.indexes, vec!["nft"]);
    assert_eq!(l.field("token"), Some(&Ty::Address));
}

#[test]
fn every_example_has_a_valid_schema() {
    for name in ["counter", "token", "dex", "oracle", "launcher", "actions"] {
        let s = Schema::from_elf(&common::build_example(name)).unwrap();
        assert!(!s.functions.is_empty(), "{name}");
    }
}

#[test]
fn garbage_markers_are_rejected() {
    // A schema symbol with a payload that is not hex must fail the whole load, never be skipped.
    assert!(Schema::from_elf(b"not an elf").is_err());
}
