//! Deterministic network simulator: seeded delays, reordering, drops, partitions, crashes,
//! restarts and Byzantine replicas. Every message is encoded to bytes and decoded on delivery,
//! so the codec is exercised too. Safety is asserted on every commit of every honest replica.

use crate::*;
use ed25519_dalek::SigningKey;
use std::collections::{BTreeMap, VecDeque};

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    pub fn chance(&mut self, permille: u64) -> bool {
        self.below(1000) < permille
    }
}

#[derive(Clone, Debug)]
pub enum Behavior {
    Honest,
    /// Stops at `at`; optionally restarts (from persisted safety state) at `restart`.
    Crash {
        at: Tick,
        restart: Option<Tick>,
    },
    /// As leader, sends conflicting proposals (`both`: to everyone, else split in halves).
    EquivLeader {
        both: bool,
    },
    /// Casts a second, conflicting vote for every vote it sends.
    DoubleVoter,
    /// Corrupts the signature on everything it sends.
    BadSigs,
    /// Runs no protocol; injects random and mutated bytes.
    Garbage {
        permille: u64,
    },
}

impl Behavior {
    pub fn is_honest(&self) -> bool {
        matches!(
            self,
            Behavior::Honest
                | Behavior::Crash {
                    restart: Some(_),
                    ..
                }
        )
    }
}

#[derive(Clone, Debug)]
pub struct Partition {
    pub from: Tick,
    pub to: Tick,
    pub group: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub seed: u64,
    pub stakes: Vec<u64>,
    pub behaviors: Vec<Behavior>,
    pub ticks: Tick,
    pub min_delay: u64,
    pub max_delay: u64,
    /// Before `gst`: drop probability (per mille) and delays stretched 4x. After: clean.
    pub drop_permille: u64,
    pub gst: Tick,
    pub partitions: Vec<Partition>,
    pub policy: LeaderPolicy,
    pub base_timeout: Tick,
    pub payload_every: Tick,
}

impl Scenario {
    pub fn new(seed: u64, n: usize) -> Self {
        Self {
            seed,
            stakes: vec![1; n],
            behaviors: vec![Behavior::Honest; n],
            ticks: 400,
            min_delay: 1,
            max_delay: 3,
            drop_permille: 0,
            gst: 0,
            partitions: Vec::new(),
            policy: LeaderPolicy::RoundRobin,
            base_timeout: 20,
            payload_every: 7,
        }
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub honest: Vec<bool>,
    pub heights: Vec<Height>,
    pub rounds: Vec<Round>,
    pub first_commit_tick: BTreeMap<Height, Tick>,
    pub latencies: Vec<u64>,
    pub evidence: Vec<(usize, Evidence)>,
    pub stats: Vec<Stats>,
    pub sent: u64,
    pub dropped: u64,
    pub ctx: Context,
}

impl Outcome {
    pub fn max_height(&self) -> Height {
        self.heights.iter().copied().max().unwrap_or(0)
    }
    /// Lowest committed height over honest replicas that are alive at the end.
    pub fn min_honest_height(&self) -> Height {
        self.heights
            .iter()
            .zip(&self.honest)
            .filter(|(_, h)| **h)
            .map(|(x, _)| *x)
            .min()
            .unwrap_or(0)
    }
    /// Number of heights first committed (by any honest replica) after tick `t`.
    pub fn committed_after(&self, t: Tick) -> usize {
        self.first_commit_tick.values().filter(|x| **x > t).count()
    }
}

struct Node {
    replica: Option<Replica>,
    storage: MemoryStorage,
    beh: Behavior,
}

struct Sim<'a> {
    sc: &'a Scenario,
    rng: Rng,
    nodes: Vec<Node>,
    keys: Vec<SigningKey>,
    set: ValidatorSet,
    cfg: Config,
    queue: BTreeMap<(Tick, u64), (usize, Vec<u8>)>,
    seq: u64,
    recent: VecDeque<Vec<u8>>,
    now: Tick,
    // checking
    last_commit: Vec<(Height, Hash)>,
    global: BTreeMap<Height, Hash>,
    proposed_at: BTreeMap<Hash, Tick>,
    forged: BTreeMap<(usize, Round), Proposal>,
    out: Outcome,
}

