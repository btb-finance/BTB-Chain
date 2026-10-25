//! The validator side of the on-chain DKG (`crate::dkg`): what a key holder does, given the
//! committed chain state. No networking and no clock; a node or a test calls [`Agent::step`]
//! after every executed block and sends the returned operations as transactions signed by the
//! consensus key.
//!
//! * **Deal.** While a session accepts deals and this key is a dealer that has not dealt, build
//!   the deal: a fresh random polynomial (fresh key) or one whose constant term is the share of
//!   the previous key (resharing), with every share encrypted to its recipient.
//! * **Check and complain.** Decrypt the share of every deal addressed to this key and check it
//!   against the dealer's commitments. A wrong one produces a [`DkgOp::Complain`] with the
//!   decryption proof.
//! * **Collect.** When a session has finished, derive this key's share of the new group key from
//!   the qualified deals.
//!
//! The polynomial and the ephemeral key are derived from the consensus secret, the chain id and
//! the epoch, so a restarted node deals the same deal again (never two different ones), and
//! nothing but the finished share has to be stored. Anyone holding the consensus secret can
//! decrypt every share addressed to the key anyway, so this exposes nothing new.

use crate::dkg::{self, DkgOp, Session, STATUS_DONE, STATUS_OPEN};
use crate::state::StateError;
use btb_oracle::dkg::verify_share;
use btb_oracle::dkg_crypto::{self, ShareCtx};
use btb_oracle::{Dealing, KeyShare, Participant};
use btb_runtime::kv::KvStore;
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Blocks to wait for a submitted operation to appear on chain before sending it again.
pub const RESEND_AFTER: u64 = 4;

/// What [`Agent::step`] found to do.
#[derive(Debug, Default)]
pub struct Step {
    /// Operations to send now, signed by the consensus key.
    pub ops: Vec<DkgOp>,
    /// Shares derived in this step, by the epoch of the key generation that produced them.
    pub new_shares: Vec<(u64, KeyShare)>,
}

pub struct Agent {
    key: SigningKey,
    chain_id: u64,
    /// This key's share per key generation (`origin` epoch).
    shares: BTreeMap<u64, KeyShare>,
    /// `(target)` -> height at which the deal was last sent.
    dealt: BTreeMap<u64, u64>,
    /// Deals whose share for us checked out.
    verified: BTreeSet<(u64, u32)>,
    /// `(target, dealer)` -> height at which the complaint was last sent.
    complained: BTreeMap<(u64, u32), u64>,
    /// Finished sessions whose share was already derived (or found underivable).
    collected: BTreeSet<u64>,
}

fn entropy(key: &SigningKey, chain_id: u64, target: u64, tag: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"btb.dkg.entropy.v1");
    h.update(tag);
    h.update(key.to_bytes());
    h.update(chain_id.to_le_bytes());
    h.update(target.to_le_bytes());
    h.finalize().into()
}

