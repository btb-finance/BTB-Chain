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

/// Largest oracle frame body accepted (the biggest message is an action vote: its result is at
/// most `btb_chain::actions::MAX_RESULT_BYTES`, plus request id, key, signature and tags).
pub const MAX_ORACLE_MSG: usize = 2048;
/// Partials for heights this far above the executed height are accepted (bounded buffering).
pub const MAX_AHEAD: u64 = 256;

const TAG_PARTIAL: u8 = 1;
const TAG_BEACON: u8 = 2;
const TAG_WANT: u8 = 3;
const TAG_PRICE: u8 = 4;
const TAG_ACTION: u8 = 20;
const TAG_REPORT_PARTIAL: u8 = 21;
const TAG_REPORT_SIG: u8 = 22;
const TAG_REPORT_WANT: u8 = 23;
const TAG_CROSS: u8 = 30;

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
    /// A validator's signed answer to a pending HTTPS action (`crate::actions`).
    Action(btb_chain::ActionVote),
    /// A holder's partial signature on report `id` (`crate::reports`).
    ReportPartial {
        id: u64,
        index: u32,
        sig: [u8; G2_BYTES],
    },
    /// A finished report signature (answer to `ReportWant`).
    ReportSig { id: u64, sig: [u8; G2_BYTES] },
    /// "Send me the signature of report `id`".
    ReportWant { id: u64 },
    /// A cross-chain signing message (`crate::cross::XMsg`, encoded).
    Cross(Vec<u8>),
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
            OracleMsg::Action(v) => {
                o.push(TAG_ACTION);
                btb_codec::Encode::encode_to(v, &mut o);
            }
            OracleMsg::ReportPartial { id, index, sig } => {
                o.push(TAG_REPORT_PARTIAL);
                o.extend_from_slice(&id.to_le_bytes());
                o.extend_from_slice(&index.to_le_bytes());
                o.extend_from_slice(sig);
            }
            OracleMsg::ReportSig { id, sig } => {
                o.push(TAG_REPORT_SIG);
                o.extend_from_slice(&id.to_le_bytes());
                o.extend_from_slice(sig);
            }
            OracleMsg::ReportWant { id } => {
                o.push(TAG_REPORT_WANT);
                o.extend_from_slice(&id.to_le_bytes());
            }
            OracleMsg::Cross(b) => {
                o.push(TAG_CROSS);
                o.extend_from_slice(b);
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
            TAG_ACTION => btb_codec::Decode::decode(body).ok().map(OracleMsg::Action),
            TAG_REPORT_PARTIAL if body.len() == 8 + 4 + G2_BYTES => {
                Some(OracleMsg::ReportPartial {
                    id: u64::from_le_bytes(body[..8].try_into().ok()?),
                    index: u32::from_le_bytes(body[8..12].try_into().ok()?),
                    sig: body[12..].try_into().ok()?,
                })
            }
            TAG_REPORT_SIG if body.len() == 8 + G2_BYTES => Some(OracleMsg::ReportSig {
                id: u64::from_le_bytes(body[..8].try_into().ok()?),
                sig: body[8..].try_into().ok()?,
            }),
            TAG_CROSS if !body.is_empty() => Some(OracleMsg::Cross(body.to_vec())),
            TAG_REPORT_WANT if body.len() == 8 => Some(OracleMsg::ReportWant {
                id: u64::from_le_bytes(body.try_into().ok()?),
            }),
            _ => None,
        }
    }

    pub fn frame(&self) -> Vec<u8> {
        crate::wire::frame(crate::wire::KIND_ORACLE, &self.encode())
    }
}

/// A borrowed view of an epoch's key: group key, holders, and this node's share if it has one.
pub type EpochKeyView<'a> = (&'a GroupKey, &'a [[u8; 32]], Option<&'a KeyShare>);

/// The key of one epoch as the beacons need it.
struct EpochKeys {
    group: GroupKey,
    holders: Vec<[u8; 32]>,
    /// This node's share, if it is a holder and has derived it.
    share: Option<KeyShare>,
}