fn key_for(seed: u64, i: usize) -> SigningKey {
    SigningKey::from_bytes(&sha256(&[
        b"sim-key",
        &seed.to_le_bytes(),
        &(i as u64).to_le_bytes(),
    ]))
}

impl<'a> Sim<'a> {
    fn new(sc: &'a Scenario) -> Self {
        let n = sc.stakes.len();
        assert_eq!(n, sc.behaviors.len());
        let keys: Vec<SigningKey> = (0..n).map(|i| key_for(sc.seed, i)).collect();
        let set = ValidatorSet::new(
            keys.iter()
                .zip(&sc.stakes)
                .map(|(k, s)| Validator {
                    key: k.verifying_key().to_bytes(),
                    stake: *s,
                })
                .collect(),
        )
        .unwrap();
        let cfg = Config {
            base_timeout: sc.base_timeout,
            leader_policy: sc.policy,
            ..Config::default()
        };
        let ctx = Context::new(&cfg.chain_id, set.clone());
        let mut s = Self {
            sc,
            rng: Rng(sc.seed ^ 0xA5A5_5A5A_1234_5678),
            nodes: Vec::new(),
            keys,
            set,
            cfg,
            queue: BTreeMap::new(),
            seq: 0,
            recent: VecDeque::new(),
            now: 0,
            last_commit: vec![(0, ctx.genesis); n],
            global: BTreeMap::new(),
            proposed_at: BTreeMap::new(),
            forged: BTreeMap::new(),
            out: Outcome {
                honest: sc.behaviors.iter().map(|b| b.is_honest()).collect(),
                heights: vec![0; n],
                rounds: vec![0; n],
                first_commit_tick: BTreeMap::new(),
                latencies: Vec::new(),
                evidence: Vec::new(),
                stats: Vec::new(),
                sent: 0,
                dropped: 0,
                ctx,
            },
        };
        for i in 0..n {
            let storage = MemoryStorage::default();
            let replica = if matches!(sc.behaviors[i], Behavior::Garbage { .. }) {
                None
            } else {
                Some(s.make_replica(i, &storage))
            };
            s.nodes.push(Node {
                replica,
                storage,
                beh: sc.behaviors[i].clone(),
            });
        }
        s
    }

    fn make_replica(&self, i: usize, storage: &MemoryStorage) -> Replica {
        Replica::new(
            self.cfg.clone(),
            self.set.clone(),
            self.keys[i].clone(),
            Some(Box::new(storage.clone())),
        )
        .unwrap()
    }

    fn partitioned(&self, a: usize, b: usize) -> bool {
        self.sc
            .partitions
            .iter()
            .any(|p| self.now >= p.from && self.now < p.to && p.group[a] != p.group[b])
    }

    fn enqueue(&mut self, to: usize, bytes: Vec<u8>) {
        let chaotic = self.now < self.sc.gst;
        if chaotic && self.rng.chance(self.sc.drop_permille) {
            self.out.dropped += 1;
            return;
        }
        let hi = if chaotic {
            self.sc.max_delay * 4
        } else {
            self.sc.max_delay
        };
        let d = self.rng.range(self.sc.min_delay, hi.max(self.sc.min_delay));
        self.seq += 1;
        self.queue.insert((self.now + d, self.seq), (to, bytes));
    }

    fn transmit(&mut self, from: usize, to: usize, msg: &Message) {
        if from == to {
            return;
        }
        self.out.sent += 1;
        if self.partitioned(from, to) {
            self.out.dropped += 1;
            return;
        }
        let bytes = msg.encode();
        if self.recent.len() >= 64 {
            self.recent.pop_front();
        }
        self.recent.push_back(bytes.clone());
        self.enqueue(to, bytes);
    }

