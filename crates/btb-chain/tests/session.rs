//! Session keys: scope, expiry, revocation, fee budget, nonce sharing, and parallel equivalence.

mod support;
use btb_chain::session::{Grant, Scope, SessionInvalid, SessionOp};
use btb_chain::*;
use btb_codec::{Address as CA, Decode, Encode};
use btb_runtime::MemKv;
use btb_vm::FunctionKind;
use support::*;

type Addr = [u8; 32];
const FUND: u64 = 50_000_000;
const GAS_LIMIT: u64 = 400_000;

/// Sign as alice with her current on-chain nonce (session calls also use it).
macro_rules! a {
    ($w:ident, $k:expr) => {{
        $w.alice.nonce = $w.net.state.nonce(&$w.alice.addr()).unwrap();
        $w.alice.sign($k)
    }};
}
macro_rules! ac {
    ($w:ident, $c:expr, $f:expr, $args:expr) => {{
        $w.alice.nonce = $w.net.state.nonce(&$w.alice.addr()).unwrap();
        $w.alice.call($c, $f, $args)
    }};
}

fn sess_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn pubkey(k: &SigningKey) -> [u8; 32] {
    k.verifying_key().to_bytes()
}

/// A call signed by `key` acting as `account`.
fn session_call(
    key: &SigningKey,
    account: Addr,
    nonce: u64,
    contract: Addr,
    f: &str,
    args: &impl Encode,
) -> Transaction {
    Transaction::sign(
        TxBody {
            chain_id: CHAIN_ID,
            nonce,
            relayer: None,
            session: Some(account),
            kind: TxKind::Call {
                contract,
                kind: FunctionKind::Mutation,
                function: f.into(),
                args: args.encode(),
                gas_limit: GAS_LIMIT,
                max_fee: u64::MAX,
            },
        },
        key,
    )
}

fn grant(key: &SigningKey, expires: u64, allowed: Vec<Scope>, limit: u64) -> TxKind {
    TxKind::Session(SessionOp::Grant(Grant {
        session_key: pubkey(key),
        expires_height: expires,
        allowed,
        spend_limit: limit,
    }))
}

fn scope(c: Addr, fns: Option<&[&str]>) -> Scope {
    Scope {
        contract: c,
        functions: fns.map(|f| f.iter().map(|s| s.to_string()).collect()),
    }
}

struct World {
    net: Net<MemKv>,
    alice: Wallet,
    bob: Wallet,
    c1: Addr,
    c2: Addr,
    skey: SigningKey,
}

fn world_with(ex: Executor) -> World {
    let mut alice = Wallet::new(1);
    let bob = Wallet::new(2);
    let g = genesis(&[&alice, &bob], 1_000 * UNITS_PER_BTB);
    let mut net = Net::new(MemKv::new(), &g);
    net.ex = ex;
    net.ex.parallel_min_txs = 2;
    let rec = net.run(vec![
        alice.deploy(elf("caller"), manifest("c1")),
        alice.deploy(elf("caller"), manifest("c2")),
    ]);
    let (c1, c2) = (deployed(&rec[0]), deployed(&rec[1]));
    let txs = vec![alice.transfer(c1, FUND), alice.transfer(c2, FUND)];
    net.run(txs).iter().for_each(ok);
    World {
        net,
        alice,
        bob,
        c1,
        c2,
        skey: sess_key(77),
    }
}

fn world() -> World {
    world_with(Executor::sequential())
}

fn invalid(r: &Receipt) -> &SessionInvalid {
    match &r.status {
        Status::Invalid(Invalid::Session(i)) => i,
        s => panic!("expected an invalid session transaction, got {s:?}"),
    }
}

fn height(w: &World) -> u64 {
    w.net.state.head().unwrap().height
}

