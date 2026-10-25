//! Simulator for epoch changes: validator sets that change at epoch boundaries, nodes that
//! join, leave, lag and crash. One physical node can run the replicas of several epochs at
//! once (an ended epoch keeps serving laggards while the next one runs). Every message is
//! tagged with its epoch (the embedder's envelope) and encoded/decoded on delivery.
//!
//! Checked on every commit: heights are gap free across epochs, each block extends the
//! previous one (the first of an epoch extends the epoch's anchor), no two replicas ever
//! disagree about a height, and nothing above an epoch's last height is committed in it.

use crate::sim::Rng;
use crate::*;
use ed25519_dalek::SigningKey;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
struct Scenario {
    seed: u64,
    epoch_len: u64,
    /// Members of each epoch as `(physical node, stake)`; the position is the validator index.
    members: Vec<Vec<(usize, u64)>>,
    nodes: usize,
    /// `(node, from, to)`: the node is offline (sends and receives nothing, is not ticked).
    down: Vec<(usize, Tick, Tick)>,
    drop_permille: u64,
    gst: Tick,
    max_ticks: Tick,
    policy: LeaderPolicy,
}

impl Scenario {
    fn new(seed: u64, epoch_len: u64, nodes: usize, members: Vec<Vec<(usize, u64)>>) -> Self {
        Scenario {
            seed,
            epoch_len,
            members,
            nodes,
            down: Vec::new(),
            drop_permille: 0,
            gst: 0,
            max_ticks: 6_000,
            policy: LeaderPolicy::RoundRobin,
        }
    }
}

struct Outcome {
    /// Tick at which each (epoch, node) instance ended.
    ended: BTreeMap<(u64, usize), Tick>,
    global: BTreeMap<Height, Hash>,
    ticks: Tick,
}

fn key_for(seed: u64, i: usize) -> SigningKey {
    SigningKey::from_bytes(&sha256(&[
        b"esim-key",
        &seed.to_le_bytes(),
        &(i as u64).to_le_bytes(),
    ]))
}

fn anchor_id(epoch: u64, last: &Hash) -> Hash {
    sha256(&[b"esim-anchor", &epoch.to_le_bytes(), last])
}

struct Sim {
    sc: Scenario,
    rng: Rng,
    keys: Vec<SigningKey>,
    inst: Vec<BTreeMap<u64, Replica>>,
    started: BTreeSet<(u64, usize)>,
    queue: BTreeMap<(Tick, u64), (usize, u64, Vec<u8>)>,
    seq: u64,
    now: Tick,
    global: BTreeMap<Height, Hash>,
    last: BTreeMap<(u64, usize), (Height, Hash)>,
    out: Outcome,
}

impl Sim {
    fn new(sc: Scenario) -> Self {
        let keys = (0..sc.nodes).map(|i| key_for(sc.seed, i)).collect();
        Sim {
            rng: Rng(sc.seed ^ 0x5EED_E90C),
            keys,
            inst: (0..sc.nodes).map(|_| BTreeMap::new()).collect(),
            started: BTreeSet::new(),
            queue: BTreeMap::new(),
            seq: 0,
            now: 0,
            global: BTreeMap::new(),
            last: BTreeMap::new(),
            out: Outcome {
                ended: BTreeMap::new(),
                global: BTreeMap::new(),
                ticks: 0,
            },
            sc,
        }
    }

    fn epochs(&self) -> u64 {
        self.sc.members.len() as u64
    }

    fn set_of(&self, epoch: u64) -> ValidatorSet {
        ValidatorSet::new(
            self.sc.members[epoch as usize]
                .iter()
                .map(|(n, s)| Validator {
                    key: self.keys[*n].verifying_key().to_bytes(),
                    stake: *s,
                })
                .collect(),
        )
        .unwrap()
    }

    fn is_down(&self, node: usize) -> bool {
        self.sc
            .down
            .iter()
            .any(|(n, a, b)| *n == node && self.now >= *a && self.now < *b)
    }

    fn end_height(&self, epoch: u64) -> Height {
        (epoch + 1) * self.sc.epoch_len
    }

