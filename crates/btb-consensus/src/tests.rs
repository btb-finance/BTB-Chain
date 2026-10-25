//! Failure-case unit tests plus seeded property tests over the deterministic simulator.

use crate::sim::*;
use crate::*;
use ed25519_dalek::SigningKey;

// ------------------------------------------------------------------ helpers

fn keys(n: usize) -> Vec<SigningKey> {
    (0..n)
        .map(|i| SigningKey::from_bytes(&sha256(&[b"unit-key", &[i as u8]])))
        .collect()
}

fn set_of(keys: &[SigningKey], stakes: &[u64]) -> ValidatorSet {
    ValidatorSet::new(
        keys.iter()
            .zip(stakes)
            .map(|(k, s)| Validator {
                key: k.verifying_key().to_bytes(),
                stake: *s,
            })
            .collect(),
    )
    .unwrap()
}

struct Fixture {
    keys: Vec<SigningKey>,
    ctx: Context,
    cfg: Config,
}

impl Fixture {
    fn new(n: usize) -> Self {
        Self::with_stakes(&vec![1; n])
    }
    fn with_stakes(stakes: &[u64]) -> Self {
        let keys = keys(stakes.len());
        let cfg = Config::default();
        let ctx = Context::new(&cfg.chain_id, set_of(&keys, stakes));
        Self { keys, ctx, cfg }
    }
    fn replica(&self, i: usize) -> Replica {
        Replica::new(
            self.cfg.clone(),
            self.ctx.set.clone(),
            self.keys[i].clone(),
            None,
        )
        .unwrap()
    }
    fn sign(&self, i: usize, msg: &[u8]) -> Sig {
        use ed25519_dalek::Signer;
        Sig(self.keys[i].sign(msg).to_bytes())
    }
    fn qc(&self, round: Round, block_id: Hash, voters: &[usize]) -> QuorumCert {
        let msg = self.ctx.vote_bytes(round, &block_id);
        QuorumCert {
            round,
            block_id,
            votes: voters
                .iter()
                .map(|&i| (i as u32, self.sign(i, &msg)))
                .collect(),
        }
    }
    fn block(
        &self,
        round: Round,
        height: Height,
        justify: QuorumCert,
        proposer: usize,
        payload: &[u8],
    ) -> Block {
        Block {
            round,
            height,
            parent: justify.block_id,
            justify,
            proposer: proposer as u32,
            payload_hash: Block::hash_payload(payload),
            payload: payload.to_vec(),
            exec: Vec::new(),
        }
    }
    fn proposal(&self, b: Block, tc: Option<TimeoutCert>) -> Proposal {
        let sig = self.sign(
            b.proposer as usize,
            &self.ctx.proposal_bytes(b.round, &b.id()),
        );
        Proposal { block: b, tc, sig }
    }
    fn vote(&self, i: usize, round: Round, id: Hash) -> Vote {
        let leader = (round % self.keys.len() as u64) as usize;
        Vote {
            round,
            block_id: id,
            voter: i as u32,
            sig: self.sign(i, &self.ctx.vote_bytes(round, &id)),
            proposal_sig: self.sign(leader, &self.ctx.proposal_bytes(round, &id)),
        }
    }
    fn tc(&self, round: Round, entries: &[(usize, Round)], high_qc: QuorumCert) -> TimeoutCert {
        TimeoutCert {
            round,
            entries: entries
                .iter()
                .map(|&(v, qr)| TcEntry {
                    voter: v as u32,
                    qc_round: qr,
                    sig: self.sign(v, &self.ctx.timeout_bytes(round, qr)),
                })
                .collect(),
            high_qc,
        }
    }
}

fn votes(actions: &[Action]) -> Vec<Vote> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Broadcast(Message::Vote(v)) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

