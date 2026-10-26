//! File storage ABI (`file_info`, `file_pin`) and the optional `MemHost` resolver for
//! cross-contract calls in tests. Chain behavior: `crates/btb-chain/tests/files.rs`; dev server:
//! `dev_multi.rs`.

use btb_codec::{Address, Decode};
use btb_sdk::files::num;
use btb_vm::host::{abi, parse_file_pin, FileRecord};
use btb_vm::testing::MemHost;
use btb_vm::{Contract, FunctionKind};
use std::sync::Arc;

mod common;
use common::*;

#[test]
fn sdk_numbers_match_the_vm() {
    assert_eq!(num::FILE_INFO, abi::FILE_INFO);
    assert_eq!(num::FILE_PIN, abi::FILE_PIN);
    assert_eq!(num::FILE_PIN_REQUEST_LEN as u64, abi::FILE_PIN_REQUEST_LEN);
    assert_eq!(num::FILE_UNKNOWN, abi::FILE_UNKNOWN);
    assert_eq!(num::FILE_STORED, abi::FILE_STORED);
    assert_eq!(num::FILE_EXPIRED, abi::FILE_EXPIRED);
    for (a, b) in [
        (num::FILE_PIN_OK, abi::FILE_PIN_OK),
        (num::FILE_PIN_EXISTS, abi::FILE_PIN_EXISTS),
        (num::FILE_PIN_INSUFFICIENT, abi::FILE_PIN_INSUFFICIENT),
        (num::FILE_PIN_NO_NODES, abi::FILE_PIN_NO_NODES),
        (num::FILE_PIN_INVALID, abi::FILE_PIN_INVALID),
        (num::FILE_PIN_UNSUPPORTED, abi::FILE_PIN_UNSUPPORTED),
    ] {
        assert_eq!(a, b);
    }
    // The numbers are in their own range, clear of the other modules.
    assert!((70..80).contains(&abi::FILE_INFO) && (70..80).contains(&abi::FILE_PIN));
    let header =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../include/btb.h"))
            .unwrap();
    assert!(
        header.contains("#define BTB_FILE_INFO 70") && header.contains("#define BTB_FILE_PIN 71")
    );
}

#[test]
fn record_and_request_layouts() {
    let r = FileRecord {
        size: 5,
        status: abi::FILE_STORED,
        expiry: 99,
    };
    let b = r.encode();
    assert_eq!(&b[..8], &5u64.to_le_bytes());
    assert_eq!(b[8], 1);
    assert_eq!(&b[9..16], &[0; 7]);
    assert_eq!(&b[16..], &99u64.to_le_bytes());
    let mut req = vec![7u8; 32];
    req.extend_from_slice(&1000u64.to_le_bytes());
    req.extend_from_slice(&[4, 2]);
    req.extend_from_slice(&12u32.to_le_bytes());
    let p = parse_file_pin(&req).unwrap();
    assert_eq!(
        (p.root, p.size, p.k, p.m, p.periods),
        ([7; 32], 1000, 4, 2, 12)
    );
    assert!(parse_file_pin(&req[..45]).is_none());
}

#[test]
fn files_example_on_memhost_info_pin_and_read_only() {
    let c = Contract::from_elf(&build_example("files")).unwrap();
    let mut h = MemHost::default();
    let root = Address([3; 32]);
    let info = |h: &mut MemHost| {
        let (o, _) = call(&c, FunctionKind::Query, "info", &(root,), h).unwrap();
        <(u64, u8, u64)>::decode(&o).unwrap()
    };
    assert_eq!(info(&mut h), (0, 0, 0));
    // attach refuses an unknown root
    let e = call(
        &c,
        FunctionKind::Mutation,
        "attach",
        &("a".to_string(), root),
        &mut h,
    )
    .unwrap_err();
    assert_eq!(abort_msg(e), "file is not stored");
    // pin records the request and the file reads as stored
    let (o, gas) = call(
        &c,
        FunctionKind::Mutation,
        "pin_and_attach",
        &("a".to_string(), root, 4_000u64, 2u8, 1u8, 5u32),
        &mut h,
    )
    .unwrap();
    assert_eq!(o, vec![0]);
    assert!(gas >= btb_vm::gas::HOST_FILE_PIN);
    assert_eq!(h.pins_seen.len(), 1);
    assert_eq!(h.pins_seen[0].periods, 5);
    assert_eq!(info(&mut h).1, 1);
    let (o, _) = call(
        &c,
        FunctionKind::Mutation,
        "attach",
        &("b".to_string(), root),
        &mut h,
    )
    .unwrap();
    assert_eq!(u64::decode(&o).unwrap(), 4_000);
    // a refusal is a status
    h.pin_result = Some(abi::FILE_PIN_INSUFFICIENT);
    let other = Address([4; 32]);
    let (o, _) = call(
        &c,
        FunctionKind::Mutation,
        "pin_and_attach",
        &("c".to_string(), other, 1u64, 1u8, 1u8, 1u32),
        &mut h,
    )
    .unwrap();
    assert_eq!(o, vec![2]);
    // pinning from a query traps
    let mut ro = h.clone();
    ro.read_only = true;
    assert!(call(
        &c,
        FunctionKind::Query,
        "bad_pin_in_query",
        &(root,),
        &mut ro
    )
    .is_err());
}

#[test]
fn memhost_resolver_runs_other_contracts() {
    let router = Arc::new(Contract::from_elf(&build_example("caller")).unwrap());
    let token = Arc::new(Contract::from_elf(&build_example("token")).unwrap());
    let (router_addr, token_addr) = ([0xA1; 32], [0xA2; 32]);
    let mut h = MemHost {
        contract: router_addr,
        ..Default::default()
    };
    h.add_contract(token_addr, token.clone());
    h.add_contract(router_addr, router.clone());
    // The token holds the router's balance after init, called as the router would.
    let mut as_router = MemHost {
        contract: token_addr,
        caller: router_addr,
        ..Default::default()
    };
    call(
        &token,
        FunctionKind::Mutation,
        "init",
        &(1_000u128,),
        &mut as_router,
    )
    .unwrap();
    h.callees.insert(token_addr, as_router);
    let bob = Address([0xB0; 32]);
    let (o, _) = call(
        &router,
        FunctionKind::Mutation,
        "forward_transfer",
        &(Address(token_addr), bob, 300u128),
        &mut h,
    )
    .unwrap();
    assert!(bool::decode(&o).unwrap());
    let (o, _) = call(
        &router,
        FunctionKind::Query,
        "balance_via",
        &(Address(token_addr), bob),
        &mut h,
    )
    .unwrap();
    assert_eq!(u128::decode(&o).unwrap(), 300);
    // an aborting callee is reverted and reported (Aborted = 3); the router goes on
    let (o, _) = call(
        &router,
        FunctionKind::Mutation,
        "try_transfer",
        &(Address(token_addr), bob, 9_999u128),
        &mut h,
    )
    .unwrap();
    assert_eq!(o, vec![3]);
    // no such contract
    let (o, _) = call(
        &router,
        FunctionKind::Mutation,
        "try_transfer",
        &(Address([9; 32]), bob, 1u128),
        &mut h,
    )
    .unwrap();
    assert_eq!(o, vec![1]);
    // without a registered contract the default stays: Unsupported (10)
    let mut bare = MemHost {
        contract: router_addr,
        ..Default::default()
    };
    let (o, _) = call(
        &router,
        FunctionKind::Mutation,
        "try_transfer",
        &(Address(token_addr), bob, 1u128),
        &mut bare,
    )
    .unwrap();
    assert_eq!(o, vec![10]);
}
