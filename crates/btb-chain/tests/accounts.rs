//! Accounts with hidden rotating keys, and names (proposal 0002 phases A and B).

mod support;
use btb_chain::account::{
    account_id, key_commitment, AccountAuth, AccountError, AccountInvalid, AccountOp, Slot,
};
use btb_chain::names::{deposit_for, NameError, NameOp};
use btb_chain::*;
use btb_runtime::MemKv;
use support::*;

type Addr = [u8; 32];
const FUND: u64 = 1_000_000 * UNITS_PER_BTB;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn commit(k: &SigningKey) -> [u8; 32] {
    key_commitment(&k.verifying_key().to_bytes())
}

fn slot(k: &SigningKey, admin: bool, rotate: bool) -> Slot {
    Slot {
        commitment: commit(k),
        admin,
        rotate,
    }
}

/// A transaction acting as `account` through `slot`, signed by `k`.
fn as_account(
    net: &Net<MemKv>,
    k: &SigningKey,
    account: Addr,
    slot: u8,
    next: Option<[u8; 32]>,
    kind: TxKind,
) -> Transaction {
    Transaction::sign(
        TxBody {
            chain_id: CHAIN_ID,
            nonce: net.state.nonce(&account).unwrap(),
            relayer: None,
            session: None,
            account: Some(AccountAuth {
                account,
                slot,
                next,
            }),
            kind,
        },
        k,
    )
}

struct W {
    net: Net<MemKv>,
    alice: Wallet,
    bob: Wallet,
}

fn world() -> W {
    let alice = Wallet::new(1);
    let bob = Wallet::new(2);
    let net = Net::new(MemKv::new(), &genesis(&[&alice, &bob], FUND));
    W { net, alice, bob }
}

/// Alice creates an account with `slots`, funded with `fund`; returns its id.
fn create(w: &mut W, slots: Vec<Slot>, fund: u64) -> Addr {
    let nonce = w.alice.nonce;
    let tx = w
        .alice
        .sign(TxKind::Account(AccountOp::Create { slots, fund }));
    let r = w.net.run(vec![tx]);
    ok(&r[0]);
    let id: Addr = r[0].output.clone().try_into().unwrap();
    assert_eq!(id, account_id(&w.alice.addr(), nonce));
    id
}

fn status(r: &[Receipt]) -> &Status {
    &r[0].status
}

