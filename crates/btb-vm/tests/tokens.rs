//! User-token host calls (16..23): exact gas, status codes, read-only traps, failure modes.

use btb_vm::gas;
use btb_vm::host::{abi, Host};
use btb_vm::testing::MemHost;
use btb_vm::token::{derive_id, CreateParams, TokenInfo};
use btb_vm::{run_with_host, Exit, Trap, Vm};

const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;
const ME: [u8; 32] = [9; 32];
const OTHER: [u8; 32] = [5; 32];
const ID: u64 = 256;
const TO: u64 = 320;
const AMT: u64 = 384;
const OUT: u64 = 512;

fn host() -> MemHost {
    MemHost {
        contract: ME,
        caller: [7; 32],
        ..Default::default()
    }
}

fn vm_with(call: u64, args: [u64; 4]) -> Vm {
    let mut vm = Vm::new(8192, 1_000_000);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.write_mem(4, &EBREAK.to_le_bytes()).unwrap();
    vm.set_reg(17, call);
    for (i, a) in args.iter().enumerate() {
        vm.set_reg(10 + i, *a);
    }
    vm
}

fn params(supply: u128, authority: Option<[u8; 32]>) -> CreateParams {
    CreateParams {
        name: "Gold".into(),
        symbol: "GLD".into(),
        decimals: 6,
        initial_supply: supply,
        mint_authority: authority,
    }
}

/// Run one call; returns (a0, gas used, vm).
fn run(h: &mut MemHost, call: u64, args: [u64; 4], setup: impl FnOnce(&mut Vm)) -> (u64, u64, Vm) {
    let mut vm = vm_with(call, args);
    setup(&mut vm);
    assert_eq!(run_with_host(&mut vm, h), Ok(Exit::Halt));
    (vm.reg(10), vm.gas_used(), vm)
}

fn create(h: &mut MemHost, p: &CreateParams) -> [u8; 32] {
    let req = p.encode();
    let (st, _, vm) = run(h, abi::TOKEN_CREATE, [TO, req.len() as u64, OUT, 0], |vm| {
        vm.write_mem(TO, &req).unwrap()
    });
    assert_eq!(st, abi::TOKEN_OK);
    vm.read_mem(OUT, 32).unwrap().try_into().unwrap()
}

fn amount_call(h: &mut MemHost, call: u64, id: &[u8; 32], to: &[u8; 32], amt: u128) -> (u64, u64) {
    let (st, gas, _) = run(h, call, [ID, TO, AMT, 0], |vm| {
        vm.write_mem(ID, id).unwrap();
        vm.write_mem(TO, to).unwrap();
        vm.write_mem(AMT, &amt.to_le_bytes()).unwrap();
    });
    (st, gas)
}

#[test]
fn call_numbers_continue_after_table_index_scan() {
    let n = [
        abi::TOKEN_CREATE,
        abi::TOKEN_TRANSFER_OF,
        abi::TOKEN_BALANCE_OF,
        abi::TOKEN_INFO,
        abi::TOKEN_HOLDERS,
        abi::TOKEN_MINT,
        abi::TOKEN_BURN,
        abi::CONTRACT_ADDRESS,
    ];
    assert_eq!(n, [16, 17, 18, 19, 20, 21, 22, 23]);
    assert_eq!(abi::TABLE_INDEX_SCAN, 15);
}

#[test]
fn create_has_exact_gas_and_deterministic_id() {
    let mut h = host();
    let p = params(1_000, None);
    let req = p.encode();
    let (st, g, vm) = run(
        &mut h,
        abi::TOKEN_CREATE,
        [TO, req.len() as u64, OUT, 0],
        |vm| vm.write_mem(TO, &req).unwrap(),
    );
    assert_eq!(st, abi::TOKEN_OK);
    assert_eq!(
        g,
        2 + gas::HOST_TOKEN_CREATE + gas::TOKEN_BYTE * req.len() as u64
    );
    let id: [u8; 32] = vm.read_mem(OUT, 32).unwrap().try_into().unwrap();
    assert_eq!(id, derive_id(&ME, 0));
    assert_eq!(h.tokens[&id].supply, 1_000);
    assert_eq!(h.token_balances[&(id, ME)], 1_000);
    // Second token: next nonce, different id.
    assert_eq!(create(&mut h, &p), derive_id(&ME, 1));
    // Native BTB is not involved.
    assert!(h.balances.is_empty());
}

