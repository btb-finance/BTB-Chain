//! Threshold BLS: Shamir-shared signing key, partial signatures, Lagrange aggregation.
//!
//! A committee of `n` holders shares a secret `s` with a polynomial of degree `t - 1`; holder
//! `i` (0-based) has the share `f(i + 1)`. A partial signature is `share * H(msg)` in G2. Any
//! `t` valid partials combine to `s * H(msg)`, which is *the* BLS signature of the group
//! public key `s * g1` on `msg`: it is unique, so every node that combines any `t` valid
//! partials obtains the same bytes, and nobody can steer which signature comes out.
//!
//! Fewer than `t` partials reveal nothing about the signature (information-theoretically, in
//! the exponent): with `t - 1` shares, every value of the signature is equally consistent with
//! what the adversary knows.

use crate::bls::{self, Fr, G1, G1_BYTES, G2, G2_BYTES};
use std::collections::{BTreeMap, BTreeSet};

/// The signing threshold for a committee of `n`: `n - floor((n - 1) / 3)`, i.e. more than two
/// thirds. With `f = floor((n - 1) / 3)` faulty holders, `n - f >= t` honest holders can
/// always sign (liveness), and `f < t` colluders cannot (unpredictability).
pub fn threshold_for(n: usize) -> usize {
    n - n.saturating_sub(1) / 3
}

/// The public description of a threshold key: threshold, group public key and every holder's
/// public share (needed to check partial signatures one by one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupKey {
    pub threshold: usize,
    pub public_key: G1,
    /// `public_shares[i] = share_i * g1`.
    pub public_shares: Vec<G1>,
}

/// Why a [`GroupKey`] is not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    /// Threshold zero, above `n`, or too many holders.
    BadThreshold,
    /// An identity element where a real key is required.
    IdentityKey,
    /// The public shares do not lie on one polynomial of degree `threshold - 1` whose value
    /// at 0 is the group public key.
    Inconsistent,
    /// Undecodable bytes.
    Malformed,
}

pub const MAX_HOLDERS: usize = 256;

impl GroupKey {
    pub fn n(&self) -> usize {
        self.public_shares.len()
    }

    /// Build and validate (the shares must be consistent with the group key).
    pub fn new(
        threshold: usize,
        public_key: G1,
        public_shares: Vec<G1>,
    ) -> Result<GroupKey, KeyError> {
        let g = GroupKey {
            threshold,
            public_key,
            public_shares,
        };
        g.check()?;
        Ok(g)
    }

    /// Check structure and consistency: `O(n * t)` group operations.
    pub fn check(&self) -> Result<(), KeyError> {
        let n = self.n();
        if self.threshold == 0 || self.threshold > n || n > MAX_HOLDERS {
            return Err(KeyError::BadThreshold);
        }
        if self.public_key.is_identity() || self.public_shares.iter().any(G1::is_identity) {
            return Err(KeyError::IdentityKey);
        }
        // Interpolate the polynomial from the first `t` public shares; every other share and
        // the group key must be on it.
        let xs: Vec<u64> = (1..=self.threshold as u64).collect();
        let at = |x: u64| -> Result<G1, KeyError> {
            let mut acc = G1::identity();
            for k in 0..xs.len() {
                let l = lagrange(&xs, k, x).ok_or(KeyError::Inconsistent)?;
                acc = acc.add(&self.public_shares[k].mul(&l));
            }
            Ok(acc)
        };
        if at(0)? != self.public_key {
            return Err(KeyError::Inconsistent);
        }
        for i in self.threshold..n {
            if at(i as u64 + 1)? != self.public_shares[i] {
                return Err(KeyError::Inconsistent);
            }
        }
        Ok(())
    }

