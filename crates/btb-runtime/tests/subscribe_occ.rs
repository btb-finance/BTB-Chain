use btb_runtime::*;

const C: [u8; 32] = [1; 32];

fn put(kv: &mut MemKv, k: &str, v: &str) -> WriteBatch {
    let mut b = WriteBatch::new();
    b.put(k.into(), v.into());
    kv.commit(b.clone()).unwrap();
    b
}

/// Query: sum of the numbers under "a" ... "b" (a range scan) as a decimal string.
fn range_query(kv: &MemKv) -> (Vec<u8>, ReadSet) {
    let o = Overlay::new(kv);
    let rows = o.scan(b"k/", Some(b"k0"), false, 100).unwrap();
    let sum: u64 = rows
        .iter()
        .map(|(_, v)| String::from_utf8_lossy(v).parse::<u64>().unwrap())
        .sum();
    (sum.to_string().into_bytes(), o.read_set())
}

fn point_query(kv: &MemKv) -> (Vec<u8>, ReadSet) {
    let o = Overlay::new(kv);
    let v = o.get(b"p").unwrap().unwrap_or_default();
    (v, o.read_set())
}

fn sub(m: &mut SubscriptionManager, q: (Vec<u8>, ReadSet)) -> SubId {
    m.subscribe(C, "q", b"", q.1, &q.0)
}

#[test]
fn fires_on_point_write_and_not_unrelated() {
    let mut kv = MemKv::new();
    put(&mut kv, "p", "1");
    let mut m = SubscriptionManager::new();
    let id = sub(&mut m, point_query(&kv));
    let b = put(&mut kv, "other", "x");
    assert!(m.affected(&b).is_empty());
    let b = put(&mut kv, "p", "2");
    assert_eq!(m.affected(&b), vec![id]);
    let ch = m
        .process_commit::<()>(&b, |_, _, _| Ok(point_query(&kv)))
        .unwrap();
    assert_eq!(
        ch,
        vec![Change {
            id,
            result: b"2".to_vec()
        }]
    );
}

#[test]
fn fires_inside_range_and_on_delete_not_outside() {
    let mut kv = MemKv::new();
    put(&mut kv, "k/1", "5");
    put(&mut kv, "k/3", "7");
    let mut m = SubscriptionManager::new();
    let id = sub(&mut m, range_query(&kv));
    // New key inside the range that was never present.
    let b = put(&mut kv, "k/2", "1");
    assert_eq!(m.affected(&b), vec![id]);
    // Outside the range (boundary key `k0` is excluded).
    let b = put(&mut kv, "k0", "9");
    assert!(m.affected(&b).is_empty());
    let b = put(&mut kv, "j", "9");
    assert!(m.affected(&b).is_empty());
    // Delete inside.
    let mut b = WriteBatch::new();
    b.delete(b"k/3".to_vec());
    kv.commit(b.clone()).unwrap();
    assert_eq!(m.affected(&b), vec![id]);
    let ch = m
        .process_commit::<()>(&b, |_, _, _| Ok(range_query(&kv)))
        .unwrap();
    assert_eq!(ch[0].result, b"6".to_vec());
}

#[test]
fn unchanged_result_not_reported() {
    let mut kv = MemKv::new();
    put(&mut kv, "p", "1");
    let mut m = SubscriptionManager::new();
    let id = sub(&mut m, point_query(&kv));
    let b = put(&mut kv, "p", "1");
    assert_eq!(m.affected(&b), vec![id]);
    let ch = m
        .process_commit::<()>(&b, |_, _, _| Ok(point_query(&kv)))
        .unwrap();
    assert!(ch.is_empty());
    // Unsubscribed ones are not found.
    assert!(m.unsubscribe(id));
    assert!(m.affected(&b).is_empty());
}

#[test]
fn many_ranges_and_limited_scan() {
    let mut kv = MemKv::new();
    for i in 0..10 {
        put(&mut kv, &format!("r{i}"), "1");
    }
    let mut m = SubscriptionManager::new();
    let ids: Vec<_> = (0..10)
        .map(|i| {
            let mut rs = ReadSet::new();
            rs.add_range(
                format!("r{i}").as_bytes(),
                Some(format!("r{i}x").as_bytes()),
            );
            m.subscribe(C, "q", b"", rs, b"")
        })
        .collect();
    let mut b = WriteBatch::new();
    b.put(b"r4m".to_vec(), vec![]);
    assert_eq!(m.affected(&b), vec![ids[4]]);
    // A scan with limit 2 only depends on keys up to its last entry.
    let o = Overlay::new(&kv);
    o.scan(b"r", None, false, 2).unwrap();
    let rs = o.read_set();
    assert!(rs.contains_key(b"r1"));
    assert!(!rs.contains_key(b"r5"));
    let o = Overlay::new(&kv);
    o.scan(b"r", None, true, 2).unwrap();
    assert!(o.read_set().contains_key(b"r8") && !o.read_set().contains_key(b"r3"));
}

