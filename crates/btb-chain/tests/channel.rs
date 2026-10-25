//! Payment channels. Security-sensitive: every refusal path is tested, the supply invariant is
//! checked after every block, and random sequences run against a model.

mod support;
use btb_chain::*;
use btb_runtime::MemKv;
use btb_voucher::{SignedVoucher, Voucher};
use support::*;

const EACH: u64 = 1_000_000_000;

struct Env {
    net: Net<MemKv>,
    payer: Wallet,
    payee: Wallet,
    other: Wallet,
}

fn env() -> Env {
    let (payer, payee, other) = (Wallet::new(1), Wallet::new(2), Wallet::new(3));
    let g = genesis(&[&payer, &payee, &other], EACH);
    Env {
        net: Net::new(MemKv::new(), &g),
        payer,
        payee,
        other,
    }
}

fn run(net: &mut Net<MemKv>, txs: Vec<Transaction>) -> Vec<Receipt> {
    let r = net.run(txs);
    assert_eq!(net.state.total_supply().unwrap(), u128::from(TOTAL_SUPPLY));
    r
}
fn one(net: &mut Net<MemKv>, tx: Transaction) -> Receipt {
    run(net, vec![tx]).remove(0)
}

impl Env {
    fn height(&self) -> u64 {
        self.net.state.head().unwrap().height
    }
    fn to(&mut self, h: u64) {
        while self.height() < h {
            run(&mut self.net, vec![]);
        }
    }
    fn open(&mut self, deposit: u64, expiry: u64) -> ChannelIdT {
        let payee = self.payee.addr();
        let r = one(
            &mut self.net,
            self.payer.sign(TxKind::Channel(ChannelOp::Open {
                payee,
                deposit,
                expiry_blocks: expiry,
            })),
        );
        ok(&r);
        r.output.try_into().unwrap()
    }
    fn voucher(&self, id: ChannelIdT, amount: u64, nonce: u64) -> SignedVoucher {
        Voucher {
            chain_id: CHAIN_ID,
            channel_id: id,
            amount,
            nonce,
        }
        .sign(&self.payer.key)
    }
}

type ChannelIdT = [u8; 32];

fn refused(r: &Receipt, e: ChannelError) {
    assert_eq!(r.status, Status::Reverted(Revert::Channel(e)), "{r:?}");
}

fn close(w: &mut Wallet, v: SignedVoucher) -> Transaction {
    w.sign(TxKind::Channel(ChannelOp::Close { voucher: v }))
}
fn start(w: &mut Wallet, id: ChannelIdT, v: Option<SignedVoucher>) -> Transaction {
    w.sign(TxKind::Channel(ChannelOp::StartClose {
        channel: id,
        voucher: v,
    }))
}
fn finalize(w: &mut Wallet, id: ChannelIdT) -> Transaction {
    w.sign(TxKind::Channel(ChannelOp::Finalize { channel: id }))
}
fn topup(w: &mut Wallet, id: ChannelIdT, amount: u64) -> Transaction {
    w.sign(TxKind::Channel(ChannelOp::TopUp {
        channel: id,
        amount,
    }))
}

fn fee(r: &Receipt) -> u64 {
    r.fee
}

#[test]
fn tx_roundtrip_and_tamper() {
    let mut e = env();
    let id = [9; 32];
    let v = e.voucher(id, 5, 1);
    for kind in [
        ChannelOp::Open {
            payee: [4; 32],
            deposit: 5,
            expiry_blocks: 9,
        },
        ChannelOp::TopUp {
            channel: id,
            amount: 3,
        },
        ChannelOp::Close { voucher: v },
        ChannelOp::StartClose {
            channel: id,
            voucher: None,
        },
        ChannelOp::StartClose {
            channel: id,
            voucher: Some(v),
        },
        ChannelOp::Finalize { channel: id },
    ] {
        let tx = e.payer.sign(TxKind::Channel(kind));
        assert_eq!(Transaction::decode(&tx.encode()).unwrap(), tx);
        assert!(tx.verify_signature());
        let mut bad = tx.clone();
        bad.body.nonce += 1;
        assert!(!bad.verify_signature());
    }
    // unknown sub-tag
    let mut raw = e
        .payer
        .sign(TxKind::Channel(ChannelOp::Finalize { channel: id }))
        .encode();
    raw[17] = 77;
    assert!(Transaction::decode(&raw).is_err());
}

