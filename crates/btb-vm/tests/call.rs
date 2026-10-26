//! `call_contract` (host call 50) at the VM boundary: request parsing, gas, statuses, traps.
//! The host is scripted (`MemHost::call_replies`); the real nested execution is tested in
//! `btb-runtime` / `btb-chain` / `btb-tests`.

use btb_vm::gas;
use btb_vm::host::{abi, parse_call};
use btb_vm::testing::MemHost;
use btb_vm::{run_with_host, CallEnd, CallReply, Exit, FunctionKind, Trap, Vm};

const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;
const REQ: u64 = 512;
const OUT: u64 = 2048;

fn request(target: u8, kind: u8, gas: u64, function: &str, args: &[u8]) -> Vec<u8> {
    let mut r = vec![target; 32];
    r.push(kind);
    r.extend_from_slice(&gas.to_le_bytes());
    r.extend_from_slice(&(function.len() as u32).to_le_bytes());
    r.extend_from_slice(function.as_bytes());
    r.extend_from_slice(args);
    r
}

fn mkvm(req: &[u8], out_cap: u64, gas_limit: u64) -> Vm {
    let mut vm = Vm::new(4096, gas_limit);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.write_mem(4, &EBREAK.to_le_bytes()).unwrap();
    vm.write_mem(REQ, req).unwrap();
    vm.set_reg(17, abi::CALL_CONTRACT);
    vm.set_reg(10, REQ);
    vm.set_reg(11, req.len() as u64);
    vm.set_reg(12, OUT);
    vm.set_reg(13, out_cap);
    vm
}

fn reply(end: CallEnd, gas_used: u64) -> CallReply {
    CallReply { end, gas_used }
}

#[test]
fn request_layout_is_stable() {
    let r = request(7, 1, 99, "transfer", &[1, 2, 3]);
    let (target, kind, gas, f, args) = parse_call(&r).unwrap();
    assert_eq!(target, [7; 32]);
    assert_eq!(kind, FunctionKind::Mutation);
    assert_eq!((gas, f, args), (99, "transfer", &[1u8, 2, 3][..]));
    assert_eq!(
        parse_call(&request(7, 0, 0, "q", &[])).unwrap().1,
        FunctionKind::Query
    );
    // unknown kind (cron, action, anything else), short, bad UTF-8, function longer than request
    assert!(parse_call(&request(7, 2, 0, "q", &[])).is_none());
    assert!(parse_call(&r[..40]).is_none());
    assert!(
        parse_call(&r[..r.len() - 3]).is_some(),
        "args may be any length"
    );
    let mut bad = request(7, 0, 0, "ab", &[]);
    bad[45] = 0xFF;
    assert!(parse_call(&bad).is_none());
    let mut long = request(7, 0, 0, "ab", &[]);
    long[41] = 200;
    assert!(parse_call(&long).is_none());
}

#[test]
fn success_charges_exact_gas_and_copies_output() {
    let req = request(7, 1, 5_000, "f", b"xy");
    let mut h = MemHost::default();
    h.call_replies
        .push_back(reply(CallEnd::Ok(vec![9, 8, 7]), 1_234));
    let mut vm = mkvm(&req, 16, 1_000_000);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    assert_eq!(vm.reg(10), 3, "output length");
    assert_eq!(vm.read_mem(OUT, 3).unwrap(), [9, 8, 7]);
    assert_eq!(
        vm.gas_used(),
        2 + gas::HOST_CALL_CONTRACT
            + gas::CALL_BYTE * req.len() as u64
            + 1_234
            + gas::CALL_RETURN_BYTE * 3
    );
    let (target, kind, f, args, forwarded, read_only, max) = h.calls_seen[0].clone();
    assert_eq!(
        (target, kind, f.as_str(), args),
        ([7; 32], FunctionKind::Mutation, "f", b"xy".to_vec())
    );
    assert_eq!((forwarded, read_only, max), (5_000, false, 16));
}

#[test]
fn forwarded_gas_is_capped_at_what_is_left() {
    let req = request(7, 0, u64::MAX, "f", &[]);
    let mut h = MemHost::default();
    h.call_replies.push_back(reply(CallEnd::Ok(vec![]), 10));
    let limit = 100_000;
    let mut vm = mkvm(&req, 0, limit);
    assert_eq!(run_with_host(&mut vm, &mut h), Ok(Exit::Halt));
    let base = gas::HOST_CALL_CONTRACT + gas::CALL_BYTE * req.len() as u64;
    // the ecall instruction (1) and the base are already charged when the callee gets its gas
    assert_eq!(h.calls_seen[0].4, limit - 1 - base);
    assert_eq!(vm.gas_used(), 2 + base + 10);
}