#[test]
fn invalid_and_malformed_create() {
    let mut h = host();
    for bad in [
        CreateParams {
            name: String::new(),
            ..params(1, None)
        },
        CreateParams {
            name: "x".repeat(65),
            ..params(1, None)
        },
        CreateParams {
            symbol: String::new(),
            ..params(1, None)
        },
        CreateParams {
            symbol: "TOOLONGSYMBOLS".into(),
            ..params(1, None)
        },
        CreateParams {
            symbol: "A B".into(),
            ..params(1, None)
        },
        CreateParams {
            decimals: 39,
            ..params(1, None)
        },
    ] {
        let req = bad.encode();
        let (st, _, _) = run(
            &mut h,
            abi::TOKEN_CREATE,
            [TO, req.len() as u64, OUT, 0],
            |vm| vm.write_mem(TO, &req).unwrap(),
        );
        assert_eq!(st, abi::TOKEN_INVALID, "{bad:?}");
    }
    assert!(h.tokens.is_empty() && h.token_nonces.is_empty());

    let mut req = params(1, None).encode();
    req.push(0); // trailing byte
    let mut vm = vm_with(abi::TOKEN_CREATE, [TO, req.len() as u64, OUT, 0]);
    vm.write_mem(TO, &req).unwrap();
    assert!(matches!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::Abort(_))
    ));
}

#[test]
fn transfer_balance_info_with_exact_gas() {
    let mut h = host();
    let id = create(&mut h, &params(100, Some(OTHER)));
    let alice = [1u8; 32];

    let (st, g) = amount_call(&mut h, abi::TOKEN_TRANSFER_OF, &id, &alice, 30);
    assert_eq!((st, g), (abi::TOKEN_OK, 2 + gas::HOST_TOKEN_TRANSFER_OF));
    // Insufficient: no change.
    let (st, _) = amount_call(&mut h, abi::TOKEN_TRANSFER_OF, &id, &alice, 71);
    assert_eq!(st, abi::TOKEN_INSUFFICIENT);
    assert_eq!(h.token_balances[&(id, ME)], 70);
    // Unknown token.
    let (st, _) = amount_call(&mut h, abi::TOKEN_TRANSFER_OF, &[0xEE; 32], &alice, 1);
    assert_eq!(st, abi::TOKEN_NO_TOKEN);
    // Recipient overflow leaves both balances alone.
    h.token_balances.insert((id, alice), u128::MAX - 5);
    let (st, _) = amount_call(&mut h, abi::TOKEN_TRANSFER_OF, &id, &alice, 6);
    assert_eq!(st, abi::TOKEN_OVERFLOW);
    assert_eq!(h.token_balances[&(id, ME)], 70);
    h.token_balances.insert((id, alice), 30);

    let (st, g, vm) = run(&mut h, abi::TOKEN_BALANCE_OF, [ID, TO, OUT, 0], |vm| {
        vm.write_mem(ID, &id).unwrap();
        vm.write_mem(TO, &alice).unwrap();
    });
    assert_eq!((st, g), (abi::TOKEN_OK, 2 + gas::HOST_TOKEN_BALANCE_OF));
    assert_eq!(vm.read_mem(OUT, 16).unwrap(), 30u128.to_le_bytes());

    let want = TokenInfo {
        name: "Gold".into(),
        symbol: "GLD".into(),
        decimals: 6,
        supply: 100,
        mint_authority: Some(OTHER),
        creator: ME,
    }
    .encode();
    let (n, g, vm) = run(&mut h, abi::TOKEN_INFO, [ID, OUT, 256, 0], |vm| {
        vm.write_mem(ID, &id).unwrap()
    });
    assert_eq!(n, want.len() as u64);
    assert_eq!(
        g,
        2 + gas::HOST_TOKEN_INFO + gas::TOKEN_BYTE * want.len() as u64
    );
    assert_eq!(vm.read_mem(OUT, want.len()).unwrap(), want);
    // Unknown token.
    let (n, _, _) = run(&mut h, abi::TOKEN_INFO, [ID, OUT, 256, 0], |_| {});
    assert_eq!(n, abi::NOT_FOUND);
}