#[test]
fn open_locks_funds_and_pays_the_fee_only_once() {
    let mut e = env();
    let before = e.net.bal(&e.payer.addr());
    let r = one(
        &mut e.net,
        e.payer.sign(TxKind::Channel(ChannelOp::Open {
            payee: e.payee.addr(),
            deposit: 400,
            expiry_blocks: 10,
        })),
    );
    ok(&r);
    let id: ChannelIdT = r.output.clone().try_into().unwrap();
    assert_eq!(id, channel::channel_id(&e.payer.addr(), 0));
    assert_eq!(e.net.bal(&e.payer.addr()), before - 400 - fee(&r));
    let ch = e.net.state.channel(&id).unwrap().unwrap();
    assert_eq!((ch.deposit, ch.payee), (400, e.payee.addr()));
    assert_eq!(e.net.state.channels_locked().unwrap(), 400);
}

#[test]
fn open_refusals() {
    let mut e = env();
    let p = e.payee.addr();
    let mk = |e: &mut Env, payee, deposit, expiry_blocks| {
        let tx = e.payer.sign(TxKind::Channel(ChannelOp::Open {
            payee,
            deposit,
            expiry_blocks,
        }));
        one(&mut e.net, tx)
    };
    refused(&mk(&mut e, p, 0, 5), ChannelError::ZeroAmount);
    let me = e.payer.addr();
    refused(&mk(&mut e, me, 5, 5), ChannelError::SelfChannel);
    refused(&mk(&mut e, p, 5, 0), ChannelError::BadExpiry);
    refused(&mk(&mut e, p, 5, u64::MAX), ChannelError::BadExpiry);
    // more than the balance after the fee: invalid, nothing used
    let r = mk(&mut e, p, EACH, 5);
    assert!(matches!(
        r.status,
        Status::Invalid(Invalid::InsufficientBalance { .. })
    ));
    e.payer.nonce -= 1;
    assert_eq!(e.net.state.channels_locked().unwrap(), 0);
}

#[test]
fn payee_claims_the_highest_voucher_and_the_rest_is_refunded() {
    let mut e = env();
    let id = e.open(1_000, 50);
    let (a0, b0) = (e.net.bal(&e.payer.addr()), e.net.bal(&e.payee.addr()));
    let v = e.voucher(id, 300, 3);
    let r = one(&mut e.net, close(&mut e.payee, v));
    ok(&r);
    assert_eq!(r.output, 300u64.to_le_bytes());
    assert_eq!(e.net.bal(&e.payer.addr()), a0 + 700);
    assert_eq!(e.net.bal(&e.payee.addr()), b0 + 300 - fee(&r));
    assert!(e.net.state.channel(&id).unwrap().is_none());
    assert_eq!(e.net.state.channels_locked().unwrap(), 0);
}

#[test]
fn claim_may_use_the_whole_deposit_and_zero_is_fine() {
    let mut e = env();
    let id = e.open(1_000, 50);
    let v = e.voucher(id, 1_000, 1);
    let b0 = e.net.bal(&e.payee.addr());
    let r = one(&mut e.net, close(&mut e.payee, v));
    ok(&r);
    assert_eq!(e.net.bal(&e.payee.addr()), b0 + 1_000 - fee(&r));
    let id = e.open(1_000, 50);
    let v = e.voucher(id, 0, 1);
    ok(&one(&mut e.net, close(&mut e.payee, v)));
}

