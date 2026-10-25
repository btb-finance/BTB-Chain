//! Distributed key generation: joint-Feldman with complaints and justifications, plus
//! resharing to a new committee.
//!
//! This module is the *deterministic core*: it never touches a network or a clock. A host
//! (the chain, or a test) provides a **public, ordered, authenticated broadcast board** (BTB
//! uses the consensus chain itself) and **authenticated private channels** (BTB uses the
//! authenticated, encrypted validator transport). The protocol:
//!
//! 1. *Deal.* Every dealer `i` picks a random polynomial `f_i` of degree `t - 1`, broadcasts
//!    the Feldman commitments `C_{i,k} = a_{i,k} * g1`, and sends `s_{ij} = f_i(j + 1)`
//!    privately to every participant `j`.
//! 2. *Complain.* Participant `j` checks `s_{ij} * g1 == sum_k C_{i,k} * (j + 1)^k`. If the
//!    share is missing or wrong it broadcasts a complaint against `i`.
//! 3. *Justify.* A dealer accused by `j` broadcasts `s_{ij}`. Everyone checks it against the
//!    commitments: a correct share clears the complaint (the accuser was wrong or lying); a
//!    wrong share, or no answer, disqualifies the dealer.
//! 4. *Finalize.* The qualified dealers `Q` are those with commitments and no open complaint.
//!    The group key is `sum_{i in Q} C_{i,0}`, the secret share of `j` is `sum_{i in Q} s_{ij}`
//!    and the public share of `j` is `sum_{i in Q} sum_k C_{i,k} * (j + 1)^k`; all of it is
//!    computable from the board, so everyone derives the same [`GroupKey`].
//!
//! *Resharing* moves the same secret to a new committee (new size, new threshold) without ever
//! reconstructing it: each old holder deals its own share (the dealer's constant commitment
//! must equal its published public share), and the new holders combine with Lagrange
//! coefficients of the qualified old holders.
//!
//! Security notes (see `docs/ORACLES.md`): joint-Feldman lets a rushing adversary bias the
//! distribution of the group public key; Gennaro, Jarecki, Krawczyk and Rabin showed this does
//! not weaken discrete-log signatures such as BLS (the same choice drand makes). A complaint
//! publishes one share of one accuser; a false complaint therefore reveals nothing the
//! accuser did not already know.

use crate::bls::{Fr, G1};
use crate::threshold::{lagrange, threshold_for, GroupKey, KeyError, KeyShare, MAX_HOLDERS};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Committee size and threshold of the *new* committee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub n: usize,
    pub t: usize,
}

impl Params {
    /// Standard parameters: threshold above two thirds.
    pub fn standard(n: usize) -> Params {
        Params {
            n,
            t: threshold_for(n),
        }
    }

