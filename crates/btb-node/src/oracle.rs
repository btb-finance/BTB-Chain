//! Node side of the oracles: beacon partial signatures and price observations.
//!
//! Messages travel as frame kind 5 between validators (`OracleMsg`). Nothing here is trusted:
//! a partial is accepted only from the validator whose key is the beacon holder named in it, a
//! beacon value only if it verifies under the group key, an observation only if it verifies and
//! comes from a current validator.
//!
//! **Release rule (the heart of the bias argument).** A holder releases its partial signature
//! for height `h` only after its own replica *committed* block `h`, that is after the
//! transactions that will consume `beacon(h)` are final. Without `threshold` partials nobody can
//! compute the beacon (see `btb-oracle`), and honest holders (more than `n - threshold`) never
//! release early, so the content of block `h` cannot depend on `beacon(h)`.

use btb_chain::oracle::MAX_OBS_PER_BLOCK;
use btb_oracle::bls::{G2, G2_BYTES};
use btb_oracle::price::OBS_BYTES;
use btb_oracle::{beacon_message, Beacon, Collector, GroupKey, KeyShare, SignedObservation};
use ed25519_dalek::SigningKey;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Largest oracle frame body accepted (the biggest message is a beacon: 8 + 8 + 96 + tag).
pub const MAX_ORACLE_MSG: usize = 256;
/// Partials for heights this far above the executed height are accepted (bounded buffering).
pub const MAX_AHEAD: u64 = 256;

const TAG_PARTIAL: u8 = 1;
const TAG_BEACON: u8 = 2;
const TAG_WANT: u8 = 3;
const TAG_PRICE: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleMsg {
    /// A holder's partial signature on the beacon message of `height`.
    Partial {
        height: u64,
        index: u32,
        sig: [u8; G2_BYTES],
    },
    /// A finished beacon (answer to `Want`, or a gossiped result).
    Beacon(Beacon),
    /// "Send me the beacon of `height`" (a node that is catching up).
    Want { height: u64 },
    /// A validator's signed price observation.
    Price(SignedObservation),
}

impl OracleMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            OracleMsg::Partial { height, index, sig } => {
                o.push(TAG_PARTIAL);
                o.extend_from_slice(&height.to_le_bytes());
                o.extend_from_slice(&index.to_le_bytes());
                o.extend_from_slice(sig);
            }
            OracleMsg::Beacon(b) => {
                o.push(TAG_BEACON);
                o.extend_from_slice(&b.encode());
            }
            OracleMsg::Want { height } => {
                o.push(TAG_WANT);
                o.extend_from_slice(&height.to_le_bytes());
            }
            OracleMsg::Price(p) => {
                o.push(TAG_PRICE);
                o.extend_from_slice(&p.encode());
            }
        }
        o
    }

    /// Total decoder: `None` for anything malformed or with trailing bytes.
    pub fn decode(b: &[u8]) -> Option<OracleMsg> {
        let (tag, body) = b.split_first()?;
        match *tag {
            TAG_PARTIAL if body.len() == 8 + 4 + G2_BYTES => Some(OracleMsg::Partial {
                height: u64::from_le_bytes(body[..8].try_into().ok()?),
                index: u32::from_le_bytes(body[8..12].try_into().ok()?),
                sig: body[12..].try_into().ok()?,
            }),
            TAG_BEACON => Beacon::decode(body).map(OracleMsg::Beacon),
            TAG_WANT if body.len() == 8 => Some(OracleMsg::Want {
                height: u64::from_le_bytes(body.try_into().ok()?),
            }),
            TAG_PRICE if body.len() == OBS_BYTES => {
                SignedObservation::decode(body).map(OracleMsg::Price)
            }
            _ => None,
        }
    }

    pub fn frame(&self) -> Vec<u8> {
        crate::wire::frame(crate::wire::KIND_ORACLE, &self.encode())
    }
}

/// Collects partials and keeps the finished beacons of recent heights.
pub struct Beacons {
    chain_id: u64,
    epoch_len: u64,
    group: GroupKey,
    holders: Vec<[u8; 32]>,
    share: Option<KeyShare>,
    collectors: BTreeMap<u64, Collector>,
    ready: BTreeMap<u64, Beacon>,
    signed: BTreeSet<u64>,
    /// Highest executed height: older entries are dropped.
    floor: u64,
    /// Partials that failed verification (statistics for `btb_status`).
    pub rejected: u64,
}