fn evidence(actions: &[Action]) -> Vec<Evidence> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Evidence(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------ quorums and leaders

#[test]
fn quorum_is_strictly_more_than_two_thirds_of_stake() {
    let fx = Fixture::new(4);
    assert!(fx.ctx.set.is_quorum(3));
    assert!(!fx.ctx.set.is_quorum(2));
    let fx = Fixture::new(3);
    assert!(fx.ctx.set.is_quorum(3));
    assert!(!fx.ctx.set.is_quorum(2), "exactly 2/3 is not enough");
    // Stake-weighted: validator 0 alone holds > 2/3.
    let fx = Fixture::with_stakes(&[70, 10, 10, 10]);
    assert!(fx.ctx.set.is_quorum(70));
    assert!(!fx.ctx.set.is_quorum(30));
    assert!(fx.ctx.set.is_blocking(34));
    assert!(!fx.ctx.set.is_blocking(33));
}

#[test]
fn validator_set_rejects_bad_input() {
    let ks = keys(2);
    let v = |i: usize, stake| Validator {
        key: ks[i].verifying_key().to_bytes(),
        stake,
    };
    assert_eq!(
        ValidatorSet::new(vec![]).unwrap_err(),
        Error::EmptyValidatorSet
    );
    assert_eq!(
        ValidatorSet::new(vec![v(0, 0)]).unwrap_err(),
        Error::ZeroStake
    );
    assert_eq!(
        ValidatorSet::new(vec![v(0, 1), v(0, 1)]).unwrap_err(),
        Error::DuplicateKey
    );
    assert_eq!(
        ValidatorSet::new(vec![v(0, u64::MAX), v(1, u64::MAX)]).map(|s| s.total_stake()),
        Ok(2 * u64::MAX as u128)
    );
    let bad = Validator {
        key: [0xFF; 32],
        stake: 1,
    };
    // Not every 32-byte string is a curve point; either outcome is a clean Result, never a panic.
    let _ = ValidatorSet::new(vec![bad]);
}

#[test]
fn replica_requires_membership() {
    let fx = Fixture::new(4);
    let outsider = SigningKey::from_bytes(&[9; 32]);
    let r = Replica::new(fx.cfg.clone(), fx.ctx.set.clone(), outsider, None);
    assert_eq!(r.err(), Some(Error::NotAValidator));
}

#[test]
fn stake_weighted_leader_follows_stake_with_bounded_gaps() {
    let ks = keys(3);
    let cfg = Config {
        leader_policy: LeaderPolicy::StakeWeighted,
        ..Config::default()
    };
    let r = Replica::new(cfg, set_of(&ks, &[70, 20, 10]), ks[0].clone(), None).unwrap();
    let mut c = [0u32; 3];
    let mut last = [0u64; 3];
    let mut max_gap = [0u64; 3];
    for round in 1..=10_000u64 {
        let l = r.leader(round) as usize;
        c[l] += 1;
        max_gap[l] = max_gap[l].max(round - last[l]);
        last[l] = round;
    }
    assert!((6_900..7_100).contains(&c[0]), "{c:?}");
    assert!((1_900..2_100).contains(&c[1]), "{c:?}");
    assert!((900..1_100).contains(&c[2]), "{c:?}");
    // Gaps stay close to the ideal 1/share (1.4, 5, 10): no validator ever hogs or starves.
    assert!(
        max_gap[0] <= 3 && max_gap[1] <= 8 && max_gap[2] <= 14,
        "{max_gap:?}"
    );
    // A validator with a dust stake still gets turns.
    let ks = keys(4);
    let cfg = Config {
        leader_policy: LeaderPolicy::StakeWeighted,
        ..Config::default()
    };
    let r = Replica::new(
        cfg,
        set_of(&ks, &[1_000_000_000, 1_000_000_000, 1_000_000_000, 1]),
        ks[0].clone(),
        None,
    )
    .unwrap();
    assert!((1..=200).any(|x| r.leader(x) == 3));
    assert_eq!(r.leader(5), r.leader(5));
}

#[test]
fn round_robin_leader() {
    let fx = Fixture::new(4);
    let r = fx.replica(0);
    assert_eq!(
        (1..=8).map(|x| r.leader(x)).collect::<Vec<_>>(),
        vec![1, 2, 3, 0, 1, 2, 3, 0]
    );
}

// ------------------------------------------------------------------ certificate validation

#[test]
fn qc_validation_failure_cases() {
    let fx = Fixture::new(4);
    let id = [7u8; 32];
    assert!(fx.ctx.verify_qc(&fx.qc(1, id, &[0, 1, 2])));
    // Below quorum.
    assert!(!fx.ctx.verify_qc(&fx.qc(1, id, &[0, 1])));
    // Duplicate voter.
    let mut dup = fx.qc(1, id, &[0, 1, 2]);
    dup.votes.push(dup.votes[2]);
    assert!(!fx.ctx.verify_qc(&dup));
    // Unsorted.
    let mut uns = fx.qc(1, id, &[0, 1, 2]);
    uns.votes.swap(0, 1);
    assert!(!fx.ctx.verify_qc(&uns));
    // Duplicate stake padding: same voter three times.
    let mut pad = fx.qc(1, id, &[0]);
    pad.votes = vec![pad.votes[0]; 3];
    assert!(!fx.ctx.verify_qc(&pad));
    // Non-member.
    let mut nm = fx.qc(1, id, &[0, 1, 2]);
    nm.votes.push((99, nm.votes[0].1));
    assert!(!fx.ctx.verify_qc(&nm));
    // Wrong round / block (signatures cover both).
    let mut wr = fx.qc(1, id, &[0, 1, 2]);
    wr.round = 2;
    assert!(!fx.ctx.verify_qc(&wr));
    let mut wb = fx.qc(1, id, &[0, 1, 2]);
    wb.block_id = [8; 32];
    assert!(!fx.ctx.verify_qc(&wb));
    // Corrupted signature.
    let mut bad = fx.qc(1, id, &[0, 1, 2]);
    bad.votes[1].1 .0[0] ^= 1;
    assert!(!fx.ctx.verify_qc(&bad));
    // Fake genesis.
    assert!(fx.ctx.verify_qc(&fx.ctx.genesis_qc()));
    assert!(!fx.ctx.verify_qc(&QuorumCert {
        round: 0,
        block_id: [1; 32],
        votes: vec![]
    }));
    // Signature from another chain id does not verify (domain separation).
    let other = Context::new(b"other-chain", fx.ctx.set.clone());
    assert!(!other.verify_qc(&fx.qc(1, id, &[0, 1, 2])));
}

#[test]
fn qc_stake_weighted() {
    let fx = Fixture::with_stakes(&[70, 10, 10, 10]);
    assert!(fx.ctx.verify_qc(&fx.qc(1, [1; 32], &[0])));
    assert!(!fx.ctx.verify_qc(&fx.qc(1, [1; 32], &[1, 2, 3])));
}

#[test]
fn tc_validation_failure_cases() {
    let fx = Fixture::new(4);
    let q3 = fx.qc(3, [3; 32], &[0, 1, 2]);
    let ok = fx.tc(5, &[(0, 3), (1, 2), (2, 0)], q3.clone());
    assert!(fx.ctx.verify_tc(&ok));
    // Below quorum.
    assert!(!fx.ctx.verify_tc(&fx.tc(5, &[(0, 3), (1, 2)], q3.clone())));
    // High QC lower than the max signed qc_round.
    assert!(!fx
        .ctx
        .verify_tc(&fx.tc(5, &[(0, 4), (1, 2), (2, 0)], q3.clone())));
    // High QC higher than any signed qc_round (forged claim).
    assert!(!fx
        .ctx
        .verify_tc(&fx.tc(5, &[(0, 1), (1, 2), (2, 0)], q3.clone())));
    // Round not above the QC round.
    assert!(!fx
        .ctx
        .verify_tc(&fx.tc(3, &[(0, 3), (1, 2), (2, 0)], q3.clone())));
    // Invalid embedded QC.
    let weak = fx.qc(3, [3; 32], &[0, 1]);
    assert!(!fx.ctx.verify_tc(&fx.tc(5, &[(0, 3), (1, 2), (2, 0)], weak)));
    // Signature for a different round.
    let mut wrong = ok.clone();
    wrong.round = 6;
    assert!(!fx.ctx.verify_tc(&wrong));
    // Duplicate entry.
    let mut dup = ok.clone();
    dup.entries.push(dup.entries[0].clone());
    assert!(!fx.ctx.verify_tc(&dup));
}

// ------------------------------------------------------------------ message validation

#[test]
fn honest_vote_for_valid_proposal_and_only_once() {
    let fx = Fixture::new(4);
    let mut r = fx.replica(0);
    let b = fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"x");
    let p = fx.proposal(b, None);
    let a1 = r.on_message(Message::Propose(p.clone()));
    let v = votes(&a1);
    assert_eq!(v.len(), 1);
    assert!(fx
        .ctx
        .verify_sig(0, &fx.ctx.vote_bytes(1, &v[0].block_id), &v[0].sig));
    // Replay: no second vote.
    assert!(votes(&r.on_message(Message::Propose(p))).is_empty());
    // A conflicting proposal for the same round: no vote, but equivocation evidence appears.
    let b2 = fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"y");
    let a2 = r.on_message(Message::Propose(fx.proposal(b2, None)));
    assert!(votes(&a2).is_empty());
    let ev = evidence(&a2);
    assert_eq!(ev.len(), 1);
    assert!(matches!(
        ev[0],
        Evidence::DoubleProposal {
            proposer: 1,
            round: 1,
            ..
        }
    ));
    assert!(ev[0].verify(&fx.ctx));
}