#[test]
fn session_acts_as_the_account_inside_its_scope() {
    let mut w = world();
    let me = w.alice.addr();
    let r = w.net.run(vec![a!(
        w,
        grant(
            &w.skey,
            height(&w) + 100,
            vec![scope(w.c1, Some(&["auth_m", "put"]))],
            10_000_000,
        )
    )]);
    ok(&r[0]);
    assert_eq!(r[0].gas_used, btb_chain::session::SESSION_GAS);
    let n = w.net.state.nonce(&me).unwrap();
    let bal = w.net.bal(&me);
    let tx = session_call(&w.skey, me, n, w.c1, "auth_m", &());
    let r = w.net.run(vec![tx]);
    ok(&r[0]);
    let (who, via) = <(CA, bool)>::decode(&r[0].output).unwrap();
    assert_eq!(who.0, me, "caller() is the account");
    assert!(via, "and the contract can tell it was a session");
    assert_eq!(
        w.net.state.nonce(&me).unwrap(),
        n + 1,
        "the account's nonce"
    );
    assert_eq!(w.net.bal(&me), bal - r[0].fee, "the account pays");
    assert!(r[0].fee > 0);
    // The account's own key is not a session.
    let r = w.net.run(vec![ac!(w, w.c1, "auth_m", &())]);
    let (who, via) = <(CA, bool)>::decode(&r[0].output).unwrap();
    assert_eq!((who.0, via), (me, false));
    assert_eq!(
        w.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
}

#[test]
fn nested_calls_never_see_a_session() {
    let mut w = world();
    let me = w.alice.addr();
    let h = height(&w) + 100;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, h, vec![scope(w.c1, None)], 10_000_000)
        )])
        .iter()
        .for_each(ok);
    let n = w.net.state.nonce(&me).unwrap();
    let r = w.net.run(vec![session_call(
        &w.skey,
        me,
        n,
        w.c1,
        "auth_m_via",
        &(CA(w.c2),),
    )]);
    ok(&r[0]);
    let (who, via) = <(CA, bool)>::decode(&r[0].output).unwrap();
    assert_eq!(who.0, w.c1, "the callee sees the calling contract");
    assert!(!via);
}

#[test]
fn contracts_can_demand_the_accounts_own_signature() {
    let mut w = world();
    let me = w.alice.addr();
    let h = height(&w) + 100;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, h, vec![scope(w.c1, None)], 10_000_000)
        )])
        .iter()
        .for_each(ok);
    let n = w.net.state.nonce(&me).unwrap();
    let r = w
        .net
        .run(vec![session_call(&w.skey, me, n, w.c1, "direct_only", &())]);
    assert!(
        matches!(r[0].status, Status::Reverted(_)),
        "{:?}",
        r[0].status
    );
    assert!(r[0].fee > 0, "a revert still costs");
    let r = w.net.run(vec![ac!(w, w.c1, "direct_only", &())]);
    ok(&r[0]);
}

#[test]
fn outside_the_contract_or_function_is_invalid_and_free() {
    let mut w = world();
    let me = w.alice.addr();
    let h = height(&w) + 100;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, h, vec![scope(w.c1, Some(&["auth_m"]))], 10_000_000,)
        )])
        .iter()
        .for_each(ok);
    let (n, bal) = (w.net.state.nonce(&me).unwrap(), w.net.bal(&me));
    let other_contract = session_call(&w.skey, me, n, w.c2, "auth_m", &());
    let other_fn = session_call(&w.skey, me, n, w.c1, "put", &(vec![1u8], vec![2u8]));
    let r = w.net.run(vec![other_contract, other_fn]);
    assert_eq!(invalid(&r[0]), &SessionInvalid::ContractNotAllowed);
    assert_eq!(invalid(&r[1]), &SessionInvalid::FunctionNotAllowed);
    assert_eq!(
        (w.net.state.nonce(&me).unwrap(), w.net.bal(&me)),
        (n, bal),
        "no nonce, no fee"
    );
    // Admission refuses them too (the node does this before the mempool).
    let t = session_call(&w.skey, me, n, w.c2, "auth_m", &());
    assert!(matches!(
        w.net.state.check_session(&t),
        Err(Invalid::Session(SessionInvalid::ContractNotAllowed))
    ));
    let t = session_call(&w.skey, me, n, w.c1, "auth_m", &());
    assert_eq!(w.net.state.check_session(&t), Ok(()));
}