    /// `threshold:u32 LE || n:u32 LE || group key[48] || n * share[48]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(8 + G1_BYTES * (1 + self.n()));
        o.extend_from_slice(&(self.threshold as u32).to_le_bytes());
        o.extend_from_slice(&(self.n() as u32).to_le_bytes());
        o.extend_from_slice(&self.public_key.to_bytes());
        for s in &self.public_shares {
            o.extend_from_slice(&s.to_bytes());
        }
        o
    }

    /// Decode and validate (including the consistency check).
    pub fn decode(b: &[u8]) -> Result<GroupKey, KeyError> {
        let g = Self::decode_unchecked(b)?;
        g.check()?;
        Ok(g)
    }

    /// Decode without the `O(n * t)` consistency check (for keys already validated, such as
    /// ones read back from committed chain state). Points are still subgroup-checked.
    pub fn decode_unchecked(b: &[u8]) -> Result<GroupKey, KeyError> {
        let bad = |_| KeyError::Malformed;
        let threshold = u32::from_le_bytes(
            b.get(..4)
                .ok_or(KeyError::Malformed)?
                .try_into()
                .map_err(bad)?,
        ) as usize;
        let n = u32::from_le_bytes(
            b.get(4..8)
                .ok_or(KeyError::Malformed)?
                .try_into()
                .map_err(bad)?,
        ) as usize;
        if n > MAX_HOLDERS || b.len() != 8 + G1_BYTES * (1 + n) {
            return Err(KeyError::Malformed);
        }
        let pt = |i: usize| -> Result<G1, KeyError> {
            let s = &b[8 + G1_BYTES * i..8 + G1_BYTES * (i + 1)];
            G1::from_bytes(s.try_into().map_err(bad)?).ok_or(KeyError::Malformed)
        };
        let public_key = pt(0)?;
        let public_shares = (1..=n).map(pt).collect::<Result<Vec<_>, _>>()?;
        if threshold == 0 || threshold > n {
            return Err(KeyError::BadThreshold);
        }
        Ok(GroupKey {
            threshold,
            public_key,
            public_shares,
        })
    }
}

/// A holder's secret share. Not `Clone`-printable; `Debug` hides the value.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyShare {
    pub index: usize,
    pub secret: Fr,
}

impl std::fmt::Debug for KeyShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyShare(index {}, ..)", self.index)
    }
}

impl KeyShare {
    pub fn public_share(&self) -> G1 {
        G1::generator().mul(&self.secret)
    }

    /// Partial signature on `msg`.
    pub fn sign(&self, msg: &[u8]) -> G2 {
        G2::hash_to_curve(msg).mul(&self.secret)
    }

    /// `index:u32 LE || secret[32] BE`.
    pub fn encode(&self) -> [u8; 36] {
        let mut o = [0u8; 36];
        o[..4].copy_from_slice(&(self.index as u32).to_le_bytes());
        o[4..].copy_from_slice(&self.secret.to_bytes_be());
        o
    }

    pub fn decode(b: &[u8]) -> Option<KeyShare> {
        let b: &[u8; 36] = b.try_into().ok()?;
        let index = u32::from_le_bytes(b[..4].try_into().ok()?) as usize;
        let secret = Fr::from_bytes_be(b[4..].try_into().ok()?)?;
        Some(KeyShare { index, secret })
    }
}

/// Lagrange coefficient for `xs[k]` when evaluating at `x`: `prod_{j != k} (x - xj) / (xk - xj)`.
/// `None` if the x-coordinates are not distinct.
pub fn lagrange(xs: &[u64], k: usize, x: u64) -> Option<Fr> {
    let (fx, fk) = (Fr::from_u64(x), Fr::from_u64(xs[k]));
    let mut num = Fr::one();
    let mut den = Fr::one();
    for (j, xj) in xs.iter().enumerate() {
        if j == k {
            continue;
        }
        let fj = Fr::from_u64(*xj);
        num = num.mul(&fx.sub(&fj));
        den = den.mul(&fk.sub(&fj));
    }
    Some(num.mul(&den.inverse()?))
}

/// Why partials did not combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombineError {
    /// Fewer than `threshold` distinct partials.
    NotEnough,
    /// A holder index outside the committee, or the same index twice.
    BadIndex,
    /// The combined signature does not verify under the group key (some partial was invalid).
    BadAggregate,
}

/// Verify one holder's partial signature against that holder's public share.
pub fn verify_partial(group: &GroupKey, index: usize, msg: &[u8], partial: &G2) -> bool {
    match group.public_shares.get(index) {
        Some(pk) => bls::verify(pk, msg, partial),
        None => false,
    }
}