#[test]
fn mint_and_burn_need_the_mint_authority() {
    // Authority is ANOTHER address: the running contract may not mint or burn.
    let mut h = host();
    let id = create(&mut h, &params(100, Some(OTHER)));
    assert_eq!(
        amount_call(&mut h, abi::TOKEN_MINT, &id, &[1; 32], 1).0,
        abi::TOKEN_UNAUTHORIZED
    );
    let (st, _, _) = run(&mut h, abi::TOKEN_BURN, [ID, AMT, 0, 0], |vm| {
        vm.write_mem(ID, &id).unwrap();
        vm.write_mem(AMT, &1u128.to_le_bytes()).unwrap();
    });
    assert_eq!(st, abi::TOKEN_UNAUTHORIZED);
    // No authority at all: fixed supply.
    let fixed = create(&mut h, &params(100, None));
    assert_eq!(
        amount_call(&mut h, abi::TOKEN_MINT, &fixed, &[1; 32], 1).0,
        abi::TOKEN_UNAUTHORIZED
    );
    assert_eq!(h.tokens[&id].supply, 100);
    assert_eq!(h.tokens[&fixed].supply, 100);

    // This contract as authority: mint and burn work, with checked math and exact gas.
    let m = create(&mut h, &params(10, Some(ME)));
    let (st, g) = amount_call(&mut h, abi::TOKEN_MINT, &m, &[1; 32], 5);
    assert_eq!((st, g), (abi::TOKEN_OK, 2 + gas::HOST_TOKEN_MINT));
    assert_eq!(
        (h.tokens[&m].supply, h.token_balances[&(m, [1; 32])]),
        (15, 5)
    );
    let (st, g, _) = run(&mut h, abi::TOKEN_BURN, [ID, AMT, 0, 0], |vm| {
        vm.write_mem(ID, &m).unwrap();
        vm.write_mem(AMT, &4u128.to_le_bytes()).unwrap();
    });
    assert_eq!((st, g), (abi::TOKEN_OK, 2 + gas::HOST_TOKEN_BURN));
    assert_eq!((h.tokens[&m].supply, h.token_balances[&(m, ME)]), (11, 6));
    assert_eq!(
        amount_call(&mut h, abi::TOKEN_MINT, &m, &[2; 32], u128::MAX).0,
        abi::TOKEN_OVERFLOW
    );
    let (st, _, _) = run(&mut h, abi::TOKEN_BURN, [ID, AMT, 0, 0], |vm| {
        vm.write_mem(ID, &m).unwrap();
        vm.write_mem(AMT, &7u128.to_le_bytes()).unwrap();
    });
    assert_eq!(st, abi::TOKEN_INSUFFICIENT);
    assert_eq!(h.tokens[&m].supply, 11);
}