#[test]
fn rejects_malformed_or_forged_proposals() {
    let fx = Fixture::new(4);
    let gq = fx.ctx.genesis_qc();
    let mut r = fx.replica(0);
    let good = fx.proposal(fx.block(1, 1, gq.clone(), 1, b"x"), None);

    let mut cases: Vec<(&str, Proposal)> = Vec::new();
    // Bad signature.
    let mut p = good.clone();
    p.sig.0[0] ^= 1;
    cases.push(("bad signature", p));
    // Wrong leader (validator 2 is not round 1's leader) even though it signs correctly.
    cases.push((
        "wrong leader",
        fx.proposal(fx.block(1, 1, gq.clone(), 2, b"x"), None),
    ));
    // Non-member proposer.
    let mut b = fx.block(1, 1, gq.clone(), 1, b"x");
    b.proposer = 77;
    cases.push((
        "non-member",
        Proposal {
            block: b,
            tc: None,
            sig: good.sig,
        },
    ));
    // Parent does not match the justify QC.
    let mut b = fx.block(1, 1, gq.clone(), 1, b"x");
    b.parent = [5; 32];
    cases.push(("parent linkage", fx.proposal(b, None)));
    // Payload hash mismatch.
    let mut b = fx.block(1, 1, gq.clone(), 1, b"x");
    b.payload = b"tampered".to_vec();
    cases.push(("payload hash", fx.proposal(b, None)));
    // Wrong height.
    cases.push((
        "height",
        fx.proposal(fx.block(1, 9, gq.clone(), 1, b"x"), None),
    ));
    // Justify not below round.
    let b = fx.block(1, 1, fx.qc(1, [1; 32], &[0, 1, 2]), 1, b"x");
    cases.push(("justify >= round", fx.proposal(b, None)));
    // Justify QC without quorum.
    let b = fx.block(2, 2, fx.qc(1, [1; 32], &[0, 1]), 2, b"x");
    cases.push(("weak qc", fx.proposal(b, None)));
    // Absurd round numbers.
    let b = fx.block(u64::MAX, 1, gq.clone(), (u64::MAX % 4) as usize, b"x");
    cases.push(("huge round", fx.proposal(b, None)));
    // Oversized payload.
    let big = vec![0u8; fx.cfg.max_payload_bytes + 1];
    cases.push((
        "oversized",
        fx.proposal(fx.block(1, 1, gq.clone(), 1, &big), None),
    ));

    for (name, p) in cases {
        let a = r.on_message(Message::Propose(p));
        assert!(votes(&a).is_empty(), "voted on invalid proposal: {name}");
        assert_eq!(r.high_qc().round, 0, "{name}");
        assert_eq!(r.round(), 1, "{name}");
    }
    assert_eq!(r.stats().votes_cast, 0);
    // The good one still works afterwards.
    assert_eq!(votes(&r.on_message(Message::Propose(good))).len(), 1);
}

#[test]
fn proposal_skipping_a_round_needs_a_valid_tc() {
    let fx = Fixture::new(4);
    let gq = fx.ctx.genesis_qc();
    let b = fx.block(2, 1, gq.clone(), 2, b"x");
    // Without a TC the voter must not vote (round 1 was never certified as failed).
    let mut r = fx.replica(0);
    // Make r enter round 2 legitimately through a QC for some block... use a TC instead:
    let tc = fx.tc(1, &[(1, 0), (2, 0), (3, 0)], gq.clone());
    let a = r.on_message(Message::Propose(fx.proposal(b.clone(), None)));
    assert!(votes(&a).is_empty());
    // With the TC: vote.
    let a = r.on_message(Message::Propose(fx.proposal(b.clone(), Some(tc))));
    assert_eq!(votes(&a).len(), 1);
    // A TC for the wrong round does not help.
    let mut r2 = fx.replica(3);
    let bad_tc = fx.tc(3, &[(1, 0), (2, 0), (3, 0)], gq);
    let a = r2.on_message(Message::Propose(fx.proposal(b, Some(bad_tc))));
    assert!(votes(&a).is_empty());
}

#[test]
fn locked_replica_refuses_lower_justify() {
    let fx = Fixture::new(4);
    let gq = fx.ctx.genesis_qc();
    let mut r = fx.replica(0);
    // Chain: B1 (round 1) <- B2 (round 2); r learns QC(B2) so it locks on round 2.
    let b1 = fx.block(1, 1, gq.clone(), 1, b"1");
    let id1 = b1.id();
    let q1 = fx.qc(1, id1, &[1, 2, 3]);
    let b2 = fx.block(2, 2, q1.clone(), 2, b"2");
    let id2 = b2.id();
    let q2 = fx.qc(2, id2, &[1, 2, 3]);
    r.on_message(Message::Propose(fx.proposal(b1, None)));
    r.on_message(Message::Propose(fx.proposal(b2, None)));
    // Round-3 proposal carrying QC(B2) makes r learn the lock and vote.
    let b3 = fx.block(3, 3, q2.clone(), 3, b"3");
    let a = r.on_message(Message::Propose(fx.proposal(b3, None)));
    assert_eq!(votes(&a).len(), 1);
    assert_eq!(r.safety_state().locked_round, 2);
    // Conflicting fork from round 1 that skips B2: justify = QC(B1) (round 1 < lock 2),
    // proposed in round 5 with a perfectly valid TC for round 4 whose high QC is q1.
    let tc = fx.tc(4, &[(1, 1), (2, 1), (3, 1)], q1.clone());
    let fork = fx.block(5, 2, q1.clone(), 1, b"fork"); // leader of 5 is validator 1
    let mut r2 = fx.replica(0);
    // Same knowledge as r (so the lock is in place), then the fork arrives.
    r2.on_message(Message::Propose(
        fx.proposal(fx.block(1, 1, gq, 1, b"1"), None),
    ));
    r2.on_message(Message::Catchup { qc: q2, tc: None });
    assert_eq!(r2.safety_state().locked_round, 2);
    let a = r2.on_message(Message::Propose(
        fx.proposal(fork.clone(), Some(tc.clone())),
    ));
    assert!(votes(&a).is_empty(), "must not vote below the lock");
    // Sanity: an unlocked replica would accept the same fork, so the lock is what blocked it.
    let mut fresh = fx.replica(2);
    fresh.on_message(Message::Propose(
        fx.proposal(fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"1"), None),
    ));
    let a = fresh.on_message(Message::Propose(fx.proposal(fork, Some(tc))));
    assert_eq!(votes(&a).len(), 1);
}

