//! Node side of signed reports for other chains (`docs/ORACLES.md`, section 4).
//!
//! A contract queues a report with the `report_request` host call; the chain keeps it under
//! `52 01 id` (`btb_chain::reports`). Every validator that holds a share of the beacon key of the
//! report's epoch signs
//! `btb_oracle::report::report_message(chain_id, contract, id, SHA256(data))` with it and
//! gossips the partial (`OracleMsg::ReportPartial`). Anyone who has `threshold` valid partials
//! combines them into the one group signature, which is the same bytes on every node (BLS
//! signatures are unique). A node that misses partials asks with `ReportWant` and gets the finished
//! `ReportSig`, which it checks before use.
//!
//! Nothing here needs consensus: the signature is a deterministic function of the report and the
//! key, and the report is public. It is kept in memory; after a restart the node re-signs the
//! reports still in chain state (retention `btb_chain::reports::RETENTION_BLOCKS`) and collects
//! the partials again. Relayers read the result over `btb_oracleReport`.
//!
//! Partials are authenticated like beacon partials: the sender must be the validator whose
//! consensus key is the holder named in the message. A partial that arrives before this node has
//! executed the block that holds the report is parked (bounded) and used once the report is known.

use crate::oracle::{Beacons, OracleMsg};
use btb_chain::reports::{self, Report};
use btb_oracle::bls::{G2, G2_BYTES};
use btb_oracle::report::report_message;
use btb_oracle::{Collector, GroupKey};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Reports remembered at once.
const MAX_ITEMS: usize = 1024;
/// Parked partials: reports and partials per report.
const MAX_EARLY_IDS: usize = 256;
const MAX_EARLY_PER_ID: usize = 64;
/// A report still unsigned this long after it was seen makes the node ask its peers.
const WANT_AFTER: Duration = Duration::from_millis(500);
const WANT_EVERY: Duration = Duration::from_millis(1_000);
/// Reports scanned from chain state per pass.
const SCAN: usize = 64;

/// A partial that arrived before its report: `(sender key, holder index, partial)`.
type Parked = ([u8; 32], u32, [u8; G2_BYTES]);

struct Item {
    report: Report,
    epoch: u64,
    group: Option<GroupKey>,
    holders: Vec<[u8; 32]>,
    collector: Option<Collector>,
    signed_by_me: bool,
    sig: Option<[u8; G2_BYTES]>,
    seen: Instant,
    last_want: Option<Instant>,
}

/// Everything the driver keeps for reports.
pub struct ReportNode {
    chain_id: u64,
    next: u64,
    items: BTreeMap<u64, Item>,
    /// `(sender key, holder index, partial)` that arrived before the report was known.
    early: BTreeMap<u64, Vec<Parked>>,
}

impl ReportNode {
    pub fn new(chain_id: u64) -> Self {
        ReportNode {
            chain_id,
            next: 0,
            items: BTreeMap::new(),
            early: BTreeMap::new(),
        }
    }

    /// The finished signature of report `id`, if this node has it.
    pub fn signature(&self, id: u64) -> Option<[u8; G2_BYTES]> {
        self.items.get(&id).and_then(|i| i.sig)
    }