impl Beacons {
    pub fn new(
        chain_id: u64,
        epoch_len: u64,
        group: GroupKey,
        holders: Vec<[u8; 32]>,
        share: Option<KeyShare>,
    ) -> Self {
        Beacons {
            chain_id,
            epoch_len: epoch_len.max(1),
            group,
            holders,
            share,
            collectors: BTreeMap::new(),
            ready: BTreeMap::new(),
            signed: BTreeSet::new(),
            floor: 0,
            rejected: 0,
        }
    }

    pub fn epoch_of(&self, height: u64) -> u64 {
        height.saturating_sub(1) / self.epoch_len
    }

    fn message(&self, height: u64) -> Vec<u8> {
        beacon_message(self.chain_id, self.epoch_of(height), height)
    }

    pub fn holds_share(&self) -> bool {
        self.share.is_some()
    }

    pub fn get(&self, height: u64) -> Option<&Beacon> {
        self.ready.get(&height)
    }

    /// Our replica committed block `height`: release our partial for it (once). Returns the
    /// message to broadcast. Never called for a height the replica has not committed.
    pub fn on_commit(&mut self, height: u64) -> Option<OracleMsg> {
        let share = self.share.as_ref()?;
        if !self.signed.insert(height) {
            return None;
        }
        let sig = share.sign(&self.message(height));
        let index = share.index;
        self.accept_partial(height, index, sig);
        Some(OracleMsg::Partial {
            height,
            index: index as u32,
            sig: sig.to_bytes(),
        })
    }

    fn collector(&mut self, height: u64) -> &mut Collector {
        let msg = self.message(height);
        let group = self.group.clone();
        self.collectors
            .entry(height)
            .or_insert_with(|| Collector::new(group, msg))
    }

    fn accept_partial(&mut self, height: u64, index: usize, sig: G2) -> bool {
        if self.ready.contains_key(&height) {
            return false;
        }
        let epoch = self.epoch_of(height);
        let c = self.collector(height);
        let before = c.rejected().len();
        let done = c.add(index, sig);
        let bad = c.rejected().len() - before;
        self.rejected += bad as u64;
        if let Some(s) = done {
            self.ready.insert(
                height,
                Beacon {
                    epoch,
                    height,
                    signature: s.to_bytes(),
                },
            );
            self.collectors.remove(&height);
            return true;
        }
        false
    }

    /// A partial arrived from the peer whose consensus key is `from`. Returns the height if it
    /// completed a beacon.
    pub fn on_partial(
        &mut self,
        from: &[u8; 32],
        height: u64,
        index: u32,
        sig: &[u8; G2_BYTES],
    ) -> Option<u64> {
        let index = index as usize;
        if self.holders.get(index) != Some(from)
            || height <= self.floor
            || height > self.floor.saturating_add(MAX_AHEAD)
        {
            return None;
        }
        let g2 = G2::from_bytes(sig)?;
        self.accept_partial(height, index, g2).then_some(height)
    }

    /// A finished beacon from a peer. Verified (one pairing check) before it is kept; ignored
    /// when we already have it or do not need it.
    pub fn on_beacon(&mut self, b: Beacon) -> bool {
        if b.height <= self.floor
            || b.height > self.floor.saturating_add(MAX_AHEAD)
            || self.ready.contains_key(&b.height)
            || b.epoch != self.epoch_of(b.height)
            || !b.verify(self.chain_id, &self.group)
        {
            return false;
        }
        self.collectors.remove(&b.height);
        self.ready.insert(b.height, b);
        true
    }

    /// A beacon loaded from the block store (restart, serving a peer): trusted as stored.
    pub fn insert_trusted(&mut self, b: Beacon) {
        self.ready.insert(b.height, b);
    }

    /// Blocks up to `executed` are done: forget their partials and (old) beacons.
    pub fn prune(&mut self, executed: u64) {
        self.floor = self.floor.max(executed);
        let floor = self.floor;
        self.collectors.retain(|h, _| *h > floor);
        // keep recent beacons to answer `Want` from peers that are slightly behind
        self.ready.retain(|h, _| *h + 64 > floor);
        self.signed.retain(|h| *h + 64 > floor);
    }
}