fn supply_ok(net: &Net<MemKv>) {
    assert_eq!(net.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
}

#[test]
fn rotating_key_works_once_then_is_retired() {
    let mut w = world();
    let (k0, k1, k2) = (key(10), key(11), key(12));
    let id = create(&mut w, vec![slot(&k0, true, true)], 1_000 * UNITS_PER_BTB);
    assert_eq!(w.net.bal(&id), 1_000 * UNITS_PER_BTB);

    let to = w.bob.addr();
    let before = w.net.bal(&to);
    let tx = as_account(
        &w.net,
        &k0,
        id,
        0,
        Some(commit(&k1)),
        TxKind::Transfer { to, amount: 5 },
    );
    ok(&w.net.run(vec![tx])[0]);
    assert_eq!(w.net.bal(&to), before + 5);
    assert_eq!(
        w.net.state.account(&id).unwrap().unwrap().slots[0].commitment,
        commit(&k1)
    );

    // The revealed key k0 can never sign again.
    let again = as_account(
        &w.net,
        &k0,
        id,
        0,
        Some(commit(&k2)),
        TxKind::Transfer { to, amount: 5 },
    );
    assert_eq!(
        status(&w.net.run(vec![again])),
        &Status::Invalid(Invalid::Account(AccountInvalid::WrongKey))
    );
    // The next key works and rotates again.
    let tx = as_account(
        &w.net,
        &k1,
        id,
        0,
        Some(commit(&k2)),
        TxKind::Transfer { to, amount: 5 },
    );
    ok(&w.net.run(vec![tx])[0]);
    assert_eq!(
        w.net.state.account(&id).unwrap().unwrap().slots[0].commitment,
        commit(&k2)
    );
    supply_ok(&w.net);
}

#[test]
fn same_key_twice_in_one_block_second_is_invalid() {
    let mut w = world();
    let (k0, k1, k2) = (key(10), key(11), key(12));
    let id = create(&mut w, vec![slot(&k0, true, true)], 1_000 * UNITS_PER_BTB);
    let to = w.bob.addr();
    let a = as_account(
        &w.net,
        &k0,
        id,
        0,
        Some(commit(&k1)),
        TxKind::Transfer { to, amount: 1 },
    );
    let mut b = as_account(
        &w.net,
        &k0,
        id,
        0,
        Some(commit(&k2)),
        TxKind::Transfer { to, amount: 1 },
    );
    b.body.nonce += 1;
    let b = Transaction::sign(b.body, &k0);
    let r = w.net.run(vec![a, b]);
    ok(&r[0]);
    assert_eq!(
        r[1].status,
        Status::Invalid(Invalid::Account(AccountInvalid::WrongKey))
    );
}

#[test]
fn rotation_rules() {
    let mut w = world();
    let (k0, k1) = (key(10), key(11));
    let id = create(
        &mut w,
        vec![slot(&k0, true, true), slot(&k1, false, false)],
        UNITS_PER_BTB,
    );
    let to = w.bob.addr();
    let t = TxKind::Transfer { to, amount: 1 };
    // rotating slot without next
    let tx = as_account(&w.net, &k0, id, 0, None, t.clone());
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::BadRotation))
    );
    // next equal to a commitment the account already has
    let tx = as_account(&w.net, &k0, id, 0, Some(commit(&k1)), t.clone());
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::BadRotation))
    );
    // non-rotating slot with next
    let tx = as_account(&w.net, &k1, id, 1, Some(commit(&key(20))), t.clone());
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::BadRotation))
    );
    // wrong slot index, unknown account, key of another slot
    let tx = as_account(&w.net, &k1, id, 7, None, t.clone());
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::NoSuchSlot))
    );
    let tx = as_account(&w.net, &k1, [9; 32], 1, None, t.clone());
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::NoSuchAccount))
    );
    let tx = as_account(&w.net, &k1, id, 0, Some(commit(&key(21))), t.clone());
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::WrongKey))
    );
    // invalid transactions changed nothing: nonce 0, slot 0 unchanged
    assert_eq!(w.net.state.nonce(&id).unwrap(), 0);
    assert_eq!(
        w.net.state.account(&id).unwrap().unwrap().slots[0].commitment,
        commit(&k0)
    );
}

#[test]
fn spending_slot_cannot_admin() {
    let mut w = world();
    let (admin, spend) = (key(10), key(11));
    let id = create(
        &mut w,
        vec![slot(&admin, true, false), slot(&spend, false, false)],
        1_000 * UNITS_PER_BTB,
    );
    // spending works
    let tx = as_account(
        &w.net,
        &spend,
        id,
        1,
        None,
        TxKind::Transfer {
            to: w.bob.addr(),
            amount: 1,
        },
    );
    ok(&w.net.run(vec![tx])[0]);
    // but not slot changes or names
    let set = TxKind::Account(AccountOp::SetSlots {
        slots: vec![slot(&spend, true, false)],
    });
    let tx = as_account(&w.net, &spend, id, 1, None, set.clone());
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::NotAdmin))
    );
    let reg = TxKind::Name(NameOp::Register {
        name: "alice".into(),
    });
    let tx = as_account(&w.net, &spend, id, 1, None, reg.clone());
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::NotAdmin))
    );
    // the admin slot can
    let tx = as_account(&w.net, &admin, id, 0, None, reg);
    ok(&w.net.run(vec![tx])[0]);
    let tx = as_account(&w.net, &admin, id, 0, None, set);
    ok(&w.net.run(vec![tx])[0]);
    // the old admin key no longer controls the account
    let tx = as_account(
        &w.net,
        &admin,
        id,
        0,
        None,
        TxKind::Transfer {
            to: w.bob.addr(),
            amount: 1,
        },
    );
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::Account(AccountInvalid::WrongKey))
    );
}

