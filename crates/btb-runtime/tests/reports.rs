//! The `report_request` host call on the runtime host: ids, records, the committee gate and the
//! per-call cap.

use btb_runtime::layout::{self, MAX_REPORTS_PER_CALL};
use btb_runtime::*;
use btb_vm::host::Host;

const C: [u8; 32] = [7; 32];

fn with_committee() -> MemKv {
    let mut kv = MemKv::new();
    // any value: the host only checks that the chain has an oracle config
    kv.put(&layout::oracle_config_key(), b"config").unwrap();
    kv
}

fn run(
    kv: &mut MemKv,
    height: u64,
    read_only: bool,
    f: impl FnOnce(&mut RuntimeHost<MemKv>),
) -> Result<(), Fault> {
    let t = Tables::new();
    let mut h = RuntimeHost::new(kv, &t, C, [1; 32], height);
    h.read_only = read_only;
    f(&mut h);
    let b = h.finish()?;
    kv.commit(b).unwrap();
    Ok(())
}

#[test]
fn reports_get_chain_wide_ids_and_records() {
    let mut kv = with_committee();
    run(&mut kv, 5, false, |h| {
        assert_eq!(h.report_request(b"first"), Some(0));
        assert_eq!(h.report_request(b""), Some(1));
    })
    .unwrap();
    run(&mut kv, 9, false, |h| {
        assert_eq!(
            h.report_request(b"third"),
            Some(2),
            "the counter survives the call"
        );
    })
    .unwrap();
    let rec = |id| kv.get(&layout::report_key(id)).unwrap().unwrap();
    let r0 = rec(0);
    assert_eq!(u64::from_le_bytes(r0[..8].try_into().unwrap()), 5, "height");
    assert_eq!(&r0[8..40], &C, "the asking contract");
    assert_eq!(&r0[40..], b"first");
    assert_eq!(rec(1).len(), 40);
    assert_eq!(u64::from_le_bytes(rec(2)[..8].try_into().unwrap()), 9);
    assert_eq!(
        u64::from_le_bytes(
            kv.get(&layout::report_counter_key())
                .unwrap()
                .unwrap()
                .try_into()
                .unwrap()
        ),
        3
    );
}

#[test]
fn a_chain_without_a_committee_has_no_reports() {
    let mut kv = MemKv::new();
    run(&mut kv, 1, false, |h| {
        assert_eq!(h.report_request(b"x"), None)
    })
    .unwrap();
    assert!(kv.get(&layout::report_counter_key()).unwrap().is_none());
}

#[test]
fn a_read_only_call_queues_nothing() {
    let mut kv = with_committee();
    run(&mut kv, 1, true, |h| {
        assert_eq!(h.report_request(b"x"), None)
    })
    .unwrap();
    assert!(kv.get(&layout::report_key(0)).unwrap().is_none());
}

#[test]
fn one_call_queues_a_bounded_number_and_a_failed_call_queues_none() {
    let mut kv = with_committee();
    run(&mut kv, 1, false, |h| {
        for i in 0..MAX_REPORTS_PER_CALL {
            assert_eq!(h.report_request(b"x"), Some(u64::from(i)));
        }
        assert_eq!(h.report_request(b"x"), None, "capped");
    })
    .unwrap();
    // the writes of a call are only applied when it is committed
    let before = kv.clone();
    let t = Tables::new();
    let mut h = RuntimeHost::new(&kv, &t, C, [1; 32], 2);
    assert!(h.report_request(b"dropped").is_some());
    drop(h); // the call trapped: its batch is never committed
    assert_eq!(kv, before);
}
