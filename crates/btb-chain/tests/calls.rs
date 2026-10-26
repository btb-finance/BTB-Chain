//! Cross-contract calls (`call_contract`) end to end: the `examples/caller` router calling the
//! `examples/token` example and other routers, executed by the chain.

mod support;
use btb_chain::*;
use btb_codec::{Address as CA, Decode, Encode};
use btb_runtime::MemKv;
use btb_vm::FunctionKind;
use support::*;

type Addr = [u8; 32];

const FUND: u64 = 50_000_000;
const NO_GAS_CAP: u64 = u64::MAX;

fn ca(a: Addr) -> CA {
    CA(a)
}

struct World {
    net: Net<MemKv>,
    alice: Wallet,
    bob: Wallet,
    token: Addr,
    /// Nine funded routers (deployed from `examples/caller`).
    r: Vec<Addr>,
}

fn world() -> World {
    let mut alice = Wallet::new(1);
    let bob = Wallet::new(2);
    let g = genesis(&[&alice, &bob], 1_000 * UNITS_PER_BTB);
    let mut net = Net::new(MemKv::new(), &g);
    let mut txs = vec![alice.deploy(elf("token"), manifest("token"))];
    for i in 0..9 {
        txs.push(alice.deploy(elf("caller"), manifest(&format!("router{i}"))));
    }
    let rec = net.run(txs);
    let token = deployed(&rec[0]);
    let r: Vec<Addr> = rec[1..].iter().map(deployed).collect();
    let mut txs: Vec<_> = r.iter().map(|a| alice.transfer(*a, FUND)).collect();
    txs.push(alice.transfer(token, FUND));
    txs.push(alice.call(token, "init", &(1_000u128,)));
    net.run(txs).iter().for_each(ok);
    World {
        net,
        alice,
        bob,
        token,
        r,
    }
}

impl World {
    fn run1(&mut self, tx: Transaction) -> Receipt {
        self.net.run(vec![tx]).remove(0)
    }

    /// Alice calls `f` of `c` as a mutation and the receipt must be a success.
    fn alice(&mut self, c: Addr, f: &str, args: &impl Encode) -> Receipt {
        let tx = self.alice.call(c, f, args);
        let r = self.run1(tx);
        ok(&r);
        r
    }

    fn alice_reverts(&mut self, c: Addr, f: &str, args: &impl Encode) -> Revert {
        let tx = self.alice.call(c, f, args);
        match self.run1(tx).status {
            Status::Reverted(r) => r,
            other => panic!("expected a revert, got {other:?}"),
        }
    }

    fn token_balance(&mut self, who: Addr) -> u128 {
        let a = self.alice.addr();
        let out = self
            .net
            .query(&self.token.clone(), "balance_of", &(ca(who),), &a);
        u128::decode(&out).unwrap()
    }

    fn get(&mut self, c: Addr, key: &[u8]) -> Vec<u8> {
        let a = self.alice.addr();
        let out = self.net.query(&c, "get", &(key.to_vec(),), &a);
        Vec::<u8>::decode(&out).unwrap()
    }
}

#[test]
fn router_moves_the_tokens_it_holds_and_the_token_sees_the_router() {
    let mut w = world();
    let (token, router, bob) = (w.token, w.r[0], w.bob.addr());
    w.alice(token, "transfer", &(ca(router), 300u128));
    assert_eq!(w.token_balance(router), 300);
    let r = w.alice(router, "forward_transfer", &(ca(token), ca(bob), 100u128));
    assert_eq!(bool::decode(&r.output), Ok(true));
    // caller() in the token was the router: its balance went down, not alice's
    assert_eq!(w.token_balance(router), 200);
    assert_eq!(w.token_balance(bob), 100);
    assert_eq!(w.token_balance(w.alice.addr()), 700);
    // a query calling a query
    let a = w.alice.addr();
    let out = w
        .net
        .query(&router, "balance_via", &(ca(token), ca(bob)), &a);
    assert_eq!(u128::decode(&out), Ok(100));
}