/// Combine at least `threshold` partials `(holder index, signature)` into the group signature.
///
/// The lowest `threshold` indices are used (any `threshold` valid partials give the same
/// result). The result is checked against the group key, so an invalid partial among the
/// chosen ones yields `BadAggregate`, never a wrong signature.
pub fn combine(group: &GroupKey, msg: &[u8], partials: &[(usize, G2)]) -> Result<G2, CombineError> {
    let mut seen = BTreeSet::new();
    let mut sorted: Vec<&(usize, G2)> = Vec::with_capacity(partials.len());
    for p in partials {
        if p.0 >= group.n() || !seen.insert(p.0) {
            return Err(CombineError::BadIndex);
        }
        sorted.push(p);
    }
    if sorted.len() < group.threshold {
        return Err(CombineError::NotEnough);
    }
    sorted.sort_by_key(|p| p.0);
    sorted.truncate(group.threshold);
    let sig = interpolate_g2(&sorted)?;
    if bls::verify(&group.public_key, msg, &sig) {
        Ok(sig)
    } else {
        Err(CombineError::BadAggregate)
    }
}

/// Lagrange-interpolate partials at 0 without any check (used by `combine` and by tests that
/// show an under-threshold set gives garbage).
pub fn interpolate_g2(sel: &[&(usize, G2)]) -> Result<G2, CombineError> {
    let xs: Vec<u64> = sel.iter().map(|p| p.0 as u64 + 1).collect();
    let mut acc = G2::identity();
    for (k, p) in sel.iter().enumerate() {
        let l = lagrange(&xs, k, 0).ok_or(CombineError::BadIndex)?;
        acc = acc.add(&p.1.mul(&l));
    }
    Ok(acc)
}

/// Collects partial signatures for one message and produces the signature as soon as
/// `threshold` valid ones are present.
///
/// Partials are accepted unverified (one pairing check per partial would cost `O(t)` pairings
/// per block); once `threshold` are present the aggregate is checked with one verification. Only
/// if that fails does the collector verify the partials one by one, evict and remember the
/// invalid ones (`rejected`), and try again. A holder whose partial was rejected is ignored for
/// the rest of the collection, so a malicious holder can force the slow path at most once.
pub struct Collector {
    group: GroupKey,
    msg: Vec<u8>,
    partials: BTreeMap<usize, G2>,
    rejected: BTreeSet<usize>,
    done: Option<G2>,
}

impl Collector {
    pub fn new(group: GroupKey, msg: Vec<u8>) -> Collector {
        Collector {
            group,
            msg,
            partials: BTreeMap::new(),
            rejected: BTreeSet::new(),
            done: None,
        }
    }

    /// Add a partial. Returns the finished signature once available (also on later calls).
    /// Out-of-range and already-rejected holders are ignored; a second partial from the same
    /// holder is ignored.
    pub fn add(&mut self, index: usize, partial: G2) -> Option<G2> {
        if let Some(s) = self.done {
            return Some(s);
        }
        if index >= self.group.n() || self.rejected.contains(&index) {
            return None;
        }
        self.partials.entry(index).or_insert(partial);
        self.try_finish()
    }

    fn try_finish(&mut self) -> Option<G2> {
        loop {
            if self.partials.len() < self.group.threshold {
                return None;
            }
            let list: Vec<(usize, G2)> = self.partials.iter().map(|(i, p)| (*i, *p)).collect();
            match combine(&self.group, &self.msg, &list) {
                Ok(sig) => {
                    self.done = Some(sig);
                    return Some(sig);
                }
                Err(CombineError::BadAggregate) => {
                    // Slow path: find the offenders among all held partials.
                    let bad: Vec<usize> = self
                        .partials
                        .iter()
                        .filter(|(i, p)| !verify_partial(&self.group, **i, &self.msg, p))
                        .map(|(i, _)| *i)
                        .collect();
                    if bad.is_empty() {
                        // Cannot happen with a consistent group key; do not loop forever.
                        return None;
                    }
                    for i in bad {
                        self.partials.remove(&i);
                        self.rejected.insert(i);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    pub fn signature(&self) -> Option<G2> {
        self.done
    }

    /// Holders whose partial failed verification.
    pub fn rejected(&self) -> Vec<usize> {
        self.rejected.iter().copied().collect()
    }

    pub fn count(&self) -> usize {
        self.partials.len()
    }
}

/// Decode a 96-byte partial signature.
pub fn decode_partial(b: &[u8]) -> Option<G2> {
    let b: &[u8; G2_BYTES] = b.try_into().ok()?;
    G2::from_bytes(b)
}