#[test]
fn a_session_key_of_another_account_or_unknown_key_is_invalid() {
    let mut w = world();
    let (me, bob) = (w.alice.addr(), w.bob.addr());
    let h = height(&w) + 100;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, h, vec![scope(w.c1, None)], 10_000_000)
        )])
        .iter()
        .for_each(ok);
    // The key alice granted claims to be bob; another key claims to be alice.
    let as_bob = session_call(&w.skey, bob, 0, w.c1, "auth_m", &());
    let n = w.net.state.nonce(&me).unwrap();
    let stranger = session_call(&sess_key(5), me, n, w.c1, "auth_m", &());
    let r = w.net.run(vec![as_bob, stranger]);
    assert_eq!(invalid(&r[0]), &SessionInvalid::NoSuchSession);
    assert_eq!(invalid(&r[1]), &SessionInvalid::NoSuchSession);
}

#[test]
fn expired_sessions_are_invalid() {
    let mut w = world();
    let me = w.alice.addr();
    let expires = height(&w) + 4;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, expires, vec![scope(w.c1, None)], 10_000_000)
        )])
        .iter()
        .for_each(ok);
    let mut n = w.net.state.nonce(&me).unwrap();
    // Valid while the block height is below `expires`.
    while height(&w) + 1 < expires {
        let r = w
            .net
            .run(vec![session_call(&w.skey, me, n, w.c1, "auth_m", &())]);
        ok(&r[0]);
        n += 1;
    }
    assert_eq!(height(&w) + 1, expires);
    let r = w
        .net
        .run(vec![session_call(&w.skey, me, n, w.c1, "auth_m", &())]);
    assert_eq!(
        invalid(&r[0]),
        &SessionInvalid::Expired {
            expires_height: expires
        }
    );
    // A grant that is already expired is refused.
    let r = w.net.run(vec![a!(
        w,
        grant(&sess_key(9), 1, vec![scope(w.c1, None)], 1)
    )]);
    assert!(matches!(
        r[0].status,
        Status::Reverted(Revert::Session(SessionError::BadGrant))
    ));
}

#[test]
fn revoked_sessions_are_invalid() {
    let mut w = world();
    let me = w.alice.addr();
    let h = height(&w) + 100;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, h, vec![scope(w.c1, None)], 10_000_000)
        )])
        .iter()
        .for_each(ok);
    let n = w.net.state.nonce(&me).unwrap();
    ok(&w
        .net
        .run(vec![session_call(&w.skey, me, n, w.c1, "auth_m", &())])[0]);
    ok(&w.net.run(vec![a!(
        w,
        TxKind::Session(SessionOp::Revoke(pubkey(&w.skey)))
    )])[0]);
    let n = w.net.state.nonce(&me).unwrap();
    let r = w
        .net
        .run(vec![session_call(&w.skey, me, n, w.c1, "auth_m", &())]);
    assert_eq!(invalid(&r[0]), &SessionInvalid::NoSuchSession);
    // Revoking twice is a (charged) revert, not a panic.
    let r = w.net.run(vec![a!(
        w,
        TxKind::Session(SessionOp::Revoke(pubkey(&w.skey)))
    )]);
    assert!(matches!(
        r[0].status,
        Status::Reverted(Revert::Session(SessionError::NoSuchSession))
    ));
}

