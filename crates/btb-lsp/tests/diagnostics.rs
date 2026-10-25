use btb_lsp::{analyze, completions, hover, Severity};

fn codes(src: &str) -> Vec<&'static str> {
    analyze(src).iter().map(|d| d.code).collect()
}

#[test]
fn clean_contract_has_no_diagnostics() {
    let src = r#"
#![no_std]
use btb_sdk::btb;
#[btb::query] pub fn get(id: u64) -> u64 { let mut v = Vec::new(); v.insert(0, 1); id }
#[btb::mutation] pub fn set(id: u64) { host::storage_set(b"k", &id.to_le_bytes()); }
"#;
    assert_eq!(codes(src), Vec::<&str>::new());
}

#[test]
fn query_writes_are_errors() {
    let src = r#"
#[btb::query]
pub fn bad(t: Note) {
    host::storage_set(b"a", b"b");
    host::storage_remove(b"a");
    t.insert();
    Note::delete(1);
    Token::transfer(&x, &to, 5);
    x.mint(&to, 1);
}
#[btb::mutation]
pub fn fine() { host::storage_set(b"a", b"b"); }
"#;
    let d = analyze(src);
    assert_eq!(d.len(), 6, "{d:?}");
    assert!(d
        .iter()
        .all(|d| d.code == "btb::query-write" && d.severity == Severity::Error));
    assert!(d[0].message.contains("queries cannot write state"));
    assert_eq!(d[0].start.0, 3);
}

#[test]
fn floats_are_errors() {
    let src = "#[btb::query] pub fn f(x: f64) -> u64 { let y: f32 = 1.5; 0 }";
    let c = codes(src);
    assert_eq!(c.iter().filter(|c| **c == "btb::float").count(), 3);
}

#[test]
fn std_thread_time_rand_are_errors() {
    let src = r#"
use std::thread;
use std::time::Instant;
use rand::Rng;
fn f() { let _ = std::time::SystemTime::now(); }
"#;
    let d = analyze(src);
    assert!(d.iter().any(|d| d.message.contains("threads")));
    assert!(d.iter().any(|d| d.code == "btb::forbidden-rand"));
    assert!(
        d.iter()
            .filter(|d| d.message.contains("system time"))
            .count()
            >= 2
    );
}

#[test]
fn entry_names_and_duplicates() {
    let src = r#"
#[btb::mutation] pub fn Bad() {}
#[btb::query] pub fn dup() {}
#[btb::query] pub fn dup() {}
#[btb::mutation] pub fn dup() {}
"#;
    let d = analyze(src);
    assert!(d
        .iter()
        .any(|d| d.code == "btb::entry-name" && d.start.0 == 1));
    let dups: Vec<_> = d
        .iter()
        .filter(|d| d.code == "btb::duplicate-entry")
        .collect();
    assert_eq!(dups.len(), 2);
    assert!(dups.iter().any(|d| d.severity == Severity::Warning));
    let long = format!("#[btb::query] pub fn {}() {{}}", "a".repeat(65));
    assert_eq!(codes(&long), vec!["btb::entry-name"]);
}

#[test]
fn cfg_test_modules_are_skipped_and_invalid_code_is_quiet() {
    assert!(analyze("#[cfg(test)] mod t { fn f(x: f64) {} }").is_empty());
    assert!(analyze("fn ((( {{ f64").is_empty());
    assert!(analyze("").is_empty());
}

#[test]
fn hover_on_attributes() {
    let src = "#[btb::query]\npub fn a() {}\n";
    for col in [2, 4, 7, 10] {
        let h = hover(src, 0, col).unwrap_or_else(|| panic!("col {col}"));
        assert!(h.contains("read-only"), "{h}");
    }
    assert!(hover(src, 1, 5).is_none());
    assert!(hover(src, 9, 0).is_none());
    assert!(hover("#[btb::table]", 0, 10).unwrap().contains("table"));
}

#[test]
fn completion_contexts() {
    let m = completions("#[btb::");
    assert!(m.iter().any(|i| i.label == "mutation") && m.iter().all(|i| i.kind == 14));
    let h = completions("    host::st");
    assert!(h.iter().any(|i| i.label == "storage_set"));
    assert!(!h.iter().any(|i| i.label == "query"));
    assert!(completions("x").len() > h.len());
}