    fn ok(&self) -> bool {
        self.t >= 1 && self.t <= self.n && self.n <= MAX_HOLDERS
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DkgError {
    BadParams,
    /// Dealer index outside the dealing committee.
    UnknownDealer,
    /// The dealer already posted commitments.
    DuplicateCommitments,
    /// Wrong number of commitments, or the reshare constant does not match the old key.
    BadCommitments,
    /// Complaint about a dealer that posted no commitments, or an out-of-range accuser.
    BadComplaint,
    /// Fewer qualified dealers than needed.
    NotEnoughDealers {
        have: usize,
        need: usize,
    },
    /// The participant has no valid share from a dealer it needs.
    MissingShare {
        dealer: usize,
    },
    Key(KeyError),
}

impl From<KeyError> for DkgError {
    fn from(e: KeyError) -> Self {
        DkgError::Key(e)
    }
}

/// Expand 32 bytes of entropy into the `k`-th field element (64 hash bytes reduced mod `r`,
/// so the bias is about `2^-256`).
fn expand(entropy: &[u8; 32], tag: &[u8], k: u32) -> Fr {
    let mut wide = [0u8; 64];
    for (c, chunk) in wide.chunks_mut(32).enumerate() {
        let mut h = Sha256::new();
        h.update(b"btb.dkg.expand.v1");
        h.update(entropy);
        h.update(tag);
        h.update(k.to_le_bytes());
        h.update([c as u8]);
        chunk.copy_from_slice(&h.finalize());
    }
    Fr::from_bytes_reduce(&wide)
}

fn eval_poly(coeffs: &[Fr], x: u64) -> Fr {
    let fx = Fr::from_u64(x);
    coeffs
        .iter()
        .rev()
        .fold(Fr::zero(), |acc, c| acc.mul(&fx).add(c))
}

/// Evaluate the committed polynomial at `x`: `sum_k C_k * x^k`.
pub fn eval_commitments(commitments: &[G1], x: u64) -> G1 {
    commitments
        .iter()
        .rev()
        .fold(G1::identity(), |acc, c| acc.mul_u64(x).add(c))
}

/// Does `share` lie on the polynomial committed to by `commitments`, as the share of the
/// participant with 0-based `recipient` index?
pub fn verify_share(commitments: &[G1], recipient: usize, share: &Fr) -> bool {
    G1::generator().mul(share) == eval_commitments(commitments, recipient as u64 + 1)
}

/// One dealer's contribution: its secret polynomial, from which it derives the public
/// commitments, the private shares and justifications.
pub struct Dealing {
    pub dealer: usize,
    coeffs: Vec<Fr>,
    pub commitments: Vec<G1>,
}

impl Dealing {
    /// A fresh random polynomial of degree `t - 1` (fresh DKG).
    pub fn fresh(dealer: usize, params: Params, entropy: &[u8; 32]) -> Result<Dealing, DkgError> {
        if !params.ok() {
            return Err(DkgError::BadParams);
        }
        let coeffs: Vec<Fr> = (0..params.t as u32)
            .map(|k| expand(entropy, b"fresh", k))
            .collect();
        Ok(Self::from_coeffs(dealer, coeffs))
    }

    /// A random polynomial of degree `t - 1` whose constant term is `old.secret` (resharing).
    /// `dealer` is the holder's index in the *old* committee.
    pub fn reshare(
        old: &KeyShare,
        params: Params,
        entropy: &[u8; 32],
    ) -> Result<Dealing, DkgError> {
        if !params.ok() {
            return Err(DkgError::BadParams);
        }
        let mut coeffs = vec![old.secret];
        coeffs.extend((1..params.t as u32).map(|k| expand(entropy, b"reshare", k)));
        Ok(Self::from_coeffs(old.index, coeffs))
    }

    fn from_coeffs(dealer: usize, coeffs: Vec<Fr>) -> Dealing {
        let commitments = coeffs.iter().map(|c| G1::generator().mul(c)).collect();
        Dealing {
            dealer,
            coeffs,
            commitments,
        }
    }

    /// The private share for participant `recipient` (0-based). Also what a justification
    /// publishes.
    pub fn share_for(&self, recipient: usize) -> Fr {
        eval_poly(&self.coeffs, recipient as u64 + 1)
    }
}

/// What kind of key generation a transcript records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Dealers are the new committee members themselves.
    Fresh,
    /// Dealers are holders of `old`; the result is the same group key on the new committee.
    Reshare { old: GroupKey },
}

/// The public record of one key generation: everything on the broadcast board. Processing it
/// is deterministic, so every observer computes the same outcome.
#[derive(Debug, Clone)]
pub struct Transcript {
    mode: Mode,
    params: Params,
    commitments: BTreeMap<usize, Vec<G1>>,
    /// Open complaints `(dealer, accuser)`.
    open: BTreeSet<(usize, usize)>,
    /// Shares revealed in justifications (valid ones only).
    revealed: BTreeMap<(usize, usize), Fr>,
    disqualified: BTreeSet<usize>,
}

/// The result of a finished key generation.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Qualified dealers, ascending.
    pub qualified: Vec<usize>,
    pub group: GroupKey,
    /// The dealers whose contributions make up the shares, with their combination weights
    /// (all `1` for a fresh DKG, the Lagrange coefficients for resharing).
    pub weights: Vec<(usize, Fr)>,
}