#[test]
fn a_reverted_transaction_still_rotates() {
    let mut w = world();
    let (k0, k1) = (key(10), key(11));
    let id = create(&mut w, vec![slot(&k0, true, true)], 1_000 * UNITS_PER_BTB);
    let tx = as_account(
        &w.net,
        &k0,
        id,
        0,
        Some(commit(&k1)),
        TxKind::Name(NameOp::Release {
            name: "nobody".into(),
        }),
    );
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Reverted(Revert::Name(NameError::NoSuchName))
    );
    assert_eq!(
        w.net.state.account(&id).unwrap().unwrap().slots[0].commitment,
        commit(&k1)
    );
}

#[test]
fn bad_slots_and_kinds_are_refused() {
    let mut w = world();
    let k = key(10);
    for slots in [
        vec![],
        vec![slot(&k, false, false)],
        vec![slot(&k, true, false), slot(&k, false, false)],
    ] {
        let tx = w
            .alice
            .sign(TxKind::Account(AccountOp::Create { slots, fund: 0 }));
        assert_eq!(
            status(&w.net.run(vec![tx])),
            &Status::Reverted(Revert::Account(AccountError::BadSlots))
        );
    }
    // SetSlots from a plain key address
    let tx = w.alice.sign(TxKind::Account(AccountOp::SetSlots {
        slots: vec![slot(&k, true, false)],
    }));
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Reverted(Revert::Account(AccountError::NotAnAccount))
    );
    // session and account together
    let id = create(&mut w, vec![slot(&k, true, false)], UNITS_PER_BTB);
    let mut tx = as_account(
        &w.net,
        &k,
        id,
        0,
        None,
        TxKind::Transfer {
            to: w.bob.addr(),
            amount: 1,
        },
    );
    tx.body.session = Some(id);
    let tx = Transaction::sign(tx.body, &k);
    // Refused before execution (the session check or the account check, whichever runs first).
    assert!(matches!(
        status(&w.net.run(vec![tx])),
        Status::Invalid(Invalid::Session(_) | Invalid::Account(AccountInvalid::WithSession))
    ));
}

#[test]
fn state_never_holds_the_public_key() {
    let mut w = world();
    let (k0, k1) = (key(10), key(11));
    let id = create(&mut w, vec![slot(&k0, true, true)], 1_000 * UNITS_PER_BTB);
    let tx = as_account(
        &w.net,
        &k0,
        id,
        0,
        Some(commit(&k1)),
        TxKind::Transfer {
            to: w.bob.addr(),
            amount: 1,
        },
    );
    ok(&w.net.run(vec![tx])[0]);
    let rec = w.net.state.account(&id).unwrap().unwrap();
    let bytes = btb_codec::Encode::encode(&rec);
    for k in [&k0, &k1] {
        let pk = k.verifying_key().to_bytes();
        assert!(!bytes.windows(32).any(|x| x == pk));
    }
}

#[test]
fn encoding_round_trips() {
    let k = key(10);
    for kind in [
        TxKind::Account(AccountOp::Create {
            slots: vec![slot(&k, true, true)],
            fund: 3,
        }),
        TxKind::Name(NameOp::List {
            name: "alice".into(),
            price: 9,
            buyer: Some([4; 32]),
            expires_height: 99,
        }),
        TxKind::Name(NameOp::Pay {
            name: "bob-2".into(),
            amount: 1,
        }),
    ] {
        for account in [
            None,
            Some(AccountAuth {
                account: [1; 32],
                slot: 2,
                next: Some([3; 32]),
            }),
        ] {
            let tx = Transaction::sign(
                TxBody {
                    chain_id: 7,
                    nonce: 1,
                    relayer: Some([5; 32]),
                    session: None,
                    account,
                    kind: kind.clone(),
                },
                &k,
            );
            assert_eq!(Transaction::decode(&tx.encode()).unwrap(), tx);
            assert!(tx.verify_signature());
        }
    }
}

// ------------------------------------------------------------------------------- names

fn name(op: NameOp) -> TxKind {
    TxKind::Name(op)
}

