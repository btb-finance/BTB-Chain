//! Table host calls: each call, exact gas, read-only traps, missing ids, bad pointers, paging.

use btb_vm::gas;
use btb_vm::host::abi;
use btb_vm::testing::MemHost;
use btb_vm::{run_with_host, Exit, Trap, Vm};

const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;
const T: u64 = 256; // table name "pools"
const R: u64 = 320; // row bytes
const OUT: u64 = 512;

fn vm_with(call: u64, args: [u64; 5], gas_limit: u64) -> Vm {
    let mut vm = Vm::new(4096, gas_limit);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.write_mem(4, &EBREAK.to_le_bytes()).unwrap();
    vm.set_reg(17, call);
    for (i, a) in args.iter().enumerate() {
        vm.set_reg(10 + i, *a);
    }
    vm.write_mem(T, b"pools").unwrap();
    vm.write_mem(R, b"hello").unwrap();
    vm
}

fn total(c: u64) -> u64 {
    2 + c
}

fn call(h: &mut MemHost, n: u64, args: [u64; 5]) -> (Vm, Result<Exit, Trap>) {
    let mut vm = vm_with(n, args, 100_000);
    let r = run_with_host(&mut vm, h);
    (vm, r)
}

fn insert(h: &mut MemHost) -> u64 {
    let (vm, r) = call(h, abi::TABLE_INSERT, [T, 5, R, 5, 0]);
    assert_eq!(r, Ok(Exit::Halt));
    vm.reg(10)
}

#[test]
fn insert_get_update_delete_with_gas() {
    let mut h = MemHost::default();
    let (vm, r) = call(&mut h, abi::TABLE_INSERT, [T, 5, R, 5, 0]);
    assert_eq!(r, Ok(Exit::Halt));
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_TABLE_INSERT + gas::TABLE_WRITE_BYTE * 10)
    );
    let id = vm.reg(10);
    assert_eq!(insert(&mut h), id + 1);

    let (vm, _) = call(&mut h, abi::TABLE_GET, [T, 5, id, OUT, 64]);
    assert_eq!(vm.reg(10), 5);
    assert_eq!(vm.read_mem(OUT, 5).unwrap(), b"hello");
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_TABLE_GET + gas::TABLE_READ_BYTE * 10)
    );

    // Small buffer: full length returned, only cap bytes written.
    let (mut vm, _) = call(&mut h, abi::TABLE_GET, [T, 5, id, OUT, 2]);
    assert_eq!(vm.reg(10), 5);
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_TABLE_GET + gas::TABLE_READ_BYTE * 7)
    );
    vm.write_mem(OUT, &[0; 1]).unwrap();

    let mut vm = vm_with(abi::TABLE_UPDATE, [T, 5, id, R, 3], 10_000);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), abi::TABLE_OK);
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_TABLE_UPDATE + gas::TABLE_WRITE_BYTE * 8)
    );
    assert_eq!(h.tables[&b"pools"[..]].rows[&id], b"hel");

    let (vm, _) = call(&mut h, abi::TABLE_DELETE, [T, 5, id, 0, 0]);
    assert_eq!(vm.reg(10), abi::TABLE_OK);
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_TABLE_DELETE + gas::TABLE_READ_BYTE * 5)
    );
    assert!(!h.tables[&b"pools"[..]].rows.contains_key(&id));
}

#[test]
fn missing_ids() {
    let mut h = MemHost::default();
    let (vm, _) = call(&mut h, abi::TABLE_GET, [T, 5, 77, OUT, 8]);
    assert_eq!(vm.reg(10), abi::NOT_FOUND);
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_TABLE_GET + gas::TABLE_READ_BYTE * 5)
    );
    let (vm, _) = call(&mut h, abi::TABLE_UPDATE, [T, 5, 77, R, 5]);
    assert_eq!(vm.reg(10), abi::TABLE_MISSING);
    let (vm, _) = call(&mut h, abi::TABLE_DELETE, [T, 5, 77, 0, 0]);
    assert_eq!(vm.reg(10), abi::TABLE_MISSING);
    assert!(h.tables.is_empty() || h.tables[&b"pools"[..]].rows.is_empty());
    let id = insert(&mut h);
    let (vm, _) = call(&mut h, abi::TABLE_DELETE, [T, 5, id, 0, 0]);
    assert_eq!(vm.reg(10), abi::TABLE_OK);
    let (vm, _) = call(&mut h, abi::TABLE_DELETE, [T, 5, id, 0, 0]);
    assert_eq!(vm.reg(10), abi::TABLE_MISSING);
}

#[test]
fn ids_never_reused_after_delete() {
    let mut h = MemHost::default();
    let a = insert(&mut h);
    let b = insert(&mut h);
    let _ = call(&mut h, abi::TABLE_DELETE, [T, 5, b, 0, 0]);
    let c = insert(&mut h);
    assert!(a < b && b < c);
    let _ = call(&mut h, abi::TABLE_DELETE, [T, 5, a, 0, 0]);
    let _ = call(&mut h, abi::TABLE_DELETE, [T, 5, c, 0, 0]);
    assert!(insert(&mut h) > c);
}

fn ids_out(vm: &Vm, n: u64) -> Vec<u64> {
    (0..n)
        .map(|i| u64::from_le_bytes(vm.read_mem(OUT + i * 8, 8).unwrap().try_into().unwrap()))
        .collect()
}