impl Transcript {
    pub fn new(mode: Mode, params: Params) -> Result<Transcript, DkgError> {
        if !params.ok() {
            return Err(DkgError::BadParams);
        }
        if let Mode::Reshare { old } = &mode {
            old.check()?;
        }
        Ok(Transcript {
            mode,
            params,
            commitments: BTreeMap::new(),
            open: BTreeSet::new(),
            revealed: BTreeMap::new(),
            disqualified: BTreeSet::new(),
        })
    }

    pub fn params(&self) -> Params {
        self.params
    }

    fn dealer_count(&self) -> usize {
        match &self.mode {
            Mode::Fresh => self.params.n,
            Mode::Reshare { old } => old.n(),
        }
    }

    pub fn commitments(&self, dealer: usize) -> Option<&[G1]> {
        self.commitments.get(&dealer).map(Vec::as_slice)
    }

    pub fn is_disqualified(&self, dealer: usize) -> bool {
        self.disqualified.contains(&dealer)
    }

    /// Record a dealer's commitments. A dealer whose commitments are malformed (wrong length,
    /// or, when resharing, a constant term that is not its published public share) is
    /// disqualified outright and the error says why.
    pub fn add_commitments(&mut self, dealer: usize, c: Vec<G1>) -> Result<(), DkgError> {
        if dealer >= self.dealer_count() {
            return Err(DkgError::UnknownDealer);
        }
        if self.commitments.contains_key(&dealer) || self.disqualified.contains(&dealer) {
            return Err(DkgError::DuplicateCommitments);
        }
        let mut ok = c.len() == self.params.t;
        if let Mode::Reshare { old } = &self.mode {
            ok = ok && c.first() == old.public_shares.get(dealer);
        }
        if !ok {
            self.disqualified.insert(dealer);
            return Err(DkgError::BadCommitments);
        }
        self.commitments.insert(dealer, c);
        Ok(())
    }

    /// Record a complaint of `accuser` (a new-committee index) against `dealer`.
    pub fn add_complaint(&mut self, accuser: usize, dealer: usize) -> Result<(), DkgError> {
        if accuser >= self.params.n || !self.commitments.contains_key(&dealer) {
            return Err(DkgError::BadComplaint);
        }
        self.open.insert((dealer, accuser));
        Ok(())
    }

    /// Record a dealer's answer to a complaint. A share that verifies clears the complaint; one
    /// that does not disqualifies the dealer. Returns whether the share was correct.
    pub fn add_justification(&mut self, dealer: usize, accuser: usize, share: Fr) -> bool {
        let Some(c) = self.commitments.get(&dealer) else {
            return false;
        };
        if !self.open.contains(&(dealer, accuser)) {
            return false;
        }
        if verify_share(c, accuser, &share) {
            self.open.remove(&(dealer, accuser));
            self.revealed.insert((dealer, accuser), share);
            true
        } else {
            self.disqualified.insert(dealer);
            false
        }
    }

    /// Dealers that still owe an answer to at least one complaint.
    pub fn unanswered(&self) -> BTreeSet<usize> {
        self.open.iter().map(|(d, _)| *d).collect()
    }

