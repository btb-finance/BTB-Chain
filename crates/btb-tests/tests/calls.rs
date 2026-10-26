//! Cross-contract call ABI: the Rust SDK, the C header and the VM agree on the numbers and the
//! request layout. (Behavior of the calls themselves: `crates/btb-chain/tests/calls.rs`.)

use btb_sdk::call::{encode_request, num, Kind};
use btb_sdk::Address;
use btb_vm::host::{abi, parse_call};
use btb_vm::FunctionKind;

#[test]
fn sdk_numbers_match_the_vm() {
    for (sdk, vm) in [
        (num::CALL_CONTRACT, abi::CALL_CONTRACT),
        (num::CALL_ERR_MIN, abi::CALL_ERR_MIN),
        (num::CALL_NO_CONTRACT, abi::CALL_NO_CONTRACT),
        (num::CALL_NO_FUNCTION, abi::CALL_NO_FUNCTION),
        (num::CALL_ABORTED, abi::CALL_ABORTED),
        (num::CALL_TRAPPED, abi::CALL_TRAPPED),
        (num::CALL_OUT_OF_GAS, abi::CALL_OUT_OF_GAS),
        (num::CALL_DEPTH, abi::CALL_DEPTH),
        (num::CALL_REENTRANT, abi::CALL_REENTRANT),
        (num::CALL_OUTPUT_TOO_LARGE, abi::CALL_OUTPUT_TOO_LARGE),
        (num::CALL_DEPOSIT, abi::CALL_DEPOSIT),
        (num::CALL_UNSUPPORTED, abi::CALL_UNSUPPORTED),
    ] {
        assert_eq!(sdk, vm);
    }
    assert_eq!(btb_sdk::call::MAX_DEPTH, abi::MAX_CALL_DEPTH);
    // the statuses are distinct and all at or above CALL_ERR_MIN
    let all = [
        abi::CALL_NO_CONTRACT,
        abi::CALL_NO_FUNCTION,
        abi::CALL_ABORTED,
        abi::CALL_TRAPPED,
        abi::CALL_OUT_OF_GAS,
        abi::CALL_DEPTH,
        abi::CALL_REENTRANT,
        abi::CALL_OUTPUT_TOO_LARGE,
        abi::CALL_DEPOSIT,
        abi::CALL_UNSUPPORTED,
    ];
    let set: std::collections::BTreeSet<_> = all.iter().collect();
    assert_eq!(set.len(), all.len());
    assert!(all.iter().all(|s| *s >= abi::CALL_ERR_MIN));
}

#[test]
fn sdk_requests_parse_in_the_vm() {
    let t = Address([9; 32]);
    let r = encode_request(&t, Kind::Mutation, "transfer", &[1, 2, 3], 777);
    let (target, kind, gas, f, args) = parse_call(&r).unwrap();
    assert_eq!(
        (target, kind, gas, f, args),
        (
            [9; 32],
            FunctionKind::Mutation,
            777,
            "transfer",
            &[1u8, 2, 3][..]
        )
    );
    let r = encode_request(&t, Kind::Query, "", &[], btb_sdk::call::GAS_ALL);
    let (_, kind, gas, f, args) = parse_call(&r).unwrap();
    assert_eq!(
        (kind, gas, f, args.len()),
        (FunctionKind::Query, u64::MAX, "", 0)
    );
}

/// The C header's expression-valued constants (`UINT64_MAX - n`) are the VM's.
#[test]
fn header_call_statuses_match_the_vm() {
    let h = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../include/btb.h"))
        .expect("read btb.h");
    let eval = |name: &str| -> u64 {
        let line = h
            .lines()
            .find_map(|l| l.strip_prefix(&format!("#define {name} ")))
            .unwrap_or_else(|| panic!("{name} missing"));
        let v = line
            .split("/*")
            .next()
            .unwrap()
            .trim()
            .trim_matches(|c| c == '(' || c == ')');
        match v.strip_prefix("UINT64_MAX") {
            Some("") => u64::MAX,
            Some(rest) => {
                u64::MAX
                    - rest
                        .trim()
                        .trim_start_matches('-')
                        .trim()
                        .parse::<u64>()
                        .unwrap()
            }
            None => panic!("{name}: {v}"),
        }
    };
    for (n, v) in [
        ("BTB_CALL_ERR_MIN", abi::CALL_ERR_MIN),
        ("BTB_CALL_NO_CONTRACT", abi::CALL_NO_CONTRACT),
        ("BTB_CALL_NO_FUNCTION", abi::CALL_NO_FUNCTION),
        ("BTB_CALL_ABORTED", abi::CALL_ABORTED),
        ("BTB_CALL_TRAPPED", abi::CALL_TRAPPED),
        ("BTB_CALL_OUT_OF_GAS", abi::CALL_OUT_OF_GAS),
        ("BTB_CALL_DEPTH", abi::CALL_DEPTH),
        ("BTB_CALL_REENTRANT", abi::CALL_REENTRANT),
        ("BTB_CALL_OUTPUT_TOO_LARGE", abi::CALL_OUTPUT_TOO_LARGE),
        ("BTB_CALL_DEPOSIT", abi::CALL_DEPOSIT),
        ("BTB_CALL_UNSUPPORTED", abi::CALL_UNSUPPORTED),
        ("BTB_GAS_ALL", u64::MAX),
    ] {
        assert_eq!(eval(n), v, "{n}");
    }
}
