//! `Overlay` savepoints: the nested-call revert scope of `call_contract`.

use btb_runtime::{KvStore, MemKv, Overlay};

fn base() -> MemKv {
    let mut kv = MemKv::new();
    kv.put(b"a", b"1").unwrap();
    kv.put(b"b", b"2").unwrap();
    kv
}

fn state(o: &Overlay<MemKv>) -> Vec<(Vec<u8>, Vec<u8>)> {
    o.scan(b"", None, false, 100).unwrap()
}

#[test]
fn rollback_restores_writes_overwrites_and_deletes() {
    let kv = base();
    let mut o = Overlay::new(&kv);
    o.put(b"c", b"3").unwrap();
    let before = state(&o);
    let sp = o.savepoint();
    o.put(b"a", b"X").unwrap(); // overwrite a base key
    o.put(b"c", b"Y").unwrap(); // overwrite a pending key
    o.delete(b"b").unwrap(); // delete a base key
    o.put(b"d", b"4").unwrap(); // new key
    o.put(b"d", b"5").unwrap(); // twice
    assert_ne!(state(&o), before);
    o.rollback(sp);
    assert_eq!(state(&o), before);
    assert_eq!(o.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(o.get(b"d").unwrap(), None);
    // the pending set is back to exactly one write (c)
    assert_eq!(o.into_batch().len(), 1);
}

#[test]
fn release_keeps_and_nesting_unwinds_in_order() {
    let kv = base();
    let mut o = Overlay::new(&kv);
    let outer = o.savepoint();
    o.put(b"x", b"1").unwrap();
    let inner = o.savepoint();
    o.put(b"x", b"2").unwrap();
    o.put(b"y", b"2").unwrap();
    o.rollback(inner);
    assert_eq!(o.get(b"x").unwrap(), Some(b"1".to_vec()));
    assert_eq!(o.get(b"y").unwrap(), None);
    let inner = o.savepoint();
    o.put(b"z", b"3").unwrap();
    o.release(inner);
    assert_eq!(o.get(b"z").unwrap(), Some(b"3".to_vec()));
    // rolling the outer one back undoes the released inner one too
    o.rollback(outer);
    for k in [&b"x"[..], b"y", b"z"] {
        assert_eq!(o.get(k).unwrap(), None);
    }
    assert!(o.into_batch().is_empty());
}

#[test]
fn writes_outside_a_savepoint_are_not_journaled_and_reads_stay_recorded() {
    let kv = base();
    let mut o = Overlay::new(&kv);
    o.put(b"p", b"1").unwrap();
    let sp = o.savepoint();
    assert_eq!(o.get(b"a").unwrap(), Some(b"1".to_vec()));
    o.put(b"q", b"1").unwrap();
    o.rollback(sp);
    o.put(b"r", b"1").unwrap(); // after the last savepoint closed: plain write
    assert_eq!(o.get(b"p").unwrap(), Some(b"1".to_vec()));
    assert_eq!(o.get(b"q").unwrap(), None);
    assert_eq!(o.get(b"r").unwrap(), Some(b"1".to_vec()));
    // a rolled-back call still depended on what it read
    assert!(o.read_set().contains_key(b"a"));
}