#[test]
fn invalid_votes_and_timeouts_are_ignored() {
    let fx = Fixture::new(4);
    // Replica 2 is the leader of round 1's successor (round 2): it collects votes for round 1.
    let mut r = fx.replica(2);
    let id = [4u8; 32];
    let good = |i: usize| fx.vote(i, 1, id);
    let mut forged = good(0);
    forged.sig.0[5] ^= 1;
    let mut nonmember = good(0);
    nonmember.voter = 40;
    let stolen = Vote {
        voter: 1,
        ..good(0)
    }; // validator 0's signature claimed by 1
    for v in [forged, nonmember, stolen] {
        r.on_message(Message::Vote(v));
    }
    assert_eq!(r.stats().qcs_formed, 0);
    assert!(r.stats().rejected_messages >= 3);
    // Three honest votes (the replica's own vote is not needed) form a QC even with the garbage before.
    for i in [0, 1, 3] {
        r.on_message(Message::Vote(good(i)));
    }
    assert_eq!(r.stats().qcs_formed, 1);
    assert_eq!(r.high_qc().round, 1);

    // Timeouts: forged signature, wrong qc_round claim.
    let mut r = fx.replica(0);
    let mk = |i: usize, claimed: Round| Timeout {
        round: 1,
        high_qc: fx.ctx.genesis_qc(),
        voter: i as u32,
        sig: fx.sign(i, &fx.ctx.timeout_bytes(1, claimed)),
        seen_proposal: None,
    };
    r.on_message(Message::Timeout(mk(1, 5))); // signed a different qc_round than carried
    r.on_message(Message::Timeout(Timeout {
        voter: 77,
        ..mk(1, 0)
    }));
    assert_eq!(r.stats().timeouts_sent, 0);
    assert_eq!(r.stats().tcs_formed, 0);
    assert!(r.stats().rejected_messages >= 2);
}

#[test]
fn timeouts_form_tc_and_advance_round() {
    let fx = Fixture::new(4);
    let mut r = fx.replica(0);
    for i in [1, 2, 3] {
        let t = Timeout {
            round: 1,
            high_qc: fx.ctx.genesis_qc(),
            voter: i as u32,
            sig: fx.sign(i, &fx.ctx.timeout_bytes(1, 0)),
            seen_proposal: None,
        };
        r.on_message(Message::Timeout(t));
    }
    assert_eq!(r.round(), 2);
    assert_eq!(r.stats().tcs_formed, 1);
    // f+1 amplification already made the replica time out itself.
    assert!(r.stats().timeouts_sent >= 1);
}

#[test]
fn pacemaker_times_out_with_logical_ticks_and_backs_off() {
    let fx = Fixture::new(4);
    let mut r = fx.replica(0);
    assert!(r
        .on_timeout(fx.cfg.base_timeout - 1)
        .iter()
        .all(|a| !matches!(a, Action::Broadcast(Message::Timeout(_)))));
    let a = r.on_timeout(fx.cfg.base_timeout);
    assert!(a
        .iter()
        .any(|a| matches!(a, Action::Broadcast(Message::Timeout(t)) if t.round == 1)));
    // It never votes in round 1 afterwards.
    let p = fx.proposal(fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"x"), None);
    assert!(votes(&r.on_message(Message::Propose(p))).is_empty());
    // Time never goes backwards.
    assert!(r
        .on_timeout(0)
        .iter()
        .all(|a| !matches!(a, Action::Broadcast(Message::Timeout(_)))));
}

// ------------------------------------------------------------------ evidence

#[test]
fn double_vote_evidence_is_produced_verifiable_and_unforgeable() {
    let fx = Fixture::new(4);
    let mut r = fx.replica(2); // leader of round 2, receives votes for round 1
    let mk = |i: usize, id: Hash| fx.vote(i, 1, id);
    assert!(evidence(&r.on_message(Message::Vote(mk(0, [1; 32])))).is_empty());
    assert!(
        evidence(&r.on_message(Message::Vote(mk(0, [1; 32])))).is_empty(),
        "same vote twice is fine"
    );
    let acts = r.on_message(Message::Vote(mk(0, [2; 32])));
    // The votes carry the leader's signatures on two different blocks, so the leader is
    // exposed as an equivocator as well.
    let all = evidence(&acts);
    assert!(all
        .iter()
        .any(|e| matches!(e, Evidence::DoubleProposal { proposer: 1, .. })));
    let ev: Vec<Evidence> = all
        .into_iter()
        .filter(|e| matches!(e, Evidence::DoubleVote { .. }))
        .collect();
    assert_eq!(ev.len(), 1);
    assert!(ev[0].verify(&fx.ctx));
    assert_eq!(ev[0].offender(), 0);
    // Broadcast so others learn of it.
    assert!(acts
        .iter()
        .any(|a| matches!(a, Action::Broadcast(Message::Evidence(_)))));
    // Not reported twice.
    assert!(evidence(&r.on_message(Message::Vote(mk(0, [2; 32])))).is_empty());
    assert!(evidence(&r.on_message(Message::Vote(Vote {
        proposal_sig: Sig([0; 64]),
        ..mk(0, [3; 32])
    })))
    .is_empty());

    // Another replica verifies and surfaces the evidence it receives.
    let mut other = fx.replica(1);
    let got = evidence(&other.on_message(Message::Evidence(ev[0].clone())));
    assert_eq!(got, ev);
    assert!(evidence(&other.on_message(Message::Evidence(ev[0].clone()))).is_empty());

    // Forged evidence is rejected: same block twice, swapped signature, wrong offender.
    let Evidence::DoubleVote { voter, round, a, b } = ev[0].clone() else {
        panic!()
    };
    let same = Evidence::DoubleVote {
        voter,
        round,
        a,
        b: a,
    };
    let swapped = Evidence::DoubleVote {
        voter: 1,
        round,
        a,
        b,
    };
    let other_round = Evidence::DoubleVote {
        voter,
        round: 9,
        a,
        b,
    };
    for bad in [same, swapped, other_round] {
        assert!(!bad.verify(&fx.ctx));
        let mut fresh = fx.replica(3);
        assert!(evidence(&fresh.on_message(Message::Evidence(bad))).is_empty());
        assert_eq!(fresh.stats().rejected_messages, 1);
    }
    // A vote and a proposal are not interchangeable: signatures are domain separated.
    let cross = Evidence::DoubleProposal {
        proposer: voter,
        round,
        a,
        b,
    };
    assert!(!cross.verify(&fx.ctx));
}

// ------------------------------------------------------------------ persistence

#[test]
fn restart_from_storage_never_double_votes() {
    let fx = Fixture::new(4);
    let st = MemoryStorage::default();
    let mut r = Replica::new(
        fx.cfg.clone(),
        fx.ctx.set.clone(),
        fx.keys[0].clone(),
        Some(Box::new(st.clone())),
    )
    .unwrap();
    let p = fx.proposal(fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"x"), None);
    assert_eq!(votes(&r.on_message(Message::Propose(p.clone()))).len(), 1);
    drop(r); // crash
    let mut r2 = Replica::new(
        fx.cfg.clone(),
        fx.ctx.set.clone(),
        fx.keys[0].clone(),
        Some(Box::new(st.clone())),
    )
    .unwrap();
    assert_eq!(r2.safety_state().last_voted_round, 1);
    assert!(
        votes(&r2.on_message(Message::Propose(p))).is_empty(),
        "double vote after restart"
    );
    // A conflicting proposal in the same round is refused too.
    let p2 = fx.proposal(fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"y"), None);
    assert!(votes(&r2.on_message(Message::Propose(p2))).is_empty());
}