#[test]
fn claim_exceeding_the_deposit_is_rejected() {
    let mut e = env();
    let id = e.open(1_000, 50);
    let v = e.voucher(id, 1_001, 1);
    refused(
        &one(&mut e.net, close(&mut e.payee, v)),
        ChannelError::ExceedsDeposit,
    );
    assert!(e.net.state.channel(&id).unwrap().is_some());
    // also when offered with a timeout close
    e.to(60);
    refused(
        &one(&mut e.net, start(&mut e.payer, id, Some(v))),
        ChannelError::ExceedsDeposit,
    );
}

#[test]
fn forged_vouchers_are_rejected() {
    let mut e = env();
    let id = e.open(1_000, 50);
    // signed by the payee itself, by a stranger, tampered amount, tampered signature
    let by_payee = Voucher {
        chain_id: CHAIN_ID,
        channel_id: id,
        amount: 900,
        nonce: 1,
    }
    .sign(&e.payee.key);
    let by_other = Voucher {
        chain_id: CHAIN_ID,
        channel_id: id,
        amount: 900,
        nonce: 1,
    }
    .sign(&e.other.key);
    let mut inflated = e.voucher(id, 100, 1);
    inflated.voucher.amount = 900;
    let mut sig = e.voucher(id, 100, 1);
    sig.signature[5] ^= 0x40;
    for v in [by_payee, by_other, inflated, sig] {
        refused(
            &one(&mut e.net, close(&mut e.payee, v)),
            ChannelError::BadSignature,
        );
    }
    assert_eq!(e.net.state.channel(&id).unwrap().unwrap().deposit, 1_000);
}

#[test]
fn voucher_for_another_channel_or_chain_is_rejected() {
    let mut e = env();
    let a = e.open(1_000, 50);
    let b = e.open(1_000, 50);
    assert_ne!(a, b);
    // a valid voucher for channel `a` cannot close `b`: it names `a`, so it settles `a` only.
    let va = e.voucher(a, 700, 1);
    let mut moved = va;
    moved.voucher.channel_id = b; // re-pointing breaks the signature
    refused(
        &one(&mut e.net, close(&mut e.payee, moved)),
        ChannelError::BadSignature,
    );
    // properly signed for a channel that does not exist
    let ghost = e.voucher([7; 32], 1, 1);
    refused(
        &one(&mut e.net, close(&mut e.payee, ghost)),
        ChannelError::NoSuchChannel,
    );
    // properly signed for another chain
    let other_chain = Voucher {
        chain_id: CHAIN_ID + 1,
        channel_id: a,
        amount: 700,
        nonce: 1,
    }
    .sign(&e.payer.key);
    refused(
        &one(&mut e.net, close(&mut e.payee, other_chain)),
        ChannelError::WrongChain,
    );
    // the right voucher still works, and only touches `a`
    ok(&one(&mut e.net, close(&mut e.payee, va)));
    assert!(e.net.state.channel(&a).unwrap().is_none());
    assert_eq!(e.net.state.channel(&b).unwrap().unwrap().deposit, 1_000);
}

#[test]
fn only_the_payee_closes_and_only_the_payer_tops_up() {
    let mut e = env();
    let id = e.open(1_000, 50);
    let v = e.voucher(id, 100, 1);
    refused(
        &one(&mut e.net, close(&mut e.other, v)),
        ChannelError::NotPayee,
    );
    refused(
        &one(&mut e.net, close(&mut e.payer, v)),
        ChannelError::NotPayee,
    );
    refused(
        &one(&mut e.net, topup(&mut e.other, id, 5)),
        ChannelError::NotPayer,
    );
    refused(
        &one(&mut e.net, topup(&mut e.payee, id, 5)),
        ChannelError::NotPayer,
    );
    refused(
        &one(&mut e.net, start(&mut e.other, id, None)),
        ChannelError::NotPayer,
    );
    refused(
        &one(&mut e.net, finalize(&mut e.other, id)),
        ChannelError::NotPayer,
    );
}