/// Collects partials and keeps the finished beacons of recent heights. Every epoch has its own
/// key (`btb_chain::dkg`): partials and beacons of an epoch whose key this node does not know
/// yet are ignored (the chain state it is catching up on supplies the key before it is needed).
pub struct Beacons {
    chain_id: u64,
    epoch_len: u64,
    keys: BTreeMap<u64, EpochKeys>,
    collectors: BTreeMap<u64, Collector>,
    ready: BTreeMap<u64, Beacon>,
    /// Heights our replica committed (the release rule allows signing them).
    committed: BTreeSet<u64>,
    signed: BTreeSet<u64>,
    /// Highest executed height: older entries are dropped.
    floor: u64,
    /// Partials that failed verification (statistics for `btb_status`).
    pub rejected: u64,
}

impl Beacons {
    pub fn new(chain_id: u64, epoch_len: u64) -> Self {
        Beacons {
            chain_id,
            epoch_len: epoch_len.max(1),
            keys: BTreeMap::new(),
            collectors: BTreeMap::new(),
            ready: BTreeMap::new(),
            committed: BTreeSet::new(),
            signed: BTreeSet::new(),
            floor: 0,
            rejected: 0,
        }
    }

    /// Install (or update the share of) the key of `epoch`. A key for the epoch that is already
    /// known with the same group and holders only gains the share.
    pub fn set_key(
        &mut self,
        epoch: u64,
        group: GroupKey,
        holders: Vec<[u8; 32]>,
        share: Option<KeyShare>,
    ) {
        match self.keys.get_mut(&epoch) {
            Some(k) if k.group == group && k.holders == holders => {
                if share.is_some() {
                    k.share = share;
                }
            }
            _ => {
                self.keys.insert(
                    epoch,
                    EpochKeys {
                        group,
                        holders,
                        share,
                    },
                );
            }
        }
    }

    pub fn has_key(&self, epoch: u64) -> bool {
        self.keys.contains_key(&epoch)
    }

    /// The group key, holders and (if this node holds one) the share of `epoch`'s key.
    pub fn epoch_key(&self, epoch: u64) -> Option<EpochKeyView<'_>> {
        let k = self.keys.get(&epoch)?;
        Some((&k.group, &k.holders, k.share.as_ref()))
    }

    pub fn epoch_of(&self, height: u64) -> u64 {
        height.saturating_sub(1) / self.epoch_len
    }

    fn message(&self, height: u64) -> Vec<u8> {
        beacon_message(self.chain_id, self.epoch_of(height), height)
    }

    /// Does this node hold a share of the key of the epoch of `height`?
    pub fn holds_share(&self) -> bool {
        self.keys.values().any(|k| k.share.is_some())
    }

    pub fn get(&self, height: u64) -> Option<&Beacon> {
        self.ready.get(&height)
    }

    /// Our replica committed block `height`: release our partial for it (once), if we hold a
    /// share of that epoch's key. Returns the message to broadcast. Never called for a height
    /// the replica has not committed.
    pub fn on_commit(&mut self, height: u64) -> Option<OracleMsg> {
        self.committed.insert(height);
        self.sign(height)
    }

    /// Release the partials of committed heights whose key became known after the commit.
    pub fn flush(&mut self) -> Vec<OracleMsg> {
        let pending: Vec<u64> = self
            .committed
            .iter()
            .copied()
            .filter(|h| !self.signed.contains(h))
            .collect();
        pending.into_iter().filter_map(|h| self.sign(h)).collect()
    }

    fn sign(&mut self, height: u64) -> Option<OracleMsg> {
        if !self.committed.contains(&height)
            || self.signed.contains(&height)
            || height <= self.floor
        {
            return None;
        }
        let share = self.keys.get(&self.epoch_of(height))?.share.clone()?;
        self.signed.insert(height);
        let sig = share.sign(&self.message(height));
        self.accept_partial(height, share.index, sig);
        Some(OracleMsg::Partial {
            height,
            index: share.index as u32,
            sig: sig.to_bytes(),
        })
    }

    fn accept_partial(&mut self, height: u64, index: usize, sig: G2) -> bool {
        if self.ready.contains_key(&height) {
            return false;
        }
        let epoch = self.epoch_of(height);
        let msg = self.message(height);
        let Some(keys) = self.keys.get(&epoch) else {
            return false;
        };
        let c = self
            .collectors
            .entry(height)
            .or_insert_with(|| Collector::new(keys.group.clone(), msg));
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
        let holder = self
            .keys
            .get(&self.epoch_of(height))
            .and_then(|k| k.holders.get(index));
        if holder != Some(from)
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
        {
            return false;
        }
        let Some(keys) = self.keys.get(&b.epoch) else {
            return false;
        };
        if !b.verify(self.chain_id, &keys.group) {
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
        self.committed.retain(|h| *h + 64 > floor);
        // keys of epochs that ended before the oldest beacon we still serve
        let oldest = self.epoch_of(floor.saturating_sub(64).max(1));
        self.keys.retain(|e, _| *e >= oldest);
    }
}

