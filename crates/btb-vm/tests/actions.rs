//! HTTPS-action support in the VM: transform entries (pure), the read-only first phase,
//! callback detection and the `report_request` host call.

use btb_vm::gas;
use btb_vm::host::abi;
use btb_vm::testing::{build_elf, MemHost};
use btb_vm::{run_with_host, CallError, Contract, ContractError, Exit, FunctionKind, Trap, Vm};

const MEM: usize = 1 << 18;
const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;
const RET: u32 = 0x0000_8067;
const SPIN: u32 = 0x0000_006f;

fn addi(rd: u32, rs1: u32, imm: u32) -> u32 {
    (imm << 20) | (rs1 << 15) | (rd << 7) | 0x13
}

/// echo (return_data(input)) @0, storage_write(input[0] -> input[0]) @12,
/// storage_read(input[0]) then return_data(a0 = result) @36 (see below), spin @60,
/// report_request(input) @68.
fn code() -> Vec<u8> {
    [
        // @0 echo
        addi(17, 0, 8),
        ECALL,
        RET,
        // @12 storage_write
        addi(13, 0, 1),
        addi(11, 0, 1),
        addi(12, 10, 0),
        addi(17, 0, 2),
        ECALL,
        RET,
        // @36 storage_read: key = input[0..1] into a buffer 64 bytes below, then return what the
        // call returned as the length (a0) by writing it to memory and returning it
        addi(12, 10, 0),
        addi(13, 0, 8),
        addi(11, 0, 1),
        addi(17, 0, 1),
        ECALL,
        RET,
        // @60 spin
        SPIN,
        // @64 padding
        addi(0, 0, 0),
        // @68 report_request(input ptr, input len)
        addi(17, 0, 42),
        ECALL,
        RET,
    ]
    .iter()
    .flat_map(|w| w.to_le_bytes())
    .collect()
}

fn contract(syms: &[(&str, u64)]) -> Result<Contract, ContractError> {
    Contract::from_elf(&build_elf(&code(), syms))
}

fn all() -> Contract {
    contract(&[
        ("btb_action_fetch", 12), // writes: must be trapped, actions run read-only
        ("btb_action_echo", 0),
        ("btb_mutation_fetch_callback", 0),
        ("btb_mutation_other", 0),
        ("btb_transform_echo", 0),
        ("btb_transform_fetch", 12),
        ("btb_transform_look", 36),
        ("btb_transform_spin", 60),
        ("btb_mutation_report", 68),
        ("btb_query_report", 68),
        ("btb_action_report", 68),
        ("btb_mutation_put", 12),
    ])
    .unwrap()
}

#[test]
fn transform_entries_are_collected_and_validated() {
    let c = all();
    assert!(c.has_transform("echo") && c.has_transform("fetch") && c.has_transform("spin"));
    assert!(!c.has_transform("nothing"));
    assert_eq!(
        c.transform_names().collect::<Vec<_>>(),
        vec!["echo", "fetch", "look", "spin"]
    );
    // transforms are not entry points of any kind
    assert!(c
        .entry_points()
        .iter()
        .all(|e| !e.name.contains("transform")));
    // a bad name is refused at deploy
    assert!(matches!(
        contract(&[("btb_query_a", 0), ("btb_transform_Bad", 0)]),
        Err(ContractError::InvalidName(_))
    ));
    assert!(matches!(
        contract(&[("btb_query_a", 0), ("btb_transform_", 0)]),
        Err(ContractError::InvalidName(_))
    ));
    // the same transform twice is refused
    assert!(matches!(
        contract(&[
            ("btb_query_a", 0),
            ("btb_transform_x", 0),
            ("btb_transform_x", 4)
        ]),
        Err(ContractError::DuplicateEntry { .. })
    ));
}

#[test]
fn a_transform_is_a_pure_function_of_its_input() {
    let c = all();
    let o = c
        .call_transform("echo", b"response bytes", 100_000, MEM)
        .unwrap();
    assert_eq!(o.output, b"response bytes");
    // same input, same output, same gas
    let o2 = c
        .call_transform("echo", b"response bytes", 100_000, MEM)
        .unwrap();
    assert_eq!(o, o2);
    // no state changes: a write traps
    assert!(matches!(
        c.call_transform("fetch", b"k", 100_000, MEM),
        Err(CallError::Trap {
            trap: Trap::WriteInQuery,
            ..
        })
    ));
    // no state to read either: whatever the chain stores, the transform sees nothing
    let o = c.call_transform("look", b"k", 100_000, MEM).unwrap();
    assert!(o.output.is_empty());
    // a missing transform
    assert!(matches!(
        c.call_transform("nothing", b"", 100_000, MEM),
        Err(CallError::NoSuchEntry { .. })
    ));
    // out of gas, not a hang
    assert!(matches!(
        c.call_transform("spin", b"", 5_000, MEM),
        Err(CallError::Trap {
            trap: Trap::OutOfGas,
            ..
        })
    ));
}

#[test]
fn a_transform_cannot_queue_reports() {
    // report_request from a transform is refused as a state change
    let mut vm = Vm::new(4096, 100_000);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.set_reg(17, abi::REPORT_REQUEST);
    vm.set_reg(10, 512);
    vm.set_reg(11, 4);
    let mut host = MemHost {
        read_only: true,
        ..Default::default()
    };
    assert_eq!(run_with_host(&mut vm, &mut host), Err(Trap::WriteInQuery));
    assert!(host.reports.is_empty());
}