/// Where a node gets prices from. Real exchange fetching (HTTPS actions) is future work; tests
/// and local testnets plug in a function.
#[derive(Clone)]
pub struct PriceSource(Arc<dyn Fn(u32) -> Option<u128> + Send + Sync>);

impl PriceSource {
    pub fn new(f: impl Fn(u32) -> Option<u128> + Send + Sync + 'static) -> Self {
        PriceSource(Arc::new(f))
    }

    pub fn get(&self, pair: u32) -> Option<u128> {
        (self.0)(pair)
    }
}

impl std::fmt::Debug for PriceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PriceSource(..)")
    }
}

/// Produces this validator's observations and holds the ones heard from others.
pub struct Prices {
    chain_id: u64,
    key: SigningKey,
    source: Option<PriceSource>,
    interval: Duration,
    last: Option<Instant>,
    pairs: Vec<u32>,
    max_age: u64,
    pending: BTreeMap<(u32, [u8; 32]), SignedObservation>,
}

impl Prices {
    pub fn new(
        chain_id: u64,
        key: SigningKey,
        source: Option<PriceSource>,
        interval: Duration,
        pairs: Vec<u32>,
        max_age: u64,
    ) -> Self {
        Prices {
            chain_id,
            key,
            source,
            interval,
            last: None,
            pairs,
            max_age,
            pending: BTreeMap::new(),
        }
    }