#[test]
fn a_failing_callee_reverts_only_itself_and_the_caller_can_go_on() {
    let mut w = world();
    let (token, router, bob) = (w.token, w.r[0], w.bob.addr());
    // the router holds nothing: the token aborts "insufficient balance"
    let r = w.alice(router, "try_transfer", &(ca(token), ca(bob), 5u128));
    assert_eq!(r.output, vec![3], "callee aborted");
    // the router's own write before the call survived
    let a = w.alice.addr();
    let n = w.net.query(&router, "attempts", &(), &a);
    assert_eq!(u64::decode(&n), Ok(1));
    assert_eq!(w.token_balance(bob), 0);
    // a callee that wrote and then aborted leaves no trace; the caller's own write stays
    let r = w.alice(
        router,
        "try_put_then_abort",
        &(ca(w.r[1]), b"k".to_vec(), b"v".to_vec()),
    );
    assert_eq!(r.output, vec![3]);
    assert!(w.get(w.r[1], b"k").is_empty(), "callee write reverted");
    assert_eq!(w.get(router, b"mine"), b"kept");
    // when the caller then aborts too, everything goes
    let rv = w.alice_reverts(
        router,
        "put_then_abort_via",
        &(ca(w.r[2]), b"k".to_vec(), b"v".to_vec()),
    );
    assert!(
        matches!(rv, Revert::Abort(ref m) if m == "outer abort"),
        "{rv:?}"
    );
    assert!(w.get(router, b"mine") == b"kept", "still the old value");
}

#[test]
fn missing_contract_and_function_are_statuses() {
    let mut w = world();
    let (router, bob) = (w.r[0], w.bob.addr());
    let r = w.alice(router, "try_transfer", &(ca([0x77; 32]), ca(bob), 1u128));
    assert_eq!(r.output, vec![1], "no contract");
    // a contract without a `transfer`
    let r = w.alice(router, "try_transfer", &(ca(w.r[1]), ca(bob), 1u128));
    assert_eq!(r.output, vec![2], "no function");
}

fn gas_of(w: &mut World, f: &str, args: &impl Encode, c: Addr) -> u64 {
    let r = w.alice(c, f, args);
    r.gas_used
}

#[test]
fn gas_accounting_is_exact() {
    let mut w = world();
    let (a, b) = (w.r[0], w.r[1]);
    let direct = |w: &mut World, n: u64| gas_of(w, "burn", &(n,), b);
    let via = |w: &mut World, n: u64, gas: u64| gas_of(w, "call_burn", &(ca(b), n, gas), a);
    let (d0, d1, d2) = (
        direct(&mut w, 0),
        direct(&mut w, 500),
        direct(&mut w, 5_000),
    );
    let (v0, v1, v2) = (
        via(&mut w, 0, NO_GAS_CAP),
        via(&mut w, 500, NO_GAS_CAP),
        via(&mut w, 5_000, NO_GAS_CAP),
    );
    // what the callee burns is charged to the caller one for one...
    assert_eq!(v1 - v0, d1 - d0);
    assert_eq!(v2 - v1, d2 - d1);
    // ...on top of a fixed cost: the host call, the request and the result copy
    let overhead = v0 - d0;
    assert!(overhead >= btb_vm::gas::HOST_CALL_CONTRACT, "{overhead}");
    assert_eq!(v1 - d1, overhead);
    assert_eq!(v2 - d2, overhead);
    // the gas that is forwarded but not used is not taken
    assert_eq!(via(&mut w, 5_000, 1_000_000), v2);
    assert_eq!(via(&mut w, 5_000, 10_000_000), v2);
}

#[test]
fn a_callee_that_runs_out_of_gas_fails_only_the_call() {
    let mut w = world();
    let (a, b) = (w.r[0], w.r[1]);
    let before = w.net.state.balance(&b).unwrap();
    // 2,000,000 loop iterations cannot run on 30,000 gas
    let r = w.alice(a, "call_burn", &(ca(b), 2_000_000u64, 30_000u64));
    assert_eq!(r.output, vec![5], "out of gas");
    // the callee used exactly what it was given and the caller paid for it
    assert!(
        r.gas_used > 30_000 && r.gas_used < 100_000,
        "{}",
        r.gas_used
    );
    assert_eq!(w.net.state.balance(&b).unwrap(), before);
    // the transaction's own limit still binds the whole stack
    let tx = w.alice.call_with(
        a,
        FunctionKind::Mutation,
        "call_burn",
        &(ca(b), 2_000_000u64, NO_GAS_CAP),
        200_000,
    );
    let r = w.run1(tx);
    match r.status {
        Status::Reverted(Revert::OutOfGas) => {}
        // the callee ran dry inside the caller's gas and the caller then could not go on
        Status::Success => assert_eq!(r.output, vec![5]),
        other => panic!("{other:?}"),
    }
    assert!(r.gas_used <= 200_000);
}