    /// Close the justification phase: dealers with an unanswered complaint are disqualified,
    /// then the key is derived from the qualified dealers.
    pub fn finalize(&self) -> Result<Outcome, DkgError> {
        let mut bad = self.disqualified.clone();
        bad.extend(self.unanswered());
        let qualified: Vec<usize> = self
            .commitments
            .keys()
            .copied()
            .filter(|d| !bad.contains(d))
            .collect();
        let need = match &self.mode {
            Mode::Fresh => self.params.t,
            Mode::Reshare { old } => old.threshold,
        };
        if qualified.len() < need {
            return Err(DkgError::NotEnoughDealers {
                have: qualified.len(),
                need,
            });
        }
        // Weights: 1 for a fresh DKG; Lagrange at 0 over the `old.t` lowest qualified old
        // holders for resharing.
        let weights: Vec<(usize, Fr)> = match &self.mode {
            Mode::Fresh => qualified.iter().map(|d| (*d, Fr::one())).collect(),
            Mode::Reshare { old } => {
                let used = &qualified[..old.threshold];
                let xs: Vec<u64> = used.iter().map(|d| *d as u64 + 1).collect();
                used.iter()
                    .enumerate()
                    .map(|(k, d)| {
                        lagrange(&xs, k, 0)
                            .map(|l| (*d, l))
                            .ok_or(DkgError::UnknownDealer)
                    })
                    .collect::<Result<_, _>>()?
            }
        };
        // Combine the dealers' commitments coefficient by coefficient first (`t` weighted sums),
        // then evaluate once per recipient: far fewer full-width multiplications than weighting
        // every evaluation.
        let combined: Vec<G1> = (0..self.params.t)
            .map(|k| {
                weights.iter().fold(G1::identity(), |acc, (d, w)| {
                    acc.add(&self.commitments[d][k].mul(w))
                })
            })
            .collect();
        let public_key = combined[0];
        let public_shares: Vec<G1> = (0..self.params.n)
            .map(|j| eval_commitments(&combined, j as u64 + 1))
            .collect();
        let group = GroupKey::new(self.params.t, public_key, public_shares)?;
        if let Mode::Reshare { old } = &self.mode {
            if group.public_key != old.public_key {
                // Impossible when every constant term matched an old public share.
                return Err(DkgError::Key(KeyError::Inconsistent));
            }
        }
        Ok(Outcome {
            qualified,
            group,
            weights,
        })
    }
}

/// One new-committee participant's private side: the shares it received.
pub struct Participant {
    pub index: usize,
    received: BTreeMap<usize, Fr>,
}

impl Participant {
    pub fn new(index: usize) -> Participant {
        Participant {
            index,
            received: BTreeMap::new(),
        }
    }

    /// Check a private share against the dealer's board commitments and keep it if correct.
    /// `false` means: file a complaint against `dealer` (also when the share never came).
    pub fn receive(&mut self, t: &Transcript, dealer: usize, share: Fr) -> bool {
        match t.commitments(dealer) {
            Some(c) if verify_share(c, self.index, &share) => {
                self.received.insert(dealer, share);
                true
            }
            _ => false,
        }
    }

    /// Dealers with commitments on the board from which no valid share arrived.
    pub fn complaints(&self, t: &Transcript) -> Vec<usize> {
        t.commitments
            .keys()
            .copied()
            .filter(|d| {
                !self.received.contains_key(d) && !t.revealed.contains_key(&(*d, self.index))
            })
            .collect()
    }

    /// The participant's secret share for the finished key generation.
    pub fn key_share(&self, t: &Transcript, out: &Outcome) -> Result<KeyShare, DkgError> {
        let mut secret = Fr::zero();
        for (d, w) in &out.weights {
            let s = self
                .received
                .get(d)
                .or_else(|| t.revealed.get(&(*d, self.index)))
                .ok_or(DkgError::MissingShare { dealer: *d })?;
            secret = secret.add(&s.mul(w));
        }
        let share = KeyShare {
            index: self.index,
            secret,
        };
        // Sanity: the derived share matches the public share on the board.
        if share.public_share() != out.group.public_shares[self.index] {
            return Err(DkgError::Key(KeyError::Inconsistent));
        }
        Ok(share)
    }
}

/// Trusted-dealer setup, for genesis files and tests: split a freshly derived secret among `n`
/// holders. One party knows the secret; BTB uses this only to bootstrap (see
/// `docs/ORACLES.md`); the DKG above replaces it per epoch.
pub fn trusted_deal(
    params: Params,
    entropy: &[u8; 32],
) -> Result<(GroupKey, Vec<KeyShare>), DkgError> {
    if !params.ok() {
        return Err(DkgError::BadParams);
    }
    let d = Dealing::fresh(0, params, entropy)?;
    let shares: Vec<KeyShare> = (0..params.n)
        .map(|j| KeyShare {
            index: j,
            secret: d.share_for(j),
        })
        .collect();
    let group = GroupKey::new(
        params.t,
        d.commitments[0],
        shares.iter().map(KeyShare::public_share).collect(),
    )?;
    Ok((group, shares))
}