    /// Time to observe again? Returns this validator's new signed observations (also kept as
    /// pending for its own proposals).
    pub fn tick(&mut self, now: Instant, head_height: u64) -> Vec<SignedObservation> {
        let Some(src) = self.source.clone() else {
            return Vec::new();
        };
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < self.interval)
        {
            return Vec::new();
        }
        self.last = Some(now);
        let mut out = Vec::new();
        for pair in self.pairs.clone() {
            if let Some(price) = src.get(pair) {
                let o = SignedObservation::sign(
                    self.chain_id,
                    &self.key,
                    btb_oracle::Observation {
                        pair,
                        price,
                        height: head_height,
                    },
                );
                self.pending.insert((pair, o.key), o);
                out.push(o);
            }
        }
        out
    }

    /// An observation from the network. Kept if it verifies, comes from a validator in
    /// `validators`, names a configured pair and is newer than what we hold.
    pub fn on_observation(&mut self, o: SignedObservation, validators: &[[u8; 32]]) -> bool {
        if !self.pairs.contains(&o.obs.pair)
            || !validators.contains(&o.key)
            || self
                .pending
                .get(&(o.obs.pair, o.key))
                .is_some_and(|p| p.obs.height >= o.obs.height)
            || !o.verify(self.chain_id)
        {
            return false;
        }
        self.pending.insert((o.obs.pair, o.key), o);
        true
    }

    /// The observations to put in the next proposal (fresh ones only). They stay pending until
    /// seen in a committed block, so a failed round does not lose them.
    pub fn for_proposal(&self, head_height: u64) -> Vec<SignedObservation> {
        self.pending
            .values()
            .filter(|o| head_height.saturating_sub(o.obs.height) <= self.max_age)
            .take(MAX_OBS_PER_BLOCK)
            .copied()
            .collect()
    }

    /// Observations that appeared in a block's payload are no longer pending.
    pub fn note_included(&mut self, included: &[SignedObservation]) {
        for o in included {
            if self
                .pending
                .get(&(o.obs.pair, o.key))
                .is_some_and(|p| p.obs.height <= o.obs.height)
            {
                self.pending.remove(&(o.obs.pair, o.key));
            }
        }
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

/// Everything the driver keeps for the oracles of a chain that has an oracle config.
pub struct OracleNode {
    pub beacons: Beacons,
    pub prices: Prices,
    /// The committed block at the head of the execution queue has been waiting for its beacon
    /// since `.1`; `.2` is when we last asked peers for it.
    pub waiting: Option<(u64, Instant, Option<Instant>)>,
}

/// How long a committed block waits for its beacon before asking peers for it, and the spacing
/// of the requests.
pub const WANT_AFTER: Duration = Duration::from_millis(250);
pub const WANT_EVERY: Duration = Duration::from_millis(500);

impl OracleNode {
    /// Build from the chain state's oracle config. `None`: the chain has no oracles.
    pub fn from_state<S: btb_runtime::KvStore>(
        state: &btb_chain::ChainState<S>,
        p: &crate::config::NodeParams,
    ) -> Result<Option<OracleNode>, String> {
        let Some(loaded) = btb_chain::oracle::load(state.store()).map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        let cfg = state.config();
        let me = p.key.verifying_key().to_bytes();
        if let Some(share) = &p.oracle_share {
            let ok = loaded.group.public_shares.get(share.index) == Some(&share.public_share())
                && loaded.genesis.holders.get(share.index) == Some(&me);
            if !ok {
                return Err(
                    "the oracle share does not belong to this validator key and the genesis beacon key"
                        .into(),
                );
            }
        }
        let pairs: Vec<u32> = loaded.genesis.pairs.iter().map(|p| p.id).collect();
        let source = p.price_source.clone();
        Ok(Some(OracleNode {
            beacons: Beacons::new(
                cfg.chain_id,
                cfg.staking.epoch_len,
                loaded.group,
                loaded.genesis.holders.clone(),
                p.oracle_share.clone(),
            ),
            prices: Prices::new(
                cfg.chain_id,
                p.key.clone(),
                source,
                Duration::from_millis(p.price_interval_ms.max(1)),
                pairs,
                loaded.genesis.max_age_blocks,
            ),
            waiting: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_oracle::bls::Fr;
    use btb_oracle::{trusted_deal, Observation, Params};

    const CHAIN: u64 = 7;

    fn setup(n: usize) -> (Vec<Beacons>, Vec<KeyShare>, GroupKey, Vec<[u8; 32]>) {
        let (group, shares) = trusted_deal(Params::standard(n), &[8; 32]).unwrap();
        let holders: Vec<[u8; 32]> = (0..n).map(|i| [i as u8 + 1; 32]).collect();
        let nodes = shares
            .iter()
            .map(|s| Beacons::new(CHAIN, 100, group.clone(), holders.clone(), Some(s.clone())))
            .collect();
        (nodes, shares, group, holders)
    }

    #[test]
    fn messages_round_trip_and_reject_garbage() {
        let (_, shares, group, _) = setup(4);
        let sig = shares[0].sign(b"x").to_bytes();
        let obs = SignedObservation::sign(
            CHAIN,
            &SigningKey::from_bytes(&[1; 32]),
            Observation {
                pair: 1,
                price: 9,
                height: 3,
            },
        );
        let beacon = Beacon {
            epoch: 0,
            height: 5,
            signature: sig,
        };
        let msgs = [
            OracleMsg::Partial {
                height: 9,
                index: 2,
                sig,
            },
            OracleMsg::Beacon(beacon),
            OracleMsg::Want { height: 77 },
            OracleMsg::Price(obs),
        ];
        for m in &msgs {
            let bytes = m.encode();
            assert!(bytes.len() <= MAX_ORACLE_MSG);
            assert_eq!(OracleMsg::decode(&bytes).as_ref(), Some(m));
            for cut in 0..bytes.len() {
                assert!(
                    OracleMsg::decode(&bytes[..cut]).is_none(),
                    "{m:?} cut {cut}"
                );
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(OracleMsg::decode(&longer).is_none());
        }
        assert!(OracleMsg::decode(&[]).is_none());
        assert!(OracleMsg::decode(&[99, 1, 2, 3]).is_none());
        let _ = group;
    }

    #[test]
    fn a_partial_is_released_only_after_the_commit_and_only_once() {
        let (mut nodes, _, _, _) = setup(4);
        let n0 = &mut nodes[0];
        // Nothing is signed or leaked before on_commit.
        assert!(n0.get(5).is_none());
        let first = n0.on_commit(5).expect("released at commit");
        assert!(matches!(
            first,
            OracleMsg::Partial {
                height: 5,
                index: 0,
                ..
            }
        ));
        assert!(n0.on_commit(5).is_none(), "never twice");
        // A node without a share releases nothing.
        let (group, _) = trusted_deal(Params::standard(4), &[8; 32]).unwrap();
        let mut follower = Beacons::new(CHAIN, 100, group, vec![[1; 32]; 4], None);
        assert!(follower.on_commit(5).is_none());
        assert!(!follower.holds_share());
    }

    #[test]
    fn partials_combine_into_the_same_beacon_on_every_node() {
        let (mut nodes, _, group, holders) = setup(4);
        let msgs: Vec<OracleMsg> = (0..4).map(|i| nodes[i].on_commit(5).unwrap()).collect();
        // node 3 receives the others' partials in some order and completes at the third
        let mut done = None;
        for (i, m) in msgs.iter().enumerate().take(3) {
            let OracleMsg::Partial { height, index, sig } = m else {
                unreachable!()
            };
            if let Some(h) = nodes[3].on_partial(&holders[i], *height, *index, sig) {
                done = Some(h);
            }
        }
        assert_eq!(done, Some(5), "own partial + 2 others = threshold 3");
        let b3 = nodes[3].get(5).cloned().unwrap();
        assert!(b3.verify(CHAIN, &group));
        // node 0 uses a different set of three
        for i in [1usize, 2] {
            let OracleMsg::Partial { height, index, sig } = &msgs[i] else {
                unreachable!()
            };
            nodes[0].on_partial(&holders[i], *height, *index, sig);
        }
        assert_eq!(
            nodes[0].get(5),
            Some(&b3),
            "any threshold subset gives the same beacon"
        );
        assert_eq!(b3.epoch, 0);
    }

    #[test]
    fn forged_misattributed_and_out_of_window_partials_are_ignored() {
        let (mut nodes, shares, _, holders) = setup(4);
        let me = &mut nodes[3];
        let good = shares[0].sign(&beacon_message(CHAIN, 0, 5)).to_bytes();
        // right partial, claimed by the wrong peer
        assert!(me.on_partial(&holders[1], 5, 0, &good).is_none());
        // index out of range
        assert!(me.on_partial(&holders[0], 5, 99, &good).is_none());
        // far in the future and in the past
        assert!(me
            .on_partial(&holders[0], 5 + MAX_AHEAD + 1, 0, &good)
            .is_none());
        me.prune(10);
        assert!(
            me.on_partial(&holders[0], 5, 0, &good).is_none(),
            "below the floor"
        );
        // not a curve point
        assert!(me
            .on_partial(&holders[0], 11, 0, &[0xAB; G2_BYTES])
            .is_none());
        // a partial for the wrong message, accepted into the collector but never completes it
        let (mut nodes, shares, group, holders) = setup(4);
        let me = &mut nodes[3];
        let wrong = shares[0].sign(b"a different message").to_bytes();
        assert!(me.on_partial(&holders[0], 5, 0, &wrong).is_none());
        let g1 = shares[1].sign(&beacon_message(CHAIN, 0, 5)).to_bytes();
        let g2 = shares[2].sign(&beacon_message(CHAIN, 0, 5)).to_bytes();
        assert!(me.on_partial(&holders[1], 5, 1, &g1).is_none());
        // threshold 3 reached with one bad partial: the collector evicts it and waits
        assert!(me.on_partial(&holders[2], 5, 2, &g2).is_none());
        assert_eq!(me.rejected, 1);
        assert!(me.get(5).is_none());
        // the fourth holder's real partial (its own, from on_commit) finishes the job
        me.on_commit(5).unwrap();
        let b = me
            .get(5)
            .expect("completed with 3 valid of 4 despite 1 bad");
        assert!(b.verify(CHAIN, &group));
        // an extra partial after completion is ignored
        assert!(me
            .on_partial(
                &holders[0],
                5,
                0,
                &shares[0].sign(&beacon_message(CHAIN, 0, 5)).to_bytes()
            )
            .is_none());
    }

    #[test]
    fn peer_supplied_beacons_are_verified() {
        let (mut nodes, shares, group, _) = setup(4);
        let make = |epoch: u64, height: u64, who: &[usize]| {
            let msg = beacon_message(CHAIN, epoch, height);
            let partials: Vec<_> = who.iter().map(|i| (*i, shares[*i].sign(&msg))).collect();
            Beacon {
                epoch,
                height,
                signature: btb_oracle::combine(&group, &msg, &partials)
                    .unwrap()
                    .to_bytes(),
            }
        };
        let me = &mut nodes[0];
        let good = make(0, 7, &[0, 1, 2]);
        let mut flipped = good.clone();
        flipped.signature[10] ^= 1;
        assert!(!me.on_beacon(flipped));
        // right signature, wrong labels
        let mut relabel = good.clone();
        relabel.height = 8;
        assert!(!me.on_beacon(relabel));
        let mut wrong_epoch = good.clone();
        wrong_epoch.epoch = 1;
        assert!(!me.on_beacon(wrong_epoch));
        // a signature of another committee
        let (g2, s2) = trusted_deal(Params::standard(4), &[9; 32]).unwrap();
        let msg = beacon_message(CHAIN, 0, 7);
        let p: Vec<_> = (0..3).map(|i| (i, s2[i].sign(&msg))).collect();
        let foreign = Beacon {
            epoch: 0,
            height: 7,
            signature: btb_oracle::combine(&g2, &msg, &p).unwrap().to_bytes(),
        };
        assert!(!me.on_beacon(foreign));
        assert!(me.get(7).is_none());
        assert!(me.on_beacon(good.clone()));
        assert!(!me.on_beacon(good.clone()), "already known");
        assert_eq!(me.get(7), Some(&good));
        // epochs follow the epoch length (100 blocks)
        assert_eq!(me.epoch_of(100), 0);
        assert_eq!(me.epoch_of(101), 1);
        let _ = Fr::one();
    }

    #[test]
    fn observations_need_a_validator_a_configured_pair_and_progress() {
        let k = |s: u8| SigningKey::from_bytes(&[s; 32]);
        let vals: Vec<[u8; 32]> = (1..=3).map(|s| k(s).verifying_key().to_bytes()).collect();
        let mut p = Prices::new(CHAIN, k(1), None, Duration::from_millis(10), vec![0, 1], 50);
        let sign = |s: u8, pair: u32, price: u128, h: u64| {
            SignedObservation::sign(
                CHAIN,
                &k(s),
                Observation {
                    pair,
                    price,
                    height: h,
                },
            )
        };
        assert!(p.on_observation(sign(2, 0, 10, 5), &vals));
        // not a validator, unknown pair, no progress, another chain, forged signature
        assert!(!p.on_observation(sign(9, 0, 10, 5), &vals));
        assert!(!p.on_observation(sign(2, 7, 10, 6), &vals));
        assert!(!p.on_observation(sign(2, 0, 11, 5), &vals));
        assert!(!p.on_observation(sign(2, 0, 11, 4), &vals));
        let other_chain = SignedObservation::sign(
            CHAIN + 1,
            &k(3),
            Observation {
                pair: 0,
                price: 1,
                height: 1,
            },
        );
        assert!(!p.on_observation(other_chain, &vals));
        let mut forged = sign(3, 0, 5, 9);
        forged.obs.price = 6;
        assert!(!p.on_observation(forged, &vals));
        // newer ones replace; proposals carry fresh ones and drop them once included
        assert!(p.on_observation(sign(2, 0, 12, 6), &vals));
        assert_eq!(p.pending(), 1);
        assert_eq!(p.for_proposal(10).len(), 1);
        assert!(
            p.for_proposal(10 + 51).is_empty(),
            "stale ones are not proposed"
        );
        p.note_included(&p.for_proposal(10));
        assert_eq!(p.pending(), 0);
        // our own observations come from the source, rate-limited
        let mut mine = Prices::new(
            CHAIN,
            k(1),
            Some(PriceSource::new(|pair| (pair == 0).then_some(500))),
            Duration::from_secs(60),
            vec![0, 1],
            50,
        );
        let t0 = Instant::now();
        let first = mine.tick(t0, 12);
        assert_eq!(first.len(), 1);
        assert!(first[0].verify(CHAIN) && first[0].obs.price == 500 && first[0].obs.height == 12);
        assert!(
            mine.tick(t0 + Duration::from_secs(1), 13).is_empty(),
            "interval not elapsed"
        );
        assert_eq!(mine.tick(t0 + Duration::from_secs(61), 14).len(), 1);
    }
}