fn chain_path(w: &World, frames: usize) -> Vec<CA> {
    w.r[1..frames].iter().map(|a| ca(*a)).collect()
}

fn last_err(w: &mut World, c: Addr) -> u8 {
    let a = w.alice.addr();
    u8::decode(&w.net.query(&c, "last_err", &(), &a)).unwrap()
}

#[test]
fn depth_limit_is_eight_frames() {
    let mut w = world();
    // 8 frames (router 0 calling 1..7): fine
    let r = w.alice(w.r[0], "chain", &(chain_path(&w, 8),));
    assert_eq!(u64::decode(&r.output), Ok(8));
    let rs = w.r.clone();
    assert!(rs.iter().all(|c| last_err(&mut w, *c) == 0));
    // 9 frames: the 8th router's call is refused (depth), it carries on and reports it
    let r = w.alice(w.r[0], "chain", &(chain_path(&w, 9),));
    assert_eq!(u64::decode(&r.output), Ok(8));
    let r7 = w.r[7];
    assert_eq!(last_err(&mut w, r7), 6, "depth");
}

#[test]
fn reentering_a_contract_on_the_stack_is_refused() {
    let mut w = world();
    let [a, b, c] = [w.r[0], w.r[1], w.r[2]];
    // a -> b -> a
    let r = w.alice(a, "chain", &(vec![ca(b), ca(a)],));
    assert_eq!(u64::decode(&r.output), Ok(2));
    assert_eq!(last_err(&mut w, b), 7, "reentrant");
    // a -> a
    w.alice(a, "chain", &(vec![ca(a)],));
    assert_eq!(last_err(&mut w, a), 7);
    // a -> b -> c -> b
    w.alice(a, "chain", &(vec![ca(b), ca(c), ca(b)],));
    assert_eq!(last_err(&mut w, c), 7);
    // calling b twice in a row (not nested) is fine
    let r = w.alice(a, "chain", &(vec![ca(b)],));
    assert_eq!(u64::decode(&r.output), Ok(2));
}

#[test]
fn a_query_cannot_call_a_mutation() {
    let mut w = world();
    let (a, b) = (w.r[0], w.r[1]);
    let me = w.alice.addr();
    let e = w
        .net
        .ex
        .query(&mut w.net.state, &a, "bad_query", &(ca(b),).encode(), &me)
        .unwrap_err();
    assert!(
        matches!(&e, Revert::Trap(t) if t.contains("WriteInQuery")),
        "{e:?}"
    );
    assert!(w.get(b, b"").is_empty());
}

#[test]
fn randomness_marked_functions_cannot_be_called_by_contracts() {
    let mut w = world();
    let (a, b) = (w.r[0], w.r[1]);
    let rv = w.alice_reverts(a, "call_roll", &(ca(b),));
    assert!(
        matches!(&rv, Revert::Trap(t) if t.contains("RandomnessForbidden")),
        "{rv:?}"
    );
    // the randomness call of a callee frame traps, whoever the root caller is
    let tx = w.alice.call(a, "marked_calls_peek", &(ca(b),));
    let r = w.run1(tx);
    ok(&r);
    assert_eq!(r.output, vec![4], "callee trapped");
    assert_eq!(r.gas_used, GAS, "a marked function pays its whole limit");
    // and an unmarked caller reaching it the same way
    let rv = w.alice_reverts(b, "peek_random", &());
    assert!(
        matches!(&rv, Revert::Trap(t) if t.contains("RandomnessForbidden")),
        "{rv:?}"
    );
}

#[test]
fn http_action_callbacks_cannot_be_called_by_contracts() {
    let mut w = world();
    let (a, b) = (w.r[0], w.r[1]);
    let rv = w.alice_reverts(a, "call_callback", &(ca(b),));
    assert!(
        matches!(&rv, Revert::Trap(t) if t.contains("CallForbidden")),
        "{rv:?}"
    );
}

#[test]
fn caller_and_contract_address_in_the_callee() {
    let mut w = world();
    let [a, b, c] = [w.r[0], w.r[1], w.r[2]];
    let alice = w.alice.addr();
    // direct: caller is the account
    let r = w.alice(b, "ident_m", &());
    assert_eq!(<(CA, CA)>::decode(&r.output), Ok((ca(alice), ca(b))));
    // through a mutation call and through a query call: caller is the calling contract
    let r = w.alice(a, "ident_m_via", &(ca(b),));
    assert_eq!(<(CA, CA)>::decode(&r.output), Ok((ca(a), ca(b))));
    let r = w.alice(a, "ident_via", &(ca(c),));
    assert_eq!(<(CA, CA)>::decode(&r.output), Ok((ca(a), ca(c))));
}

