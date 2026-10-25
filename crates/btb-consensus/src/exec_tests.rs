//! Execution attestation: a QC on a block certifies the execution result of its committed
//! ancestor. Unit tests for the hooks plus simulator runs with a fake execution function
//! (`sim::FakeExec`) and Byzantine proposers that attest wrong results.

use crate::sim::*;
use crate::tests::random_scenario;
use crate::*;
use ed25519_dalek::SigningKey;
use std::cell::Cell;
use std::rc::Rc;

fn replicas(n: usize) -> Vec<Replica> {
    let keys: Vec<SigningKey> = (0..n)
        .map(|i| SigningKey::from_bytes(&sha256(&[b"exec-key", &[i as u8]])))
        .collect();
    let set = ValidatorSet::new(
        keys.iter()
            .map(|k| Validator {
                key: k.verifying_key().to_bytes(),
                stake: 1,
            })
            .collect(),
    )
    .unwrap();
    keys.into_iter()
        .map(|k| Replica::new(Config::default(), set.clone(), k, None).unwrap())
        .collect()
}

fn proposal_of(actions: &[Action]) -> Option<Proposal> {
    actions.iter().find_map(|a| match a {
        Action::Broadcast(Message::Propose(p)) => Some(p.clone()),
        _ => None,
    })
}

fn voted(actions: &[Action]) -> bool {
    actions
        .iter()
        .any(|a| matches!(a, Action::Broadcast(Message::Vote(_))))
}

/// Hooks over a cell holding the highest executed height (digest = height-derived).
fn hooks(executed: &Rc<Cell<Option<Height>>>) -> (ExecProvider, ExecCheck) {
    let a = executed.clone();
    let b = executed.clone();
    let claim = |h: Height| sha256(&[b"claim", &h.to_le_bytes()]).to_vec();
    (
        Box::new(move |h| (a.get().is_some_and(|e| h <= e)).then(|| claim(h))),
        Box::new(move |blk, h| match b.get() {
            Some(e) if h <= e => {
                if blk.exec == claim(h) {
                    ExecVerdict::Accept
                } else {
                    ExecVerdict::Reject
                }
            }
            _ => ExecVerdict::Defer,
        }),
    )
}

#[test]
fn leader_waits_for_its_own_execution_then_proposes() {
    let mut leader = replicas(4).remove(1); // leader of round 1
    let executed = Rc::new(Cell::new(None));
    let (p, c) = hooks(&executed);
    leader.set_exec_hooks(p, c);
    // Nothing executed (not even the anchor): no proposal, and the payload stays queued.
    assert!(proposal_of(&leader.propose(b"tx".to_vec())).is_none());
    assert_eq!(leader.stats().proposals_made, 0);
    // The anchor (height 0) is executed: `resume` proposes with the queued payload.
    executed.set(Some(0));
    let prop = proposal_of(&leader.resume()).expect("proposes after execution");
    assert_eq!(prop.block.payload, b"tx");
    assert_eq!(
        prop.block.exec,
        sha256(&[b"claim", &0u64.to_le_bytes()]).to_vec()
    );
    assert_eq!(leader.exec_height(&prop.block.id()), Some(0));
}

#[test]
fn voter_defers_until_it_executed_then_votes_only_on_a_match() {
    let mut rs = replicas(4);
    let mut leader = rs.remove(1);
    let l_exec = Rc::new(Cell::new(Some(0)));
    let (p, c) = hooks(&l_exec);
    leader.set_exec_hooks(p, c);
    let prop = proposal_of(&leader.propose(b"x".to_vec())).unwrap();

    // A voter that has not executed the anchor yet: no vote, not counted as rejected.
    let v_exec = Rc::new(Cell::new(None));
    let mut voter = rs.remove(0);
    let (p, c) = hooks(&v_exec);
    voter.set_exec_hooks(p, c);
    let acts = voter.on_message(Message::Propose(prop.clone()));
    assert!(!voted(&acts));
    assert_eq!(voter.stats().exec_deferred, 1);
    assert_eq!(voter.stats().exec_rejected, 0);
    assert_eq!(
        voter.safety_state().last_voted_round,
        0,
        "a deferral is not a vote"
    );
    // Once it executed, `resume` votes for the same proposal.
    v_exec.set(Some(0));
    assert!(voted(&voter.resume()));
    assert_eq!(voter.safety_state().last_voted_round, 1);
}