#[test]
fn a_host_cannot_overcharge_beyond_forwarded() {
    let req = request(7, 0, 50, "f", &[]);
    let mut h = MemHost::default();
    h.call_replies.push_back(reply(CallEnd::Aborted, 10_000));
    let mut vm = mkvm(&req, 0, 1_000_000);
    run_with_host(&mut vm, &mut h).unwrap();
    assert_eq!(
        vm.gas_used(),
        2 + gas::HOST_CALL_CONTRACT + gas::CALL_BYTE * req.len() as u64 + 50
    );
}

#[test]
fn every_end_has_its_status() {
    let cases = [
        (CallEnd::NoContract, abi::CALL_NO_CONTRACT),
        (CallEnd::NoFunction, abi::CALL_NO_FUNCTION),
        (CallEnd::Aborted, abi::CALL_ABORTED),
        (CallEnd::Trapped, abi::CALL_TRAPPED),
        (CallEnd::OutOfGas, abi::CALL_OUT_OF_GAS),
        (CallEnd::Depth, abi::CALL_DEPTH),
        (CallEnd::Reentrant, abi::CALL_REENTRANT),
        (CallEnd::OutputTooLarge, abi::CALL_OUTPUT_TOO_LARGE),
        (CallEnd::Deposit, abi::CALL_DEPOSIT),
        (CallEnd::Unsupported, abi::CALL_UNSUPPORTED),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (end, status) in cases {
        let mut h = MemHost::default();
        h.call_replies.push_back(reply(end.clone(), 0));
        let mut vm = mkvm(&request(7, 0, 100, "f", &[]), 8, 100_000);
        run_with_host(&mut vm, &mut h).unwrap();
        assert_eq!(vm.reg(10), status, "{end:?}");
        assert!(status >= abi::CALL_ERR_MIN && seen.insert(status));
    }
    // the default host cannot run other contracts
    let mut vm = mkvm(&request(7, 0, 100, "f", &[]), 8, 100_000);
    run_with_host(&mut vm, &mut MemHost::default()).unwrap();
    assert_eq!(vm.reg(10), abi::CALL_UNSUPPORTED);
}

#[test]
fn forbidden_calls_trap_the_caller() {
    for trap in [Trap::RandomnessForbidden, Trap::CallForbidden("x")] {
        let mut h = MemHost::default();
        h.call_replies
            .push_back(CallReply::refused(CallEnd::Forbidden(trap.clone())));
        let mut vm = mkvm(&request(7, 1, 100, "f", &[]), 8, 100_000);
        assert_eq!(run_with_host(&mut vm, &mut h), Err(trap));
    }
}

#[test]
fn a_mutation_from_a_read_only_frame_traps_before_the_host_is_asked() {
    let mut h = MemHost {
        read_only: true,
        ..Default::default()
    };
    let mut vm = mkvm(&request(7, 1, 100, "f", &[]), 8, 100_000);
    assert_eq!(run_with_host(&mut vm, &mut h), Err(Trap::WriteInQuery));
    assert!(h.calls_seen.is_empty());
    // a query is fine, and the host is told the frame is read-only
    h.call_replies.push_back(reply(CallEnd::Ok(vec![]), 0));
    let mut vm = mkvm(&request(7, 0, 100, "f", &[]), 8, 100_000);
    run_with_host(&mut vm, &mut h).unwrap();
    assert!(h.calls_seen[0].5);
}

#[test]
fn malformed_requests_and_windows_trap_without_calling_the_host() {
    let mut h = MemHost::default();
    // unknown kind
    let mut v = mkvm(&request(7, 9, 100, "f", &[]), 8, 100_000);
    assert!(matches!(run_with_host(&mut v, &mut h), Err(Trap::Abort(_))));
    // request outside memory
    let mut v = mkvm(&request(7, 0, 100, "f", &[]), 8, 10_000_000);
    v.set_reg(11, 1 << 20);
    assert!(matches!(
        run_with_host(&mut v, &mut h),
        Err(Trap::MemoryOutOfBounds { .. })
    ));
    // output window outside memory
    let mut v = mkvm(&request(7, 0, 100, "f", &[]), 1 << 40, 100_000);
    assert!(matches!(
        run_with_host(&mut v, &mut h),
        Err(Trap::MemoryOutOfBounds { .. })
    ));
    // too little gas for the base cost
    let mut v = mkvm(&request(7, 0, 100, "f", &[]), 8, 500);
    assert_eq!(run_with_host(&mut v, &mut h), Err(Trap::OutOfGas));
    assert!(h.calls_seen.is_empty());
}

#[test]
fn callee_gas_that_the_caller_cannot_afford_traps_the_caller() {
    // The host reports usage within what was forwarded, but the output copy then runs dry.
    let req = request(7, 0, 100, "f", &[]);
    let base = 1 + gas::HOST_CALL_CONTRACT + gas::CALL_BYTE * req.len() as u64;
    let mut h = MemHost::default();
    h.call_replies
        .push_back(reply(CallEnd::Ok(vec![0; 64]), 100));
    let mut v = mkvm(&req, 64, base + 100 + 10);
    assert_eq!(run_with_host(&mut v, &mut h), Err(Trap::OutOfGas));
}