fn read_modify(store: &MemKv, key: &str, add: u64) -> Result<(u64, WriteBatch, ReadSet), ()> {
    let o = Overlay::new(store);
    let cur: u64 = o
        .get(key.as_bytes())
        .unwrap()
        .map_or(0, |v| String::from_utf8(v).unwrap().parse().unwrap());
    let mut o = o;
    o.put(key.as_bytes(), (cur + add).to_string().as_bytes())
        .unwrap();
    let (b, r) = o.into_parts();
    Ok((cur + add, b, r))
}

fn txn(occ: &Occ<MemKv>, key: &str, add: u64) -> Txn<u64> {
    let snapshot = occ.snapshot();
    let (output, batch, reads) = read_modify(occ.store(), key, add).unwrap();
    Txn {
        snapshot,
        reads,
        batch,
        output,
    }
}

#[test]
fn conflicting_second_fails_then_retry_succeeds() {
    let mut occ = Occ::new(MemKv::new());
    let (t1, t2) = (txn(&occ, "x", 1), txn(&occ, "x", 10));
    assert!(occ.commit::<_, ()>(t1).is_ok());
    assert_eq!(occ.commit::<_, ()>(t2).unwrap_err(), OccError::Conflict);
    assert_eq!(occ.store().get(b"x").unwrap().unwrap(), b"1");
    // Retry via execute_with_retry; the first execution is stale only if state moved.
    let c = occ
        .execute_with_retry(3, |s, _| read_modify(s, "x", 10))
        .unwrap();
    assert_eq!(c.output, 11);
    assert_eq!(occ.store().get(b"x").unwrap().unwrap(), b"11");
}

#[test]
fn retry_limit_respected() {
    let occ = std::cell::RefCell::new(Occ::new(MemKv::new()));
    let mut runs = 0;
    // Every attempt is invalidated by a rival commit made after it executed.
    let r = execute_with_retry::<Committed<u64>, ()>(3, |_| {
        runs += 1;
        let t = txn(&occ.borrow(), "x", 1);
        let rival = txn(&occ.borrow(), "x", 100);
        occ.borrow_mut().commit::<_, ()>(rival).unwrap();
        occ.borrow_mut().commit(t)
    });
    assert_eq!(r.unwrap_err(), OccError::Conflict);
    assert_eq!(runs, 4);
    // Zero retries: one execution.
    runs = 0;
    let r = execute_with_retry::<Committed<u64>, ()>(0, |_| {
        runs += 1;
        let t = txn(&occ.borrow(), "x", 1);
        let rival = txn(&occ.borrow(), "x", 100);
        occ.borrow_mut().commit::<_, ()>(rival).unwrap();
        occ.borrow_mut().commit(t)
    });
    assert!(r.is_err() && runs == 1);
}

#[test]
fn retry_succeeds_with_correct_final_state() {
    let occ = std::cell::RefCell::new(Occ::new(MemKv::new()));
    let (c, n) = execute_with_retry::<Committed<u64>, ()>(3, |attempt| {
        let t = txn(&occ.borrow(), "x", 1);
        if attempt == 1 {
            let rival = txn(&occ.borrow(), "x", 100);
            occ.borrow_mut().commit::<_, ()>(rival).unwrap();
        }
        occ.borrow_mut().commit(t)
    })
    .unwrap();
    assert_eq!((c.output, n), (101, 2));
    assert_eq!(occ.borrow().store().get(b"x").unwrap().unwrap(), b"101");
}

#[test]
fn stale_snapshot_after_prune_conflicts() {
    let mut occ = Occ::new(MemKv::new());
    let old = txn(&occ, "y", 1);
    let t = txn(&occ, "x", 1);
    occ.commit::<_, ()>(t).unwrap();
    occ.prune(1);
    assert_eq!(occ.commit::<_, ()>(old).unwrap_err(), OccError::Conflict);
}

#[test]
fn non_conflicting_both_commit_and_read_only_never_conflicts() {
    let mut occ = Occ::new(MemKv::new());
    let (a, b) = (txn(&occ, "a", 1), txn(&occ, "b", 2));
    let ro = {
        let o = Overlay::new(occ.store());
        o.get(b"a").unwrap();
        let (batch, reads) = o.into_parts();
        Txn {
            snapshot: occ.snapshot(),
            reads,
            batch,
            output: (),
        }
    };
    assert!(occ.commit::<_, ()>(a).is_ok());
    assert!(occ.commit::<_, ()>(b).is_ok());
    assert!(occ.commit::<_, ()>(ro).is_ok());
    assert_eq!(occ.snapshot(), 2);
}

#[test]
fn range_read_conflicts_with_insert_in_range() {
    let mut occ = Occ::new(MemKv::new());
    let scan = {
        let o = Overlay::new(occ.store());
        o.scan(b"k/", Some(b"k0"), false, 10).unwrap();
        let mut o = o;
        o.put(b"out", b"1").unwrap();
        let (batch, reads) = o.into_parts();
        Txn {
            snapshot: 0,
            reads,
            batch,
            output: (),
        }
    };
    let ins = txn(&occ, "k/5", 1);
    occ.commit::<_, ()>(ins).unwrap();
    assert_eq!(occ.commit::<_, ()>(scan).unwrap_err(), OccError::Conflict);
}