#[test]
fn scan_pagination_and_gas() {
    let mut h = MemHost::default();
    let ids: Vec<u64> = (0..5).map(|_| insert(&mut h)).collect();
    let _ = call(&mut h, abi::TABLE_DELETE, [T, 5, ids[1], 0, 0]);
    let want = [ids[0], ids[2], ids[3], ids[4]];

    // out_cap 20 -> room for 2 ids (20/8).
    let (vm, _) = call(&mut h, abi::TABLE_SCAN, [T, 5, 0, OUT, 20]);
    assert_eq!(vm.reg(10), 2);
    assert_eq!(ids_out(&vm, 2), want[..2]);
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_TABLE_SCAN + gas::TABLE_READ_BYTE * 5 + gas::TABLE_SCAN_ID * 2)
    );
    let next = want[1] + 1;
    let (vm, _) = call(&mut h, abi::TABLE_SCAN, [T, 5, next, OUT, 64]);
    assert_eq!(vm.reg(10), 2);
    assert_eq!(ids_out(&vm, 2), want[2..]);
    let (vm, _) = call(&mut h, abi::TABLE_SCAN, [T, 5, want[3] + 1, OUT, 64]);
    assert_eq!(vm.reg(10), 0);
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_TABLE_SCAN + gas::TABLE_READ_BYTE * 5)
    );
    // Unknown table and zero capacity.
    let (vm, _) = call(&mut h, abi::TABLE_SCAN, [T, 4, 0, OUT, 64]);
    assert_eq!(vm.reg(10), 0);
    let (vm, _) = call(&mut h, abi::TABLE_SCAN, [T, 5, 0, OUT, 7]);
    assert_eq!(vm.reg(10), 0);
}

#[test]
fn read_only_traps_writes_allows_reads() {
    let mut h = MemHost::default();
    let id = insert(&mut h);
    h.read_only = true;
    for (n, args) in [
        (abi::TABLE_INSERT, [T, 5, R, 5, 0]),
        (abi::TABLE_UPDATE, [T, 5, id, R, 5]),
        (abi::TABLE_DELETE, [T, 5, id, 0, 0]),
    ] {
        let (vm, r) = call(&mut h, n, args);
        assert_eq!(r, Err(Trap::WriteInQuery));
        assert_eq!(vm.gas_used(), 1);
    }
    assert_eq!(h.tables[&b"pools"[..]].rows[&id], b"hello");
    let (vm, r) = call(&mut h, abi::TABLE_GET, [T, 5, id, OUT, 8]);
    assert_eq!((r, vm.reg(10)), (Ok(Exit::Halt), 5));
    let (vm, r) = call(&mut h, abi::TABLE_SCAN, [T, 5, 0, OUT, 8]);
    assert_eq!((r, vm.reg(10)), (Ok(Exit::Halt), 1));
}

#[test]
fn bad_pointers_trap_without_effect() {
    let mut h = MemHost::default();
    let id = insert(&mut h);
    let bad = 1 << 40;
    for (n, args) in [
        (abi::TABLE_INSERT, [bad, 5, R, 5, 0]),
        (abi::TABLE_INSERT, [T, 5, bad, 5, 0]),
        (abi::TABLE_UPDATE, [T, 5, id, bad, 5]),
        (abi::TABLE_UPDATE, [bad, 5, id, R, 5]),
        (abi::TABLE_DELETE, [bad, 5, id, 0, 0]),
        (abi::TABLE_GET, [bad, 5, id, OUT, 8]),
        (abi::TABLE_GET, [T, 5, id, bad, 8]),
        (abi::TABLE_SCAN, [bad, 5, 0, OUT, 8]),
        (abi::TABLE_SCAN, [T, 5, 0, bad, 8]),
        (abi::TABLE_SCAN, [T, 5, 0, 4090, 64]),
    ] {
        let (_, r) = call(&mut h, n, args);
        assert!(
            matches!(r, Err(Trap::MemoryOutOfBounds { .. })),
            "{n} {r:?}"
        );
    }
    let t = &h.tables[&b"pools"[..]];
    assert_eq!(t.rows.len(), 1);
    assert_eq!(t.rows[&id], b"hello");
}

#[test]
fn out_of_gas_has_no_effect_and_huge_lengths_do_not_allocate() {
    let mut h = MemHost::default();
    let mut vm = vm_with(
        abi::TABLE_INSERT,
        [T, 5, R, 5, 0],
        1 + gas::HOST_TABLE_INSERT,
    );
    assert_eq!(run_with_host(&mut vm, &mut h), Err(Trap::OutOfGas));
    assert!(h.tables.is_empty());
    for (n, args) in [
        (abi::TABLE_INSERT, [T, 5, R, u64::MAX, 0]),
        (abi::TABLE_UPDATE, [T, u64::MAX, 1, R, u64::MAX]),
        (abi::TABLE_GET, [T, u64::MAX, 1, OUT, 8]),
        (abi::TABLE_SCAN, [T, u64::MAX, 0, OUT, 8]),
    ] {
        let (_, r) = call(&mut h, n, args);
        assert_eq!(r, Err(Trap::OutOfGas));
    }
    assert!(h.tables.is_empty());
}