impl Agent {
    pub fn new(chain_id: u64, key: SigningKey) -> Agent {
        Agent {
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

    /// Register a share obtained elsewhere (loaded from disk, or the interim dealer's).
    pub fn insert_share(&mut self, origin: u64, share: KeyShare) {
        self.shares.insert(origin, share);
    }

    /// Drop (and return the origins of) the shares of key generations older than `origin`.
    pub fn forget_before(&mut self, origin: u64) -> Vec<u64> {
        let old: Vec<u64> = self.shares.range(..origin).map(|(o, _)| *o).collect();
        for o in &old {
            self.shares.remove(o);
        }
        old
    }

    pub fn share_of_origin(&self, origin: u64) -> Option<&KeyShare> {
        self.shares.get(&origin)
    }

    /// This key's share of the key that signs the beacons of `epoch` (`None`: not a holder, or
    /// the share is not known).
    pub fn share_for_epoch(
        &self,
        kv: &impl KvStore,
        epoch: u64,
    ) -> Result<Option<KeyShare>, StateError> {
        let Some(k) = dkg::epoch_key(kv, epoch)? else {
            return Ok(None);
        };
        let me = self.public_key();
        let Some(idx) = k.holders.iter().position(|h| *h == me) else {
            return Ok(None);
        };
        Ok(self
            .shares
            .get(&k.origin)
            .filter(|s| s.index == idx && k.group.public_shares.get(idx) == Some(&s.public_share()))
            .cloned())
    }

    /// Build this key's deal for `s`. `None` if this key is not a dealer of the session or
    /// lacks what a reshare needs (the previous share).
    pub fn build_deal(&self, kv: &impl KvStore, s: &Session) -> Result<Option<DkgOp>, StateError> {
        let me = self.public_key();
        let Some(i) = s.dealers.iter().position(|k| *k == me) else {
            return Ok(None);
        };
        let params = s.params();
        let ent = entropy(&self.key, self.chain_id, s.target, b"poly");
        let dealing = match s.from {
            None => Dealing::fresh(i, params, &ent),
            Some(from) => {
                let Some(old) = dkg::epoch_key(kv, from)? else {
                    return Ok(None);
                };
                match self.shares.get(&old.origin) {
                    Some(share)
                        if share.index == i
                            && old.group.public_shares.get(i) == Some(&share.public_share()) =>
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
        let shares: Vec<_> = (0..params.n).map(|j| dealing.share_for(j)).collect();
        let delivery = dkg_crypto::deliver(
            self.chain_id,
            s.target,
            i as u32,
            &entropy(&self.key, self.chain_id, s.target, b"eph"),
            &s.committee(),
            &shares,
        );
        Ok(Some(DkgOp::Deal {
            target: s.target,
            eph: delivery.eph,
            commitments: dealing.commitments.iter().map(|c| c.to_bytes()).collect(),
            ciphertexts: delivery.ciphertexts,
        }))
    }

    /// Look at the committed state (the next block will have height `next_height`) and return
    /// what to send and what was learned.
    pub fn step(&mut self, kv: &impl KvStore, next_height: u64) -> Result<Step, StateError> {
        let mut out = Step::default();
        for s in dkg::sessions(kv)? {
            if s.status == STATUS_OPEN {
                self.step_open(kv, &s, next_height, &mut out)?;
            } else if s.status == STATUS_DONE
                && !self.shares.contains_key(&s.target)
                && self.collected.insert(s.target)
            {
                if let Some(share) = self.collect(kv, &s)? {
                    self.shares.insert(s.target, share.clone());
                    out.new_shares.push((s.target, share));
                }
            }
        }
        Ok(out)
    }

    fn step_open(
        &mut self,
        kv: &impl KvStore,
        s: &Session,
        next_height: u64,
        out: &mut Step,
    ) -> Result<(), StateError> {
        let me = self.public_key();
        let board = dkg::deals(kv, s.target)?;
        // deal
        if let Some(i) = s.dealers.iter().position(|k| *k == me) {
            let have = board.iter().any(|(d, _)| *d as usize == i);
            let due = self
                .dealt
                .get(&s.target)
                .is_none_or(|at| next_height >= at + RESEND_AFTER);
            if !have && due && next_height <= s.deal_end {
                if let Some(op) = self.build_deal(kv, s)? {
                    self.dealt.insert(s.target, next_height);
                    out.ops.push(op);
                }
            }
        }
        // check what was dealt to us
        let committee = s.committee();
        let Some(j) = committee.iter().position(|k| *k == me) else {
            return Ok(());
        };
        for (dealer, d) in &board {
            let id = (s.target, *dealer);
            if d.disqualified || self.verified.contains(&id) {
                continue;
            }
            let ctx = ShareCtx {
                chain_id: self.chain_id,
                target: s.target,
                dealer: *dealer,
                recipient: j as u32,
            };
            let good = d.points().is_some_and(|points| {
                d.ciphertexts
                    .get(j)
                    .and_then(|ct| dkg_crypto::decrypt(&self.key, &d.eph, ct, &ctx))
                    .is_some_and(|share| verify_share(&points, j, &share))
            });
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
                    out.ops.push(DkgOp::Complain {
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
    fn collect(&self, kv: &impl KvStore, s: &Session) -> Result<Option<KeyShare>, StateError> {
        let me = self.public_key();
        let Some(j) = s.committee().iter().position(|k| *k == me) else {
            return Ok(None);
        };
        let Some((t, outcome)) = dkg::outcome(kv, s)? else {
            return Ok(None);
        };
        let board: BTreeMap<u32, dkg::Deal> = dkg::deals(kv, s.target)?.into_iter().collect();
        let mut p = Participant::new(j);
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
                .and_then(|ct| dkg_crypto::decrypt(&self.key, &d.eph, ct, &ctx));
            match share {
                Some(sh) if p.receive(&t, *dealer, sh) => {}
                _ => return Ok(None), // a qualified dealer whose share we cannot use
            }
        }
        Ok(p.key_share(&t, &outcome).ok())
    }
}
