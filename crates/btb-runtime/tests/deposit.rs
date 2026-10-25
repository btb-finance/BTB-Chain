use btb_runtime::layout::{self, DEPOSIT_PER_BYTE};
use btb_runtime::*;
use btb_vm::host::Host;

const C: [u8; 32] = [7; 32];
const T: &[u8] = b"t";

fn num(kv: &MemKv, key: Vec<u8>) -> u64 {
    kv.get(&key)
        .unwrap()
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
}
fn balance(kv: &MemKv) -> u64 {
    num(kv, layout::balance_key(&C))
}
fn usage(kv: &MemKv) -> u64 {
    num(kv, layout::usage_key(&C))
}
fn deposit(kv: &MemKv) -> u64 {
    num(kv, layout::deposit_key(&C))
}
fn funded(n: u64) -> MemKv {
    let mut kv = MemKv::new();
    kv.put(&layout::balance_key(&C), &n.to_le_bytes()).unwrap();
    kv
}
fn call(kv: &mut MemKv, t: &Tables, f: impl FnOnce(&mut RuntimeHost<MemKv>)) -> Result<(), Fault> {
    let mut h = RuntimeHost::new(kv, t, C, [1; 32], 1);
    f(&mut h);
    let b = h.finish()?;
    kv.commit(b).unwrap();
    Ok(())
}
fn skey(k: &[u8]) -> u64 {
    layout::storage_key(&C, k).len() as u64
}

#[test]
fn insert_charges_exact_amount() {
    let (t, mut kv) = (Tables::new(), funded(10_000));
    call(&mut kv, &t, |h| h.storage_write(b"k", b"vvvv")).unwrap();
    let bytes = skey(b"k") + 4;
    assert_eq!(usage(&kv), bytes);
    assert_eq!(deposit(&kv), bytes * DEPOSIT_PER_BYTE);
    assert_eq!(balance(&kv), 10_000 - bytes * DEPOSIT_PER_BYTE);
}

#[test]
fn update_charges_and_refunds_the_delta() {
    let (t, mut kv) = (Tables::new(), funded(10_000));
    call(&mut kv, &t, |h| h.storage_write(b"k", b"vv")).unwrap();
    let b0 = balance(&kv);
    call(&mut kv, &t, |h| h.storage_write(b"k", b"vvvvvvv")).unwrap();
    assert_eq!(balance(&kv), b0 - 5 * DEPOSIT_PER_BYTE);
    call(&mut kv, &t, |h| h.storage_write(b"k", b"vvv")).unwrap();
    assert_eq!(balance(&kv), b0 - DEPOSIT_PER_BYTE);
    assert_eq!(usage(&kv), skey(b"k") + 3);
    assert_eq!(deposit(&kv), usage(&kv) * DEPOSIT_PER_BYTE);
}

#[test]
fn delete_refunds_fully_including_rows_and_indexes() {
    let mut t = Tables::new();
    t.register(
        C,
        T,
        vec![IndexDef::new("first", |r| {
            Ok(Some(IndexValue::U64(
                r.first().copied().unwrap_or(0).into(),
            )))
        })],
    )
    .unwrap();
    let mut kv = funded(100_000);
    call(&mut kv, &t, |h| {
        h.storage_write(b"a", b"x");
        h.table_insert(T, b"hello");
    })
    .unwrap();
    // storage + row + index entry are all metered.
    let row = layout::row_key(&C, T, 0).len() as u64 + 5;
    let idx = layout::index_key(&C, T, "first", &IndexValue::U64(b'h'.into()), 0).len() as u64;
    assert_eq!(usage(&kv), skey(b"a") + 1 + row + idx);
    call(&mut kv, &t, |h| {
        h.storage_remove(b"a");
        assert!(h.table_delete(T, 0));
    })
    .unwrap();
    assert_eq!((usage(&kv), deposit(&kv), balance(&kv)), (0, 0, 100_000));
}

#[test]
fn insufficient_funds_aborts_atomically() {
    let t = Tables::new();
    let mut kv = funded(5);
    let before = kv.clone();
    let r = call(&mut kv, &t, |h| h.storage_write(b"k", b"v"));
    assert!(matches!(r, Err(Fault::InsufficientDeposit { .. })));
    assert_eq!(kv, before);
}

#[test]
fn conservation_over_random_sequence() {
    let total = 1_000_000u64;
    let mut t = Tables::new();
    t.register(
        C,
        T,
        vec![IndexDef::new("len", |r| {
            Ok(Some(IndexValue::U64(r.len() as u64)))
        })],
    )
    .unwrap();
    let mut kv = funded(total);
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let (mut ok, mut failed) = (0, 0);
    for _ in 0..600 {
        let (op, a, n) = (next() % 5, next() % 8, (next() % 120) as usize);
        let val = vec![1u8; n + 1];
        let r = call(&mut kv, &t, |h| match op {
            0 => h.storage_write(&[a as u8], &val),
            1 => h.storage_remove(&[a as u8]),
            2 => {
                h.table_insert(T, &val);
            }
            3 => {
                h.table_update(T, a, &val);
            }
            _ => {
                h.table_delete(T, a);
            }
        });
        if r.is_ok() {
            ok += 1;
        } else {
            failed += 1;
        }
        assert_eq!(balance(&kv) + deposit(&kv), total);
        assert_eq!(deposit(&kv), usage(&kv) * DEPOSIT_PER_BYTE);
    }
    assert!(ok > 100);
    let _ = failed;
}
