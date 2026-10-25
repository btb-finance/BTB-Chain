//! The validator side of the cross-chain key generation (`crate::xdkg`): what a key holder does
//! given the committed chain state. The twin of `crate::dkg_agent` for the FROST keys: no
//! networking and no clock; a node or a test calls [`XAgent::step`] after every executed block
//! and sends the returned operations as transactions signed by the consensus key.
//!
//! The polynomial and the ephemeral key are derived from the consensus secret, the chain id, the
//! scheme and the epoch, so a restarted node deals the same deal again (never two different
//! ones) and only finished shares have to be stored.

use crate::cross::CrossOp;
use crate::state::StateError;
use crate::xdkg::{self, XSession, STATUS_DONE, STATUS_OPEN};
use btb_oracle::dkg_crypto::{self, ShareCtx};
use btb_oracle::frost::{self, Dealing, KeyShare, Scheme};
use btb_runtime::kv::KvStore;
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Blocks to wait for a submitted operation to appear on chain before sending it again.
pub const RESEND_AFTER: u64 = 4;

/// What [`XAgent::step`] found to do.
#[derive(Debug, Default)]
pub struct XStep {
    /// Operations to send now, signed by the consensus key.
    pub ops: Vec<CrossOp>,
    /// Shares derived in this step: `(scheme, origin epoch, share)`.
    pub new_shares: Vec<(Scheme, u64, KeyShare)>,
}

pub struct XAgent {
    key: SigningKey,
    chain_id: u64,
    /// This key's share per `(scheme, origin epoch)`.
    shares: BTreeMap<(Scheme, u64), KeyShare>,
    dealt: BTreeMap<(Scheme, u64), u64>,
    verified: BTreeSet<(Scheme, u64, u32)>,
    complained: BTreeMap<(Scheme, u64, u32), u64>,
    collected: BTreeSet<(Scheme, u64)>,
}

fn entropy(key: &SigningKey, chain_id: u64, scheme: Scheme, target: u64, tag: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"btb.xdkg.entropy.v1");
    h.update(tag);
    h.update([scheme.byte()]);
    h.update(key.to_bytes());
    h.update(chain_id.to_le_bytes());
    h.update(target.to_le_bytes());
    h.finalize().into()
}