    fn tamper(&mut self, i: usize, list: Vec<(usize, Message)>) -> Vec<(usize, Message)> {
        let n = self.nodes.len();
        match self.nodes[i].beh.clone() {
            Behavior::BadSigs => list
                .into_iter()
                .map(|(to, mut m)| {
                    match &mut m {
                        Message::Propose(p) => p.sig.0[3] ^= 1,
                        Message::Vote(v) => v.sig.0[3] ^= 1,
                        Message::Timeout(t) => t.sig.0[3] ^= 1,
                        _ => {}
                    }
                    (to, m)
                })
                .collect(),
            Behavior::DoubleVoter => {
                let mut res = Vec::new();
                for (to, m) in list {
                    if let Message::Vote(v) = &m {
                        let mut v2 = v.clone();
                        v2.block_id = sha256(&[b"fake", &v.block_id]);
                        let r = self.nodes[i].replica.as_ref().unwrap();
                        v2.sig = r.sign_for_test(&r.context().vote_bytes(v2.round, &v2.block_id));
                        res.push((to, Message::Vote(v2)));
                    }
                    res.push((to, m));
                }
                res
            }
            Behavior::EquivLeader { both } => {
                let mut res = Vec::new();
                for (to, m) in list {
                    if let Message::Propose(p) = &m {
                        if p.block.proposer as usize == i {
                            let round = p.block.round;
                            if !self.forged.contains_key(&(i, round)) {
                                let r = self.nodes[i].replica.as_ref().unwrap();
                                if let Some(f) = r.forge_proposal(vec![0xEE, round as u8]) {
                                    self.forged.insert((i, round), f);
                                }
                            }
                            if let Some(f) = self.forged.get(&(i, round)).cloned() {
                                if both {
                                    res.push((to, Message::Propose(f)));
                                } else if to * 2 >= n {
                                    res.push((to, Message::Propose(f)));
                                    continue;
                                }
                            }
                        }
                    }
                    res.push((to, m));
                }
                res
            }
            _ => list,
        }
    }

    fn apply(&mut self, i: usize, actions: Vec<Action>) {
        let n = self.nodes.len();
        let honest = self.out.honest[i];
        let mut list: Vec<(usize, Message)> = Vec::new();
        for a in actions {
            match a {
                Action::Send { to, msg } => list.push((to as usize, msg)),
                Action::Broadcast(m) => {
                    if honest {
                        if let Message::Propose(p) = &m {
                            self.proposed_at.entry(p.block.id()).or_insert(self.now);
                        }
                    }
                    for j in 0..n {
                        list.push((j, m.clone()));
                    }
                }
                Action::Commit(b) => {
                    if honest {
                        self.check_commit(i, &b);
                    }
                }
                Action::Evidence(e) => self.out.evidence.push((i, e)),
            }
        }
        for (to, m) in self.tamper(i, list) {
            if to < n {
                self.transmit(i, to, &m);
            }
        }
    }

    fn check_commit(&mut self, i: usize, b: &Block) {
        let seed = self.sc.seed;
        let (lh, lid) = self.last_commit[i];
        assert_eq!(
            b.height,
            lh + 1,
            "seed {seed}: replica {i} commit gap/duplicate"
        );
        assert_eq!(
            b.parent, lid,
            "seed {seed}: replica {i} committed block not extending its chain"
        );
        let id = b.id();
        match self.global.get(&b.height) {
            Some(prev) => assert_eq!(
                *prev, id,
                "seed {seed}: SAFETY VIOLATION at height {}",
                b.height
            ),
            None => {
                self.global.insert(b.height, id);
            }
        }
        self.last_commit[i] = (b.height, id);
        self.out
            .first_commit_tick
            .entry(b.height)
            .or_insert(self.now);
        if let Some(t) = self.proposed_at.get(&id) {
            self.out.latencies.push(self.now - t);
        }
    }