#[test]
fn wrong_attestation_gets_no_vote_even_when_everything_else_is_valid() {
    let mut rs = replicas(4);
    let mut leader = rs.remove(1);
    let exec = Rc::new(Cell::new(Some(0)));
    let (p, c) = hooks(&exec);
    leader.set_exec_hooks(p, c);
    let good = proposal_of(&leader.propose(b"x".to_vec())).unwrap();

    // Re-sign a variant with a corrupted root (the leader's key is valid: only the root is wrong).
    let mut bad = good.clone();
    bad.block.exec[5] ^= 1;
    let id = bad.block.id();
    bad.sig = leader.sign_for_test(&leader.context().proposal_bytes(1, &id));
    // Also a missing and an oversized attestation.
    let mut empty = bad.clone();
    empty.block.exec = Vec::new();
    empty.sig = leader.sign_for_test(&leader.context().proposal_bytes(1, &empty.block.id()));
    let mut huge = bad.clone();
    huge.block.exec = vec![7; MAX_EXEC_BYTES + 1];
    huge.sig = leader.sign_for_test(&leader.context().proposal_bytes(1, &huge.block.id()));

    for variant in [bad, empty, huge] {
        let mut voter = replicas(4).remove(2);
        let v_exec = Rc::new(Cell::new(Some(0)));
        let (p, c) = hooks(&v_exec);
        voter.set_exec_hooks(p, c);
        assert!(!voted(&voter.on_message(Message::Propose(variant))));
        assert_eq!(voter.safety_state().last_voted_round, 0);
    }
    // The honest proposal still gets the vote.
    let mut voter = replicas(4).remove(2);
    let v_exec = Rc::new(Cell::new(Some(0)));
    let (p, c) = hooks(&v_exec);
    voter.set_exec_hooks(p, c);
    assert!(voted(&voter.on_message(Message::Propose(good))));
}

#[test]
fn block_codec_covers_exec_and_rejects_oversized() {
    let mut leader = replicas(4).remove(1);
    let exec = Rc::new(Cell::new(Some(0)));
    let (p, c) = hooks(&exec);
    leader.set_exec_hooks(p, c);
    let prop = proposal_of(&leader.propose(b"x".to_vec())).unwrap();
    let msg = Message::Propose(prop.clone());
    assert_eq!(Message::decode(&msg.encode()), Some(msg));
    // The block id covers `exec`.
    let mut other = prop.block.clone();
    other.exec[0] ^= 1;
    assert_ne!(other.id(), prop.block.id());
    // Oversized attestations do not decode.
    let mut huge = prop;
    huge.block.exec = vec![1; MAX_EXEC_BYTES + 1];
    assert_eq!(Message::decode(&Message::Propose(huge).encode()), None);
}

/// A chain where rounds are skipped: the attested height lags (`exec_height` follows the
/// 2-chain commit rule of the ancestry, not the parent's height).
#[test]
fn exec_height_follows_the_commit_rule_of_the_ancestry() {
    let o = run(&{
        let mut sc = Scenario::new(3, 4);
        sc.exec = true;
        sc.drop_permille = 200;
        sc.gst = 200;
        sc.ticks = 700;
        sc
    });
    assert!(o.attested_commits > 20, "{}", o.attested_commits);
}

#[test]
fn honest_cluster_attests_every_committed_block() {
    for seed in 0..8 {
        let mut sc = Scenario::new(seed, 4);
        sc.exec = true;
        sc.ticks = 500;
        let o = run(&sc);
        assert!(o.min_honest_height() > 20, "seed {seed}: {:?}", o.heights);
        assert!(o.attested_commits as u64 >= o.min_honest_height());
        assert_eq!(o.wrong_exec_blocks, 0);
        assert!(o.stats.iter().all(|s| s.exec_rejected == 0));
    }
}

#[test]
fn divergent_proposer_gets_no_votes_and_the_chain_continues() {
    for seed in 0..12 {
        let mut sc = Scenario::new(seed, 4);
        sc.exec = true;
        sc.ticks = 900;
        let byz = seed as usize % 4;
        sc.behaviors[byz] = Behavior::WrongExec;
        let o = run(&sc);
        assert!(
            o.wrong_exec_blocks > 0,
            "seed {seed}: the attacker never led"
        );
        // The Sim asserts that no honest replica ever voted for a wrong block and that no
        // committed block attests a wrong root; the chain must also keep moving.
        assert!(
            o.min_honest_height_excluding(byz) > 10,
            "seed {seed}: {:?}",
            o.heights
        );
        // Honest replicas counted the rejected attestations.
        let rejected: u64 = o.stats.iter().map(|s| s.exec_rejected).sum();
        assert!(rejected > 0, "seed {seed}: nothing was rejected");
    }
}