    /// The anchor of `epoch` (None for epoch 0), once the previous epoch's last block is final.
    fn anchor_of(&self, epoch: u64) -> Option<Option<Anchor>> {
        if epoch == 0 {
            return Some(None);
        }
        let h = self.end_height(epoch - 1);
        let id = self.global.get(&h)?;
        Some(Some(Anchor {
            height: h,
            id: anchor_id(epoch - 1, id),
        }))
    }

    fn start_instance(&mut self, epoch: u64, node: usize) {
        let Some(anchor) = self.anchor_of(epoch) else {
            return;
        };
        let cfg = Config {
            epoch,
            end_height: self.end_height(epoch),
            leader_policy: self.sc.policy,
            ..Config::default()
        };
        let mut r = Replica::new_epoch(
            cfg,
            self.set_of(epoch),
            self.keys[node].clone(),
            None,
            anchor,
        )
        .unwrap();
        let a = r.start();
        self.inst[node].insert(epoch, r);
        self.started.insert((epoch, node));
        self.last.insert(
            (epoch, node),
            (
                anchor.map_or(0, |a| a.height),
                anchor.map_or_else(|| self.inst[node][&epoch].context().root, |a| a.id),
            ),
        );
        self.apply(node, epoch, a);
    }

    fn enqueue(&mut self, to: usize, epoch: u64, bytes: Vec<u8>) {
        let chaotic = self.now < self.sc.gst;
        if chaotic && self.rng.chance(self.sc.drop_permille) {
            return;
        }
        let hi = if chaotic { 12 } else { 3 };
        let d = self.rng.range(1, hi);
        self.seq += 1;
        self.queue
            .insert((self.now + d, self.seq), (to, epoch, bytes));
    }

    fn send(&mut self, from: usize, epoch: u64, to_idx: usize, msg: &Message) {
        let Some((to, _)) = self.sc.members[epoch as usize].get(to_idx).copied() else {
            return;
        };
        if to == from || self.is_down(from) || self.is_down(to) {
            return;
        }
        self.enqueue(to, epoch, msg.encode());
    }

    fn apply(&mut self, node: usize, epoch: u64, actions: Vec<Action>) {
        let n = self.sc.members[epoch as usize].len();
        for a in actions {
            match a {
                Action::Send { to, msg } => self.send(node, epoch, to as usize, &msg),
                Action::Broadcast(m) => {
                    for j in 0..n {
                        self.send(node, epoch, j, &m);
                    }
                }
                Action::Commit(b) => self.check_commit(node, epoch, &b),
                Action::Evidence(_) => panic!("honest nodes must never produce evidence"),
            }
        }
        let ended = self.inst[node].get(&epoch).is_some_and(|r| r.epoch_ended());
        if ended && !self.out.ended.contains_key(&(epoch, node)) {
            self.out.ended.insert((epoch, node), self.now);
            assert_eq!(
                self.last[&(epoch, node)].0,
                self.end_height(epoch),
                "epoch ended at the wrong height"
            );
        }
    }

    fn check_commit(&mut self, node: usize, epoch: u64, b: &Block) {
        let seed = self.sc.seed;
        let (lh, lid) = self.last[&(epoch, node)];
        assert_eq!(b.height, lh + 1, "seed {seed}: gap/duplicate commit");
        assert_eq!(
            b.parent, lid,
            "seed {seed}: block does not extend the chain"
        );
        assert!(
            b.height <= self.end_height(epoch),
            "seed {seed}: committed past the epoch end"
        );
        assert!(
            b.height > epoch * self.sc.epoch_len,
            "seed {seed}: committed below the epoch start"
        );
        let id = b.id();
        match self.global.get(&b.height) {
            Some(prev) => assert_eq!(*prev, id, "seed {seed}: SAFETY VIOLATION at {}", b.height),
            None => {
                self.global.insert(b.height, id);
            }
        }
        self.last.insert((epoch, node), (b.height, id));
    }

