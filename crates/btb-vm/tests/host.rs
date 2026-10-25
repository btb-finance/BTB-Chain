//! Host-call ABI tests: each call, exact gas, failure modes.

use btb_vm::gas;
use btb_vm::host::abi;
use btb_vm::testing::MemHost;
use btb_vm::{run_with_host, Exit, Trap, Vm};

const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;
const DATA: u64 = 256;

/// VM with `ecall; ebreak` at 0, call number and args set, plenty of gas unless given.
fn vm_with(call: u64, args: [u64; 4], gas_limit: u64) -> Vm {
    let mut vm = Vm::new(4096, gas_limit);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.write_mem(4, &EBREAK.to_le_bytes()).unwrap();
    vm.set_reg(17, call);
    for (i, a) in args.iter().enumerate() {
        vm.set_reg(10 + i, *a);
    }
    vm
}

fn host() -> MemHost {
    let mut h = MemHost {
        contract: [9; 32],
        caller: [7; 32],
        height: 42,
        ..Default::default()
    };
    h.balances.insert([9; 32], 100);
    h
}

/// ecall + ebreak + host cost.
fn total(host_cost: u64) -> u64 {
    2 + host_cost
}

#[test]
fn storage_write_read_remove() {
    let mut h = host();
    let mut vm = vm_with(abi::STORAGE_WRITE, [DATA, 3, DATA + 8, 5], 10_000);
    vm.write_mem(DATA, b"key").unwrap();
    vm.write_mem(DATA + 8, b"hello").unwrap();
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_STORAGE_WRITE + gas::STORAGE_WRITE_BYTE * 8)
    );
    assert_eq!(h.storage[&b"key"[..]], b"hello");

    let mut vm = vm_with(abi::STORAGE_READ, [DATA, 3, 512, 64], 10_000);
    vm.write_mem(DATA, b"key").unwrap();
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), 5);
    assert_eq!(vm.read_mem(512, 5).unwrap(), b"hello");
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_STORAGE_READ + gas::STORAGE_READ_BYTE * 8)
    );

    let mut vm = vm_with(abi::STORAGE_REMOVE, [DATA, 3, 0, 0], 10_000);
    vm.write_mem(DATA, b"key").unwrap();
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert!(h.storage.is_empty());
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_STORAGE_REMOVE + gas::STORAGE_REMOVE_BYTE * 3)
    );
}

#[test]
fn storage_read_missing() {
    let mut vm = vm_with(abi::STORAGE_READ, [DATA, 3, 512, 8], 10_000);
    assert_eq!(run_with_host(&mut vm, &mut host()), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), abi::NOT_FOUND);
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_STORAGE_READ + gas::STORAGE_READ_BYTE * 3)
    );
}

#[test]
fn storage_read_small_buffer_returns_full_len_writes_only_cap() {
    let mut h = host();
    h.storage.insert(b"k".to_vec(), b"0123456789".to_vec());
    let mut vm = vm_with(abi::STORAGE_READ, [DATA, 1, 512, 4], 10_000);
    vm.write_mem(DATA, b"k").unwrap();
    vm.write_mem(512, &[0xAA; 16]).unwrap();
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), 10);
    assert_eq!(vm.read_mem(512, 4).unwrap(), b"0123");
    assert_eq!(vm.read_mem(516, 12).unwrap(), &[0xAA; 12]);
    // key byte + 4 copied bytes
    assert_eq!(
        vm.gas_used(),
        total(gas::HOST_STORAGE_READ + gas::STORAGE_READ_BYTE * 5)
    );
}

#[test]
fn caller_height_balance_transfer() {
    let mut h = host();
    let mut vm = vm_with(abi::CALLER, [DATA, 0, 0, 0], 1000);
    run_with_host(&mut vm, &mut h).unwrap();
    assert_eq!(vm.read_mem(DATA, 32).unwrap(), &[7; 32]);
    assert_eq!(vm.gas_used(), total(gas::HOST_CALLER));

    let mut vm = vm_with(abi::BLOCK_HEIGHT, [0; 4], 1000);
    run_with_host(&mut vm, &mut h).unwrap();
    assert_eq!(vm.reg(10), 42);
    assert_eq!(vm.gas_used(), total(gas::HOST_BLOCK_HEIGHT));

    let mut vm = vm_with(abi::TOKEN_BALANCE, [DATA, 0, 0, 0], 1000);
    vm.write_mem(DATA, &[9; 32]).unwrap();
    run_with_host(&mut vm, &mut h).unwrap();
    assert_eq!(vm.reg(10), 100);
    assert_eq!(vm.gas_used(), total(gas::HOST_TOKEN_BALANCE));

    let mut vm = vm_with(abi::TOKEN_TRANSFER, [DATA, 60, 0, 0], 1000);
    vm.write_mem(DATA, &[5; 32]).unwrap();
    run_with_host(&mut vm, &mut h).unwrap();
    assert_eq!(vm.reg(10), abi::TRANSFER_OK);
    assert_eq!(vm.gas_used(), total(gas::HOST_TOKEN_TRANSFER));
    assert_eq!(h.balances[&[9; 32]], 40);
    assert_eq!(h.balances[&[5; 32]], 60);

    let mut vm = vm_with(abi::TOKEN_TRANSFER, [DATA, 41, 0, 0], 1000);
    vm.write_mem(DATA, &[5; 32]).unwrap();
    run_with_host(&mut vm, &mut h).unwrap();
    assert_eq!(vm.reg(10), abi::TRANSFER_INSUFFICIENT);
    assert_eq!(h.balances[&[9; 32]], 40);
}