#[test]
fn holders_pages_with_cursor_and_exact_gas() {
    let mut h = host();
    let id = create(&mut h, &params(100, None));
    for n in [3u8, 1, 2] {
        assert_eq!(
            amount_call(&mut h, abi::TOKEN_TRANSFER_OF, &id, &[n; 32], 10).0,
            0
        );
    }
    // Holders ascending by address: [1;32], [2;32], [3;32], ME=[9;32].
    let mut req = id.to_vec();
    req.extend_from_slice(&2u32.to_le_bytes());
    let (total, g, vm) = run(
        &mut h,
        abi::TOKEN_HOLDERS,
        [TO, req.len() as u64, OUT, 1024],
        |vm| vm.write_mem(TO, &req).unwrap(),
    );
    assert_eq!(total, 8 + 2 * 48 + 32);
    assert_eq!(
        g,
        2 + gas::HOST_TOKEN_HOLDERS
            + gas::TOKEN_BYTE * req.len() as u64
            + gas::TOKEN_HOLDER_ENTRY * 2
            + gas::TOKEN_BYTE * 32
    );
    let out = vm.read_mem(OUT, total as usize).unwrap().to_vec();
    assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 2);
    assert_eq!(&out[8..40], &[1u8; 32]);
    assert_eq!(
        &out[8 + 96..8 + 96 + 32],
        &[2u8; 32],
        "cursor = last address"
    );

    // Resume after the cursor; the last page has no cursor.
    let mut req2 = id.to_vec();
    req2.extend_from_slice(&5u32.to_le_bytes());
    req2.extend_from_slice(&[2u8; 32]);
    let (total, _, vm) = run(
        &mut h,
        abi::TOKEN_HOLDERS,
        [TO, req2.len() as u64, OUT, 1024],
        |vm| vm.write_mem(TO, &req2).unwrap(),
    );
    assert_eq!(total, 8 + 2 * 48);
    let out = vm.read_mem(OUT, total as usize).unwrap();
    assert_eq!(&out[8..40], &[3u8; 32]);
    assert_eq!(&out[8 + 48..8 + 48 + 32], &ME);

    // Too small a buffer: full length returned, nothing copied.
    let (t, _, vm) = run(
        &mut h,
        abi::TOKEN_HOLDERS,
        [TO, req2.len() as u64, OUT, 8],
        |vm| vm.write_mem(TO, &req2).unwrap(),
    );
    assert_eq!(t, 8 + 2 * 48);
    assert_eq!(vm.read_mem(OUT, 8).unwrap(), [0u8; 8]);

    // Unknown token -> NOT_FOUND; malformed request traps.
    let unknown = [0xAB; 36];
    let (n, _, _) = run(&mut h, abi::TOKEN_HOLDERS, [TO, 36, OUT, 1024], |vm| {
        vm.write_mem(TO, &unknown).unwrap()
    });
    assert_eq!(n, abi::NOT_FOUND);
    let mut vm = vm_with(abi::TOKEN_HOLDERS, [TO, 10, OUT, 1024]);
    assert!(matches!(
        run_with_host(&mut vm, &mut h),
        Err(Trap::Abort(_))
    ));
}

#[test]
fn read_only_mode_traps_every_token_write() {
    let mut h = host();
    let id = create(&mut h, &params(100, Some(ME)));
    let before = (
        h.tokens.clone(),
        h.token_balances.clone(),
        h.token_nonces.clone(),
    );
    h.read_only = true;
    let req = params(1, None).encode();
    for (call, args) in [
        (abi::TOKEN_CREATE, [TO, req.len() as u64, OUT, 0]),
        (abi::TOKEN_TRANSFER_OF, [ID, TO, AMT, 0]),
        (abi::TOKEN_MINT, [ID, TO, AMT, 0]),
        (abi::TOKEN_BURN, [ID, AMT, 0, 0]),
    ] {
        let mut vm = vm_with(call, args);
        vm.write_mem(ID, &id).unwrap();
        vm.write_mem(TO, &req).unwrap();
        vm.write_mem(AMT, &1u128.to_le_bytes()).unwrap();
        assert_eq!(
            run_with_host(&mut vm, &mut h),
            Err(Trap::WriteInQuery),
            "call {call}"
        );
    }
    assert_eq!(
        before,
        (
            h.tokens.clone(),
            h.token_balances.clone(),
            h.token_nonces.clone()
        )
    );
    // Reads still work in read-only mode.
    let (st, _, _) = run(&mut h, abi::TOKEN_BALANCE_OF, [ID, TO, OUT, 0], |vm| {
        vm.write_mem(ID, &id).unwrap()
    });
    assert_eq!(st, abi::TOKEN_OK);
}

#[test]
fn contract_address_call() {
    let mut h = host();
    let (_, g, vm) = run(&mut h, abi::CONTRACT_ADDRESS, [OUT, 0, 0, 0], |_| {});
    assert_eq!(g, 2 + gas::HOST_CONTRACT_ADDRESS);
    assert_eq!(vm.read_mem(OUT, 32).unwrap(), ME);
    assert_eq!(h.contract_address(), ME);
}

#[test]
fn out_of_gas_charges_before_effects() {
    let mut h = host();
    let req = params(1, None).encode();
    let mut vm = Vm::new(8192, 5);
    vm.write_mem(0, &ECALL.to_le_bytes()).unwrap();
    vm.set_reg(17, abi::TOKEN_CREATE);
    vm.set_reg(10, TO);
    vm.set_reg(11, req.len() as u64);
    vm.set_reg(12, OUT);
    vm.write_mem(TO, &req).unwrap();
    assert_eq!(run_with_host(&mut vm, &mut h), Err(Trap::OutOfGas));
    assert!(h.tokens.is_empty());
}