    fn maybe_start_epochs(&mut self) {
        for epoch in 1..self.epochs() {
            for idx in 0..self.sc.members[epoch as usize].len() {
                let node = self.sc.members[epoch as usize][idx].0;
                if self.started.contains(&(epoch, node)) || self.is_down(node) {
                    continue;
                }
                let prev = self.sc.members[epoch as usize - 1]
                    .iter()
                    .any(|(n, _)| *n == node);
                let go = if prev {
                    // A continuing member moves on when its own replica has finished.
                    self.out.ended.contains_key(&(epoch - 1, node))
                } else {
                    // A joiner syncs the finalized chain from any node that finished the epoch.
                    self.global.contains_key(&self.end_height(epoch - 1))
                };
                if go {
                    self.start_instance(epoch, node);
                }
            }
        }
    }

    fn done(&self) -> bool {
        let last = self.epochs() - 1;
        self.sc.members[last as usize].iter().all(|(n, _)| {
            self.out.ended.contains_key(&(last, *n))
                || self
                    .sc
                    .down
                    .iter()
                    .any(|(m, _, to)| m == n && *to >= self.sc.max_ticks)
        })
    }

    fn run(mut self) -> Outcome {
        for idx in 0..self.sc.members[0].len() {
            let node = self.sc.members[0][idx].0;
            self.start_instance(0, node);
        }
        for now in 0..self.sc.max_ticks {
            self.now = now;
            self.maybe_start_epochs();
            for node in 0..self.sc.nodes {
                if self.is_down(node) {
                    continue;
                }
                let epochs: Vec<u64> = self.inst[node].keys().copied().collect();
                for e in epochs {
                    let r = self.inst[node].get_mut(&e).unwrap();
                    let mut a = r.on_timeout(now);
                    if now % 5 == 0 {
                        a.extend(r.propose(vec![node as u8, e as u8, (now & 0xff) as u8]));
                    }
                    self.apply(node, e, a);
                }
            }
            while let Some((&key, _)) = self.queue.iter().next() {
                if key.0 > now {
                    break;
                }
                let (to, epoch, bytes) = self.queue.remove(&key).unwrap();
                if self.is_down(to) {
                    continue;
                }
                let Some(msg) = Message::decode(&bytes) else {
                    continue;
                };
                if let Some(r) = self.inst[to].get_mut(&epoch) {
                    let a = r.on_message(msg);
                    self.apply(to, epoch, a);
                }
            }
            if self.done() {
                self.out.ticks = now;
                break;
            }
        }
        self.out.global = std::mem::take(&mut self.global);
        self.out
    }
}

fn run(sc: Scenario) -> Outcome {
    Sim::new(sc).run()
}

fn full_chain(o: &Outcome, epochs: u64, epoch_len: u64) {
    for h in 1..=epochs * epoch_len {
        assert!(o.global.contains_key(&h), "height {h} never finalized");
    }
}

fn eq(nodes: &[usize]) -> Vec<(usize, u64)> {
    nodes.iter().map(|n| (*n, 1)).collect()
}

#[test]
fn epochs_with_unchanged_set_chain_together() {
    let sc = Scenario::new(1, 5, 4, vec![eq(&[0, 1, 2, 3]); 4]);
    let o = run(sc);
    full_chain(&o, 4, 5);
    for e in 0..4 {
        for n in 0..4 {
            assert!(o.ended.contains_key(&(e, n)), "epoch {e} node {n}");
        }
    }
}

#[test]
fn validators_join_and_leave_between_epochs() {
    // 0..3 -> 2..5 -> 4..7 -> 0, 1, 6, 7: complete turnover within three epochs.
    let sc = Scenario::new(
        2,
        6,
        8,
        vec![
            eq(&[0, 1, 2, 3]),
            eq(&[2, 3, 4, 5]),
            eq(&[4, 5, 6, 7]),
            eq(&[0, 1, 6, 7]),
        ],
    );
    let o = run(sc);
    full_chain(&o, 4, 6);
}

#[test]
fn unequal_stakes_and_stake_weighted_leaders_across_epochs() {
    let mut sc = Scenario::new(
        3,
        5,
        6,
        vec![
            vec![(0, 50), (1, 20), (2, 20), (3, 10)],
            vec![(1, 40), (2, 30), (4, 20), (5, 10)],
            vec![(0, 10), (4, 40), (5, 30), (3, 20)],
        ],
    );
    sc.policy = LeaderPolicy::StakeWeighted;
    let o = run(sc);
    full_chain(&o, 3, 5);
}