#[test]
fn replay_of_a_lower_voucher_after_a_higher_one_cannot_pay_twice() {
    let mut e = env();
    let id = e.open(1_000, 50);
    let low = e.voucher(id, 100, 1);
    let high = e.voucher(id, 600, 2);
    let b0 = e.net.bal(&e.payee.addr());
    // the payee (or an attacker holding the old voucher) submits both in one block
    let rs = run(
        &mut e.net,
        vec![close(&mut e.payee, high), close(&mut e.payee, low)],
    );
    ok(&rs[0]);
    refused(&rs[1], ChannelError::NoSuchChannel);
    assert_eq!(
        e.net.bal(&e.payee.addr()),
        b0 + 600 - fee(&rs[0]) - fee(&rs[1])
    );
    // a new channel has a new id: the old vouchers are dead there too
    let id2 = e.open(1_000, 50);
    assert_ne!(id, id2);
    refused(
        &one(&mut e.net, close(&mut e.payee, high)),
        ChannelError::NoSuchChannel,
    );
}

#[test]
fn double_close_pays_once() {
    let mut e = env();
    let id = e.open(1_000, 50);
    let v = e.voucher(id, 250, 1);
    let rs = run(
        &mut e.net,
        vec![close(&mut e.payee, v), close(&mut e.payee, v)],
    );
    ok(&rs[0]);
    refused(&rs[1], ChannelError::NoSuchChannel);
    refused(
        &one(&mut e.net, finalize(&mut e.payer, id)),
        ChannelError::NoSuchChannel,
    );
    refused(
        &one(&mut e.net, topup(&mut e.payer, id, 1)),
        ChannelError::NoSuchChannel,
    );
}

#[test]
fn topup_raises_the_ceiling() {
    let mut e = env();
    let id = e.open(500, 50);
    let big = e.voucher(id, 800, 1);
    refused(
        &one(&mut e.net, close(&mut e.payee, big)),
        ChannelError::ExceedsDeposit,
    );
    let r = one(&mut e.net, topup(&mut e.payer, id, 400));
    ok(&r);
    assert_eq!(r.output, 900u64.to_le_bytes());
    assert_eq!(e.net.state.channel(&id).unwrap().unwrap().deposit, 900);
    assert_eq!(e.net.state.channels_locked().unwrap(), 900);
    refused(
        &one(&mut e.net, topup(&mut e.payer, id, 0)),
        ChannelError::ZeroAmount,
    );
    let (a0, b0) = (e.net.bal(&e.payer.addr()), e.net.bal(&e.payee.addr()));
    let r = one(&mut e.net, close(&mut e.payee, big));
    ok(&r);
    assert_eq!(e.net.bal(&e.payer.addr()), a0 + 100);
    assert_eq!(e.net.bal(&e.payee.addr()), b0 + 800 - fee(&r));
    // top-up beyond the balance is invalid (cannot afford)
    let id = e.open(10, 50);
    let r = one(&mut e.net, topup(&mut e.payer, id, u64::MAX));
    assert!(matches!(r.status, Status::Invalid(_)));
}

#[test]
fn timeout_close_before_expiry_is_refused() {
    let mut e = env();
    let id = e.open(1_000, 20);
    let expiry = e.net.state.channel(&id).unwrap().unwrap().expiry_height;
    e.to(expiry - 2);
    // this tx executes in block `expiry - 1`
    refused(
        &one(&mut e.net, start(&mut e.payer, id, None)),
        ChannelError::NotExpired,
    );
    // and the payee can still claim any time before
    let v = e.voucher(id, 10, 1);
    assert_eq!(e.height() + 1, expiry);
    ok(&one(&mut e.net, close(&mut e.payee, v)));
}