#[test]
fn the_first_phase_of_an_action_runs_read_only() {
    let c = all();
    let mut h = MemHost::default();
    // the same code is a plain write as a mutation...
    c.call(FunctionKind::Mutation, "put", b"k", &mut h, 100_000, MEM)
        .unwrap();
    assert_eq!(h.storage.len(), 1);
    // ...and a trap as an action, even though the host itself allows writes
    h.storage.clear();
    let e = c
        .call(FunctionKind::Action, "fetch", b"k", &mut h, 100_000, MEM)
        .unwrap_err();
    assert!(matches!(
        e,
        CallError::Trap {
            trap: Trap::WriteInQuery,
            ..
        }
    ));
    assert!(h.storage.is_empty());
    // a pure action returns its output
    let o = c
        .call(FunctionKind::Action, "echo", b"spec", &mut h, 100_000, MEM)
        .unwrap();
    assert_eq!(o.output, b"spec");
}

#[test]
fn callbacks_of_actions_are_recognised() {
    let c = all();
    assert!(c.is_action_callback("fetch_callback"));
    assert!(!c.is_action_callback("fetch"));
    assert!(!c.is_action_callback("other"));
    assert!(!c.is_action_callback("other_callback"), "no action 'other'");
    assert!(!c.is_action_callback("_callback"));
    // 'echo' is an action: the name `echo_callback` is reserved even if no such mutation exists
    assert!(c.is_action_callback("echo_callback"));
}

#[test]
fn report_request_queues_in_mutations_only() {
    let c = all();
    let mut h = MemHost::default();
    let data = b"ETH/USD=3412.50 at 77";
    let o = c
        .call(
            FunctionKind::Mutation,
            "report",
            data,
            &mut h,
            1_000_000,
            MEM,
        )
        .unwrap();
    assert_eq!(h.reports, vec![data.to_vec()]);
    // the gas: the ecall and instructions plus the host call's price
    assert!(o.gas_used >= gas::HOST_REPORT_REQUEST + gas::REPORT_BYTE * data.len() as u64);
    // a second call gets the next id
    c.call(
        FunctionKind::Mutation,
        "report",
        b"x",
        &mut h,
        1_000_000,
        MEM,
    )
    .unwrap();
    assert_eq!(h.reports.len(), 2);
    // queries and actions are read-only: trapped, nothing queued
    for kind in [FunctionKind::Query, FunctionKind::Action] {
        let before = h.reports.len();
        let e = c
            .call(kind, "report", data, &mut h, 1_000_000, MEM)
            .unwrap_err();
        assert!(
            matches!(
                e,
                CallError::Trap {
                    trap: Trap::WriteInQuery,
                    ..
                }
            ),
            "{kind:?}: {e:?}"
        );
        assert_eq!(h.reports.len(), before);
    }
}

#[test]
fn report_request_limits_and_gas() {
    fn vm_with(len: u64, gas_limit: u64) -> Vm {
        let mut vm = Vm::new(8192, gas_limit);
        vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
        vm.write_mem(4, &EBREAK.to_le_bytes()).unwrap();
        vm.set_reg(17, abi::REPORT_REQUEST);
        vm.set_reg(10, 1024);
        vm.set_reg(11, len);
        vm
    }
    let mut h = MemHost::default();
    // exactly the maximum is fine and costs base + per byte, charged first
    let mut vm = vm_with(abi::MAX_REPORT_BYTES, 1_000_000);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(
        vm.gas_used(),
        2 + gas::HOST_REPORT_REQUEST + gas::REPORT_BYTE * abi::MAX_REPORT_BYTES
    );
    assert_eq!(vm.reg(10), 0, "the first report has id 0");
    assert_eq!(h.reports[0].len() as u64, abi::MAX_REPORT_BYTES);
    // one byte more aborts, queues nothing
    let mut vm = vm_with(abi::MAX_REPORT_BYTES + 1, 1_000_000);
    assert!(matches!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::Abort(_))
    ));
    assert_eq!(h.reports.len(), 1);
    // out of gas is detected before anything is read or queued
    let mut vm = vm_with(100, 1_000);
    assert_eq!(run_with_host(&mut vm, &mut h), Err(Trap::OutOfGas));
    assert_eq!(h.reports.len(), 1);
    // a window outside memory traps
    let mut vm = vm_with(100, 1_000_000);
    vm.set_reg(10, 8190);
    assert!(matches!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::MemoryOutOfBounds { .. })
    ));
    // a host without a report queue answers NOT_FOUND
    struct NoReports(MemHost);
    impl btb_vm::host::Host for NoReports {
        fn storage_read(&mut self, k: &[u8]) -> Option<Vec<u8>> {
            self.0.storage_read(k)
        }
        fn storage_write(&mut self, k: &[u8], v: &[u8]) {
            self.0.storage_write(k, v)
        }
        fn storage_remove(&mut self, k: &[u8]) {
            self.0.storage_remove(k)
        }
        fn caller(&self) -> [u8; 32] {
            [0; 32]
        }
        fn block_height(&self) -> u64 {
            0
        }
        fn token_balance(&self, _: &[u8; 32]) -> u64 {
            0
        }
        fn token_transfer(&mut self, _: &[u8; 32], _: u64) -> bool {
            false
        }
        fn table_insert(&mut self, _: &[u8], _: &[u8]) -> u64 {
            0
        }
        fn table_get(&mut self, _: &[u8], _: u64) -> Option<Vec<u8>> {
            None
        }
        fn table_update(&mut self, _: &[u8], _: u64, _: &[u8]) -> bool {
            false
        }
        fn table_delete(&mut self, _: &[u8], _: u64) -> bool {
            false
        }
        fn table_scan(&mut self, _: &[u8], _: u64, _: u64) -> Vec<u64> {
            vec![]
        }
    }
    let mut vm = vm_with(10, 1_000_000);
    assert_eq!(
        run_with_host(&mut vm, &mut NoReports(MemHost::default())),
        Ok(Exit::Halt)
    );
    assert_eq!(vm.reg(10), abi::NOT_FOUND);
}