#[test]
fn return_data_and_abort() {
    let mut vm = vm_with(abi::RETURN_DATA, [DATA, 3, 0, 0], 1000);
    vm.write_mem(DATA, b"abc").unwrap();
    assert_eq!(
        run_with_host(&mut vm, &mut host()),
        Ok(Exit::Return(b"abc".to_vec()))
    );
    // ecall + host cost; the ebreak is never reached
    assert_eq!(
        vm.gas_used(),
        1 + gas::HOST_RETURN_DATA + gas::RETURN_BYTE * 3
    );

    let mut vm = vm_with(abi::ABORT, [DATA, 4, 0, 0], 1000);
    vm.write_mem(DATA, b"oops").unwrap();
    assert_eq!(
        run_with_host(&mut vm, &mut host()),
        Err(Trap::Abort("oops".into()))
    );
    assert_eq!(vm.gas_used(), 1 + gas::HOST_ABORT + gas::ABORT_BYTE * 4);
}

#[test]
fn out_of_gas_before_call_has_no_side_effect() {
    let mut h = host();
    let need = 1 + gas::HOST_STORAGE_WRITE + gas::STORAGE_WRITE_BYTE * 2;
    let mut vm = vm_with(abi::STORAGE_WRITE, [DATA, 1, DATA + 1, 1], need - 1);
    assert_eq!(run_with_host(&mut vm, &mut h), Err(Trap::OutOfGas));
    assert!(h.storage.is_empty());
    assert_eq!(vm.gas_used(), 1); // only the ecall instruction

    let mut vm = vm_with(
        abi::TOKEN_TRANSFER,
        [DATA, 10, 0, 0],
        gas::HOST_TOKEN_TRANSFER,
    );
    assert_eq!(run_with_host(&mut vm, &mut h), Err(Trap::OutOfGas));
    assert_eq!(h.balances[&[9; 32]], 100);
    assert!(!h.balances.contains_key(&[0; 32]));

    // exactly enough works
    let mut vm = vm_with(abi::STORAGE_WRITE, [DATA, 1, DATA + 1, 1], need + 1);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(h.storage.len(), 1);
}

#[test]
fn huge_lengths_run_out_of_gas_not_memory() {
    let mut vm = vm_with(abi::STORAGE_WRITE, [0, u64::MAX, 0, u64::MAX], u64::MAX);
    assert_eq!(run_with_host(&mut vm, &mut host()), Err(Trap::OutOfGas));
}

#[test]
fn bad_pointers_trap_without_effect() {
    let mut h = host();
    let oob = 4090;
    for (call, args) in [
        (abi::STORAGE_READ, [oob, 20, DATA, 4]),
        (abi::STORAGE_READ, [DATA, 1, oob, 20]),
        (abi::STORAGE_READ, [u64::MAX, 2, DATA, 4]),
        (abi::STORAGE_WRITE, [oob, 20, DATA, 1]),
        (abi::STORAGE_WRITE, [DATA, 1, oob, 20]),
        (abi::STORAGE_REMOVE, [oob, 20, 0, 0]),
        (abi::CALLER, [oob, 0, 0, 0]),
        (abi::TOKEN_BALANCE, [oob, 0, 0, 0]),
        (abi::TOKEN_TRANSFER, [oob, 1, 0, 0]),
        (abi::RETURN_DATA, [oob, 20, 0, 0]),
        (abi::ABORT, [oob, 20, 0, 0]),
    ] {
        let mut vm = vm_with(call, args, 100_000);
        assert!(
            matches!(
                run_with_host(&mut vm, &mut h),
                Err(Trap::MemoryOutOfBounds { .. })
            ),
            "call {call} args {args:?}"
        );
    }
    assert!(h.storage.is_empty());
    assert_eq!(h.balances[&[9; 32]], 100);
}

#[test]
fn unknown_call() {
    for n in [0, 10, 9999, u64::MAX] {
        let mut vm = vm_with(n, [0; 4], 1000);
        assert_eq!(
            run_with_host(&mut vm, &mut host()),
            Err(Trap::UnknownHostCall(n))
        );
    }
}

#[test]
fn read_only_mode_traps_on_writes_but_allows_reads() {
    let mut h = host();
    h.read_only = true;
    h.storage.insert(b"k".to_vec(), b"v".to_vec());
    for (call, args) in [
        (abi::STORAGE_WRITE, [DATA, 1, DATA, 1]),
        (abi::STORAGE_REMOVE, [DATA, 1, 0, 0]),
        (abi::TOKEN_TRANSFER, [DATA, 1, 0, 0]),
    ] {
        let mut vm = vm_with(call, args, 10_000);
        assert_eq!(run_with_host(&mut vm, &mut h), Err(Trap::WriteInQuery));
    }
    assert_eq!(h.storage.len(), 1);
    assert_eq!(h.balances[&[9; 32]], 100);

    let mut vm = vm_with(abi::STORAGE_READ, [DATA, 1, 512, 8], 10_000);
    vm.write_mem(DATA, b"k").unwrap();
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), 1);
}