#[test]
fn a_faulty_majority_of_leaders_cannot_get_a_wrong_root_certified() {
    // Two of four validators attest wrong roots: more than 1/3 faulty, liveness is lost, but
    // honest replicas still vote for nothing wrong (asserted inside the Sim on every vote).
    for seed in 0..6 {
        let mut sc = Scenario::new(seed, 4);
        sc.exec = true;
        sc.ticks = 600;
        sc.behaviors[0] = Behavior::WrongExec;
        sc.behaviors[1] = Behavior::WrongExec;
        let o = run(&sc);
        assert!(o.wrong_exec_blocks > 0);
    }
}

#[test]
fn stake_weighted_crash_restart_and_loss_with_attestation() {
    for seed in 0..10 {
        let mut sc = Scenario::new(seed, 5);
        sc.exec = true;
        sc.stakes = vec![30, 20, 20, 15, 15];
        sc.policy = LeaderPolicy::StakeWeighted;
        sc.drop_permille = 150;
        sc.gst = 150;
        sc.ticks = 800;
        sc.behaviors[(seed % 5) as usize] = Behavior::Crash {
            at: 40,
            restart: Some(120),
        };
        let o = run(&sc);
        assert!(o.min_honest_height() > 10, "seed {seed}: {:?}", o.heights);
    }
}

#[test]
fn property_attestation_over_random_scenarios() {
    let mut wrong_seen = 0usize;
    for seed in 0..60 {
        let mut sc = random_scenario(seed);
        sc.exec = true;
        // Add a divergent proposer when the stake bound allows another fault.
        let total: u64 = sc.stakes.iter().sum();
        let faulty: u64 = sc
            .behaviors
            .iter()
            .zip(&sc.stakes)
            .filter(|(b, _)| {
                !matches!(
                    b,
                    Behavior::Honest
                        | Behavior::Crash {
                            restart: Some(_),
                            ..
                        }
                )
            })
            .map(|(_, s)| *s)
            .sum();
        if let Some(i) = (0..sc.stakes.len()).find(|&i| {
            matches!(sc.behaviors[i], Behavior::Honest) && (faulty + sc.stakes[i]) * 3 < total
        }) {
            if seed % 2 == 0 {
                sc.behaviors[i] = Behavior::WrongExec;
            }
        }
        // `run` asserts: agreement at every height, gap-free commits, no honest vote for a
        // wrong-attestation block, every committed attestation equals the true execution.
        let o = run(&sc);
        wrong_seen += o.wrong_exec_blocks;
        let live = o.honest.iter().zip(&o.heights).any(|(h, x)| *h && *x > 0);
        assert!(
            live || sc.gst > 200,
            "seed {seed}: no progress {:?}",
            o.heights
        );
    }
    assert!(
        wrong_seen > 0,
        "the property test never exercised a wrong attestation"
    );
}

#[test]
fn a_payload_that_waited_for_execution_does_not_outlive_its_round() {
    let mut leader = replicas(4).remove(1);
    leader.set_hold_empty_proposals(true);
    let executed = Rc::new(Cell::new(None));
    let (p, c) = hooks(&executed);
    leader.set_exec_hooks(p, c);
    assert!(proposal_of(&leader.propose(b"stale".to_vec())).is_none());
    assert_eq!(leader.queued_payloads(), 1);
    // The round times out (a TC would move it on; a QC does the same): the queue is dropped.
    let mut others = replicas(4);
    let tc_round_1 = {
        // Timeouts of three other validators for round 1 form a TC at the leader.
        let mut acts = Vec::new();
        for r in others.iter_mut().filter(|r| r.index() != 1) {
            let t = r.on_timeout(1_000);
            acts.extend(t);
        }
        acts
    };
    for a in tc_round_1 {
        if let Action::Broadcast(m @ Message::Timeout(_)) = a {
            leader.on_message(m);
        }
    }
    assert_eq!(leader.round(), 2);
    assert_eq!(leader.queued_payloads(), 0, "the stale payload was dropped");
    executed.set(Some(0));
    assert!(leader
        .resume()
        .iter()
        .all(|a| !matches!(a, Action::Broadcast(Message::Propose(_)))));
}