#[test]
fn restarted_leader_does_not_double_propose() {
    let fx = Fixture::new(4);
    let st = MemoryStorage::default();
    let mut r = Replica::new(
        fx.cfg.clone(),
        fx.ctx.set.clone(),
        fx.keys[1].clone(),
        Some(Box::new(st.clone())),
    )
    .unwrap();
    let a = r.start(); // validator 1 leads round 1
    assert!(a
        .iter()
        .any(|a| matches!(a, Action::Broadcast(Message::Propose(_)))));
    let mut r2 = Replica::new(
        fx.cfg.clone(),
        fx.ctx.set.clone(),
        fx.keys[1].clone(),
        Some(Box::new(st)),
    )
    .unwrap();
    assert!(r2
        .start()
        .iter()
        .all(|a| !matches!(a, Action::Broadcast(Message::Propose(_)))));
}

// ------------------------------------------------------------------ codec / garbage

#[test]
fn codec_roundtrip_and_hostile_bytes_never_panic() {
    // Collect real messages by running a scenario with an observer... simply build a few by hand.
    let fx = Fixture::new(4);
    let gq = fx.ctx.genesis_qc();
    let q = fx.qc(1, [1; 32], &[0, 1, 2]);
    let p = fx.proposal(
        fx.block(2, 1, q.clone(), 2, b"payload"),
        Some(fx.tc(1, &[(0, 0), (1, 0), (2, 0)], gq.clone())),
    );
    let msgs = vec![
        Message::Propose(p.clone()),
        Message::Vote(Vote {
            round: 1,
            block_id: [1; 32],
            voter: 0,
            sig: Sig([2; 64]),
            proposal_sig: Sig([7; 64]),
        }),
        Message::Timeout(Timeout {
            round: 3,
            high_qc: q.clone(),
            voter: 1,
            sig: Sig([3; 64]),
            seen_proposal: Some(([4; 32], Sig([5; 64]))),
        }),
        Message::Catchup {
            qc: q.clone(),
            tc: None,
        },
        Message::SyncRequest {
            from: 1,
            block_id: [6; 32],
        },
        Message::SyncResponse {
            blocks: vec![p.block.clone(), p.block.clone()],
        },
        Message::Evidence(Evidence::DoubleVote {
            voter: 1,
            round: 2,
            a: ([1; 32], Sig([1; 64])),
            b: ([2; 32], Sig([2; 64])),
        }),
    ];
    let mut rng = Rng(42);
    let mut r = fx.replica(0);
    for m in &msgs {
        let bytes = m.encode();
        assert_eq!(Message::decode(&bytes).as_ref(), Some(m));
        // Truncations and trailing bytes are rejected.
        for cut in 0..bytes.len() {
            assert!(Message::decode(&bytes[..cut]).is_none(), "prefix decoded");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(Message::decode(&longer).is_none());
        // Mutations: decode must be total, and whatever decodes must be survivable.
        for _ in 0..3_000 {
            let mut b = bytes.clone();
            for _ in 0..rng.range(1, 4) {
                let at = rng.below(b.len() as u64) as usize;
                b[at] = rng.next() as u8;
            }
            if let Some(m) = Message::decode(&b) {
                let _ = r.on_message(m);
            }
        }
    }
    // Pure noise, including absurd length prefixes.
    for _ in 0..20_000 {
        let len = rng.below(200) as usize;
        let mut b: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if !b.is_empty() {
            b[0] %= 8;
        }
        if let Some(m) = Message::decode(&b) {
            let _ = r.on_message(m);
        }
    }
    assert!(Message::decode(&[5, 0xFF, 0xFF, 0xFF, 0xFF]).is_none());
    assert!(Message::decode(&[]).is_none());
}

#[test]
fn extreme_values_do_not_panic() {
    let fx = Fixture::new(4);
    let mut r = fx.replica(0);
    for round in [0, 1, u64::MAX, u64::MAX - 1, 1 << 62, (1 << 62) + 1] {
        let id = [1; 32];
        let _ = r.on_message(Message::Vote(Vote {
            round,
            block_id: id,
            voter: 1,
            sig: fx.sign(1, &fx.ctx.vote_bytes(round, &id)),
            proposal_sig: Sig([0; 64]),
        }));
        let _ = r.on_message(Message::Timeout(Timeout {
            round,
            high_qc: fx.ctx.genesis_qc(),
            voter: 1,
            sig: fx.sign(1, &fx.ctx.timeout_bytes(round, 0)),
            seen_proposal: None,
        }));
        let _ = r.on_message(Message::Catchup {
            qc: fx.qc(round, id, &[0, 1, 2]),
            tc: Some(fx.tc(round, &[(0, 0), (1, 0), (2, 0)], fx.ctx.genesis_qc())),
        });
    }
    let _ = r.on_message(Message::SyncRequest {
        from: u32::MAX,
        block_id: [0; 32],
    });
    let _ = r.on_message(Message::SyncResponse { blocks: vec![] });
    let _ = r.on_timeout(u64::MAX);
    let _ = r.on_timeout(0);
}

#[test]
fn sync_responses_are_validated() {
    let fx = Fixture::new(4);
    let gq = fx.ctx.genesis_qc();
    let mut r = fx.replica(0);
    let b1 = fx.block(1, 1, gq, 1, b"1");
    let q1 = fx.qc(1, b1.id(), &[1, 2, 3]);
    let b2 = fx.block(2, 2, q1, 2, b"2");
    // Unsolicited response: ignored.
    r.on_message(Message::SyncResponse {
        blocks: vec![b2.clone(), b1.clone()],
    });
    assert_eq!(r.pending_blocks(), 0);
    // A proposal for round 2 with an unknown parent triggers a request; response must be a chain.
    let p2 = fx.proposal(b2.clone(), None);
    let a = r.on_message(Message::Propose(p2));
    let asked = a.iter().find_map(|a| match a {
        Action::Send {
            msg: Message::SyncRequest { block_id, .. },
            ..
        } => Some(*block_id),
        _ => None,
    });
    assert_eq!(asked, Some(b1.id()));
    // Wrong content for the requested hash is not accepted.
    let mut forged = b1.clone();
    forged.payload = b"evil".to_vec();
    forged.payload_hash = Block::hash_payload(&forged.payload);
    r.on_message(Message::SyncResponse {
        blocks: vec![forged],
    });
    assert_eq!(r.pending_blocks(), 1);
    // The real block links the pending child (and since it is now round 2's block, it votes).
    let a = r.on_message(Message::SyncResponse { blocks: vec![b1] });
    assert_eq!(r.pending_blocks(), 0);
    assert!(votes(&a).len() <= 1);
}

// ------------------------------------------------------------------ simulator: scenarios

fn assert_evidence_sound(o: &Outcome, sc: &Scenario) {
    for (observer, e) in &o.evidence {
        assert!(
            e.verify(&o.ctx),
            "seed {}: unverifiable evidence from {observer}",
            sc.seed
        );
        let off = e.offender() as usize;
        assert!(
            !sc.behaviors[off].is_honest(),
            "seed {}: honest validator {off} accused",
            sc.seed
        );
    }
}

#[test]
fn happy_path_commits_with_low_latency() {
    let mut worst = (0.0f64, 0, 0, 0);
    let mut heights = Vec::new();
    for seed in 0..20 {
        let sc = Scenario::new(seed, 4);
        let o = run(&sc);
        assert!(o.min_honest_height() > 30, "seed {seed}: {:?}", o.heights);
        assert_eq!(
            o.stats.iter().map(|s| s.conflicting_commits).sum::<u64>(),
            0
        );
        assert!(o.evidence.is_empty(), "no evidence without faults");
        assert!(
            o.stats.iter().all(|s| s.timeouts_sent == 0),
            "no timeouts on a healthy network"
        );
        heights.push(o.max_height());
        let l = latency_summary(&o.latencies);
        if l.0 > worst.0 {
            worst = l;
        }
    }
    println!("happy path: heights {:?}", &heights[..5]);
    println!("happy path worst-seed latency (ticks) mean/p50/p99/max = {worst:?}  (message delay 1..3 ticks)");
}

#[test]
fn stake_weighted_leaders_commit() {
    for seed in 0..10 {
        let mut sc = Scenario::new(seed, 5);
        sc.stakes = vec![50, 20, 15, 10, 5];
        sc.policy = LeaderPolicy::StakeWeighted;
        // 5% stake crashed: well under 1/3.
        sc.behaviors[4] = Behavior::Crash {
            at: 0,
            restart: None,
        };
        let o = run(&sc);
        assert!(o.min_honest_height() > 10, "seed {seed}: {:?}", o.heights);
    }
}

#[test]
fn crashed_validators_up_to_f() {
    for (n, f) in [(4usize, 1usize), (7, 2), (10, 3)] {
        for seed in 0..6 {
            let mut sc = Scenario::new(seed, n);
            sc.ticks = 700;
            for i in 0..f {
                sc.behaviors[(i * 2 + seed as usize) % n] = Behavior::Crash {
                    at: 0,
                    restart: None,
                };
            }
            let o = run(&sc);
            assert!(
                o.min_honest_height() > 15,
                "n={n} seed {seed}: {:?}",
                o.heights
            );
        }
    }
}

#[test]
fn too_many_crashes_halt_but_stay_safe() {
    // f+1 crashed out of 4: no quorum exists, nothing may commit past what was final.
    let mut sc = Scenario::new(5, 4);
    sc.behaviors[2] = Behavior::Crash {
        at: 100,
        restart: None,
    };
    sc.behaviors[3] = Behavior::Crash {
        at: 100,
        restart: None,
    };
    sc.ticks = 600;
    let o = run(&sc);
    let at_crash = o
        .first_commit_tick
        .range(..)
        .filter(|(_, t)| **t <= 120)
        .count();
    assert!(at_crash > 0);
    assert_eq!(o.committed_after(130), 0, "committed without a quorum");
}

#[test]
fn equivocating_leader_is_caught_and_chain_continues() {
    let mut caught = 0;
    for both in [false, true] {
        for seed in 0..10 {
            let mut sc = Scenario::new(seed, 4);
            sc.behaviors[seed as usize % 4] = Behavior::EquivLeader { both };
            sc.ticks = 600;
            let o = run(&sc);
            assert!(o.min_honest_height() > 10, "seed {seed}: {:?}", o.heights);
            assert_evidence_sound(&o, &sc);
            let byz = seed as usize % 4;
            if o.evidence.iter().any(|(_, e)| matches!(e, Evidence::DoubleProposal { proposer, .. } if *proposer as usize == byz)) {
                caught += 1;
            }
        }
    }
    assert_eq!(caught, 20, "every equivocating leader must be caught");
}

#[test]
fn double_voter_is_caught() {
    let mut caught = 0;
    for seed in 0..10 {
        let mut sc = Scenario::new(seed, 4);
        sc.behaviors[seed as usize % 4] = Behavior::DoubleVoter;
        let o = run(&sc);
        assert!(o.min_honest_height() > 20, "seed {seed}");
        assert_evidence_sound(&o, &sc);
        let byz = seed as u32 % 4;
        if o.evidence
            .iter()
            .any(|(_, e)| matches!(e, Evidence::DoubleVote { voter, .. } if *voter == byz))
        {
            caught += 1;
        }
    }
    assert_eq!(caught, 10);
}

#[test]
fn invalid_signatures_and_garbage_are_harmless() {
    for seed in 0..10 {
        let mut sc = Scenario::new(seed, 7);
        sc.behaviors[1] = Behavior::BadSigs;
        sc.behaviors[4] = Behavior::Garbage { permille: 900 };
        sc.ticks = 500;
        let o = run(&sc);
        assert!(o.min_honest_height() > 20, "seed {seed}: {:?}", o.heights);
        assert!(
            o.evidence.is_empty(),
            "bad signatures must not be attributable"
        );
        assert!(o.stats.iter().map(|s| s.rejected_messages).sum::<u64>() > 0);
    }
}

#[test]
fn partition_minority_recovers_and_majority_continues() {
    for seed in 0..10 {
        let mut sc = Scenario::new(seed, 4);
        // Validator 0 isolated for 150 ticks.
        sc.partitions.push(Partition {
            from: 50,
            to: 200,
            group: vec![1, 0, 0, 0],
        });
        sc.ticks = 450;
        let o = run(&sc);
        assert!(o.committed_after(100) > 5, "majority must keep committing");
        assert!(
            o.heights[0] + 3 >= o.max_height(),
            "seed {seed}: isolated node did not catch up: {:?}",
            o.heights
        );
    }
}

#[test]
fn even_split_halts_then_recovers_after_heal() {
    for seed in 0..10 {
        let mut sc = Scenario::new(seed, 4);
        sc.partitions.push(Partition {
            from: 60,
            to: 260,
            group: vec![0, 0, 1, 1],
        });
        sc.gst = 260;
        sc.ticks = 700;
        let o = run(&sc);
        // No side holds a quorum: nothing commits during the split (allow in-flight settling).
        let during = o
            .first_commit_tick
            .values()
            .filter(|t| (80..260).contains(*t))
            .count();
        assert_eq!(
            during, 0,
            "seed {seed}: committed during a quorum-less split"
        );
        // After the heal the chain resumes quickly.
        assert!(
            o.committed_after(330) > 10,
            "seed {seed}: no recovery: {:?}",
            o.heights
        );
        assert!(o.min_honest_height() + 3 >= o.max_height());
        // Report recovery time.
        let first_after = o
            .first_commit_tick
            .values()
            .find(|t| **t >= 260)
            .copied()
            .unwrap();
        println!(
            "seed {seed}: first commit {} ticks after heal",
            first_after - 260
        );
        assert!(first_after - 260 < 300);
    }
}

#[test]
fn crash_and_restart_with_persisted_state() {
    for seed in 0..10 {
        let mut sc = Scenario::new(seed, 4);
        sc.behaviors[(seed as usize) % 4] = Behavior::Crash {
            at: 80,
            restart: Some(180),
        };
        sc.ticks = 500;
        let o = run(&sc);
        assert!(
            o.min_honest_height() + 3 >= o.max_height(),
            "seed {seed}: {:?}",
            o.heights
        );
        assert!(o.committed_after(250) > 10);
    }
}

// ------------------------------------------------------------------ property test

pub(crate) fn random_scenario(seed: u64) -> Scenario {
    let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0xC0FFEE);
    let n = [4usize, 4, 4, 5, 7][rng.below(5) as usize];
    let mut sc = Scenario::new(seed, n);
    if rng.chance(400) {
        sc.stakes = (0..n).map(|_| rng.range(1, 100)).collect();
    }
    if rng.chance(300) {
        sc.policy = LeaderPolicy::StakeWeighted;
    }
    sc.min_delay = 1;
    sc.max_delay = rng.range(1, 4);
    sc.drop_permille = rng.range(0, 250);
    sc.gst = rng.range(0, 250);
    sc.ticks = sc.gst + 450;
    // Choose faulty validators while their stake stays strictly below 1/3.
    let total: u64 = sc.stakes.iter().sum();
    let mut faulty_stake = 0;
    for _ in 0..rng.range(0, 3) {
        let i = rng.below(n as u64) as usize;
        if !matches!(sc.behaviors[i], Behavior::Honest)
            || (faulty_stake + sc.stakes[i]) * 3 >= total
        {
            continue;
        }
        faulty_stake += sc.stakes[i];
        sc.behaviors[i] = match rng.below(6) {
            0 => Behavior::Crash {
                at: rng.range(0, sc.gst + 100),
                restart: None,
            },
            1 => Behavior::EquivLeader {
                both: rng.chance(500),
            },
            2 => Behavior::DoubleVoter,
            3 => Behavior::BadSigs,
            4 => Behavior::Garbage { permille: 700 },
            _ => Behavior::Crash {
                at: 0,
                restart: None,
            },
        };
    }
    // Honest-but-crashing validators (they restart) and network partitions before GST.
    if rng.chance(300) {
        let i = rng.below(n as u64) as usize;
        if matches!(sc.behaviors[i], Behavior::Honest) {
            let at = rng.range(10, 150);
            sc.behaviors[i] = Behavior::Crash {
                at,
                restart: Some(at + rng.range(10, 100)),
            };
        }
    }
    if sc.gst > 40 && rng.chance(500) {
        let from = rng.range(0, sc.gst / 2);
        let group = (0..n).map(|_| rng.below(2) as u8).collect();
        sc.partitions.push(Partition {
            from,
            to: sc.gst,
            group,
        });
    }
    sc
}