impl XAgent {
    pub fn new(chain_id: u64, key: SigningKey) -> XAgent {
        XAgent {
            key,
            chain_id,
            shares: BTreeMap::new(),
            dealt: BTreeMap::new(),
            verified: BTreeSet::new(),
            complained: BTreeMap::new(),
            collected: BTreeSet::new(),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// Register a share obtained elsewhere (loaded from disk).
    pub fn insert_share(&mut self, scheme: Scheme, origin: u64, share: KeyShare) {
        self.shares.insert((scheme, origin), share);
    }

    /// Drop (and return the origins of) the shares of `scheme` older than `origin`.
    pub fn forget_before(&mut self, scheme: Scheme, origin: u64) -> Vec<u64> {
        let old: Vec<u64> = self
            .shares
            .range((scheme, 0)..(scheme, origin))
            .map(|((_, o), _)| *o)
            .collect();
        for o in &old {
            self.shares.remove(&(scheme, *o));
        }
        old
    }

    pub fn share_of_origin(&self, scheme: Scheme, origin: u64) -> Option<&KeyShare> {
        self.shares.get(&(scheme, origin))
    }

    /// This key's share of the key that signs for `epoch` (`None`: not a holder, or the share is
    /// not known).
    pub fn share_for_epoch(
        &self,
        kv: &impl KvStore,
        scheme: Scheme,
        epoch: u64,
    ) -> Result<Option<(xdkg::XKey, KeyShare)>, StateError> {
        let Some((_, k)) = xdkg::key_at(kv, scheme, epoch)? else {
            return Ok(None);
        };
        let me = self.public_key();
        let Some(idx) = k.holder_index(&me) else {
            return Ok(None);
        };
        Ok(self
            .shares
            .get(&(scheme, k.origin))
            .filter(|s| {
                s.index == idx
                    && k.group.public_shares.get(idx).map(Vec::as_slice) == Some(s.public_share())
            })
            .cloned()
            .map(|s| (k, s)))
    }

    /// Build this key's deal for `s`. `None` if this key is not a dealer of the session or lacks
    /// what a reshare needs (the previous share).
    pub fn build_deal(
        &self,
        kv: &impl KvStore,
        s: &XSession,
    ) -> Result<Option<CrossOp>, StateError> {
        let me = self.public_key();
        let Some(i) = s.dealers.iter().position(|k| *k == me) else {
            return Ok(None);
        };
        let params = s.params();
        let ent = entropy(&self.key, self.chain_id, s.scheme, s.target, b"poly");
        let dealing = match s.from {
            None => Dealing::fresh(s.scheme, i, params, &ent),
            Some(from) => {
                let Some(old) = xdkg::xkey(kv, s.scheme, from)? else {
                    return Ok(None);
                };
                match self.shares.get(&(s.scheme, old.origin)) {
                    Some(share)
                        if share.index == i
                            && old.group.public_shares.get(i).map(Vec::as_slice)
                                == Some(share.public_share()) =>
                    {
                        Dealing::reshare(share, params, &ent)
                    }
                    _ => return Ok(None),
                }
            }
        };
        let Ok(dealing) = dealing else {
            return Ok(None);
        };
        let delivery = dkg_crypto::deliver_raw(
            &xdkg::domain(s.scheme),
            self.chain_id,
            s.target,
            i as u32,
            &entropy(&self.key, self.chain_id, s.scheme, s.target, b"eph"),
            &s.committee(),
            &dealing.shares(params.n),
        );
        Ok(Some(CrossOp::Deal {
            scheme: s.scheme,
            target: s.target,
            eph: delivery.eph,
            commitments: dealing.commitments.clone(),
            ciphertexts: delivery.ciphertexts,
        }))
    }

    /// Look at the committed state (the next block will have height `next_height`) and return
    /// what to send and what was learned.
    pub fn step(&mut self, kv: &impl KvStore, next_height: u64) -> Result<XStep, StateError> {
        let mut out = XStep::default();
        for s in xdkg::sessions(kv)? {
            let id = (s.scheme, s.target);
            if s.status == STATUS_OPEN {
                self.step_open(kv, &s, next_height, &mut out)?;
            } else if s.status == STATUS_DONE
                && !self.shares.contains_key(&id)
                && self.collected.insert(id)
            {
                if let Some(share) = self.collect(kv, &s)? {
                    self.shares.insert(id, share.clone());
                    out.new_shares.push((s.scheme, s.target, share));
                }
            }
        }
        Ok(out)
    }

    fn step_open(
        &mut self,
        kv: &impl KvStore,
        s: &XSession,
        next_height: u64,
        out: &mut XStep,
    ) -> Result<(), StateError> {
        let me = self.public_key();
        let board = xdkg::deals(kv, s.scheme, s.target)?;
        let sid = (s.scheme, s.target);
        // deal
        if let Some(i) = s.dealers.iter().position(|k| *k == me) {
            let have = board.iter().any(|(d, _)| *d as usize == i);
            let due = self
                .dealt
                .get(&sid)
                .is_none_or(|at| next_height >= at + RESEND_AFTER);
            if !have && due && next_height <= s.deal_end {
                if let Some(op) = self.build_deal(kv, s)? {
                    self.dealt.insert(sid, next_height);
                    out.ops.push(op);
                }
            }
        }
        // check what was dealt to us
        let committee = s.committee();
        let Some(j) = committee.iter().position(|k| *k == me) else {
            return Ok(());
        };
        let domain = xdkg::domain(s.scheme);
        for (dealer, d) in &board {
            let id = (s.scheme, s.target, *dealer);
            if d.disqualified || self.verified.contains(&id) {
                continue;
            }
            let ctx = ShareCtx {
                chain_id: self.chain_id,
                target: s.target,
                dealer: *dealer,
                recipient: j as u32,
            };
            let good = d
                .ciphertexts
                .get(j)
                .and_then(|ct| dkg_crypto::decrypt_raw(&domain, &self.key, &d.eph, ct, &ctx))
                .is_some_and(|share| frost::verify_share(s.scheme, &d.commitments, j, &share));
            if good {
                self.verified.insert(id);
                continue;
            }
            let due = self
                .complained
                .get(&id)
                .is_none_or(|at| next_height >= at + RESEND_AFTER);
            if due && next_height <= s.window_end {
                if let Some(r) = dkg_crypto::reveal(&self.key, &d.eph) {
                    self.complained.insert(id, next_height);
                    out.ops.push(CrossOp::Complain {
                        scheme: s.scheme,
                        target: s.target,
                        dealer: *dealer,
                        shared: r.shared,
                        proof: r.proof,
                    });
                }
            }
        }
        Ok(())
    }

    /// Derive this key's share from a finished session, if it is a member of the committee.
    fn collect(&self, kv: &impl KvStore, s: &XSession) -> Result<Option<KeyShare>, StateError> {
        let me = self.public_key();
        let Some(j) = s.committee().iter().position(|k| *k == me) else {
            return Ok(None);
        };
        let Some((_, outcome)) = xdkg::outcome(kv, s)? else {
            return Ok(None);
        };
        let board: BTreeMap<u32, xdkg::XDeal> =
            xdkg::deals(kv, s.scheme, s.target)?.into_iter().collect();
        let domain = xdkg::domain(s.scheme);
        let mut received = BTreeMap::new();
        for (dealer, _) in &outcome.weights {
            let Some(d) = board.get(&(*dealer as u32)) else {
                return Ok(None);
            };
            let ctx = ShareCtx {
                chain_id: self.chain_id,
                target: s.target,
                dealer: *dealer as u32,
                recipient: j as u32,
            };
            let share = d
                .ciphertexts
                .get(j)
                .and_then(|ct| dkg_crypto::decrypt_raw(&domain, &self.key, &d.eph, ct, &ctx));
            match share {
                Some(sh) if frost::verify_share(s.scheme, &d.commitments, j, &sh) => {
                    received.insert(*dealer, sh);
                }
                _ => return Ok(None), // a qualified dealer whose share we cannot use
            }
        }
        Ok(outcome.derive_share(j, &received).ok())
    }
}
