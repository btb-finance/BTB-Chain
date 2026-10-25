//! Tests for the embedder hooks: `set_payload_check` and `set_hold_empty_proposals`.

use crate::*;
use ed25519_dalek::SigningKey;

fn replicas(n: usize) -> Vec<Replica> {
    let keys: Vec<SigningKey> = (0..n)
        .map(|i| SigningKey::from_bytes(&sha256(&[b"hook-key", &[i as u8]])))
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

#[test]
fn payload_check_blocks_votes_for_rejected_payloads() {
    let mut rs = replicas(4);
    // Round 1 is led by validator 1.
    let mut leader = rs.remove(1);
    let p = proposal_of(&leader.propose(b"good".to_vec())).expect("leader proposes");

    // Without a check the follower votes.
    let mut plain = rs.remove(0);
    assert!(voted(&plain.on_message(Message::Propose(p.clone()))));

    // A check that rejects the payload: no vote, counted as rejected.
    let mut strict = replicas(4).remove(2);
    strict.set_payload_check(Box::new(|b| b.payload != b"good"));
    let acts = strict.on_message(Message::Propose(p.clone()));
    assert!(!voted(&acts));
    assert_eq!(strict.stats().votes_cast, 0);
    assert!(strict.stats().rejected_messages >= 1);

    // A check that accepts it: votes.
    let mut lenient = replicas(4).remove(3);
    lenient.set_payload_check(Box::new(|b| b.payload == b"good"));
    assert!(voted(&lenient.on_message(Message::Propose(p))));
}

#[test]
fn hold_empty_proposals_waits_for_propose() {
    let mut leader = replicas(4).remove(1);
    leader.set_hold_empty_proposals(true);
    // Leader of round 1 stays quiet: no empty block on its own initiative.
    assert!(proposal_of(&leader.start()).is_none());
    assert!(proposal_of(&leader.on_timeout(1)).is_none());
    // An explicit (even empty) payload is proposed immediately.
    let p = proposal_of(&leader.propose(Vec::new())).expect("proposes on request");
    assert!(p.block.payload.is_empty());
    assert_eq!(p.block.round, 1);

    // Default behaviour is unchanged: an idle leader proposes an empty block at once.
    let mut eager = replicas(4).remove(1);
    assert!(proposal_of(&eager.start()).is_some());
}