    /// The group key and epoch the report is signed under, once known.
    pub fn group_of(&self, id: u64) -> Option<(u64, &GroupKey)> {
        let i = self.items.get(&id)?;
        Some((i.epoch, i.group.as_ref()?))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn message(&self, r: &Report) -> Vec<u8> {
        report_message(self.chain_id, &r.contract, r.id, &r.data)
    }

    /// Look at new reports in chain state and make progress on every unsigned one: attach the
    /// key of its epoch, release our partial, combine. Returns the messages to broadcast.
    pub fn step<S: btb_runtime::KvStore>(
        &mut self,
        state: &btb_chain::ChainState<S>,
        beacons: &Beacons,
        now: Instant,
    ) -> Vec<OracleMsg> {
        let mut out = Vec::new();
        // new reports (ids ascend with height, so scanning forward from `next` finds them all)
        while let Ok(list) = reports::list_from(state.store(), self.next, SCAN) {
            if list.is_empty() {
                break;
            }
            for r in list {
                self.next = r.id + 1;
                let epoch = beacons.epoch_of(r.height);
                self.items.insert(
                    r.id,
                    Item {
                        report: r,
                        epoch,
                        group: None,
                        holders: Vec::new(),
                        collector: None,
                        signed_by_me: false,
                        sig: None,
                        seen: now,
                        last_want: None,
                    },
                );
            }
        }
        while self.items.len() > MAX_ITEMS {
            self.items.pop_first();
        }
        let ids: Vec<u64> = self
            .items
            .iter()
            .filter(|(_, i)| i.sig.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let msg = {
                let i = &self.items[&id];
                self.message(&i.report)
            };
            let early = self.early.remove(&id).unwrap_or_default();
            let Some(i) = self.items.get_mut(&id) else {
                continue;
            };
            if i.collector.is_none() {
                let Some((group, holders, _)) = beacons.epoch_key(i.epoch) else {
                    // key not known yet: keep what we parked and try again later
                    if !early.is_empty() {
                        self.early.insert(id, early);
                    }
                    continue;
                };
                i.group = Some(group.clone());
                i.holders = holders.to_vec();
                i.collector = Some(Collector::new(group.clone(), msg.clone()));
            }
            // parked partials, now that the holder list is known
            for (from, index, sig) in early {
                accept(i, &from, index, &sig);
            }
            if !i.signed_by_me {
                if let Some((_, _, Some(share))) = beacons.epoch_key(i.epoch) {
                    i.signed_by_me = true;
                    let partial = share.sign(&msg);
                    let index = share.index;
                    if let Some(c) = i.collector.as_mut() {
                        if let Some(s) = c.add(index, partial) {
                            i.sig = Some(s.to_bytes());
                        }
                    }
                    out.push(OracleMsg::ReportPartial {
                        id,
                        index: index as u32,
                        sig: partial.to_bytes(),
                    });
                }
            }
            if i.sig.is_none()
                && now.duration_since(i.seen) >= WANT_AFTER
                && i.last_want
                    .is_none_or(|l| now.duration_since(l) >= WANT_EVERY)
            {
                i.last_want = Some(now);
                out.push(OracleMsg::ReportWant { id });
            }
        }
        out
    }

    /// A partial from the peer whose consensus key is `from`.
    pub fn on_partial(&mut self, from: &[u8; 32], id: u64, index: u32, sig: &[u8; G2_BYTES]) {
        if let Some(i) = self.items.get_mut(&id) {
            if i.sig.is_some() {
                return;
            }
            if i.collector.is_some() {
                accept(i, from, index, sig);
                return;
            }
        }
        if id < self.next && !self.items.contains_key(&id) {
            return; // an old report this node dropped
        }
        if !self.early.contains_key(&id) && self.early.len() >= MAX_EARLY_IDS {
            return;
        }
        let v = self.early.entry(id).or_default();
        if v.len() < MAX_EARLY_PER_ID && !v.iter().any(|(f, ..)| f == from) {
            v.push((*from, index, *sig));
        }
    }

    /// A finished signature from a peer: kept only if it verifies under the epoch's group key.
    pub fn on_signature(&mut self, id: u64, sig: &[u8; G2_BYTES]) -> bool {
        let msg = match self.items.get(&id) {
            Some(i) if i.sig.is_none() && i.group.is_some() => self.message(&i.report),
            _ => return false,
        };
        let Some(i) = self.items.get_mut(&id) else {
            return false;
        };
        let (Some(group), Some(g2)) = (&i.group, G2::from_bytes(sig)) else {
            return false;
        };
        if btb_oracle::bls::verify(&group.public_key, &msg, &g2) {
            i.sig = Some(*sig);
            i.collector = None;
            true
        } else {
            false
        }
    }
}

fn accept(i: &mut Item, from: &[u8; 32], index: u32, sig: &[u8; G2_BYTES]) {
    if i.holders.get(index as usize) != Some(from) {
        return;
    }
    let (Some(c), Some(g2)) = (i.collector.as_mut(), G2::from_bytes(sig)) else {
        return;
    };
    if let Some(s) = c.add(index as usize, g2) {
        i.sig = Some(s.to_bytes());
    }
}