    fn run(mut self) -> Outcome {
        let n = self.nodes.len();
        for i in 0..n {
            if let Some(r) = self.nodes[i].replica.as_mut() {
                let a = r.start();
                self.apply(i, a);
            }
        }
        for now in 0..self.sc.ticks {
            self.now = now;
            for i in 0..n {
                // crash / restart schedule
                if let Behavior::Crash { at, restart } = self.nodes[i].beh.clone() {
                    if now == at {
                        self.nodes[i].replica = None;
                    }
                    if restart == Some(now) {
                        let st = self.nodes[i].storage.clone();
                        let mut r = self.make_replica(i, &st);
                        let a = r.on_timeout(now);
                        self.nodes[i].replica = Some(r);
                        self.apply(i, a);
                    }
                }
                if let Behavior::Garbage { permille } = self.nodes[i].beh {
                    if self.rng.chance(permille) {
                        let to = self.rng.below(n as u64) as usize;
                        let bytes = self.garbage();
                        if to != i {
                            self.enqueue(to, bytes);
                        }
                    }
                    continue;
                }
                if let Some(r) = self.nodes[i].replica.as_mut() {
                    let a = r.on_timeout(now);
                    self.apply(i, a);
                }
                if now % self.sc.payload_every == 0 {
                    if let Some(r) = self.nodes[i].replica.as_mut() {
                        let a = r.propose(vec![i as u8, (now & 0xff) as u8]);
                        self.apply(i, a);
                    }
                }
            }
            while let Some((&key, _)) = self.queue.iter().next() {
                if key.0 > now {
                    break;
                }
                let (to, bytes) = self.queue.remove(&key).unwrap();
                let Some(msg) = Message::decode(&bytes) else {
                    continue;
                };
                if let Some(r) = self.nodes[to].replica.as_mut() {
                    let a = r.on_message(msg);
                    self.apply(to, a);
                }
            }
        }
        for i in 0..n {
            if let Some(r) = &self.nodes[i].replica {
                self.out.heights[i] = r.committed_height();
                self.out.rounds[i] = r.round();
                self.out.stats.push(r.stats().clone());
            } else {
                self.out.stats.push(Stats::default());
                self.out.honest[i] = false;
            }
        }
        self.out
    }

    fn garbage(&mut self) -> Vec<u8> {
        match self.rng.below(3) {
            0 => {
                let len = self.rng.below(300) as usize;
                (0..len).map(|_| self.rng.next() as u8).collect()
            }
            kind => {
                if self.recent.is_empty() {
                    return vec![0xFF; 5];
                }
                let k = self.rng.below(self.recent.len() as u64) as usize;
                let mut b = self.recent[k].clone();
                if kind == 1 {
                    for _ in 0..self.rng.range(1, 4) {
                        let at = self.rng.below(b.len() as u64) as usize;
                        b[at] ^= 1 << self.rng.below(8);
                    }
                } else {
                    let cut = self.rng.below(b.len() as u64) as usize;
                    b.truncate(cut);
                }
                b
            }
        }
    }
}

/// Run a scenario. Panics (with the seed) on any safety violation.
pub fn run(sc: &Scenario) -> Outcome {
    Sim::new(sc).run()
}

/// (mean, p50, p99, max) of a latency sample, in ticks.
pub fn latency_summary(l: &[u64]) -> (f64, u64, u64, u64) {
    if l.is_empty() {
        return (0.0, 0, 0, 0);
    }
    let mut v = l.to_vec();
    v.sort_unstable();
    let mean = v.iter().sum::<u64>() as f64 / v.len() as f64;
    (
        mean,
        v[v.len() / 2],
        v[(v.len() * 99 / 100).min(v.len() - 1)],
        v[v.len() - 1],
    )
}