#[test]
fn the_fee_budget_holds_across_many_transactions() {
    let mut w = world();
    let me = w.alice.addr();
    // Learn the fee of one call, then allow about six of them.
    let h = height(&w) + 1_000;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, h, vec![scope(w.c1, None)], u64::MAX)
        )])
        .iter()
        .for_each(ok);
    let n = w.net.state.nonce(&me).unwrap();
    let one = w
        .net
        .run(vec![session_call(&w.skey, me, n, w.c1, "auth_m", &())])[0]
        .fee;
    let reservation = GAS_LIMIT; // gas_price 1
    let limit = reservation + 5 * one + one / 2;
    ok(&w.net.run(vec![a!(
        w,
        grant(&w.skey, h, vec![scope(w.c1, None)], limit)
    )])[0]);
    // Re-granting resets the budget; send calls in blocks of several until refused.
    let (mut accepted, mut refused) = (0u64, 0u64);
    let mut n = w.net.state.nonce(&me).unwrap();
    for _ in 0..4 {
        let txs: Vec<_> = (0..4)
            .map(|i| session_call(&w.skey, me, n + i, w.c1, "auth_m", &()))
            .collect();
        for r in w.net.run(txs) {
            match &r.status {
                Status::Success => {
                    accepted += 1;
                    n += 1;
                }
                Status::Invalid(Invalid::Session(SessionInvalid::BudgetExceeded { .. })) => {
                    refused += 1
                }
                s => panic!("{s:?}"),
            }
        }
    }
    let spent = w.net.state.sessions(&me).unwrap()[0].spent;
    assert_eq!(spent, accepted * one);
    assert!(spent <= limit);
    assert!(
        refused > 0 && accepted >= 4,
        "accepted {accepted}, refused {refused}"
    );
    assert!(
        limit - spent < reservation,
        "refusal only when the reservation no longer fits"
    );
    // The owner is unaffected: the account's own key still works.
    ok(&w.net.run(vec![ac!(w, w.c1, "auth_m", &())])[0]);
}

#[test]
fn a_session_cannot_do_anything_but_call() {
    let mut w = world();
    let me = w.alice.addr();
    let h = height(&w) + 100;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, h, vec![scope(w.c1, None)], 10_000_000)
        )])
        .iter()
        .for_each(ok);
    let n = w.net.state.nonce(&me).unwrap();
    let body = |kind| TxBody {
        chain_id: CHAIN_ID,
        nonce: n,
        relayer: None,
        session: Some(me),
        kind,
    };
    let kinds = vec![
        TxKind::Transfer {
            to: w.bob.addr(),
            amount: 1,
        },
        TxKind::Stake {
            validator: [3; 32],
            amount: 1,
        },
        TxKind::Unstake { amount: 1 },
        grant(&sess_key(8), h, vec![scope(w.c1, None)], 1),
        TxKind::Session(SessionOp::Revoke(pubkey(&w.skey))),
        TxKind::Deploy {
            code: elf("caller").to_vec(),
            manifest: manifest("x"),
        },
        TxKind::Host(HostOp::Unregister),
    ];
    let bal = w.net.bal(&me);
    for kind in kinds {
        let tx = Transaction::sign(body(kind), &w.skey);
        // Stateless: the mempool rejects it before any state is consulted.
        let mut pool = Mempool::new(ChainConfig::new(CHAIN_ID), MempoolConfig::default());
        assert!(matches!(
            pool.insert(tx.clone(), n, bal),
            Err(MempoolError::Invalid(Invalid::Session(
                SessionInvalid::KindNotAllowed
            )))
        ));
        let r = w.net.run(vec![tx]);
        assert_eq!(invalid(&r[0]), &SessionInvalid::KindNotAllowed);
    }
    assert_eq!(w.net.bal(&me), bal);
    assert_eq!(
        w.net.state.sessions(&me).unwrap().len(),
        1,
        "no new session appeared"
    );
    assert_eq!(
        w.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
}

#[test]
fn nonces_are_the_accounts_so_nothing_replays() {
    let mut w = world();
    let me = w.alice.addr();
    let h = height(&w) + 100;
    w.net
        .run(vec![a!(
            w,
            grant(&w.skey, h, vec![scope(w.c1, None)], 10_000_000)
        )])
        .iter()
        .for_each(ok);
    let n = w.net.state.nonce(&me).unwrap();
    let tx = session_call(&w.skey, me, n, w.c1, "auth_m", &());
    ok(&w.net.run(vec![tx.clone()])[0]);
    // Replay: the same bytes again.
    let r = w.net.run(vec![tx]);
    assert!(matches!(
        r[0].status,
        Status::Invalid(Invalid::BadNonce { .. })
    ));
    // The owner's key and the session share one counter: a collision is just a bad nonce.
    let mine = ac!(w, w.c1, "auth_m", &());
    let theirs = session_call(&w.skey, me, n + 1, w.c1, "auth_m", &());
    let r = w.net.run(vec![mine, theirs]);
    ok(&r[0]);
    assert!(matches!(
        r[1].status,
        Status::Invalid(Invalid::BadNonce { .. })
    ));
    // Binding: changing the account in the signed body breaks the signature.
    let mut forged = session_call(&w.skey, me, n + 2, w.c1, "auth_m", &());
    forged.body.session = Some(w.bob.addr());
    let r = w.net.run(vec![forged]);
    assert_eq!(r[0].status, Status::Invalid(Invalid::BadSignature));
}