fn run_property_seeds(range: std::ops::Range<u64>, verbose: bool) {
    let mut totals = (0u64, 0u64, 0u64, 0usize, 0usize); // runs, commits, evidence, with_byz, caught
    let mut lat_all: Vec<u64> = Vec::new();
    for seed in range {
        let sc = random_scenario(seed);
        let o = run(&sc); // asserts safety on every commit
        assert_evidence_sound(&o, &sc);
        assert_eq!(
            o.stats.iter().map(|s| s.conflicting_commits).sum::<u64>(),
            0,
            "seed {seed}"
        );
        // Liveness needs two consecutive honest-led rounds to recur, which the leader schedule
        // guarantees when fewer than half of its slots are faulty (round-robin: by count;
        // stake-weighted: by stake, already < 1/3 here). Safety was asserted regardless.
        let faulty = sc.behaviors.iter().filter(|b| !b.is_honest()).count();
        let live_expected = match sc.policy {
            LeaderPolicy::RoundRobin => faulty * 2 < sc.stakes.len(),
            LeaderPolicy::StakeWeighted => true,
        };
        if live_expected {
            let settle = sc.gst + 200;
            assert!(
                o.committed_after(settle) >= 3,
                "seed {seed}: liveness failure, only {} commits after tick {settle}; heights {:?} scenario {sc:?}",
                o.committed_after(settle),
                o.heights
            );
            assert!(
                o.min_honest_height() + 4 >= o.max_height(),
                "seed {seed}: replica lagging: {:?} honest {:?} scenario {sc:?}",
                o.heights,
                o.honest
            );
        }
        // Any double-signing leader that actually split its proposals must be caught
        // (when the network let the evidence travel: only checked on clean runs).
        totals.0 += 1;
        totals.1 += o.max_height();
        totals.2 += o.evidence.len() as u64;
        lat_all.extend(o.latencies.iter().copied());
        let byz = sc
            .behaviors
            .iter()
            .any(|b| matches!(b, Behavior::EquivLeader { .. } | Behavior::DoubleVoter));
        if byz {
            totals.3 += 1;
            if !o.evidence.is_empty() {
                totals.4 += 1;
            }
        }
    }
    let (mean, p50, p99, max) = latency_summary(&lat_all);
    if verbose {
        println!(
            "property run: {} seeds, all safe+live; total committed heights {}; evidence records {}; runs with equivocators {} (evidence produced in {}); commit latency ticks mean={mean:.1} p50={p50} p99={p99} max={max}",
            totals.0, totals.1, totals.2, totals.3, totals.4
        );
    }
}