#[test]
fn register_pay_release() {
    let mut w = world();
    let a = w.alice.addr();
    let before = w.net.bal(&a);
    let tx = w.alice.sign(name(NameOp::Register {
        name: "alice".into(),
    }));
    let r = w.net.run(vec![tx]);
    ok(&r[0]);
    let dep = deposit_for(5);
    assert_eq!(w.net.bal(&a), before - dep - r[0].fee);
    assert_eq!(w.net.state.name("alice").unwrap().unwrap().owner, a);
    assert_eq!(w.net.state.name_of(&a).unwrap().as_deref(), Some("alice"));
    assert_eq!(w.net.state.name_deposit("alice").unwrap(), dep);
    supply_ok(&w.net);

    // Bob pays alice by name; the output is the address paid.
    let ab = w.net.bal(&a);
    let tx = w.bob.sign(name(NameOp::Pay {
        name: "alice".into(),
        amount: 77,
    }));
    let r = w.net.run(vec![tx]);
    ok(&r[0]);
    assert_eq!(r[0].output, a.to_vec());
    assert_eq!(w.net.bal(&a), ab + 77);

    // Taken, and one name per account.
    let tx = w.bob.sign(name(NameOp::Register {
        name: "alice".into(),
    }));
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Reverted(Revert::Name(NameError::Taken))
    );
    let tx = w.alice.sign(name(NameOp::Register {
        name: "second".into(),
    }));
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Reverted(Revert::Name(NameError::AlreadyHasName))
    );

    // Release returns the deposit.
    let ab = w.net.bal(&a);
    let tx = w.alice.sign(name(NameOp::Release {
        name: "alice".into(),
    }));
    let r = w.net.run(vec![tx]);
    ok(&r[0]);
    assert_eq!(w.net.bal(&a), ab + dep - r[0].fee);
    assert!(w.net.state.name("alice").unwrap().is_none());
    supply_ok(&w.net);
}

#[test]
fn bad_names_are_refused() {
    let mut w = world();
    for n in ["ab", "Alice", "-ab", "ab-", "a_b", "a.btb"] {
        let tx = w.alice.sign(name(NameOp::Register { name: n.into() }));
        assert_eq!(
            status(&w.net.run(vec![tx])),
            &Status::Reverted(Revert::Name(NameError::BadName)),
            "{n}"
        );
    }
    // Longer than the limit never enters a block.
    let tx = w.alice.sign(name(NameOp::Register {
        name: "x".repeat(33),
    }));
    assert_eq!(
        status(&w.net.run(vec![tx])),
        &Status::Invalid(Invalid::TooLarge)
    );
}

#[test]
fn sell_a_name_atomically() {
    let mut w = world();
    let (a, b) = (w.alice.addr(), w.bob.addr());
    ok(&w.net.run(vec![w.alice.sign(name(NameOp::Register {
        name: "gold".into(),
    }))])[0]);
    let dep = deposit_for(4);
    let price = 5_000 * UNITS_PER_BTB;
    let h = w.net.state.head().unwrap().height;
    ok(&w.net.run(vec![w.alice.sign(name(NameOp::List {
        name: "gold".into(),
        price,
        buyer: None,
        expires_height: h + 100,
    }))])[0]);

    // Bob offers too little: nothing changes but his fee.
    let (ab, bb) = (w.net.bal(&a), w.net.bal(&b));
    let r = w.net.run(vec![w.bob.sign(name(NameOp::Buy {
        name: "gold".into(),
        max_price: price - 1,
    }))]);
    assert_eq!(
        r[0].status,
        Status::Reverted(Revert::Name(NameError::PriceAboveMax))
    );
    assert_eq!((w.net.bal(&a), w.net.bal(&b)), (ab, bb - r[0].fee));

    // A real buy: price and deposit to alice, name to bob, listing gone.
    let (ab, bb) = (w.net.bal(&a), w.net.bal(&b));
    let r = w.net.run(vec![w.bob.sign(name(NameOp::Buy {
        name: "gold".into(),
        max_price: price,
    }))]);
    ok(&r[0]);
    assert_eq!(w.net.bal(&a), ab + price + dep);
    assert_eq!(w.net.bal(&b), bb - price - dep - r[0].fee);
    let rec = w.net.state.name("gold").unwrap().unwrap();
    assert_eq!((rec.owner, rec.listing), (b, None));
    assert_eq!(w.net.state.name_of(&a).unwrap(), None);
    assert_eq!(w.net.state.name_of(&b).unwrap().as_deref(), Some("gold"));
    assert_eq!(w.net.state.name_deposit("gold").unwrap(), dep);
    supply_ok(&w.net);

    // Alice can no longer sell it; buying it again needs a new listing.
    let r = w.net.run(vec![w.alice.sign(name(NameOp::Give {
        name: "gold".into(),
        to: a,
    }))]);
    assert_eq!(
        r[0].status,
        Status::Reverted(Revert::Name(NameError::NotOwner))
    );
    let r = w.net.run(vec![w.alice.sign(name(NameOp::Buy {
        name: "gold".into(),
        max_price: price,
    }))]);
    assert_eq!(
        r[0].status,
        Status::Reverted(Revert::Name(NameError::NotListed))
    );
}