#[test]
fn timeout_close_after_expiry_refunds_the_payer_after_the_challenge() {
    let mut e = env();
    let id = e.open(1_000, 20);
    let expiry = e.net.state.channel(&id).unwrap().unwrap().expiry_height;
    e.to(expiry - 1);
    let r = one(&mut e.net, start(&mut e.payer, id, None)); // block `expiry`
    ok(&r);
    let deadline = u64::from_le_bytes(r.output.clone().try_into().unwrap());
    assert_eq!(deadline, expiry + CHANNEL_CHALLENGE_BLOCKS);
    refused(
        &one(&mut e.net, start(&mut e.payer, id, None)),
        ChannelError::AlreadyClosing,
    );
    refused(
        &one(&mut e.net, topup(&mut e.payer, id, 1)),
        ChannelError::Closing,
    );
    // too early
    refused(
        &one(&mut e.net, finalize(&mut e.payer, id)),
        ChannelError::ChallengeOpen,
    );
    e.to(deadline - 2);
    refused(
        &one(&mut e.net, finalize(&mut e.payer, id)),
        ChannelError::ChallengeOpen,
    );
    let a0 = e.net.bal(&e.payer.addr());
    // block `deadline`
    let r = one(&mut e.net, finalize(&mut e.payer, id));
    ok(&r);
    assert!(e.net.state.channel(&id).unwrap().is_none());
    // the whole deposit came back to the payer; only fees were paid
    assert_eq!(e.net.bal(&e.payer.addr()), a0 + 1_000 - fee(&r));
}

#[test]
fn challenge_with_a_higher_voucher_wins() {
    let mut e = env();
    let id = e.open(1_000, 20);
    let expiry = e.net.state.channel(&id).unwrap().unwrap().expiry_height;
    e.to(expiry - 1);
    // the payer offers an old voucher (amount 100); the payee holds 700
    let old = e.voucher(id, 100, 1);
    let new = e.voucher(id, 700, 7);
    ok(&one(&mut e.net, start(&mut e.payer, id, Some(old))));
    let (a0, b0) = (e.net.bal(&e.payer.addr()), e.net.bal(&e.payee.addr()));
    let r = one(&mut e.net, close(&mut e.payee, new));
    ok(&r);
    assert_eq!(r.output, 700u64.to_le_bytes());
    assert_eq!(e.net.bal(&e.payee.addr()), b0 + 700 - fee(&r));
    assert_eq!(e.net.bal(&e.payer.addr()), a0 + 300);
    assert!(e.net.state.channel(&id).unwrap().is_none());
}

#[test]
fn payee_cannot_claim_less_than_the_payer_offered_and_unchallenged_close_pays_the_offer() {
    let mut e = env();
    let id = e.open(1_000, 20);
    let expiry = e.net.state.channel(&id).unwrap().unwrap().expiry_height;
    e.to(expiry - 1);
    let offered = e.voucher(id, 400, 4);
    ok(&one(&mut e.net, start(&mut e.payer, id, Some(offered))));
    // a lower voucher cannot reduce what the payer already recorded
    let lower = e.voucher(id, 50, 1);
    let b0 = e.net.bal(&e.payee.addr());
    let r = one(&mut e.net, close(&mut e.payee, lower));
    ok(&r);
    assert_eq!(e.net.bal(&e.payee.addr()), b0 + 400 - fee(&r));

    // unchallenged: the offer is paid at finalize
    let id = e.open(1_000, 20);
    let expiry = e.net.state.channel(&id).unwrap().unwrap().expiry_height;
    e.to(expiry - 1);
    let offered = e.voucher(id, 400, 4);
    let r = one(&mut e.net, start(&mut e.payer, id, Some(offered)));
    let deadline = u64::from_le_bytes(r.output.try_into().unwrap());
    e.to(deadline - 1);
    let b0 = e.net.bal(&e.payee.addr());
    ok(&one(&mut e.net, finalize(&mut e.payer, id)));
    assert_eq!(e.net.bal(&e.payee.addr()), b0 + 400);
}