#[test]
fn grants_are_validated_and_bounded() {
    let mut w = world();
    let h = height(&w) + 100;
    let bad = vec![
        grant(&sess_key(10), h, vec![], 1),
        grant(&sess_key(10), h, vec![scope(w.c1, Some(&[]))], 1),
    ];
    for k in bad {
        let tx = a!(w, k);
        let mut pool = Mempool::new(ChainConfig::new(CHAIN_ID), MempoolConfig::default());
        assert!(matches!(
            pool.insert(tx, 0, u64::MAX),
            Err(MempoolError::Invalid(_))
        ));
    }
    for i in 0..btb_chain::session::MAX_SESSIONS as u8 {
        ok(&w.net.run(vec![a!(
            w,
            grant(&sess_key(100 + i), h, vec![scope(w.c1, None)], 1)
        )])[0]);
    }
    let r = w.net.run(vec![a!(
        w,
        grant(&sess_key(200), h, vec![scope(w.c1, None)], 1)
    )]);
    assert!(matches!(
        r[0].status,
        Status::Reverted(Revert::Session(SessionError::TooManySessions))
    ));
    // Replacing an existing key is not a new session.
    ok(&w.net.run(vec![a!(
        w,
        grant(&sess_key(100), h, vec![scope(w.c2, None)], 5)
    )])[0]);
}

#[test]
fn old_encoding_is_unchanged_and_session_round_trips() {
    let k = sess_key(3);
    let mk = |session| {
        Transaction::sign(
            TxBody {
                chain_id: 7,
                nonce: 1,
                relayer: Some([5; 32]),
                session,
                kind: TxKind::Transfer {
                    to: [9; 32],
                    amount: 1,
                },
            },
            &k,
        )
    };
    let plain = mk(None);
    let with = mk(Some([4; 32]));
    assert_eq!(plain.encode()[16] & 0xC0, 0x80, "only the relayer bit");
    assert_eq!(with.encode()[16] & 0xC0, 0xC0, "relayer and session bits");
    assert_eq!(with.encode().len(), plain.encode().len() + 32);
    assert_eq!(Transaction::decode(&with.encode()).unwrap(), with);
    assert_eq!(with.sender(), [4; 32]);
    assert_eq!(plain.sender(), account_address(&pubkey(&k)));
    assert_eq!(with.signer(), account_address(&pubkey(&k)));
}

