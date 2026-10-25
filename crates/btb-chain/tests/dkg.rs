//! The on-chain DKG that replaces the trusted dealer: genesis key generation, resharing at every
//! epoch change, and every way a dealer or accuser can misbehave. Security-sensitive: each
//! refusal and each exclusion path is tested, as is the liveness fallback (a failed generation
//! keeps the previous key) and the handover property (old and new shares do not combine).
//!
//! The harness plays every validator with a real `Agent` (what the node runs), signs their
//! transactions with their consensus keys, builds blocks with real threshold beacons and runs the
//! sequential executor.

mod support;
use btb_chain::dkg::{self, DkgError, DkgOp};
use btb_chain::dkg_agent::Agent;
use btb_chain::*;
use btb_oracle::beacon::{beacon_message, Beacon};
use btb_oracle::bls::Fr;
use btb_oracle::dkg_crypto;
use btb_oracle::{combine, Dealing, GroupKey, KeyShare, Params};
use btb_runtime::MemKv;
use std::collections::HashMap;
use support::*;

const E: u64 = 40;
const W: u64 = 12;
const EACH: u64 = 1_000_000_000_000;
const STAKE: u64 = 100_000;

fn ckey(i: usize) -> SigningKey {
    SigningKey::from_bytes(&[100 + i as u8; 32])
}