/// Where a node gets prices from: a function (tests, local testnets) or the exchange fetcher of
/// `crate::price_sources`.
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
    /// This validator's part of the beacon key generation and its key shares.
    pub dkg: crate::dkg::DkgNode,
    /// New shares appeared since the keys were last installed.
    pub shares_changed: bool,
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
        let Some(og) = btb_chain::oracle::load(state.store()).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let cfg = state.config();
        let me = p.key.verifying_key().to_bytes();
        if let Some(share) = &p.oracle_share {
            // The interim trusted-dealer share: it must belong to this key and the genesis key.
            let key0 = btb_chain::oracle::epoch_key(state.store(), 0).map_err(|e| e.to_string())?;
            let ok = key0.is_some_and(|k| {
                k.group.public_shares.get(share.index) == Some(&share.public_share())
                    && k.holders.get(share.index) == Some(&me)
            });
            if !ok {
                return Err(
                    "the oracle share does not belong to this validator key and the genesis beacon key"
                        .into(),
                );
            }
        }
        let pairs: Vec<u32> = og.pairs.iter().map(|p| p.id).collect();
        let source = match (&p.price_source, p.price_sources.is_empty()) {
            (Some(s), _) => Some(s.clone()),
            (None, true) => None,
            (None, false) => {
                let pairs = crate::price_sources::resolve(&p.price_sources, &og.pairs)?;
                Some(
                    crate::price_sources::PriceFetcher::spawn(
                        pairs,
                        Duration::from_millis(p.price_fetch_interval_ms.max(100)),
                        p.fetch_policy.clone(),
                    )
                    .into_source(),
                )
            }
        };
        let store = crate::dkg::ShareStore::new(p.dkg_dir.clone(), p.key_passphrase.clone());
        let dkg =
            crate::dkg::DkgNode::new(cfg.chain_id, p.key.clone(), store, p.oracle_share.clone())?;
        Ok(Some(OracleNode {
            beacons: Beacons::new(cfg.chain_id, cfg.staking.epoch_len),
            prices: Prices::new(
                cfg.chain_id,
                p.key.clone(),
                source,
                Duration::from_millis(p.price_interval_ms.max(1)),
                pairs,
                og.max_age_blocks,
            ),
            dkg,
            shares_changed: true,
            waiting: None,
        }))
    }

    /// Install the keys of the current and the next epoch from the committed state, with this
    /// node's shares. Cheap when nothing changed.
    pub fn sync_keys<S: btb_runtime::KvStore>(
        &mut self,
        state: &btb_chain::ChainState<S>,
        epoch: u64,
    ) {
        for e in epoch..=epoch + 1 {
            if self.beacons.has_key(e) && !self.shares_changed {
                continue;
            }
            let Ok(Some(k)) = btb_chain::oracle::epoch_key(state.store(), e) else {
                continue;
            };
            let share = self
                .dkg
                .agent
                .share_for_epoch(state.store(), e)
                .ok()
                .flatten();
            self.beacons.set_key(e, k.group, k.holders, share);
        }
        self.shares_changed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_oracle::bls::Fr;
    use btb_oracle::{trusted_deal, Observation, Params};

    const CHAIN: u64 = 7;

    fn node(group: &GroupKey, holders: &[[u8; 32]], share: Option<KeyShare>) -> Beacons {
        let mut b = Beacons::new(CHAIN, 100);
        b.set_key(0, group.clone(), holders.to_vec(), share);
        b
    }

    fn setup(n: usize) -> (Vec<Beacons>, Vec<KeyShare>, GroupKey, Vec<[u8; 32]>) {
        let (group, shares) = trusted_deal(Params::standard(n), &[8; 32]).unwrap();
        let holders: Vec<[u8; 32]> = (0..n).map(|i| [i as u8 + 1; 32]).collect();
        let nodes = shares
            .iter()
            .map(|s| node(&group, &holders, Some(s.clone())))
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
            OracleMsg::Action(btb_chain::ActionVote::sign(
                CHAIN,
                &SigningKey::from_bytes(&[1; 32]),
                [4; 32],
                0,
                vec![9; btb_chain::actions::MAX_RESULT_BYTES],
            )),
            OracleMsg::ReportPartial {
                id: 3,
                index: 1,
                sig,
            },
            OracleMsg::ReportSig { id: 3, sig },
            OracleMsg::ReportWant { id: 3 },
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
        let mut follower = node(&group, &[[1; 32]; 4], None);
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
    fn every_epoch_has_its_own_key_and_a_partial_waits_for_it() {
        // epoch length 100: heights 1..=100 are epoch 0, 101..=200 epoch 1
        let (g0, s0) = trusted_deal(Params::standard(4), &[8; 32]).unwrap();
        let (g1, s1) = trusted_deal(Params::standard(4), &[9; 32]).unwrap();
        let h0: Vec<[u8; 32]> = (1..=4).map(|i| [i as u8; 32]).collect();
        let h1: Vec<[u8; 32]> = vec![[5; 32], [2; 32], [3; 32], [4; 32]]; // validator 1 left, 5 joined
        let mut me = Beacons::new(CHAIN, 100);
        me.set_key(0, g0.clone(), h0.clone(), Some(s0[1].clone()));
        // our replica commits a block of epoch 1 before we know epoch 1's key: nothing is signed
        assert!(me.on_commit(101).is_none());
        assert!(!me.has_key(1));
        // epoch 1's key arrives (we are not a holder of it): still nothing to sign
        me.set_key(1, g1.clone(), h1.clone(), None);
        assert!(me.flush().is_empty());
        // a share of the new key arrives later: the waiting height is signed at once, once
        me.set_key(1, g1.clone(), h1.clone(), Some(s1[1].clone()));
        let out = me.flush();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            OracleMsg::Partial {
                height: 101,
                index: 1,
                ..
            }
        ));
        assert!(me.flush().is_empty());
        // partials are checked against the holders of THEIR epoch's key
        let m = beacon_message(CHAIN, 1, 102);
        let sig = s1[2].sign(&m).to_bytes();
        // the old holder at index 0 (key [1;32]) is not a holder in epoch 1: [5;32] is
        assert!(me.on_partial(&h0[0], 102, 0, &sig).is_none());
        assert!(
            me.on_partial(&h1[2], 102, 2, &sig).is_none(),
            "one partial is not enough"
        );
        // an old-key share signing for epoch 1 never completes the beacon
        let old = s0[0].sign(&m).to_bytes();
        assert!(me.on_partial(&h1[0], 102, 0, &old).is_none());
        // heights of an epoch whose key we do not have are ignored
        assert!(me.on_partial(&h0[0], 250, 0, &sig).is_none());
        // a finished beacon of epoch 1 is verified under epoch 1's key only
        let make = |g: &GroupKey, shares: &[KeyShare], height: u64| {
            let msg = beacon_message(CHAIN, 1, height);
            let partials: Vec<_> = shares
                .iter()
                .take(3)
                .map(|s| (s.index, s.sign(&msg)))
                .collect();
            Beacon {
                epoch: 1,
                height,
                signature: btb_oracle::combine(g, &msg, &partials).unwrap().to_bytes(),
            }
        };
        assert!(me.on_beacon(make(&g1, &s1, 103)));
        assert!(
            !me.on_beacon(make(&g0, &s0, 104)),
            "the old committee cannot sign epoch 1"
        );
        // executed heights are never signed afterwards
        me.prune(105);
        assert!(me.on_commit(104).is_none());
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