#[test]
fn an_offline_new_member_below_one_third_does_not_stop_the_epoch() {
    // Node 4 joins in epoch 1 but is offline for all of it (1 of 4 seats, 25% of stake).
    let mut sc = Scenario::new(
        4,
        5,
        5,
        vec![eq(&[0, 1, 2, 3]), eq(&[1, 2, 3, 4]), eq(&[0, 1, 2, 3])],
    );
    sc.down = vec![(4, 0, 5_000)];
    sc.max_ticks = 8_000;
    let o = run(sc);
    full_chain(&o, 3, 5);
}

#[test]
fn laggard_finishes_the_epoch_through_the_ended_replicas() {
    // Node 3 is offline around the boundary: the others finish epoch 0 and move on; node 3
    // wakes up and must still learn the epoch's last block from the ended replicas, then
    // continue as a member of epoch 1.
    let mut sc = Scenario::new(5, 6, 4, vec![eq(&[0, 1, 2, 3]), eq(&[0, 1, 2, 3])]);
    sc.down = vec![(3, 10, 400)];
    let o = run(sc);
    full_chain(&o, 2, 6);
    assert!(
        o.ended.contains_key(&(0, 3)),
        "laggard never finished epoch 0"
    );
    assert!(
        o.ended.contains_key(&(1, 3)),
        "laggard never finished epoch 1"
    );
}

#[test]
fn leaving_validator_may_vanish_right_after_the_boundary() {
    // Node 0 leaves after epoch 0 and goes offline for good; the rest carry on.
    let mut sc = Scenario::new(
        6,
        5,
        6,
        vec![
            eq(&[0, 1, 2, 3]),
            eq(&[1, 2, 3, 4, 5]),
            eq(&[1, 2, 3, 4, 5]),
        ],
    );
    sc.down = vec![(0, 120, 100_000)];
    let o = run(sc);
    full_chain(&o, 3, 5);
}

#[test]
fn lossy_network_before_gst_with_changing_sets() {
    for seed in 0..12u64 {
        let mut sc = Scenario::new(
            100 + seed,
            4,
            7,
            vec![
                eq(&[0, 1, 2, 3]),
                eq(&[1, 2, 3, 4, 5]),
                eq(&[3, 4, 5, 6]),
                eq(&[0, 4, 5, 6]),
            ],
        );
        sc.drop_permille = 200;
        sc.gst = 300;
        sc.max_ticks = 10_000;
        let o = run(sc);
        full_chain(&o, 4, 4);
    }
}

#[test]
fn signatures_do_not_cross_epochs() {
    // A vote cast in epoch 0 is worthless in epoch 1 even with an identical validator set.
    let keys: Vec<SigningKey> = (0..4).map(|i| key_for(9, i)).collect();
    let set = |_: ()| {
        ValidatorSet::new(
            keys.iter()
                .map(|k| Validator {
                    key: k.verifying_key().to_bytes(),
                    stake: 1,
                })
                .collect(),
        )
        .unwrap()
    };
    let c0 = Context::for_epoch(b"x", 0, set(()), None);
    let c1 = Context::for_epoch(b"x", 1, set(()), Some([7; 32]));
    assert_ne!(c0.genesis, c1.genesis);
    let msg = c0.vote_bytes(3, &[1; 32]);
    use ed25519_dalek::Signer;
    let sig = Sig(keys[0].sign(&msg).to_bytes());
    assert!(c0.verify_sig(0, &msg, &sig));
    assert!(!c1.verify_sig(0, &c1.vote_bytes(3, &[1; 32]), &sig));
    // Evidence is bound to its epoch's context too.
    let sig_b = Sig(keys[0].sign(&c0.vote_bytes(3, &[2; 32])).to_bytes());
    let ev = Evidence::DoubleVote {
        voter: 0,
        round: 3,
        a: ([1; 32], sig),
        b: ([2; 32], sig_b),
    };
    assert!(ev.verify(&c0));
    assert!(!ev.verify(&c1));
    // The root is not part of the signed domain: the chain can rebuild the context without it.
    let c1b = Context::for_epoch(b"x", 1, set(()), None);
    assert_eq!(c1.genesis, c1b.genesis);
}