#[test]
fn reserved_and_expired_listings() {
    let mut w = world();
    let carol = Wallet::new(3);
    ok(&w.net.run(vec![w.alice.sign(name(NameOp::Register {
        name: "silver".into(),
    }))])[0]);
    let h = w.net.state.head().unwrap().height;
    ok(&w.net.run(vec![w.alice.sign(name(NameOp::List {
        name: "silver".into(),
        price: 1,
        buyer: Some(carol.addr()),
        expires_height: h + 3,
    }))])[0]);
    let r = w.net.run(vec![w.bob.sign(name(NameOp::Buy {
        name: "silver".into(),
        max_price: 10,
    }))]);
    assert_eq!(
        r[0].status,
        Status::Reverted(Revert::Name(NameError::NotForYou))
    );
    w.net.empty_blocks(3);
    let mut carol = carol;
    let tx = w.bob.transfer(carol.addr(), 10_000 * UNITS_PER_BTB);
    ok(&w.net.run(vec![tx])[0]);
    let r = w.net.run(vec![carol.sign(name(NameOp::Buy {
        name: "silver".into(),
        max_price: 10,
    }))]);
    assert_eq!(
        r[0].status,
        Status::Reverted(Revert::Name(NameError::ListingExpired))
    );
    // a listing in the past and to yourself are refused
    let h = w.net.state.head().unwrap().height;
    let r = w.net.run(vec![w.alice.sign(name(NameOp::List {
        name: "silver".into(),
        price: 1,
        buyer: None,
        expires_height: h,
    }))]);
    assert_eq!(
        r[0].status,
        Status::Reverted(Revert::Name(NameError::BadListing))
    );
}

#[test]
fn give_and_listing_does_not_survive_a_transfer() {
    let mut w = world();
    let b = w.bob.addr();
    ok(&w.net.run(vec![w.alice.sign(name(NameOp::Register {
        name: "gift".into(),
    }))])[0]);
    let h = w.net.state.head().unwrap().height;
    ok(&w.net.run(vec![w.alice.sign(name(NameOp::List {
        name: "gift".into(),
        price: 1,
        buyer: None,
        expires_height: h + 50,
    }))])[0]);
    ok(&w.net.run(vec![w.alice.sign(name(NameOp::Give {
        name: "gift".into(),
        to: b,
    }))])[0]);
    let rec = w.net.state.name("gift").unwrap().unwrap();
    assert_eq!((rec.owner, rec.listing), (b, None));
    supply_ok(&w.net);
}

#[test]
fn an_account_owns_and_sells_a_name() {
    let mut w = world();
    let (k0, k1) = (key(10), key(11));
    let id = create(&mut w, vec![slot(&k0, true, true)], 2_000 * UNITS_PER_BTB);
    let tx = as_account(
        &w.net,
        &k0,
        id,
        0,
        Some(commit(&k1)),
        name(NameOp::Register {
            name: "vault".into(),
        }),
    );
    ok(&w.net.run(vec![tx])[0]);
    assert_eq!(w.net.state.name("vault").unwrap().unwrap().owner, id);
    // Payments by name reach the account.
    let before = w.net.bal(&id);
    ok(&w.net.run(vec![w.bob.sign(name(NameOp::Pay {
        name: "vault".into(),
        amount: 5,
    }))])[0]);
    assert_eq!(w.net.bal(&id), before + 5);
    supply_ok(&w.net);
}
