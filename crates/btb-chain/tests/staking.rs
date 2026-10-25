//! Staking, epochs, rewards and slashing. Security-sensitive: every refusal path is tested, and
//! the supply invariant is checked after every block (by the executor and again here).

mod support;
use btb_chain::*;
use btb_consensus::{Evidence, Sig};
use btb_runtime::MemKv;
use ed25519_dalek::Signer;
use support::*;

const EACH: u64 = 100_000_000;
const E: u64 = 10;
const U: u64 = 25;

fn scfg() -> StakingConfig {
    StakingConfig {
        epoch_len: E,
        max_validators: 3,
        min_stake: 1_000,
        unbonding_blocks: U,
        slash_bps: 2_000,
        reporter_reward_bps: 1_000,
    }
}

struct Env {
    net: Net<MemKv>,
    ops: Vec<Wallet>,
    ck: Vec<SigningKey>,
    rep: Wallet,
}

fn ckey(i: usize) -> SigningKey {
    SigningKey::from_bytes(&[100 + i as u8; 32])
}

/// A 32-byte string that is not a curve point.
fn not_a_key() -> [u8; 32] {
    (0u8..=255)
        .map(|i| {
            let mut k = [2u8; 32];
            k[0] = i;
            k[31] = 0x7F;
            k
        })
        .find(|k| ed25519_dalek::VerifyingKey::from_bytes(k).is_err())
        .expect("some strings are not points")
}

fn ckey_bytes(i: usize) -> [u8; 32] {
    ckey(i).verifying_key().to_bytes()
}

/// `n` operator wallets; the first `stakes.len()` are genesis validators with those stakes.
fn env_with(n: usize, stakes: &[u64], staking: StakingConfig) -> Env {
    let ops: Vec<Wallet> = (0..n).map(|i| Wallet::new(10 + i as u8)).collect();
    let rep = Wallet::new(99);
    let mut all: Vec<&Wallet> = ops.iter().collect();
    all.push(&rep);
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.staking = staking;
    let g = genesis_with(&all, EACH, cfg);
    let vals: Vec<GenesisValidator> = stakes
        .iter()
        .enumerate()
        .map(|(i, s)| GenesisValidator {
            operator: ops[i].addr(),
            consensus_key: ckey_bytes(i),
            network_addr: format!("/ip4/10.0.0.{i}/udp/9000/quic").into_bytes(),
            stake: *s,
        })
        .collect();
    let state = ChainState::from_genesis_with_validators(MemKv::new(), &g, &vals).unwrap();
    Env {
        net: Net {
            state,
            ex: Executor::new(),
        },
        ops,
        ck: (0..n).map(ckey).collect(),
        rep,
    }
}

fn env(n: usize, stakes: &[u64]) -> Env {
    env_with(n, stakes, scfg())
}

impl Env {
    fn height(&self) -> u64 {
        self.net.state.head().unwrap().height
    }
    fn run(&mut self, txs: Vec<Transaction>) -> Vec<Receipt> {
        let r = self.net.run(txs);
        assert_eq!(
            self.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY)
        );
        r
    }
    fn run_one(&mut self, tx: Transaction) -> Receipt {
        self.run(vec![tx]).remove(0)
    }
    /// Empty blocks until the head is at `h`.
    fn to(&mut self, h: u64) {
        while self.height() < h {
            self.run(vec![]);
        }
    }
    fn reg(&mut self, i: usize) -> Transaction {
        let ck = ckey_bytes(i);
        self.ops[i].sign(TxKind::RegisterValidator {
            consensus_key: ck,
            network_addr: b"/dns/node/udp/9000/quic".to_vec(),
        })
    }
    fn stake(&mut self, i: usize, amount: u64) -> Transaction {
        let validator = self.ops[i].key.verifying_key().to_bytes();
        self.ops[i].sign(TxKind::Stake { validator, amount })
    }
    fn unstake(&mut self, i: usize, amount: u64) -> Transaction {
        self.ops[i].sign(TxKind::Unstake { amount })
    }
    fn bal(&self, i: usize) -> u64 {
        self.net.bal(&self.ops[i].addr())
    }
    fn staked(&self, i: usize) -> u64 {
        self.net.state.staked(&self.ops[i].addr()).unwrap()
    }
    fn unbonding(&self, i: usize) -> u64 {
        self.net.state.unbonding(&self.ops[i].addr()).unwrap()
    }
    fn set(&self, epoch: u64) -> Vec<ActiveValidator> {
        self.net
            .state
            .epoch_record(epoch)
            .unwrap()
            .expect("epoch record")
            .validators
    }
    fn set_ops(&self, epoch: u64) -> Vec<Address> {
        self.set(epoch).iter().map(|v| v.operator).collect()
    }
    /// A double vote by validator `idx` of `epoch`'s set in `round`.
    fn double_vote(&self, epoch: u64, idx: usize, round: u64) -> Evidence {
        let ctx = self.net.state.consensus_context(epoch).unwrap().unwrap();
        let key = self
            .net
            .state
            .epoch_record(epoch)
            .unwrap()
            .unwrap()
            .validators[idx]
            .consensus_key;
        let sk = self
            .ck
            .iter()
            .find(|k| k.verifying_key().to_bytes() == key)
            .unwrap();
        let sign = |id: [u8; 32]| (id, Sig(sk.sign(&ctx.vote_bytes(round, &id)).to_bytes()));
        Evidence::DoubleVote {
            voter: idx as u32,
            round,
            a: sign([1; 32]),
            b: sign([2; 32]),
        }
    }
    fn double_proposal(&self, epoch: u64, idx: usize, round: u64) -> Evidence {
        let ctx = self.net.state.consensus_context(epoch).unwrap().unwrap();
        let key = self
            .net
            .state
            .epoch_record(epoch)
            .unwrap()
            .unwrap()
            .validators[idx]
            .consensus_key;
        let sk = self
            .ck
            .iter()
            .find(|k| k.verifying_key().to_bytes() == key)
            .unwrap();
        let sign = |id: [u8; 32]| (id, Sig(sk.sign(&ctx.proposal_bytes(round, &id)).to_bytes()));
        Evidence::DoubleProposal {
            proposer: idx as u32,
            round,
            a: sign([3; 32]),
            b: sign([4; 32]),
        }
    }
    fn evidence(&mut self, epoch: u64, evidence: Evidence) -> Receipt {
        let tx = self.rep.sign(TxKind::SubmitEvidence { epoch, evidence });
        self.run_one(tx)
    }
}