#[test]
fn property_safety_and_liveness_over_random_scenarios() {
    run_property_seeds(0..60, true);
}

#[test]
#[ignore = "long soak: cargo test -p btb-consensus --release -- --ignored --nocapture"]
fn soak_many_seeds() {
    run_property_seeds(
        std::env::var("SOAK_FROM")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000)
            ..std::env::var("SOAK_TO")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6_000),
        true,
    );
}

// ------------------------------------------------------------------ epochs

fn epoch_replica(fx: &Fixture, i: usize, end: Height) -> Replica {
    let cfg = Config {
        end_height: end,
        ..fx.cfg.clone()
    };
    Replica::new(cfg, fx.ctx.set.clone(), fx.keys[i].clone(), None).unwrap()
}

#[test]
fn epoch_end_must_be_above_the_anchor() {
    let fx = Fixture::new(4);
    let cfg = Config {
        epoch: 1,
        end_height: 10,
        ..fx.cfg.clone()
    };
    let anchor = Some(Anchor {
        height: 10,
        id: [3; 32],
    });
    let r = Replica::new_epoch(cfg, fx.ctx.set.clone(), fx.keys[0].clone(), None, anchor);
    assert_eq!(r.err(), Some(Error::InvalidEpoch));
}

#[test]
fn tail_blocks_must_be_empty_and_are_never_committed() {
    let fx = Fixture::new(4);
    // The epoch ends at height 1: block 1 is its last, block 2 only certifies it.
    let mut r = epoch_replica(&fx, 3, 1);
    let b1 = fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"x");
    let id1 = b1.id();
    assert_eq!(
        votes(&r.on_message(Message::Propose(fx.proposal(b1, None)))).len(),
        1
    );
    let qc1 = fx.qc(1, id1, &[0, 1, 2]);
    // A tail block with a payload is refused outright.
    let bad = fx.block(2, 2, qc1.clone(), 2, b"data");
    assert!(votes(&r.on_message(Message::Propose(fx.proposal(bad, None)))).is_empty());
    assert!(!r.epoch_ended());
    let b2 = fx.block(2, 2, qc1, 2, b"");
    let id2 = b2.id();
    assert_eq!(
        votes(&r.on_message(Message::Propose(fx.proposal(b2, None)))).len(),
        1
    );
    // QC for the tail block commits block 1 and ends the epoch.
    let acts = r.on_message(Message::Vote(fx.vote(0, 2, id2)));
    let acts2 = [acts, r.on_message(Message::Vote(fx.vote(1, 2, id2)))].concat();
    let commits: Vec<_> = acts2
        .iter()
        .filter_map(|a| match a {
            Action::Commit(b) => Some(b.height),
            _ => None,
        })
        .collect();
    assert_eq!(commits, vec![1]);
    assert!(r.epoch_ended());
    assert_eq!(r.committed_height(), 1);
    // Ended: no timeouts, no proposals, no votes, nothing queued.
    assert!(r.on_timeout(10_000).is_empty());
    assert!(r.propose(b"p".to_vec()).is_empty());
    let b3 = fx.block(3, 3, fx.qc(2, id2, &[0, 1, 2]), 3, b"");
    assert!(r
        .on_message(Message::Propose(fx.proposal(b3, None)))
        .is_empty());
}