/// The same blocks on the sequential oracle and the parallel engine give identical receipts and roots.
#[test]
fn parallel_execution_equals_sequential() {
    let mut roots = Vec::new();
    let mut all_receipts = Vec::new();
    for ex in [
        Executor::sequential(),
        Executor::parallel(1),
        Executor::parallel(4),
    ] {
        let mut w = world_with(ex);
        let (me, bob) = (w.alice.addr(), w.bob.addr());
        let k2 = sess_key(78);
        let h = height(&w) + 50;
        let mut bobw = Wallet::new(2);
        let mut receipts = Vec::new();
        receipts.extend(w.net.run(vec![
            a!(w, grant(&w.skey, h, vec![scope(w.c1, None)], 3_000_000)),
            a!(
                w,
                grant(&k2, h, vec![scope(w.c2, Some(&["put", "auth_m"]))], 100_000)
            ),
            bobw.sign(grant(&w.skey, h, vec![scope(w.c2, None)], 3_000_000)),
        ]));
        let mut na = w.net.state.nonce(&me).unwrap();
        let mut nb = w.net.state.nonce(&bob).unwrap();
        for round in 0..6u64 {
            let mut txs = Vec::new();
            for _ in 0..3 {
                txs.push(session_call(&w.skey, me, na, w.c1, "auth_m", &()));
                na += 1;
                txs.push(session_call(
                    &w.skey,
                    bob,
                    nb,
                    w.c2,
                    "put",
                    &(vec![round as u8], vec![1u8]),
                ));
                nb += 1;
                txs.push(session_call(&k2, me, na, w.c2, "auth_m", &()));
                na += 1;
            }
            // violations and a plain transfer from the same accounts in the middle
            txs.push(session_call(&k2, me, na, w.c1, "auth_m", &()));
            txs.push(session_call(&w.skey, me, na, w.c2, "auth_m", &()));
            txs.push(bobw.transfer(me, 10));
            nb += 1;
            if round == 3 {
                w.alice.nonce = na;
                txs.push(
                    w.alice
                        .sign(TxKind::Session(SessionOp::Revoke(pubkey(&w.skey)))),
                );
                na += 1;
            }
            receipts.extend(w.net.run(txs));
        }
        roots.push(w.net.state.root().unwrap());
        all_receipts.push(receipts);
        assert_eq!(
            w.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY)
        );
    }
    assert!(all_receipts[0]
        .iter()
        .any(|r| matches!(r.status, Status::Invalid(Invalid::Session(_)))));
    assert!(all_receipts[0].iter().any(|r| r.status == Status::Success));
    for i in 1..roots.len() {
        assert_eq!(roots[0], roots[i], "state root, executor {i}");
        assert_eq!(all_receipts[0], all_receipts[i], "receipts, executor {i}");
    }
}

/// `sdk-ts/test/fixtures/session.json`: the TypeScript signer must produce these exact bytes.
/// Regenerate with `BTB_WRITE_FIXTURES=1 cargo test -p btb-chain --test session`.
#[test]
fn ts_fixture_matches() {
    let h = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let (account_key, skey) = (sess_key(7), sess_key(8));
    let account = account_address(&pubkey(&account_key));
    let sign = |k: &SigningKey, nonce, relayer, session, kind| {
        Transaction::sign(
            TxBody {
                chain_id: 7,
                nonce,
                relayer,
                session,
                kind,
            },
            k,
        )
    };
    let allowed = vec![
        scope([0xC0; 32], Some(&["increment", "put"])),
        scope([0xC1; 32], None),
    ];
    let call = TxKind::Call {
        contract: [0xC0; 32],
        kind: FunctionKind::Mutation,
        function: "increment".into(),
        args: vec![1, 2, 3],
        gas_limit: 500_000,
        max_fee: u64::MAX,
    };
    let txs = [
        (
            "grant",
            sign(
                &account_key,
                4,
                None,
                None,
                grant(&skey, 1_000, allowed, 5_000_000),
            ),
        ),
        (
            "revoke",
            sign(
                &account_key,
                5,
                None,
                None,
                TxKind::Session(SessionOp::Revoke(pubkey(&skey))),
            ),
        ),
        (
            "session_call",
            sign(&skey, 6, None, Some(account), call.clone()),
        ),
        (
            "session_call_relayed",
            sign(&skey, 6, Some([5; 32]), Some(account), call),
        ),
    ];
    let rows: Vec<String> = txs
        .iter()
        .map(|(n, t)| {
            assert!(t.verify_signature());
            format!(
                "    {{\"name\": \"{n}\", \"wire\": \"{}\", \"id\": \"{}\"}}",
                h(&t.encode()),
                h(&t.id())
            )
        })
        .collect();
    let out = format!(
        "{{\n  \"account_seed\": \"{}\",\n  \"session_seed\": \"{}\",\n  \"account\": \"{}\",\n  \"session_key\": \"{}\",\n  \"txs\": [\n{}\n  ]\n}}\n",
        h(&[7; 32]),
        h(&[8; 32]),
        h(&account),
        h(&pubkey(&skey)),
        rows.join(",\n")
    );
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../sdk-ts/test/fixtures/session.json"
    );
    if std::env::var_os("BTB_WRITE_FIXTURES").is_some() {
        std::fs::write(path, &out).unwrap();
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), out, "stale fixture");
}