fn reverted(r: &Receipt, e: StakeError) {
    assert_eq!(r.status, Status::Reverted(Revert::Stake(e)), "{r:?}");
}

// ------------------------------------------------------------------ transactions

#[test]
fn tx_kinds_roundtrip_and_reject_tampering() {
    let mut e = env(2, &[]);
    let ev = {
        let mut x = env(1, &[5_000]);
        x.to(0);
        x.double_vote(0, 0, 4)
    };
    let txs = vec![
        e.reg(0),
        e.stake(0, 5),
        e.unstake(0, 5),
        e.rep.sign(TxKind::SubmitEvidence {
            epoch: 3,
            evidence: ev,
        }),
        e.rep.sign(TxKind::SubmitEvidence {
            epoch: 3,
            evidence: Evidence::DoubleProposal {
                proposer: 2,
                round: 9,
                a: ([1; 32], Sig([7; 64])),
                b: ([2; 32], Sig([8; 64])),
            },
        }),
    ];
    for tx in txs {
        let bytes = tx.encode();
        assert_eq!(Transaction::decode(&bytes).unwrap(), tx);
        assert!(tx.verify_signature());
        let mut t2 = tx.clone();
        t2.signature[3] ^= 1;
        assert!(!t2.verify_signature());
        // Truncation or trailing bytes never decode.
        assert!(Transaction::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut long = bytes.clone();
        long.push(0);
        assert!(Transaction::decode(&long).is_err());
    }
    // Tampered amount: the signature no longer matches, so the block skips it.
    let mut tx = e.stake(0, 5);
    if let TxKind::Stake { amount, .. } = &mut tx.body.kind {
        *amount = 6;
    }
    let r = e.run_one(tx);
    assert_eq!(r.status, Status::Invalid(Invalid::BadSignature));
}

#[test]
fn mempool_admits_staking_transactions() {
    let mut e = env(1, &[]);
    let mut p = Mempool::new(ChainConfig::new(CHAIN_ID), MempoolConfig::default());
    let tx = e.reg(0);
    p.insert(tx, 0, EACH).unwrap();
    let tx = e.stake(0, EACH); // cannot also pay the fee
    assert!(matches!(
        p.insert(tx, 1, EACH),
        Err(MempoolError::InsufficientBalance { .. })
    ));
}

#[test]
fn register_validates_key_address_and_uniqueness() {
    let mut e = env(3, &[]);
    // Not a curve point / small-order point.
    let tx = e.ops[0].sign(TxKind::RegisterValidator {
        consensus_key: not_a_key(),
        network_addr: vec![],
    });
    reverted(&e.run_one(tx), StakeError::BadConsensusKey);
    let mut identity = [0u8; 32];
    identity[0] = 1; // the identity point: a weak key
    let tx = e.ops[0].sign(TxKind::RegisterValidator {
        consensus_key: identity,
        network_addr: vec![],
    });
    reverted(&e.run_one(tx), StakeError::BadConsensusKey);
    // Oversized address: not even a valid transaction.
    let tx = e.ops[0].sign(TxKind::RegisterValidator {
        consensus_key: ckey_bytes(0),
        network_addr: vec![0; 257],
    });
    assert_eq!(e.run_one(tx).status, Status::Invalid(Invalid::TooLarge));
    e.ops[0].nonce -= 1; // the invalid transaction did not use its nonce

    let tx = e.reg(0);
    ok(&e.run_one(tx));
    // Another operator cannot take the same consensus key.
    let tx = e.ops[1].sign(TxKind::RegisterValidator {
        consensus_key: ckey_bytes(0),
        network_addr: vec![],
    });
    reverted(&e.run_one(tx), StakeError::KeyTaken);
    // The owner can rotate its key; the old key is free again afterwards.
    let tx = e.ops[0].sign(TxKind::RegisterValidator {
        consensus_key: ckey_bytes(5),
        network_addr: b"new".to_vec(),
    });
    ok(&e.run_one(tx));
    let v = e.net.state.validator(&e.ops[0].addr()).unwrap().unwrap();
    assert_eq!(v.consensus_key, ckey_bytes(5));
    assert_eq!(v.network_addr, b"new");
    let tx = e.ops[1].sign(TxKind::RegisterValidator {
        consensus_key: ckey_bytes(0),
        network_addr: vec![],
    });
    ok(&e.run_one(tx));
}

#[test]
fn stake_and_unstake_refusals_change_nothing() {
    let mut e = env(2, &[]);
    // Staking before registering.
    let tx = e.stake(0, 5_000);
    reverted(&e.run_one(tx), StakeError::NotRegistered);
    let tx = e.reg(0);
    ok(&e.run_one(tx));
    // Zero.
    let tx = e.stake(0, 0);
    reverted(&e.run_one(tx), StakeError::ZeroAmount);
    // More than the balance (with the fee): not even a valid transaction.
    let before = e.bal(0);
    let tx = e.stake(0, before);
    assert!(matches!(
        e.run_one(tx).status,
        Status::Invalid(Invalid::InsufficientBalance { .. })
    ));
    e.ops[0].nonce -= 1;
    // Somebody else's validator key (no delegation yet).
    let other = e.ops[1].key.verifying_key().to_bytes();
    let tx = e.ops[0].sign(TxKind::Stake {
        validator: other,
        amount: 10,
    });
    reverted(&e.run_one(tx), StakeError::NotOperator);
    assert_eq!(e.staked(0), 0);

    let tx = e.stake(0, 5_000);
    ok(&e.run_one(tx));
    assert_eq!(e.staked(0), 5_000);
    // Cannot unstake more than is staked; nothing moves.
    let bal = e.bal(0);
    let tx = e.unstake(0, 5_001);
    let r = e.run_one(tx);
    reverted(&r, StakeError::InsufficientStake);
    assert_eq!(e.staked(0), 5_000);
    assert_eq!(e.unbonding(0), 0);
    assert_eq!(e.bal(0), bal - r.fee);
    let tx = e.unstake(0, 0);
    reverted(&e.run_one(tx), StakeError::ZeroAmount);
    // Cannot unstake what is already unbonding, either.
    let tx = e.unstake(0, 5_000);
    ok(&e.run_one(tx));
    let tx = e.unstake(0, 1);
    reverted(&e.run_one(tx), StakeError::InsufficientStake);
    // A stranger has no stake.
    let tx = e.unstake(1, 1);
    reverted(&e.run_one(tx), StakeError::InsufficientStake);
}

#[test]
fn unbonding_matures_at_epoch_end_plus_unbonding_period() {
    let mut e = env(1, &[]);
    let tx = e.reg(0);
    ok(&e.run_one(tx)); // block 1
    let tx = e.stake(0, 10_000);
    ok(&e.run_one(tx)); // block 2
    e.to(2);
    let tx = e.unstake(0, 4_000);
    let r = e.run_one(tx); // block 3, in epoch 0 which ends at block 10
    ok(&r);
    let mature = E + U; // 35
    let me = e.ops[0].addr();
    assert_eq!(
        e.net.state.unbonding_entries(&me).unwrap(),
        vec![(mature, 4_000)]
    );
    assert_eq!((e.staked(0), e.unbonding(0)), (6_000, 4_000));
    // A second unstake in the same epoch merges into one entry.
    let tx = e.unstake(0, 1_000);
    ok(&e.run_one(tx)); // block 4
    assert_eq!(
        e.net.state.unbonding_entries(&me).unwrap(),
        vec![(mature, 5_000)]
    );
    let balance_locked = e.bal(0);
    // Nothing is released before the maturity height...
    e.to(mature - 1);
    assert_eq!(e.bal(0), balance_locked);
    assert_eq!(e.unbonding(0), 5_000);
    // ...and it is released at the start of block `mature`.
    e.run(vec![]);
    assert_eq!(e.height(), mature);
    assert_eq!(e.bal(0), balance_locked + 5_000);
    assert_eq!(e.unbonding(0), 0);
    assert!(e.net.state.unbonding_entries(&me).unwrap().is_empty());
    // Unstaking in the next epoch matures one epoch later.
    let tx = e.unstake(0, 500);
    ok(&e.run_one(tx)); // block 36, epoch 3 (31..=40)
    assert_eq!(
        e.net.state.unbonding_entries(&me).unwrap(),
        vec![(4 * E + U, 500)]
    );
}

// ------------------------------------------------------------------ epochs and selection

#[test]
fn genesis_validators_form_epoch_zero_and_bond_their_stake() {
    let e = env(4, &[3_000, 1_000, 2_000]);
    assert_eq!(e.net.state.current_epoch().unwrap(), 0);
    let set = e.set(0);
    let stakes: Vec<u64> = set.iter().map(|v| v.stake).collect();
    assert_eq!(stakes, vec![3_000, 2_000, 1_000]); // stake descending
    assert_eq!(e.bal(0), EACH - 3_000);
    assert_eq!(e.staked(0), 3_000);
    assert_eq!(
        e.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
    assert!(e.net.state.consensus_set(0).unwrap().is_some());
    assert!(e.net.state.epoch_record(1).unwrap().is_none());

    // The genesis hash commits to the validators; bad genesis validators are refused.
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.staking = scfg();
    let w = Wallet::new(10);
    let g = genesis_with(&[&w], EACH, cfg);
    let gv = |stake, key: [u8; 32]| GenesisValidator {
        operator: w.addr(),
        consensus_key: key,
        network_addr: vec![],
        stake,
    };
    let kv = || MemKv::new();
    for bad in [
        gv(999, ckey_bytes(0)),      // below min_stake
        gv(EACH + 1, ckey_bytes(0)), // more than the allocation
        gv(2_000, not_a_key()),      // invalid key
    ] {
        assert!(matches!(
            ChainState::from_genesis_with_validators(kv(), &g, &[bad]),
            Err(GenesisError::Config(_))
        ));
    }
    let dup = [gv(2_000, ckey_bytes(0)), gv(2_000, ckey_bytes(1))];
    assert!(ChainState::from_genesis_with_validators(kv(), &g, &dup).is_err());
    let stranger = GenesisValidator {
        operator: [9; 32],
        ..gv(2_000, ckey_bytes(0))
    };
    assert!(ChainState::from_genesis_with_validators(kv(), &g, &[stranger]).is_err());
    assert_ne!(
        g.hash(),
        g.hash_with_validators(&[gv(2_000, ckey_bytes(0))])
    );
}

#[test]
fn top_n_selection_by_stake_above_the_minimum_with_deterministic_ties() {
    // N = 3. Operators 0..=5; stakes: 4 and 5 tie for the last seat, 3 is below the minimum.
    let mut e = env(6, &[5_000]);
    for i in 1..6 {
        let tx = e.reg(i);
        e.run_one(tx);
    }
    let amounts = [0, 4_000, 3_000, 999, 2_000, 2_000];
    for (i, a) in amounts.iter().enumerate().skip(1) {
        let tx = e.stake(i, *a);
        e.run_one(tx);
    }
    // Epoch 0 does not change while stakes move.
    assert_eq!(e.set(0).len(), 1);
    e.to(E);
    assert_eq!(e.net.state.current_epoch().unwrap(), 1);
    let set = e.set(1);
    let ops: Vec<Address> = set.iter().map(|v| v.operator).collect();
    // 5_000, 4_000, 3_000; the 2_000 tie and the 999 are out.
    assert_eq!(ops, vec![e.ops[0].addr(), e.ops[1].addr(), e.ops[2].addr()]);

    // Ties: operators 4 and 5 (2_000 each) compete for one free seat. Lower the cut so only
    // one seat is left by taking operator 2 out, then compare against the address order.
    let tx = e.unstake(2, 3_000);
    e.run_one(tx);
    e.to(2 * E);
    let ops = e.set_ops(2);
    let mut tied = [e.ops[4].addr(), e.ops[5].addr()];
    tied.sort();
    assert_eq!(ops[..2], [e.ops[0].addr(), e.ops[1].addr()]);
    assert_eq!(ops[2], tied[0], "the lower address wins a tie");

    // The order does not depend on registration order: rebuild with the opposite order.
    let mut f = env(6, &[5_000]);
    for i in (1..6).rev() {
        let tx = f.reg(i);
        f.run_one(tx);
    }
    for (i, a) in amounts.iter().enumerate().skip(1).rev() {
        let tx = f.stake(i, *a);
        f.run_one(tx);
    }
    f.to(E);
    assert_eq!(f.set_ops(1), e.set_ops(1));
    // The selected set is a pure function of the stakes.
    assert_eq!(f.net.state.selected_set().unwrap().len(), 3);
}

#[test]
fn epoch_rotation_publishes_the_next_set_and_the_consensus_context() {
    let mut e = env(4, &[4_000, 3_000, 2_000]);
    let tx = e.reg(3);
    e.run_one(tx);
    let tx = e.stake(3, 3_500);
    e.run_one(tx);
    // Before the boundary the next epoch has no set.
    e.to(E - 1);
    assert!(e.net.state.epoch_record(1).unwrap().is_none());
    assert!(e.net.state.consensus_context(1).unwrap().is_none());
    let before = e.net.state.head().unwrap();
    e.run(vec![]);
    assert_eq!(e.height(), E);
    let head = e.net.state.head().unwrap();
    assert_ne!(head.hash, before.hash);
    assert_eq!(e.net.state.current_epoch().unwrap(), 1);
    let rec = e.net.state.epoch_record(1).unwrap().unwrap();
    // Operator 3 (3_500) replaced operator 2 (2_000).
    assert_eq!(
        rec.validators.iter().map(|v| v.stake).collect::<Vec<_>>(),
        vec![4_000, 3_500, 3_000]
    );
    assert_eq!(rec.start_height, E);
    assert_eq!(
        rec.anchor, head.hash,
        "anchored to the last block of the epoch"
    );
    // The epoch-0 set is still there (evidence window).
    assert_eq!(e.set(0).len(), 3);

    // The context the chain gives out is the one a consensus replica uses.
    let set = e.net.state.consensus_set(1).unwrap().unwrap();
    let ctx = e.net.state.consensus_context(1).unwrap().unwrap();
    let cfg = btb_consensus::Config {
        chain_id: e.net.state.consensus_chain_id(),
        epoch: 1,
        end_height: 2 * E,
        ..Default::default()
    };
    let key = e.ck[3].clone();
    let r = btb_consensus::Replica::new_epoch(
        cfg,
        set,
        key,
        None,
        Some(btb_consensus::Anchor {
            height: rec.start_height,
            id: rec.anchor,
        }),
    )
    .unwrap();
    assert_eq!(r.context().genesis, ctx.genesis);
    assert_eq!(r.context().root, ctx.root);
    // An operator that dropped out has no seat.
    assert!(r.context().set.index_of(&ckey_bytes(2)).is_none());
}

#[test]
fn an_empty_selection_keeps_the_previous_set() {
    let mut e = env(2, &[4_000, 3_000]);
    for i in 0..2 {
        let all = e.staked(i);
        let tx = e.unstake(i, all);
        e.run_one(tx);
    }
    e.to(E);
    assert_eq!(e.set(1), e.set(0));
}

#[test]
fn stake_changes_count_from_the_next_epoch() {
    let mut e = env(3, &[3_000, 2_000]);
    let tx = e.reg(2);
    e.run_one(tx);
    let tx = e.stake(2, 10_000);
    e.run_one(tx);
    assert_eq!(e.set(0).len(), 2, "the running epoch's set is fixed");
    e.to(E);
    assert_eq!(e.set(1)[0].operator, e.ops[2].addr());
    assert_eq!(e.set(1)[0].stake, 10_000);
}

// ------------------------------------------------------------------ slashing

#[test]
fn double_vote_slashes_bonded_and_unbonding_stake_once() {
    let mut e = env(3, &[10_000, 6_000, 4_000]);
    // Validator 0 unstakes a third of its stake after the offence is possible.
    let tx = e.unstake(0, 4_000);
    ok(&e.run_one(tx));
    assert_eq!((e.staked(0), e.unbonding(0)), (6_000, 4_000));
    let offender = e.ops[0].addr();
    let rep_before = e.net.bal(&e.rep.addr());
    let ev = e.double_vote(0, 0, 7);
    let r = e.evidence(0, ev.clone());
    ok(&r);
    // 20% of 6_000 + 20% of 4_000 = 2_000; 10% of that to the reporter.
    assert_eq!(r.output, 2_000u64.to_le_bytes());
    assert_eq!((e.staked(0), e.unbonding(0)), (4_800, 3_200));
    assert_eq!(e.net.bal(&e.rep.addr()), rep_before - r.fee + 200);
    assert_eq!(e.net.state.slash_vault_balance().unwrap(), 1_800);
    let info = e.net.state.validator(&offender).unwrap().unwrap();
    assert!(info.jailed);
    assert!(e.net.state.slashed_in_epoch(0, &offender).unwrap());

    // The same evidence again is rejected and costs only the fee.
    let before = (
        e.staked(0),
        e.unbonding(0),
        e.net.state.slash_vault_balance().unwrap(),
    );
    reverted(&e.evidence(0, ev), StakeError::Duplicate);
    // A different offence by the same validator in the same epoch is not slashed again.
    let ev2 = e.double_proposal(0, 0, 9);
    reverted(&e.evidence(0, ev2), StakeError::Duplicate);
    let ev3 = e.double_vote(0, 0, 8);
    reverted(&e.evidence(0, ev3), StakeError::Duplicate);
    assert_eq!(
        before,
        (
            e.staked(0),
            e.unbonding(0),
            e.net.state.slash_vault_balance().unwrap()
        )
    );
    // Other validators are unaffected and can be slashed for the same epoch.
    assert_eq!(e.staked(1), 6_000);
    let ev4 = e.double_proposal(0, 1, 9);
    ok(&e.evidence(0, ev4));
    assert_eq!(e.staked(1), 4_800);
}

#[test]
fn slashed_funds_are_never_minted_and_the_vault_is_locked() {
    let mut e = env(2, &[10_000, 6_000]);
    let ev = e.double_vote(0, 0, 1);
    ok(&e.evidence(0, ev));
    let vault = slash_vault();
    assert_eq!(e.net.bal(&vault), 1_800);
    assert_eq!(
        e.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
    // Nobody has the key of the vault: a transfer "from" it cannot be signed. A transaction
    // from any account only moves that account's own funds.
    let tx = e.rep.transfer(vault, 5);
    ok(&e.run_one(tx));
    assert_eq!(e.net.bal(&vault), 1_805);
}

#[test]
fn reporting_yourself_does_not_pay_the_reward() {
    let mut e = env(2, &[10_000, 6_000]);
    let ev = e.double_vote(0, 0, 1);
    let tx = e.ops[0].sign(TxKind::SubmitEvidence {
        epoch: 0,
        evidence: ev,
    });
    let before = e.bal(0);
    let r = e.run_one(tx);
    ok(&r);
    assert_eq!(e.bal(0), before - r.fee, "no reward for the offender");
    assert_eq!(e.net.state.slash_vault_balance().unwrap(), 2_000);
}

#[test]
fn jailed_validator_is_excluded_next_epoch_and_cannot_come_back() {
    let mut e = env(4, &[10_000, 6_000, 4_000]);
    let tx = e.reg(3);
    e.run_one(tx);
    let tx = e.stake(3, 1_500);
    e.run_one(tx);
    let ev = e.double_vote(0, 0, 3);
    ok(&e.evidence(0, ev));
    // Still in the running epoch...
    assert_eq!(e.set(0).len(), 3);
    assert_eq!(e.set(0)[0].operator, e.ops[0].addr());
    // ...and gone from the next, even though its remaining stake is the largest.
    assert!(e.staked(0) > 1_500 * 4);
    e.to(E);
    let next = e.set_ops(1);
    assert!(!next.contains(&e.ops[0].addr()));
    assert_eq!(next.len(), 3);
    assert!(next.contains(&e.ops[3].addr()));
    // No new stake, no re-registration; it can still leave.
    let tx = e.stake(0, 100);
    reverted(&e.run_one(tx), StakeError::Jailed);
    let tx = e.reg(0);
    reverted(&e.run_one(tx), StakeError::Jailed);
    let all = e.staked(0);
    let tx = e.unstake(0, all);
    ok(&e.run_one(tx));
    // Jailing is permanent in later epochs too.
    e.to(2 * E);
    assert!(!e.set_ops(2).contains(&e.ops[0].addr()));
}

#[test]
fn forged_and_malformed_evidence_is_rejected() {
    let mut e = env(3, &[10_000, 6_000, 4_000]);
    let good = e.double_vote(0, 1, 5);
    let snapshot = |e: &Env| {
        (
            (0..3)
                .map(|i| (e.staked(i), e.unbonding(i)))
                .collect::<Vec<_>>(),
            e.net.state.slash_vault_balance().unwrap(),
        )
    };
    let before = snapshot(&e);

    let Evidence::DoubleVote { voter, round, a, b } = good.clone() else {
        panic!()
    };
    let mut bad_sig = (a, b);
    bad_sig.0 .1 .0[10] ^= 1;
    let cases = vec![
        // A flipped signature bit.
        (
            0,
            Evidence::DoubleVote {
                voter,
                round,
                a: bad_sig.0,
                b: bad_sig.1,
            },
            StakeError::BadEvidence,
        ),
        // The same block twice is not equivocation.
        (
            0,
            Evidence::DoubleVote {
                voter,
                round,
                a,
                b: a,
            },
            StakeError::BadEvidence,
        ),
        // Signatures of validator 1 attributed to validator 2.
        (
            0,
            Evidence::DoubleVote {
                voter: 2,
                round,
                a,
                b,
            },
            StakeError::BadEvidence,
        ),
        // The right votes for another round.
        (
            0,
            Evidence::DoubleVote {
                voter,
                round: round + 1,
                a,
                b,
            },
            StakeError::BadEvidence,
        ),
        // A vote signature used as a proposal signature.
        (
            0,
            Evidence::DoubleProposal {
                proposer: voter,
                round,
                a,
                b,
            },
            StakeError::BadEvidence,
        ),
        // A validator index outside the set.
        (
            0,
            Evidence::DoubleVote {
                voter: 3,
                round,
                a,
                b,
            },
            StakeError::UnknownValidator,
        ),
        (
            0,
            Evidence::DoubleVote {
                voter: u32::MAX,
                round,
                a,
                b,
            },
            StakeError::UnknownValidator,
        ),
        // An epoch that does not exist (yet).
        (1, good.clone(), StakeError::UnknownEpoch),
        (u64::MAX, good.clone(), StakeError::UnknownEpoch),
    ];
    for (epoch, ev, err) in cases {
        let r = e.evidence(epoch, ev);
        reverted(&r, err);
        assert!(r.fee > 0, "a rejected report still pays the fee");
        assert_eq!(snapshot(&e), before);
    }
    // Signed by an outsider key: a well-formed pair that is not the validator's.
    let ctx = e.net.state.consensus_context(0).unwrap().unwrap();
    let outsider = SigningKey::from_bytes(&[77; 32]);
    let sign = |id: [u8; 32]| (id, Sig(outsider.sign(&ctx.vote_bytes(5, &id)).to_bytes()));
    let r = e.evidence(
        0,
        Evidence::DoubleVote {
            voter: 1,
            round: 5,
            a: sign([1; 32]),
            b: sign([2; 32]),
        },
    );
    reverted(&r, StakeError::BadEvidence);
    assert_eq!(snapshot(&e), before);
    // The genuine evidence still works afterwards.
    ok(&e.evidence(0, good));
}

#[test]
fn evidence_from_another_epoch_or_chain_is_rejected() {
    let mut e = env(3, &[10_000, 6_000, 4_000]);
    e.to(E);
    // Same validators in epoch 1: evidence signed for epoch 0 does not verify for epoch 1.
    let old = e.double_vote(0, 0, 2);
    reverted(&e.evidence(1, old), StakeError::BadEvidence);
    // Evidence signed for another chain id does not verify here.
    let rec = e.net.state.epoch_record(0).unwrap().unwrap();
    let other = btb_consensus::Context::for_epoch(
        &btb_chain::staking::consensus_chain_id(CHAIN_ID + 1),
        0,
        rec.consensus_set().unwrap(),
        None,
    );
    let sk = &e.ck[0];
    let sign = |id: [u8; 32]| (id, Sig(sk.sign(&other.vote_bytes(2, &id)).to_bytes()));
    let foreign = Evidence::DoubleVote {
        voter: 0,
        round: 2,
        a: sign([1; 32]),
        b: sign([2; 32]),
    };
    reverted(&e.evidence(0, foreign), StakeError::BadEvidence);
    // The genuine one works in its own epoch.
    let good = e.double_vote(1, 0, 2);
    ok(&e.evidence(1, good));
}

#[test]
fn evidence_is_accepted_only_inside_the_unbonding_window() {
    let mut e = env(2, &[10_000, 6_000]);
    let ev0 = e.double_vote(0, 0, 1);
    let ev1 = e.double_vote(0, 1, 1);
    // Evidence about epoch 0 is valid while height < 10 + 25 = 35.
    e.to(E + U - 2);
    assert_eq!(e.height(), 33);
    ok(&e.evidence(0, ev0)); // block 34
    assert_eq!(e.height(), 34);
    // Block 35 is past the window.
    let r = e.evidence(0, ev1.clone());
    assert_eq!(e.height(), 35);
    reverted(&r, StakeError::Expired);
    // Once pruned the epoch is simply unknown.
    e.to(4 * E);
    assert!(e.net.state.epoch_record(0).unwrap().is_none());
    reverted(&e.evidence(0, ev1), StakeError::UnknownEpoch);
    // Pruning keeps what the window still needs, and the vault and balances are intact.
    assert!(e.net.state.epoch_record(3).unwrap().is_some());
    assert!(e.net.state.epoch_record(4).unwrap().is_some());
}

#[test]
fn stake_unbonding_when_evidence_arrives_is_still_slashed_but_matured_stake_escapes() {
    let mut e = env(2, &[10_000, 6_000]);
    let tx = e.unstake(0, 10_000);
    ok(&e.run_one(tx)); // block 1 -> matures at 35
    let ev = e.double_vote(0, 0, 1);
    e.to(E + U - 2);
    // At block 34 the whole stake is still unbonding and is slashed.
    ok(&e.evidence(0, ev));
    assert_eq!(e.unbonding(0), 8_000);
    // At 35 the rest is released; there is nothing left to slash and the window is closed.
    e.run(vec![]);
    assert_eq!(e.unbonding(0), 0);
}

// ------------------------------------------------------------------ rewards

#[test]
fn rewards_split_the_pool_by_stake_and_sum_exactly() {
    // Three equal validators and a pool that does not divide by three.
    let mut e = env(3, &[1_000, 1_000, 1_000]);
    let mut w = Wallet::new(98);
    let _ = &mut w;
    for _ in 0..4 {
        let tx = e.rep.transfer([5; 32], 1);
        ok(&e.run_one(tx));
    }
    let pool_before_last = e.net.state.validator_pool_balance().unwrap();
    assert!(pool_before_last > 0);
    let bals: Vec<u64> = (0..3).map(|i| e.bal(i)).collect();
    // The boundary block carries one more fee.
    e.to(E - 1);
    let tx = e.rep.transfer([5; 32], 1);
    let r = e.run_one(tx);
    assert_eq!(e.height(), E);
    let fee_part = r.fee; // no relayer: the whole fee
    let pool = pool_before_last + fee_part;
    assert_eq!(
        e.net.state.validator_pool_balance().unwrap(),
        0,
        "pool paid out"
    );
    let gains: Vec<u64> = (0..3).map(|i| e.bal(i) - bals[i]).collect();
    assert_eq!(gains.iter().sum::<u64>(), pool, "rewards sum to the pool");
    // Floor shares, then one extra unit to the first `pool % 3` validators in set order.
    let order = e.set_ops(0);
    let share = pool / 3;
    let rem = (pool % 3) as usize;
    assert!(rem != 0, "the test wants a remainder, pool = {pool}");
    for (i, op) in order.iter().enumerate() {
        let idx = e.ops.iter().position(|w| w.addr() == *op).unwrap();
        assert_eq!(gains[idx], share + u64::from(i < rem));
    }
    // Transactions without a relayer pay the validator pool only.
    assert_eq!(e.net.state.relay_pool_balance().unwrap(), 0);
}

#[test]
fn rewards_are_pro_rata_and_jailed_validators_get_none() {
    let mut e = env(3, &[3_000, 2_000, 1_000]);
    for _ in 0..3 {
        let tx = e.rep.transfer([5; 32], 1);
        e.run_one(tx);
    }
    let ev = e.double_vote(0, 2, 1); // the smallest validator is slashed and jailed
    ok(&e.evidence(0, ev));
    let pool_now = e.net.state.validator_pool_balance().unwrap();
    let b: Vec<u64> = (0..3).map(|i| e.bal(i)).collect();
    e.to(E);
    let pool = e.net.state.validator_pool_balance().unwrap();
    assert_eq!(pool, 0);
    let gains: Vec<u64> = (0..3).map(|i| e.bal(i) - b[i]).collect();
    assert_eq!(gains[2], 0, "jailed");
    assert_eq!(gains[0] + gains[1], pool_now);
    // 3:2 within one unit of rounding.
    let exact0 = u128::from(pool_now) * 3 / 5;
    assert!(
        u128::from(gains[0]).abs_diff(exact0) <= 1,
        "{gains:?} {pool_now}"
    );
}

#[test]
fn rewards_are_accounting_only_and_never_mint() {
    let mut e = env(3, &[3_000, 2_000, 1_000]);
    for _ in 0..3 * E {
        let tx = e.rep.transfer([5; 32], 1);
        e.run_one(tx);
        assert_eq!(
            e.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY)
        );
    }
}

#[test]
fn a_chain_with_no_validators_just_accumulates_fees() {
    let mut e = env(1, &[]);
    for _ in 0..(2 * E) {
        let tx = e.rep.transfer([5; 32], 1);
        e.run_one(tx);
    }
    assert!(e.net.state.validator_pool_balance().unwrap() > 0);
    assert!(e.net.state.epoch_record(1).unwrap().is_none());
}

// ------------------------------------------------------------------ end to end with consensus

#[test]
fn evidence_found_by_a_consensus_replica_slashes_on_chain() {
    use btb_consensus::{Action, Message, Vote};
    let mut e = env(4, &[1_000, 1_000, 1_000]);
    let ctx = e.net.state.consensus_context(0).unwrap().unwrap();
    let rec = e.net.state.epoch_record(0).unwrap().unwrap();
    let key_of = |idx: usize| {
        e.ck.iter()
            .find(|k| k.verifying_key().to_bytes() == rec.validators[idx].consensus_key)
            .unwrap()
            .clone()
    };
    // Replica of validator 2 in epoch 0 sees validator 1 vote twice in round 1.
    let cfg = btb_consensus::Config {
        chain_id: e.net.state.consensus_chain_id(),
        epoch: 0,
        end_height: E,
        ..Default::default()
    };
    let mut r = btb_consensus::Replica::new_epoch(
        cfg,
        e.net.state.consensus_set(0).unwrap().unwrap(),
        key_of(2),
        None,
        None,
    )
    .unwrap();
    assert_eq!(r.context().genesis, ctx.genesis);
    let leader_key = key_of(r.leader(1) as usize);
    let vote = |id: [u8; 32]| Vote {
        round: 1,
        block_id: id,
        voter: 1,
        sig: Sig(key_of(1).sign(&ctx.vote_bytes(1, &id)).to_bytes()),
        proposal_sig: Sig(leader_key.sign(&ctx.proposal_bytes(1, &id)).to_bytes()),
    };
    let mut found = Vec::new();
    for id in [[1u8; 32], [2; 32]] {
        for a in r.on_message(Message::Vote(vote(id))) {
            if let Action::Evidence(ev) = a {
                found.push(ev);
            }
        }
    }
    let double_vote = found
        .into_iter()
        .find(|ev| matches!(ev, Evidence::DoubleVote { .. }))
        .expect("double vote found");
    let offender_op = rec.validators[1].operator;
    let r = e.evidence(0, double_vote);
    ok(&r);
    assert!(e.net.state.validator(&offender_op).unwrap().unwrap().jailed);
}

// ------------------------------------------------------------------ randomised

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

fn random_run(seed: u64) -> (Vec<[u8; 32]>, u64) {
    let mut rng = Lcg(seed);
    let mut e = env(6, &[5_000, 4_000, 3_000]);
    let mut roots = Vec::new();
    for _ in 0..70 {
        let mut txs = Vec::new();
        for _ in 0..rng.below(5) {
            let i = rng.below(6) as usize;
            let amount = rng.below(6_000);
            let tx = match rng.below(8) {
                0 => e.reg(i),
                1 | 2 => e.stake(i, amount),
                3 | 4 => e.unstake(i, amount),
                5 => e.rep.transfer(e.ops[i].addr(), amount),
                _ => {
                    // Evidence about a random epoch in the window against a random seat.
                    let cur = e.net.state.current_epoch().unwrap();
                    let epoch = cur.saturating_sub(rng.below(4));
                    let Some(rec) = e.net.state.epoch_record(epoch).unwrap() else {
                        continue;
                    };
                    let idx = rng.below(rec.validators.len() as u64) as usize;
                    // Only the keys known to this test can sign; others are forged on purpose.
                    let known = e
                        .ck
                        .iter()
                        .any(|k| k.verifying_key().to_bytes() == rec.validators[idx].consensus_key);
                    let evidence = if known && rng.below(4) != 0 {
                        e.double_vote(epoch, idx, rng.below(20))
                    } else {
                        Evidence::DoubleVote {
                            voter: idx as u32,
                            round: 1,
                            a: ([1; 32], Sig([3; 64])),
                            b: ([2; 32], Sig([4; 64])),
                        }
                    };
                    e.rep.sign(TxKind::SubmitEvidence { epoch, evidence })
                }
            };
            txs.push(tx);
        }
        let receipts = e.run(txs);
        for r in &receipts {
            assert!(
                !matches!(r.status, Status::Reverted(Revert::Fault(_))),
                "seed {seed}: {r:?}"
            );
        }
        // Money adds up from the parts as well, not only through `total_supply`.
        let mut sum = e.net.state.total_supply().unwrap();
        assert_eq!(sum, u128::from(TOTAL_SUPPLY), "seed {seed}");
        sum = 0;
        for w in e
            .ops
            .iter()
            .map(|w| w.addr())
            .chain([e.rep.addr(), treasury()])
        {
            sum += u128::from(e.net.bal(&w))
                + u128::from(e.net.state.staked(&w).unwrap())
                + u128::from(e.net.state.unbonding(&w).unwrap());
        }
        for pool in [validator_pool(), relay_pool(), slash_vault()] {
            sum += u128::from(e.net.bal(&pool));
        }
        assert_eq!(sum, u128::from(TOTAL_SUPPLY), "seed {seed}: parts differ");
        roots.push(e.net.state.root().unwrap());
    }
    (roots, e.net.state.slash_vault_balance().unwrap())
}

#[test]
fn supply_invariant_holds_over_random_staking_sequences() {
    let mut slashed_somewhere = false;
    for seed in 0..12 {
        let (roots, vault) = random_run(seed);
        slashed_somewhere |= vault > 0;
        // Same seed, same chain: the whole thing is deterministic.
        assert_eq!(random_run(seed).0, roots);
    }
    assert!(
        slashed_somewhere,
        "the random runs should exercise slashing"
    );
}