#[test]
fn storage_deposit_is_charged_to_the_contract_that_grew() {
    let mut w = world();
    let (a, b) = (w.r[0], w.r[1]);
    let (dep_a, dep_b) = (
        w.net.state.deposit(&a).unwrap(),
        w.net.state.deposit(&b).unwrap(),
    );
    let (bal_a, bal_b) = (
        w.net.state.balance(&a).unwrap(),
        w.net.state.balance(&b).unwrap(),
    );
    let r = w.alice(a, "grow_via", &(ca(b), 1_000u32));
    assert_eq!(r.output, vec![0]);
    let (nd_a, nd_b) = (
        w.net.state.deposit(&a).unwrap(),
        w.net.state.deposit(&b).unwrap(),
    );
    assert_eq!(nd_a, dep_a, "the caller grew nothing");
    assert!(nd_b >= dep_b + 1_000 * 10, "{dep_b} -> {nd_b}");
    assert_eq!(w.net.state.balance(&a).unwrap(), bal_a);
    assert_eq!(w.net.state.balance(&b).unwrap(), bal_b - (nd_b - dep_b));
    // shrinking gives the deposit back to the callee, also through a call
    let r = w.alice(a, "grow_via", &(ca(b), 10u32));
    assert_eq!(r.output, vec![0]);
    assert!(w.net.state.deposit(&b).unwrap() < nd_b);

    // a callee that cannot pay fails its call (and is reverted); the caller carries on
    let broke = w.r[8];
    let tx = w.alice.call(broke, "pay", &(ca(w.alice.addr()), FUND));
    ok(&w.run1(tx)); // drain it (the router pays alice back)
    assert_eq!(w.net.state.balance(&broke).unwrap(), 0);
    let r = w.alice(a, "grow_via", &(ca(broke), 1_000u32));
    assert_eq!(r.output, vec![9], "deposit");
    assert!(w.get(broke, b"blob").is_empty());
    assert_eq!(w.net.state.deposit(&broke).unwrap(), 0);
}

#[test]
fn transfers_by_the_callee_are_the_callees() {
    let mut w = world();
    let (a, b, bob) = (w.r[0], w.r[1], w.bob.addr());
    let (bal_a, bal_b, bal_bob) = (
        w.net.state.balance(&a).unwrap(),
        w.net.state.balance(&b).unwrap(),
        w.net.state.balance(&bob).unwrap(),
    );
    let r = w.alice(a, "pay_via", &(ca(b), ca(bob), 1_234u64));
    assert_eq!(r.output, vec![0]);
    assert_eq!(w.net.state.balance(&a).unwrap(), bal_a);
    assert_eq!(w.net.state.balance(&b).unwrap(), bal_b - 1_234);
    assert_eq!(w.net.state.balance(&bob).unwrap(), bal_bob + 1_234);
    // more than the callee has: its own refusal, not the caller's money
    let r = w.alice(a, "pay_via", &(ca(b), ca(bob), u64::MAX));
    assert_eq!(r.output, vec![100]);
    assert_eq!(w.net.state.balance(&a).unwrap(), bal_a);
}

#[test]
fn a_result_larger_than_the_buffer_fails_the_call() {
    let mut w = world();
    let (a, b) = (w.r[0], w.r[1]);
    w.alice(b, "put", &(b"big".to_vec(), vec![1u8; 3_000]));
    w.alice(b, "put", &(b"small".to_vec(), vec![1u8; 100]));
    let r = w.alice(a, "get_len_via", &(ca(b), b"small".to_vec()));
    assert_eq!(u32::decode(&r.output), Ok(100));
    let r = w.alice(a, "get_len_via", &(ca(b), b"big".to_vec()));
    assert_eq!(u32::decode(&r.output), Ok(10_008));
}

#[test]
fn supply_and_deposits_stay_consistent() {
    let mut w = world();
    let (token, a, b, bob) = (w.token, w.r[0], w.r[1], w.bob.addr());
    w.alice(token, "transfer", &(ca(a), 10u128));
    w.alice(a, "forward_transfer", &(ca(token), ca(bob), 4u128));
    w.alice(a, "grow_via", &(ca(b), 500u32));
    w.alice(a, "chain", &(vec![ca(b), ca(a)],));
    // the executor's own supply invariant runs after every block (check_supply); also by hand:
    assert_eq!(
        w.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
}