#[test]
fn payee_is_too_late_after_the_challenge_period() {
    let mut e = env();
    let id = e.open(1_000, 20);
    let expiry = e.net.state.channel(&id).unwrap().unwrap().expiry_height;
    e.to(expiry - 1);
    let r = one(&mut e.net, start(&mut e.payer, id, None));
    let deadline = u64::from_le_bytes(r.output.try_into().unwrap());
    e.to(deadline - 1);
    let v = e.voucher(id, 500, 1);
    refused(
        &one(&mut e.net, close(&mut e.payee, v)),
        ChannelError::ChallengeOver,
    );
    ok(&one(&mut e.net, finalize(&mut e.payee, id))); // the payee may also trigger the settlement
    assert!(e.net.state.channel(&id).unwrap().is_none());
}

#[test]
fn finalize_without_a_started_close_is_refused() {
    let mut e = env();
    let id = e.open(1_000, 20);
    e.to(100);
    refused(
        &one(&mut e.net, finalize(&mut e.payer, id)),
        ChannelError::NotClosing,
    );
}

#[test]
fn channel_txs_are_rejected_on_another_chain_and_with_bad_signatures() {
    let mut e = env();
    let id = [1; 32];
    let mut t = finalize(&mut e.payer, id);
    t.signature[0] ^= 1;
    let r = one(&mut e.net, t);
    assert_eq!(r.status, Status::Invalid(Invalid::BadSignature));
    e.payer.nonce -= 1;
    e.payer.chain_id = 99;
    let r = one(&mut e.net, finalize(&mut e.payer, id));
    assert!(matches!(
        r.status,
        Status::Invalid(Invalid::WrongChainId { .. })
    ));
}

// ------------------------------------------------------------------ random sequences

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_run(seed: u64) {
    let mut e = env();
    let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut ids: Vec<ChannelIdT> = Vec::new();
    let mut nonce = 0u64;
    for _ in 0..80 {
        let n = 1 + rng.below(4);
        let mut txs = Vec::new();
        for _ in 0..n {
            let pick = if ids.is_empty() {
                None
            } else {
                Some(ids[rng.below(ids.len() as u64) as usize])
            };
            let amt = rng.below(2_000);
            let tx = match (rng.below(8), pick) {
                (0..=1, _) | (_, None) => e.payer.sign(TxKind::Channel(ChannelOp::Open {
                    payee: e.payee.addr(),
                    deposit: rng.below(1_500) + 1,
                    expiry_blocks: rng.below(12) + 1,
                })),
                (2, Some(id)) => topup(&mut e.payer, id, amt),
                (3 | 4, Some(id)) => {
                    nonce += 1;
                    let v = e.voucher(id, amt, nonce);
                    close(&mut e.payee, v)
                }
                (5, Some(id)) => {
                    nonce += 1;
                    let v = (rng.below(2) == 0).then(|| e.voucher(id, amt, nonce));
                    start(&mut e.payer, id, v)
                }
                (6, Some(id)) => finalize(&mut e.payer, id),
                (_, Some(id)) => {
                    // a forged voucher from the wrong key
                    let v = Voucher {
                        chain_id: CHAIN_ID,
                        channel_id: id,
                        amount: amt,
                        nonce: 1,
                    }
                    .sign(&e.other.key);
                    close(&mut e.payee, v)
                }
            };
            txs.push(tx);
        }
        for r in run(&mut e.net, txs) {
            if let (Status::Success, true) = (&r.status, r.output.len() == 32) {
                ids.push(r.output.clone().try_into().unwrap());
            }
        }
        // locked funds equal the sum over the channels that still exist
        let mut locked = 0u128;
        for id in &ids {
            if let Some(c) = e.net.state.channel(id).unwrap() {
                locked += u128::from(c.deposit);
            }
        }
        assert_eq!(locked, e.net.state.channels_locked().unwrap());
    }
}

#[test]
fn supply_invariant_holds_over_random_channel_sequences() {
    for seed in 1..=12 {
        random_run(seed);
    }
}