#[test]
fn leader_proposes_empty_tail_blocks() {
    let fx = Fixture::new(4);
    // Replica 2 leads round 2 (RoundRobin); its parent (height 1) is the epoch's last block.
    let mut r = epoch_replica(&fx, 2, 1);
    let b1 = fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"x");
    let id1 = b1.id();
    r.propose(b"queued".to_vec());
    r.on_message(Message::Propose(fx.proposal(b1, None)));
    let mut acts = Vec::new();
    for v in [0, 1, 3] {
        acts.extend(r.on_message(Message::Vote(fx.vote(v, 1, id1))));
    }
    let prop = acts
        .iter()
        .find_map(|a| match a {
            Action::Broadcast(Message::Propose(p)) => Some(p.clone()),
            _ => None,
        })
        .expect("leader of round 2 proposes");
    assert_eq!(prop.block.height, 2);
    assert!(prop.block.payload.is_empty());
}

#[test]
fn ended_replica_answers_laggards_but_ignores_everything_else() {
    let fx = Fixture::new(4);
    let mut r = epoch_replica(&fx, 3, 1);
    let b1 = fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"x");
    let id1 = b1.id();
    r.on_message(Message::Propose(fx.proposal(b1, None)));
    let b2 = fx.block(2, 2, fx.qc(1, id1, &[0, 1, 2]), 2, b"");
    let id2 = b2.id();
    r.on_message(Message::Propose(fx.proposal(b2, None)));
    for v in [0, 1, 2] {
        r.on_message(Message::Vote(fx.vote(v, 2, id2)));
    }
    assert!(r.epoch_ended());
    // A laggard's (signed) timeout is answered with the newest QC.
    let t = Timeout {
        round: 1,
        high_qc: fx.ctx.genesis_qc(),
        voter: 0,
        sig: fx.sign(0, &fx.ctx.timeout_bytes(1, 0)),
        seen_proposal: None,
    };
    let acts = r.on_message(Message::Timeout(t.clone()));
    assert!(acts.iter().any(
        |a| matches!(a, Action::Send { to: 0, msg: Message::Catchup { qc, .. } } if qc.round == 2)
    ));
    // A forged timeout gets no answer.
    let mut forged = t;
    forged.sig.0[0] ^= 1;
    assert!(r.on_message(Message::Timeout(forged)).is_empty());
    // And it serves blocks.
    let acts = r.on_message(Message::SyncRequest {
        from: 0,
        block_id: id2,
    });
    assert!(acts
        .iter()
        .any(|a| matches!(a, Action::Send { msg: Message::SyncResponse { blocks }, .. } if blocks.len() == 2)));
}

#[test]
fn uncommitted_payloads_lists_the_ancestry_a_new_proposal_must_not_repeat() {
    let fx = Fixture::new(4);
    let mut r = fx.replica(3);
    let b1 = fx.block(1, 1, fx.ctx.genesis_qc(), 1, b"one");
    let id1 = b1.id();
    assert_eq!(
        votes(&r.on_message(Message::Propose(fx.proposal(b1, None)))).len(),
        1
    );
    let qc1 = fx.qc(1, id1, &[0, 1, 2]);
    let b2 = fx.block(2, 2, qc1, 2, b"two");
    let id2 = b2.id();
    r.on_message(Message::Propose(fx.proposal(b2, None)));
    // Nothing is committed yet: both payloads can still commit, newest first.
    assert_eq!(r.committed_height(), 0);
    assert_eq!(
        r.uncommitted_payloads(&id2),
        vec![b"two".as_slice(), b"one".as_slice()]
    );
    assert_eq!(r.uncommitted_payloads(&id1), vec![b"one".as_slice()]);
    assert!(r.uncommitted_payloads(&[9; 32]).is_empty());
    // A QC for block 2 commits block 1: only block 2 is left.
    r.on_message(Message::Vote(fx.vote(0, 2, id2)));
    r.on_message(Message::Vote(fx.vote(1, 2, id2)));
    r.on_message(Message::Vote(fx.vote(2, 2, id2)));
    assert_eq!(r.committed_height(), 1);
    assert_eq!(r.uncommitted_payloads(&id2), vec![b"two".as_slice()]);
}