fn ckey_bytes(i: usize) -> [u8; 32] {
    ckey(i).verifying_key().to_bytes()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Behave {
    Honest,
    /// Posts nothing (no deal, no complaint).
    Silent,
    /// Deals, but encrypts a wrong share for this validator (by number, not committee index).
    BadShareTo(usize),
    /// Honest, and additionally complains against dealer 0 although its share is fine.
    FalseComplaint,
}

struct Sim {
    net: Net<MemKv>,
    /// Operator accounts (stake); wallets `0..n` are the genesis validators.
    ops: Vec<Wallet>,
    /// Consensus-key accounts (pay for the DKG transactions).
    cw: Vec<Wallet>,
    agents: Vec<Agent>,
    behave: Vec<Behave>,
    user: Wallet,
    pending: Vec<Transaction>,
    /// tx id -> (validator, kind) for the DKG transactions the harness sent.
    sent: HashMap<Hash, (usize, &'static str)>,
    receipts: Vec<(u64, usize, &'static str, Receipt)>,
    false_sent: bool,
}

fn kind_name(op: &DkgOp) -> &'static str {
    match op {
        DkgOp::Deal { .. } => "deal",
        DkgOp::Complain { .. } => "complain",
    }
}

impl Sim {
    /// `n` genesis validators out of `wallets` operator accounts; `interim` = trusted-dealer key.
    fn new(n: usize, wallets: usize, window: u64) -> Sim {
        let ops: Vec<Wallet> = (0..wallets).map(|i| Wallet::new(10 + i as u8)).collect();
        let cw: Vec<Wallet> = (0..wallets)
            .map(|i| Wallet {
                key: ckey(i),
                nonce: 0,
                chain_id: CHAIN_ID,
            })
            .collect();
        let user = Wallet::new(50);
        let mut all: Vec<&Wallet> = ops.iter().chain(cw.iter()).collect();
        all.push(&user);
        let mut cfg = ChainConfig::new(CHAIN_ID);
        cfg.staking = StakingConfig {
            epoch_len: E,
            max_validators: 8,
            min_stake: 1_000,
            unbonding_blocks: E,
            slash_bps: 2_000,
            reporter_reward_bps: 1_000,
        };
        let g = genesis_with(&all, EACH, cfg);
        let vals: Vec<GenesisValidator> = (0..n)
            .map(|i| GenesisValidator {
                operator: ops[i].addr(),
                consensus_key: ckey_bytes(i),
                network_addr: Vec::new(),
                stake: STAKE,
            })
            .collect();
        let og = OracleGenesis {
            group_key: Vec::new(),
            holders: Vec::new(),
            pairs: vec![],
            max_age_blocks: 20,
            dkg_window: window,
        };
        let state = ChainState::from_genesis_full(MemKv::new(), &g, &vals, Some(&og)).unwrap();
        Sim {
            net: Net {
                state,
                ex: Executor::sequential(),
            },
            ops,
            cw,
            agents: (0..wallets)
                .map(|i| Agent::new(CHAIN_ID, ckey(i)))
                .collect(),
            behave: vec![Behave::Honest; wallets],
            user,
            pending: Vec::new(),
            sent: HashMap::new(),
            receipts: Vec::new(),
            false_sent: false,
        }
    }

    fn height(&self) -> u64 {
        self.net.state.head().unwrap().height
    }

    fn kv(&self) -> &MemKv {
        self.net.state.store()
    }

    fn epoch_key(&self, e: u64) -> Option<dkg::EpochKey> {
        dkg::epoch_key(self.kv(), e).unwrap()
    }

    /// Index of validator `v` in the committee of the key generation for `target`.
    fn pos(&self, v: usize, target: u64) -> usize {
        self.session(target)
            .committee()
            .iter()
            .position(|k| *k == ckey_bytes(v))
            .expect("validator is in the committee")
    }

    fn session(&self, target: u64) -> dkg::Session {
        dkg::session(self.kv(), target).unwrap().expect("session")
    }

    /// The beacon for the next block, signed by the key holders the agents know.
    fn beacon_for_next(&self) -> Option<Beacon> {
        let h = self.height() + 1;
        let e = (h - 1) / E;
        let key = self.epoch_key(e)?;
        let msg = beacon_message(CHAIN_ID, e, h);
        let partials: Vec<_> = self
            .agents
            .iter()
            .filter_map(|a| a.share_for_epoch(self.kv(), e).unwrap())
            .map(|s| (s.index, s.sign(&msg)))
            .collect();
        let sig = combine(&key.group, &msg, &partials).expect("enough key holders for the beacon");
        Some(Beacon {
            epoch: e,
            height: h,
            signature: sig.to_bytes(),
        })
    }

    fn block(&mut self, extra: Vec<Transaction>) -> Vec<Receipt> {
        let mut txs = std::mem::take(&mut self.pending);
        txs.extend(extra);
        let beacon = self.beacon_for_next();
        let b = self.net.block_for(txs.clone()).with_oracle(OracleInput {
            beacon,
            ..Default::default()
        });
        let (receipts, root) = self
            .net
            .ex
            .execute_block(&mut self.net.state, &b)
            .expect("block executes");
        assert_eq!(root, self.net.state.root().unwrap());
        assert_eq!(
            self.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY)
        );
        let h = self.height();
        for (tx, r) in txs.iter().zip(&receipts) {
            if let Some((who, kind)) = self.sent.get(&tx.id()) {
                self.receipts.push((h, *who, kind, r.clone()));
            }
        }
        for i in 0..self.agents.len() {
            let step = self.agents[i].step(self.net.state.store(), h + 1).unwrap();
            self.dispatch(i, step.ops);
        }
        receipts
    }

    fn dispatch(&mut self, who: usize, ops: Vec<DkgOp>) {
        let mut ops = match self.behave[who] {
            Behave::Silent => return,
            _ => ops,
        };
        if let Behave::BadShareTo(j) = self.behave[who] {
            ops = ops.into_iter().map(|op| self.tamper(who, op, j)).collect();
        }
        if self.behave[who] == Behave::FalseComplaint && self.height() > 1 {
            if let Some(op) = self.false_complaint(who) {
                self.false_sent = true;
                ops.push(op);
            }
        }
        for op in ops {
            let kind = kind_name(&op);
            let tx = self.cw[who].sign(TxKind::Dkg(op));
            self.sent.insert(tx.id(), (who, kind));
            self.pending.push(tx);
        }
    }

    /// Replace a deal by one that encrypts a wrong share for recipient `j`.
    fn tamper(&self, who: usize, op: DkgOp, j: usize) -> DkgOp {
        let DkgOp::Deal { target, .. } = &op else {
            return op;
        };
        let s = self.session(*target);
        let j = s
            .committee()
            .iter()
            .position(|k| *k == ckey_bytes(j))
            .unwrap();
        let me = ckey_bytes(who);
        let i = s.dealers.iter().position(|k| *k == me).unwrap();
        let params = s.params();
        let ent = [0xAA ^ who as u8; 32];
        let dealing = match s.from {
            None => Dealing::fresh(i, params, &ent).unwrap(),
            Some(from) => {
                let old = self.epoch_key(from).unwrap();
                let share = self.agents[who].share_of_origin(old.origin).unwrap();
                Dealing::reshare(share, params, &ent).unwrap()
            }
        };
        let mut shares: Vec<Fr> = (0..params.n).map(|k| dealing.share_for(k)).collect();
        shares[j] = shares[j].add(&Fr::one());
        let d = dkg_crypto::deliver(
            CHAIN_ID,
            s.target,
            i as u32,
            &[7; 32],
            &s.committee(),
            &shares,
        );
        DkgOp::Deal {
            target: s.target,
            eph: d.eph,
            commitments: dealing.commitments.iter().map(|c| c.to_bytes()).collect(),
            ciphertexts: d.ciphertexts,
        }
    }

    /// A complaint against dealer 0 with a valid proof, although dealer 0's share was fine.
    fn false_complaint(&self, who: usize) -> Option<DkgOp> {
        if self.false_sent {
            return None;
        }
        let s = dkg::sessions(self.kv())
            .unwrap()
            .into_iter()
            .find(|s| s.status == dkg::STATUS_OPEN)?;
        let d = dkg::deal(self.kv(), s.target, 0).unwrap()?;
        let r = dkg_crypto::reveal(&ckey(who), &d.eph)?;
        Some(DkgOp::Complain {
            target: s.target,
            dealer: 0,
            shared: r.shared,
            proof: r.proof,
        })
    }

    fn run_to(&mut self, height: u64) {
        while self.height() < height {
            self.block(vec![]);
        }
    }

    fn receipts_of(&self, kind: &str) -> Vec<&(u64, usize, &'static str, Receipt)> {
        self.receipts.iter().filter(|r| r.2 == kind).collect()
    }

    fn qualified(&self, target: u64) -> Vec<usize> {
        let s = self.session(target);
        dkg::outcome(self.kv(), &s)
            .unwrap()
            .map(|(_, o)| o.qualified)
            .unwrap_or_default()
    }

    /// Sign `msg` with the shares of the key of `epoch` held by `who`.
    fn sign_with(&self, e: u64, who: &[usize], msg: &[u8]) -> Vec<(usize, btb_oracle::bls::G2)> {
        who.iter()
            .map(|i| {
                let s = self.agents[*i]
                    .share_for_epoch(self.kv(), e)
                    .unwrap()
                    .expect("share");
                (s.index, s.sign(msg))
            })
            .collect()
    }
}

fn dkg_error(r: &Receipt) -> DkgError {
    match &r.status {
        Status::Reverted(Revert::Dkg(e)) => e.clone(),
        other => panic!("expected a DKG refusal, got {other:?}"),
    }
}

fn group_of(k: &dkg::EpochKey) -> &GroupKey {
    &k.group
}

// ------------------------------------------------------------------------------ genesis

#[test]
fn an_honest_genesis_dkg_gives_every_validator_a_consistent_share_of_one_key() {
    let mut s = Sim::new(4, 4, W);
    // No key yet: blocks in the window carry no beacon, and a block that did would be refused.
    s.run_to(2);
    assert!(s.epoch_key(0).is_none());
    let sess = s.session(0);
    assert_eq!((sess.window_start, sess.window_end), (1, W));
    assert_eq!(sess.status, dkg::STATUS_OPEN);
    s.run_to(W);
    assert_eq!(s.session(0).status, dkg::STATUS_DONE);
    let key = s.epoch_key(0).expect("epoch 0 has a key after the window");
    assert_eq!(key.origin, 0);
    assert_eq!(key.group.n(), 4);
    assert_eq!(key.group.threshold, 3);
    // The beacon committee is the epoch's validator set, in consensus order.
    let rec0 = s.net.state.epoch_record(0).unwrap().unwrap();
    let committee: Vec<[u8; 32]> = rec0.validators.iter().map(|v| v.consensus_key).collect();
    assert_eq!(key.holders, committee);
    assert_eq!(s.qualified(0), vec![0, 1, 2, 3]);
    key.group.check().unwrap();
    // Every validator derived its own share, consistent with the public key.
    for i in 0..4 {
        let sh = s.agents[i]
            .share_for_epoch(s.kv(), 0)
            .unwrap()
            .expect("share");
        assert_eq!(sh.index, s.pos(i, 0));
        assert_eq!(sh.public_share(), key.group.public_shares[sh.index]);
    }
    // Any threshold subset signs the same message under the one group key.
    let msg = beacon_message(CHAIN_ID, 0, 999);
    let a = combine(&key.group, &msg, &s.sign_with(0, &[0, 1, 2], &msg)).unwrap();
    let b = combine(&key.group, &msg, &s.sign_with(0, &[1, 2, 3], &msg)).unwrap();
    assert_eq!(a, b);
    // The next block needs a beacon; one without is refused, with one it runs.
    let b = s.net.block_for(vec![]);
    assert!(matches!(
        s.net.ex.execute_block(&mut s.net.state, &b),
        Err(BlockError::Oracle(_))
    ));
    s.run_to(W + 5);
    // ... and an unneeded beacon during the window would have been refused too: a fresh chain.
    let s2 = Sim::new(4, 4, W);
    let early = Beacon {
        epoch: 0,
        height: 1,
        signature: a.to_bytes(),
    };
    let b = s2.net.block_for(vec![]).with_oracle(OracleInput {
        beacon: Some(early),
        ..Default::default()
    });
    let mut s2 = s2;
    assert!(matches!(
        s2.net.ex.execute_block(&mut s2.net.state, &b),
        Err(BlockError::Oracle(_))
    ));
    // The DKG transactions all succeeded and every validator dealt exactly once.
    let deals = s.receipts_of("deal");
    assert_eq!(deals.len(), 4);
    assert!(deals.iter().all(|r| r.3.status == Status::Success));
    assert!(s.receipts_of("complain").is_empty());
}

#[test]
fn randomness_is_unavailable_until_the_genesis_key_exists() {
    let mut s = Sim::new(4, 4, W);
    let tx = s.user.deploy(elf("oracle"), manifest("oracle"));
    let rs = s.block(vec![tx]);
    let c = deployed(&rs[0]);
    let fund = s.user.transfer(c, 100_000_000_000);
    s.block(vec![fund]);
    let early = s.user.call(c, "roll", &());
    let r = s.block(vec![early]).remove(0);
    match &r.status {
        Status::Reverted(Revert::Trap(t)) => {
            assert!(t.contains("RandomnessUnavailable"), "{t}")
        }
        other => panic!("expected a trap, got {other:?}"),
    }
    s.run_to(W + 1);
    let late = s.user.call(c, "roll", &());
    let r = s.block(vec![late]).remove(0);
    ok(&r);
}

// ------------------------------------------------------------------------------ misbehaviour

#[test]
fn a_dealer_that_sends_a_bad_share_is_complained_against_and_excluded() {
    let mut s = Sim::new(4, 4, W);
    s.behave[1] = Behave::BadShareTo(2);
    s.run_to(W);
    // Member 2 found the share wrong, proved it, and the chain upheld the complaint.
    let complaints = s.receipts_of("complain");
    assert_eq!(complaints.len(), 1, "{complaints:?}");
    assert_eq!(complaints[0].1, 2);
    assert_eq!(complaints[0].3.status, Status::Success);
    let cheat = s.pos(1, 0);
    assert!(
        dkg::deal(s.kv(), 0, cheat as u32)
            .unwrap()
            .unwrap()
            .disqualified
    );
    let honest: Vec<usize> = (0..4).filter(|d| *d != cheat).collect();
    assert_eq!(s.qualified(0), honest);
    // The key exists without the cheater's contribution, and everybody holds a consistent share.
    let key = s.epoch_key(0).unwrap();
    for i in 0..4 {
        let sh = s.agents[i]
            .share_for_epoch(s.kv(), 0)
            .unwrap()
            .expect("share");
        assert_eq!(
            sh.public_share(),
            key.group.public_shares[sh.index],
            "validator {i}"
        );
    }
    let msg = beacon_message(CHAIN_ID, 0, 77);
    combine(&key.group, &msg, &s.sign_with(0, &[0, 2, 3], &msg)).unwrap();
    // Only the single bad recipient needed to complain; the others saw correct shares.
    s.run_to(W + 3);
}

#[test]
fn a_false_complaint_is_rejected_and_the_dealer_stays_qualified() {
    let mut s = Sim::new(4, 4, W);
    s.behave[3] = Behave::FalseComplaint;
    s.run_to(W);
    let complaints = s.receipts_of("complain");
    assert!(!complaints.is_empty());
    for c in &complaints {
        assert_eq!(c.1, 3);
        assert_eq!(dkg_error(&c.3), DkgError::ComplaintRejected);
    }
    // the accuser paid for it
    assert!(complaints.iter().all(|c| c.3.fee > 0));
    assert!(!dkg::deal(s.kv(), 0, 0).unwrap().unwrap().disqualified);
    assert_eq!(s.qualified(0), vec![0, 1, 2, 3]);
    assert!(s.epoch_key(0).is_some());
}

#[test]
fn a_silent_dealer_is_excluded_and_the_key_still_forms() {
    let mut s = Sim::new(4, 4, W);
    s.behave[2] = Behave::Silent;
    s.run_to(W);
    let silent = s.pos(2, 0);
    assert_eq!(
        s.qualified(0),
        (0..4).filter(|d| *d != silent).collect::<Vec<_>>()
    );
    assert_eq!(s.session(0).status, dkg::STATUS_DONE);
    let key = s.epoch_key(0).unwrap();
    // The silent validator is still a recipient: it derives its share from the others' deals
    // (its agent runs, only its transactions are withheld), so it can sign too.
    for i in 0..4 {
        let sh = s.agents[i]
            .share_for_epoch(s.kv(), 0)
            .unwrap()
            .expect("share");
        assert_eq!(sh.public_share(), key.group.public_shares[sh.index]);
    }
}

#[test]
fn fewer_than_threshold_qualified_dealers_fail_the_genesis_dkg_and_the_next_epoch_retries() {
    let mut s = Sim::new(4, 4, W);
    s.behave[2] = Behave::Silent;
    s.behave[3] = Behave::Silent;
    s.run_to(W + 2);
    // two of four qualified, threshold three: no key; the chain keeps running without beacons
    assert_eq!(s.session(0).status, dkg::STATUS_FAILED);
    assert!(s.epoch_key(0).is_none());
    s.run_to(E - W);
    assert!(s.epoch_key(0).is_none());
    // the validators recover: the tail window of epoch 0 deals a fresh key for epoch 1
    s.behave[2] = Behave::Honest;
    s.behave[3] = Behave::Honest;
    s.run_to(E + 3);
    let sess = s.session(1);
    assert_eq!(sess.from, None, "no key to reshare, so a fresh one");
    assert_eq!(sess.status, dkg::STATUS_DONE);
    let k1 = s.epoch_key(1).expect("key for epoch 1");
    assert_eq!(k1.origin, 1);
    // epoch 0 never had one
    assert!(s.epoch_key(0).is_none());
}

#[test]
fn a_failed_resharing_keeps_the_previous_key_and_beacons_continue() {
    let mut s = Sim::new(4, 4, W);
    s.run_to(W + 1);
    let k0 = s.epoch_key(0).unwrap();
    s.behave[0] = Behave::Silent;
    s.behave[1] = Behave::Silent;
    s.run_to(E + 4);
    let sess = s.session(1);
    assert_eq!(sess.from, Some(0));
    assert_eq!(
        sess.status,
        dkg::STATUS_FAILED,
        "2 of 4 old holders dealt, 3 needed"
    );
    let k1 = s.epoch_key(1).expect("the previous key carries over");
    assert_eq!(k1, k0, "same key, same holders, same origin");
    // beacons for epoch 1 verify under the unchanged key (run_to made them with it)
    // The next tail resharing, with everybody back, succeeds from the carried key.
    s.behave[0] = Behave::Honest;
    s.behave[1] = Behave::Honest;
    s.run_to(2 * E + 4);
    let sess = s.session(2);
    assert_eq!(sess.from, Some(1));
    assert_eq!(sess.status, dkg::STATUS_DONE);
    let k2 = s.epoch_key(2).unwrap();
    assert_eq!(k2.origin, 2);
    assert_eq!(k2.group.public_key, k0.group.public_key);
}

// ------------------------------------------------------------------------------ resharing

#[test]
fn resharing_keeps_the_group_key_through_three_epochs_with_a_validator_set_change() {
    // Five operators; validators 0..4 at genesis, then 3 leaves and 4 joins before the tail.
    let mut s = Sim::new(4, 5, W);
    s.run_to(W + 1);
    let k0 = s.epoch_key(0).unwrap();
    let pk = k0.group.public_key;

    // Validator 3 unbonds everything, validator 4 registers and stakes.
    let unstake = s.ops[3].sign(TxKind::Unstake { amount: STAKE });
    let reg = s.ops[4].sign(TxKind::RegisterValidator {
        consensus_key: ckey_bytes(4),
        network_addr: Vec::new(),
    });
    let stake_key = s.ops[4].key.verifying_key().to_bytes();
    let stake = s.ops[4].sign(TxKind::Stake {
        validator: stake_key,
        amount: STAKE,
    });
    for r in s.block(vec![unstake, reg, stake]) {
        ok(&r);
    }
    s.run_to(E + 2);

    // Epoch 1's committee is exactly its validator set: {0, 1, 2, 4}.
    let rec1 = s.net.state.epoch_record(1).unwrap().unwrap();
    let keys1: Vec<[u8; 32]> = rec1.validators.iter().map(|v| v.consensus_key).collect();
    assert_eq!(keys1.len(), 4);
    assert!(keys1.contains(&ckey_bytes(4)) && !keys1.contains(&ckey_bytes(3)));
    let sess1 = s.session(1);
    assert_eq!(sess1.from, Some(0));
    assert_eq!(sess1.status, dkg::STATUS_DONE);
    assert_eq!(sess1.committee(), keys1);
    let k1 = s.epoch_key(1).unwrap();
    assert_eq!(k1.holders, keys1);
    assert_eq!(k1.origin, 1);
    assert_eq!(
        group_of(&k1).public_key,
        pk,
        "resharing keeps the group key"
    );
    assert_ne!(
        k1.group.public_shares, k0.group.public_shares,
        "but every share is new"
    );
    // the four old holders all dealt
    assert_eq!(
        dkg::outcome(s.kv(), &sess1).unwrap().unwrap().1.qualified,
        vec![0, 1, 2, 3]
    );

    // The new committee signs under the unchanged key; the removed validator holds no share.
    let msg = beacon_message(CHAIN_ID, 1, 12345);
    let new_signers: Vec<usize> = vec![0, 1, 4];
    let sig = combine(&k1.group, &msg, &s.sign_with(1, &new_signers, &msg)).unwrap();
    assert!(btb_oracle::bls::verify(&pk, &msg, &sig));
    assert!(s.agents[3].share_for_epoch(s.kv(), 1).unwrap().is_none());
    assert!(s.agents[4].share_for_epoch(s.kv(), 0).unwrap().is_none());

    // After the handover, old and new shares do not combine: two old shares and one new one
    // (three distinct x-coordinates, the threshold) interpolate to garbage, and two shares of
    // either generation alone are below the threshold.
    assert_eq!(k1.group.threshold, 3);
    let new_idx = k1.holders.iter().position(|h| *h == ckey_bytes(4)).unwrap();
    let newer = s.agents[4].share_for_epoch(s.kv(), 1).unwrap().unwrap();
    assert_eq!(newer.index, new_idx);
    let old: Vec<(usize, btb_oracle::bls::G2)> = [0usize, 1, 2]
        .iter()
        .map(|i| s.agents[*i].share_of_origin(0).unwrap())
        .filter(|sh| sh.index != new_idx)
        .take(2)
        .map(|sh| (sh.index, sh.sign(&msg)))
        .collect();
    assert_eq!(old.len(), 2);
    let mut mixed = old.clone();
    mixed.push((newer.index, newer.sign(&msg)));
    assert_eq!(
        combine(&k1.group, &msg, &mixed),
        Err(btb_oracle::CombineError::BadAggregate),
        "mixed old/new shares must not sign"
    );
    assert_eq!(
        combine(&k1.group, &msg, &old),
        Err(btb_oracle::CombineError::NotEnough)
    );

    // Epoch 2: another reshare, from the epoch-1 key, keeps the key again; beacons never stop.
    s.run_to(2 * E + 3);
    let k2 = s.epoch_key(2).unwrap();
    assert_eq!(k2.origin, 2);
    assert_eq!(k2.group.public_key, pk);
    assert_eq!(s.session(2).from, Some(1));
    s.run_to(3 * E + 3);
    let k3 = s.epoch_key(3).unwrap();
    assert_eq!(k3.group.public_key, pk);
    assert_eq!(k3.origin, 3);
    let h = s.height();
    assert!(h > 3 * E);
    // Every block of every epoch carried a valid beacon (the executor refuses blocks without).
    let last = oracle::last_beacon(s.kv()).unwrap().unwrap();
    assert_eq!((last.epoch, last.height), (3, h));
    assert!(last.verify(CHAIN_ID, &k3.group));
}

// ------------------------------------------------------------------------------ refusals

#[test]
fn dkg_transactions_are_refused_outside_their_rules() {
    let mut s = Sim::new(4, 5, 20);
    s.behave = vec![Behave::Silent; 5];
    s.run_to(2);
    let sess = s.session(0);
    // a valid deal built by an honest agent, to mutate
    let good = s.agents[0].build_deal(s.kv(), &sess).unwrap().unwrap();
    let DkgOp::Deal {
        target,
        eph,
        commitments,
        ciphertexts,
    } = good.clone()
    else {
        unreachable!()
    };
    let send = |s: &mut Sim, who: usize, op: DkgOp| -> Receipt {
        let tx = s.cw[who].sign(TxKind::Dkg(op));
        s.block(vec![tx]).remove(0)
    };
    // not a dealer (validator 4 is not in the committee)
    let r = send(&mut s, 4, good.clone());
    assert_eq!(dkg_error(&r), DkgError::NotDealer);
    // wrong shapes
    let mut short = commitments.clone();
    short.pop();
    let r = send(
        &mut s,
        0,
        DkgOp::Deal {
            target,
            eph,
            commitments: short,
            ciphertexts: ciphertexts.clone(),
        },
    );
    assert_eq!(dkg_error(&r), DkgError::BadShape);
    let mut few = ciphertexts.clone();
    few.pop();
    let r = send(
        &mut s,
        0,
        DkgOp::Deal {
            target,
            eph,
            commitments: commitments.clone(),
            ciphertexts: few,
        },
    );
    assert_eq!(dkg_error(&r), DkgError::BadShape);
    // junk commitment, junk ephemeral key
    let mut junk = commitments.clone();
    junk[1] = [0xAB; 48];
    let r = send(
        &mut s,
        0,
        DkgOp::Deal {
            target,
            eph,
            commitments: junk,
            ciphertexts: ciphertexts.clone(),
        },
    );
    assert_eq!(dkg_error(&r), DkgError::BadCommitments);
    let r = send(
        &mut s,
        0,
        DkgOp::Deal {
            target,
            eph: [0; 32],
            commitments: commitments.clone(),
            ciphertexts: ciphertexts.clone(),
        },
    );
    assert_eq!(dkg_error(&r), DkgError::BadEphemeral);
    // unknown session
    let r = send(
        &mut s,
        0,
        DkgOp::Deal {
            target: 9,
            eph,
            commitments: commitments.clone(),
            ciphertexts: ciphertexts.clone(),
        },
    );
    assert_eq!(dkg_error(&r), DkgError::NoSession);
    // the good one works once, then it is a duplicate
    ok(&send(&mut s, 0, good.clone()));
    let r = send(&mut s, 0, good.clone());
    assert_eq!(dkg_error(&r), DkgError::AlreadyDealt);

    // complaints: unknown deal, non-member, bad proof, wrong key's proof
    let d0 = s.pos(0, 0) as u32;
    let honest = dkg_crypto::reveal(&ckey(1), &eph).unwrap();
    let complain = |who_dealer: u32, r: &dkg_crypto::Reveal| DkgOp::Complain {
        target,
        dealer: who_dealer,
        shared: r.shared,
        proof: r.proof,
    };
    let r = send(&mut s, 1, complain((d0 + 1) % 4, &honest));
    assert_eq!(dkg_error(&r), DkgError::NoSuchDeal);
    let r = send(&mut s, 4, complain(d0, &honest));
    assert_eq!(dkg_error(&r), DkgError::NotMember);
    // validator 1 sends validator 2's proof
    let other = dkg_crypto::reveal(&ckey(2), &eph).unwrap();
    let r = send(&mut s, 1, complain(d0, &other));
    assert_eq!(dkg_error(&r), DkgError::BadProof);
    let mut damaged = honest;
    damaged.proof[70] ^= 1;
    let r = send(&mut s, 1, complain(d0, &damaged));
    assert_eq!(dkg_error(&r), DkgError::BadProof);
    // a correct proof about a correct share is a false complaint
    let r = send(&mut s, 1, complain(d0, &honest));
    assert_eq!(dkg_error(&r), DkgError::ComplaintRejected);

    // phases: after the deal phase no deal is accepted; after the window nothing is
    s.run_to(sess.deal_end + 1);
    let late = s.agents[1]
        .build_deal(s.kv(), &s.session(0))
        .unwrap()
        .unwrap();
    let r = send(&mut s, 1, late);
    assert_eq!(dkg_error(&r), DkgError::WrongPhase);
    s.run_to(20);
    let r = send(&mut s, 1, complain(d0, &honest));
    assert_eq!(
        dkg_error(&r),
        DkgError::NoSession,
        "the session is finished"
    );
}

#[test]
fn a_reshare_deal_must_start_from_the_dealers_old_public_share() {
    let mut s = Sim::new(4, 4, W);
    s.run_to(W + 1);
    // let everybody run honestly until the tail window opens
    s.run_to(E - W + 1);
    let sess = s.session(1);
    assert_eq!(sess.from, Some(0));
    s.behave = vec![Behave::Silent; 4];
    s.pending.clear();
    for i in 0..4 {
        s.cw[i].nonce = s.net.state.nonce(&s.cw[i].addr()).unwrap();
    }
    // a dealer that reshares a different secret (a fresh polynomial) is refused at the door
    let params = Params::standard(4);
    let d = Dealing::fresh(0, params, &[5; 32]).unwrap();
    let shares: Vec<Fr> = (0..4).map(|j| d.share_for(j)).collect();
    let del = dkg_crypto::deliver(CHAIN_ID, 1, 0, &[6; 32], &sess.committee(), &shares);
    let op = DkgOp::Deal {
        target: 1,
        eph: del.eph,
        commitments: d.commitments.iter().map(|c| c.to_bytes()).collect(),
        ciphertexts: del.ciphertexts,
    };
    let tx = s.cw[0].sign(TxKind::Dkg(op));
    let r = s.block(vec![tx]).remove(0);
    assert_eq!(dkg_error(&r), DkgError::BadCommitments);
    // the honest reshare (constant term = old share) is accepted
    let ok_op = s.agents[0].build_deal(s.kv(), &sess).unwrap().unwrap();
    let tx = s.cw[0].sign(TxKind::Dkg(ok_op));
    ok(&s.block(vec![tx]).remove(0));
}

#[test]
fn key_records_serve_the_trusted_dealer_interim_and_reshare_from_it() {
    // The explicit testnet fallback: a genesis key from a trusted dealer is epoch 0's key, and
    // the first epoch change already reshares it on chain.
    let n = 4;
    let (group, shares) = btb_oracle::trusted_deal(Params::standard(n), &[3; 32]).unwrap();
    let mut s = Sim::new(n, n, W);
    // rebuild the chain with the interim key
    let ops: Vec<&Wallet> = s.ops.iter().chain(s.cw.iter()).chain([&s.user]).collect();
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.staking = StakingConfig {
        epoch_len: E,
        max_validators: 8,
        min_stake: 1_000,
        unbonding_blocks: E,
        slash_bps: 2_000,
        reporter_reward_bps: 1_000,
    };
    let g = genesis_with(&ops, EACH, cfg);
    let vals: Vec<GenesisValidator> = (0..n)
        .map(|i| GenesisValidator {
            operator: s.ops[i].addr(),
            consensus_key: ckey_bytes(i),
            network_addr: Vec::new(),
            stake: STAKE,
        })
        .collect();
    let og = OracleGenesis {
        group_key: group.encode(),
        holders: (0..n).map(ckey_bytes).collect(),
        pairs: vec![],
        max_age_blocks: 20,
        dkg_window: W,
    };
    s.net.state = ChainState::from_genesis_full(MemKv::new(), &g, &vals, Some(&og)).unwrap();
    for (i, sh) in shares.iter().enumerate() {
        s.agents[i].insert_share(0, KeyShare::decode(&sh.encode()).unwrap());
    }
    // epoch 0 has the key from the first block (no genesis session)
    assert!(dkg::session(s.kv(), 0).unwrap().is_none());
    s.run_to(E + 3);
    let k1 = s.epoch_key(1).unwrap();
    assert_eq!(k1.origin, 1);
    assert_eq!(k1.group.public_key, group.public_key);
    assert_eq!(s.session(1).from, Some(0));
}

#[test]
fn the_parallel_executor_runs_the_dkg_to_the_same_state_as_the_sequential_one() {
    let run = |parallel: bool| {
        let mut s = Sim::new(4, 4, W);
        if parallel {
            s.net.ex = Executor::parallel(4);
            s.net.ex.parallel_min_txs = 2;
        }
        s.behave[1] = Behave::BadShareTo(2);
        s.run_to(E + 3);
        (
            s.net.state.root().unwrap(),
            s.epoch_key(0).unwrap().group.public_key,
            s.epoch_key(1).unwrap().group.public_key,
        )
    };
    let (root_a, k0a, k1a) = run(false);
    let (root_b, k0b, k1b) = run(true);
    assert_eq!(root_a, root_b, "state roots differ between executors");
    assert_eq!(k0a, k0b);
    assert_eq!(k1a, k1b);
    assert_eq!(k0a, k1a);
}
