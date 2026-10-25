//! Key generation and resharing for FROST keys, over the same on-chain transport as the beacon
//! DKG (`btb_oracle::dkg`, `btb_oracle::dkg_crypto`): joint-Feldman with publicly decidable
//! complaints, plus resharing to a new committee that keeps the group public key.
//!
//! The protocol is the one of `crate::dkg` transplanted to a prime-order group chosen by
//! [`Scheme`]: every dealer `i` picks a polynomial `f_i` of degree `t - 1`, publishes the
//! commitments `C_{i,k} = a_{i,k} * G` and delivers `s_{ij} = f_i(j + 1)` to recipient `j`
//! encrypted (the host provides the board and the channels, see `dkg_crypto::deliver_raw`). A
//! recipient checks `s_{ij} * G == sum_k C_{i,k} (j + 1)^k`. The group key is `sum_{i in Q} C_{i,0}`
//! over the qualified dealers `Q` and the share of `j` is `sum_{i in Q} s_{ij}`. Resharing has each
//! old holder deal its own share (its constant commitment must equal its published public share)
//! and combines with Lagrange coefficients, so the secret is never reconstructed and the group
//! key (hence every external address) is unchanged.
//!
//! No proof of knowledge of the constant term is needed: a dealer that does not know the
//! discrete logarithm of its constant commitment cannot produce shares that verify against its
//! commitments, so the rogue-key attack that FROST's own DKG guards against with a proof of
//! knowledge ends in the dealer's disqualification. As with every joint-Feldman DKG, a rushing
//! adversary can bias the distribution of the group key (Gennaro, Jarecki, Krawczyk, Rabin 2007)
//! without gaining any ability to forge signatures.

use super::suite::{
    dispatch, el, el_bytes, eval_commitments, generator, identity, lagrange, one, sc, sc_bytes,
    sc_from_u64, zero, El, Sc, Scheme, Suite,
};
use crate::dkg::Params;
use crate::threshold::MAX_HOLDERS;
use frost_core::keys::{KeyPackage, PublicKeyPackage, SigningShare, VerifyingShare};
use frost_core::{Ciphersuite, Field, Group, Identifier, VerifyingKey};
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrostError {
    BadParams,
    /// Dealer index outside the dealing committee.
    UnknownDealer,
    DuplicateCommitments,
    /// Wrong number of commitments, an invalid point, or a reshare constant that is not the
    /// dealer's public share.
    BadCommitments,
    /// Fewer qualified dealers than needed.
    NotEnoughDealers {
        have: usize,
        need: usize,
    },
    /// The recipient has no share from a dealer it needs.
    MissingShare {
        dealer: usize,
    },
    /// A key or share that is malformed, inconsistent or does not belong together.
    BadKey,
    Malformed,
}

// ------------------------------------------------------------------ group key

/// The public description of a threshold key: scheme, threshold, the group (internal) public
/// key and every holder's public share (needed to check signature shares one by one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupKey {
    pub scheme: Scheme,
    pub threshold: usize,
    /// The DKG output `P` (compressed). For Taproot this is the *internal* key; addresses commit
    /// to [`GroupKey::output_key`].
    pub public_key: Vec<u8>,
    pub public_shares: Vec<Vec<u8>>,
}

impl GroupKey {
    pub fn n(&self) -> usize {
        self.public_shares.len()
    }

    /// Validate and build: all points valid, `1 <= t <= n <= MAX_HOLDERS`, and the shares lie on
    /// one polynomial of degree `t - 1` whose value at 0 is `public_key`.
    pub fn new(
        scheme: Scheme,
        threshold: usize,
        public_key: Vec<u8>,
        public_shares: Vec<Vec<u8>>,
    ) -> Result<GroupKey, FrostError> {
        let g = GroupKey {
            scheme,
            threshold,
            public_key,
            public_shares,
        };
        g.check()?;
        Ok(g)
    }

    pub fn check(&self) -> Result<(), FrostError> {
        dispatch!(self.scheme, S => check_group::<S>(self))
    }

    /// The 32 bytes an external verifier commits to: the Taproot output key `Q` (x-only) or the
    /// Ed25519 public key.
    pub fn output_key(&self) -> Result<[u8; 32], FrostError> {
        dispatch!(self.scheme, S => {
            let pk = public_package::<S>(self, true)?;
            S::output_key(&pk).ok_or(FrostError::BadKey)
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut o = vec![self.scheme.byte()];
        o.extend_from_slice(&(self.threshold as u32).to_le_bytes());
        o.extend_from_slice(&(self.n() as u32).to_le_bytes());
        o.extend_from_slice(&self.public_key);
        for s in &self.public_shares {
            o.extend_from_slice(s);
        }
        o
    }

    /// Decode and validate.
    pub fn decode(b: &[u8]) -> Result<GroupKey, FrostError> {
        let g = Self::decode_unchecked(b)?;
        g.check()?;
        Ok(g)
    }

    /// Decode without the consistency check (for state that was validated when stored).
    pub fn decode_unchecked(b: &[u8]) -> Result<GroupKey, FrostError> {
        let scheme = Scheme::from_byte(*b.first().ok_or(FrostError::Malformed)?)
            .ok_or(FrostError::Malformed)?;
        let threshold = u32::from_le_bytes(
            b.get(1..5)
                .ok_or(FrostError::Malformed)?
                .try_into()
                .unwrap(),
        ) as usize;
        let n = u32::from_le_bytes(
            b.get(5..9)
                .ok_or(FrostError::Malformed)?
                .try_into()
                .unwrap(),
        ) as usize;
        let pl = scheme.point_len();
        if n > MAX_HOLDERS || b.len() != 9 + pl * (1 + n) {
            return Err(FrostError::Malformed);
        }
        let public_key = b[9..9 + pl].to_vec();
        let public_shares = b[9 + pl..].chunks(pl).map(<[u8]>::to_vec).collect();
        Ok(GroupKey {
            scheme,
            threshold,
            public_key,
            public_shares,
        })
    }
}

fn check_group<C: Suite>(g: &GroupKey) -> Result<(), FrostError> {
    let (t, n) = (g.threshold, g.n());
    if t == 0 || t > n || n > MAX_HOLDERS {
        return Err(FrostError::BadKey);
    }
    let pk = el::<C>(&g.public_key).ok_or(FrostError::BadKey)?;
    let shares: Vec<El<C>> = g
        .public_shares
        .iter()
        .map(|s| el::<C>(s))
        .collect::<Option<_>>()
        .ok_or(FrostError::BadKey)?;
    // The first `t` shares determine the polynomial; every other share and the key must agree.
    let xs: Vec<u64> = (1..=t as u64).collect();
    let at = |x: u64| -> Result<El<C>, FrostError> {
        let mut acc = identity::<C>();
        for (k, share) in shares.iter().take(t).enumerate() {
            acc = acc + *share * lagrange::<C>(&xs, k, x).ok_or(FrostError::BadKey)?;
        }
        Ok(acc)
    };
    if at(0)? != pk {
        return Err(FrostError::BadKey);
    }
    for (j, share) in shares.iter().enumerate().skip(t) {
        if at(j as u64 + 1)? != *share {
            return Err(FrostError::BadKey);
        }
    }
    Ok(())
}

pub(crate) fn identifier<C: Ciphersuite>(index: usize) -> Result<Identifier<C>, FrostError> {
    u16::try_from(index + 1)
        .ok()
        .and_then(|i| Identifier::<C>::try_from(i).ok())
        .ok_or(FrostError::BadKey)
}

/// The FROST public key package of `g`; `tweaked` applies the scheme's output tweak.
pub(crate) fn public_package<C: Suite>(
    g: &GroupKey,
    tweaked: bool,
) -> Result<PublicKeyPackage<C>, FrostError> {
    let mut shares = BTreeMap::new();
    for (j, s) in g.public_shares.iter().enumerate() {
        shares.insert(
            identifier::<C>(j)?,
            VerifyingShare::<C>::deserialize(s).map_err(|_| FrostError::BadKey)?,
        );
    }
    let key = VerifyingKey::<C>::deserialize(&g.public_key).map_err(|_| FrostError::BadKey)?;
    let pk = PublicKeyPackage::<C>::new(shares, key, Some(g.threshold as u16));
    Ok(if tweaked { C::tweak_public(pk) } else { pk })
}

// ------------------------------------------------------------------ key share

/// One holder's secret share of a [`GroupKey`].
#[derive(Clone, PartialEq, Eq)]
pub struct KeyShare {
    /// Holder index (0-based; the share's x-coordinate is `index + 1`).
    pub index: usize,
    pub secret: [u8; 32],
    pub group: GroupKey,
}

impl std::fmt::Debug for KeyShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyShare")
            .field("scheme", &self.group.scheme)
            .field("index", &self.index)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl KeyShare {
    /// Build and check: the secret must match the group's public share for `index`.
    pub fn new(index: usize, secret: [u8; 32], group: GroupKey) -> Result<KeyShare, FrostError> {
        dispatch!(group.scheme, S => {
            let s = sc::<S>(&secret).ok_or(FrostError::BadKey)?;
            let public = group.public_shares.get(index).ok_or(FrostError::BadKey)?;
            if el_bytes::<S>(&(generator::<S>() * s)).as_deref() != Some(public.as_slice()) {
                return Err(FrostError::BadKey);
            }
        });
        Ok(KeyShare {
            index,
            secret,
            group,
        })
    }

    pub fn scheme(&self) -> Scheme {
        self.group.scheme
    }

    pub fn public_share(&self) -> &[u8] {
        &self.group.public_shares[self.index]
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut o = (self.index as u32).to_le_bytes().to_vec();
        o.extend_from_slice(&self.secret);
        o.extend_from_slice(&self.group.encode());
        o
    }

    pub fn decode(b: &[u8]) -> Option<KeyShare> {
        let index = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
        let secret: [u8; 32] = b.get(4..36)?.try_into().ok()?;
        let group = GroupKey::decode(b.get(36..)?).ok()?;
        KeyShare::new(index, secret, group).ok()
    }

    pub(crate) fn key_package<C: Suite>(&self) -> Result<KeyPackage<C>, FrostError> {
        let id = identifier::<C>(self.index)?;
        let signing =
            SigningShare::<C>::deserialize(&self.secret).map_err(|_| FrostError::BadKey)?;
        let verifying = VerifyingShare::<C>::deserialize(self.public_share())
            .map_err(|_| FrostError::BadKey)?;
        let key = VerifyingKey::<C>::deserialize(&self.group.public_key)
            .map_err(|_| FrostError::BadKey)?;
        Ok(KeyPackage::new(
            id,
            signing,
            verifying,
            key,
            self.group.threshold as u16,
        ))
    }
}

// ------------------------------------------------------------------ dealing

/// One dealer's contribution.
pub struct Dealing {
    pub scheme: Scheme,
    pub dealer: usize,
    coeffs: Vec<[u8; 32]>,
    /// `t` compressed points.
    pub commitments: Vec<Vec<u8>>,
}

fn expand<C: Ciphersuite>(entropy: &[u8; 32], tag: &[u8]) -> ChaCha20Rng {
    use sha2::{Digest, Sha256};
    let seed: [u8; 32] = Sha256::new()
        .chain_update(b"btb.frost.dkg.expand.v1")
        .chain_update(C::ID.as_bytes())
        .chain_update(tag)
        .chain_update(entropy)
        .finalize()
        .into();
    ChaCha20Rng::from_seed(seed)
}

impl Dealing {
    /// A fresh random polynomial of degree `t - 1` (new key).
    pub fn fresh(
        scheme: Scheme,
        dealer: usize,
        params: Params,
        entropy: &[u8; 32],
    ) -> Result<Dealing, FrostError> {
        check_params(params)?;
        dispatch!(scheme, S => {
            let mut rng = expand::<S>(entropy, b"fresh");
            let coeffs: Vec<Sc<S>> = (0..params.t)
                .map(|_| <<<S as Ciphersuite>::Group as Group>::Field as Field>::random(&mut rng))
                .collect();
            from_coeffs::<S>(dealer, &coeffs)
        })
    }

    /// A random polynomial of degree `t - 1` whose constant term is the holder's share
    /// (resharing). The dealer index is the holder's index in the old committee.
    pub fn reshare(
        old: &KeyShare,
        params: Params,
        entropy: &[u8; 32],
    ) -> Result<Dealing, FrostError> {
        check_params(params)?;
        dispatch!(old.scheme(), S => {
            let mut rng = expand::<S>(entropy, b"reshare");
            let mut coeffs = vec![sc::<S>(&old.secret).ok_or(FrostError::BadKey)?];
            coeffs.extend((1..params.t).map(|_| {
                <<<S as Ciphersuite>::Group as Group>::Field as Field>::random(&mut rng)
            }));
            from_coeffs::<S>(old.index, &coeffs)
        })
    }

    /// The private share for recipient `j` (0-based). Also what a justification would publish.
    pub fn share_for(&self, recipient: usize) -> [u8; 32] {
        dispatch!(self.scheme, S => {
            let x = sc_from_u64::<S>(recipient as u64 + 1);
            let mut acc = zero::<S>();
            for c in self.coeffs.iter().rev() {
                let c = sc::<S>(c).expect("own coefficient");
                acc = acc * x + c;
            }
            sc_bytes::<S>(&acc)
        })
    }

    /// Every recipient's share, in committee order.
    pub fn shares(&self, n: usize) -> Vec<[u8; 32]> {
        (0..n).map(|j| self.share_for(j)).collect()
    }
}

fn check_params(p: Params) -> Result<(), FrostError> {
    if p.t >= 1 && p.t <= p.n && p.n <= MAX_HOLDERS {
        Ok(())
    } else {
        Err(FrostError::BadParams)
    }
}

fn from_coeffs<C: Suite>(dealer: usize, coeffs: &[Sc<C>]) -> Result<Dealing, FrostError> {
    let commitments = coeffs
        .iter()
        .map(|c| el_bytes::<C>(&(generator::<C>() * *c)).ok_or(FrostError::BadKey))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Dealing {
        scheme: C::SCHEME,
        dealer,
        coeffs: coeffs.iter().map(sc_bytes::<C>).collect(),
        commitments,
    })
}

/// Does `share` lie on the polynomial committed to by `commitments`, as the share of the
/// 0-based `recipient`?
pub fn verify_share(
    scheme: Scheme,
    commitments: &[Vec<u8>],
    recipient: usize,
    share: &[u8; 32],
) -> bool {
    dispatch!(scheme, S => {
        let Some(s) = sc::<S>(share) else { return false };
        let Some(points) = commitments.iter().map(|c| el::<S>(c)).collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        generator::<S>() * s == eval_commitments::<S>(&points, recipient as u64 + 1)
    })
}

/// Is `b` a valid commitment point of `scheme`?
pub fn valid_point(scheme: Scheme, b: &[u8]) -> bool {
    dispatch!(scheme, S => el::<S>(b).is_some())
}

// ------------------------------------------------------------------ transcript

/// What kind of key generation a transcript records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Dealers are the new committee members themselves.
    Fresh,
    /// Dealers are holders of `old`; the result is the same group key on the new committee.
    Reshare { old: GroupKey },
}

/// The public record of one key generation: the commitments of the dealers that were not
/// disqualified. Processing it is deterministic, so every observer computes the same outcome.
#[derive(Debug, Clone)]
pub struct Transcript {
    scheme: Scheme,
    mode: Mode,
    params: Params,
    commitments: BTreeMap<usize, Vec<Vec<u8>>>,
}

/// The result of a finished key generation.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Qualified dealers, ascending.
    pub qualified: Vec<usize>,
    pub group: GroupKey,
    /// The dealers whose shares make up each recipient's share, with their weights (all `1` for
    /// a fresh key, Lagrange coefficients at 0 for a reshare).
    pub weights: Vec<(usize, [u8; 32])>,
}

impl Transcript {
    pub fn new(scheme: Scheme, mode: Mode, params: Params) -> Result<Transcript, FrostError> {
        check_params(params)?;
        if let Mode::Reshare { old } = &mode {
            old.check()?;
            if old.scheme != scheme {
                return Err(FrostError::BadKey);
            }
        }
        Ok(Transcript {
            scheme,
            mode,
            params,
            commitments: BTreeMap::new(),
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

    /// Validate a dealer's commitments against this transcript (shape, points, reshare
    /// constant). Used by the chain before accepting a deal.
    pub fn check_commitments(&self, dealer: usize, c: &[Vec<u8>]) -> Result<(), FrostError> {
        if dealer >= self.dealer_count() {
            return Err(FrostError::UnknownDealer);
        }
        if c.len() != self.params.t || c.iter().any(|p| !valid_point(self.scheme, p)) {
            return Err(FrostError::BadCommitments);
        }
        if let Mode::Reshare { old } = &self.mode {
            if c.first() != old.public_shares.get(dealer) {
                return Err(FrostError::BadCommitments);
            }
        }
        Ok(())
    }

    /// Record a dealer's commitments.
    pub fn add_commitments(&mut self, dealer: usize, c: Vec<Vec<u8>>) -> Result<(), FrostError> {
        self.check_commitments(dealer, &c)?;
        if self.commitments.contains_key(&dealer) {
            return Err(FrostError::DuplicateCommitments);
        }
        self.commitments.insert(dealer, c);
        Ok(())
    }

    pub fn commitments(&self, dealer: usize) -> Option<&[Vec<u8>]> {
        self.commitments.get(&dealer).map(Vec::as_slice)
    }

    /// Derive the group key from the recorded (qualified) dealers.
    pub fn finalize(&self) -> Result<Outcome, FrostError> {
        dispatch!(self.scheme, S => self.finalize_in::<S>())
    }

    fn finalize_in<C: Suite>(&self) -> Result<Outcome, FrostError> {
        let qualified: Vec<usize> = self.commitments.keys().copied().collect();
        let need = match &self.mode {
            Mode::Fresh => self.params.t,
            Mode::Reshare { old } => old.threshold,
        };
        if qualified.len() < need {
            return Err(FrostError::NotEnoughDealers {
                have: qualified.len(),
                need,
            });
        }
        let weights: Vec<(usize, Sc<C>)> = match &self.mode {
            Mode::Fresh => qualified.iter().map(|d| (*d, one::<C>())).collect(),
            Mode::Reshare { old } => {
                // Lagrange at 0 over the `old.threshold` lowest qualified old holders.
                let used = &qualified[..old.threshold];
                let xs: Vec<u64> = used.iter().map(|d| *d as u64 + 1).collect();
                used.iter()
                    .enumerate()
                    .map(|(k, d)| {
                        lagrange::<C>(&xs, k, 0)
                            .map(|l| (*d, l))
                            .ok_or(FrostError::UnknownDealer)
                    })
                    .collect::<Result<_, _>>()?
            }
        };
        // Combine coefficient-wise first (`t` weighted sums), then evaluate once per recipient.
        let combined: Vec<El<C>> = (0..self.params.t)
            .map(|k| {
                weights.iter().try_fold(identity::<C>(), |acc, (d, w)| {
                    let c = el::<C>(&self.commitments[d][k]).ok_or(FrostError::BadCommitments)?;
                    Ok::<_, FrostError>(acc + c * *w)
                })
            })
            .collect::<Result<_, _>>()?;
        let public_key = el_bytes::<C>(&combined[0]).ok_or(FrostError::BadKey)?;
        let public_shares = (0..self.params.n)
            .map(|j| {
                el_bytes::<C>(&eval_commitments::<C>(&combined, j as u64 + 1))
                    .ok_or(FrostError::BadKey)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let group = GroupKey::new(self.scheme, self.params.t, public_key, public_shares)?;
        if let Mode::Reshare { old } = &self.mode {
            if group.public_key != old.public_key {
                return Err(FrostError::BadKey);
            }
        }
        Ok(Outcome {
            qualified,
            group,
            weights: weights
                .into_iter()
                .map(|(d, w)| (d, sc_bytes::<C>(&w)))
                .collect(),
        })
    }
}

impl Outcome {
    /// The dealers whose shares recipient `j` needs.
    pub fn needed(&self) -> BTreeSet<usize> {
        self.weights.iter().map(|(d, _)| *d).collect()
    }

    /// Recipient `index`'s secret share from the decrypted shares of the weighted dealers.
    pub fn derive_share(
        &self,
        index: usize,
        received: &BTreeMap<usize, [u8; 32]>,
    ) -> Result<KeyShare, FrostError> {
        dispatch!(self.group.scheme, S => {
            let mut acc = zero::<S>();
            for (d, w) in &self.weights {
                let s = received
                    .get(d)
                    .and_then(|b| sc::<S>(b))
                    .ok_or(FrostError::MissingShare { dealer: *d })?;
                acc += sc::<S>(w).ok_or(FrostError::BadKey)? * s;
            }
            KeyShare::new(index, sc_bytes::<S>(&acc), self.group.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(scheme: Scheme, mode: Mode, dealers: &[KeyShare], n: usize) -> (Outcome, Vec<KeyShare>) {
        let params = Params::standard(n);
        let mut t = Transcript::new(scheme, mode.clone(), params).unwrap();
        let mut deals = Vec::new();
        let count = match &mode {
            Mode::Fresh => n,
            Mode::Reshare { old } => old.n(),
        };
        #[allow(clippy::needless_range_loop)]
        for d in 0..count {
            let e = [d as u8 + 7; 32];
            let deal = match &mode {
                Mode::Fresh => Dealing::fresh(scheme, d, params, &e).unwrap(),
                Mode::Reshare { .. } => Dealing::reshare(&dealers[d], params, &e).unwrap(),
            };
            t.add_commitments(d, deal.commitments.clone()).unwrap();
            deals.push(deal);
        }
        let out = t.finalize().unwrap();
        let shares = (0..n)
            .map(|j| {
                let got = out
                    .needed()
                    .into_iter()
                    .map(|d| {
                        let s = deals[d].share_for(j);
                        assert!(verify_share(scheme, t.commitments(d).unwrap(), j, &s));
                        (d, s)
                    })
                    .collect();
                out.derive_share(j, &got).unwrap()
            })
            .collect();
        (out, shares)
    }

    #[test]
    fn fresh_then_reshare_keeps_the_group_key_for_both_schemes() {
        for scheme in Scheme::ALL {
            let (out, shares) = run(scheme, Mode::Fresh, &[], 4);
            assert_eq!(out.group.threshold, 3);
            let group = out.group;
            // reshare to a bigger committee, then a smaller one
            let (out2, shares2) = run(scheme, Mode::Reshare { old: group.clone() }, &shares, 7);
            assert_eq!(out2.group.public_key, group.public_key, "{scheme:?}");
            assert_eq!(out2.group.threshold, 5);
            assert_eq!(
                out2.group.output_key().unwrap(),
                group.output_key().unwrap()
            );
            assert_ne!(shares2[0].secret, shares[0].secret);
            let (out3, _) = run(
                scheme,
                Mode::Reshare {
                    old: out2.group.clone(),
                },
                &shares2,
                4,
            );
            assert_eq!(out3.group.public_key, group.public_key);
        }
    }

    #[test]
    fn bad_commitments_and_shares_are_caught() {
        for scheme in Scheme::ALL {
            let params = Params::standard(4);
            let t = Transcript::new(scheme, Mode::Fresh, params).unwrap();
            let deal = Dealing::fresh(scheme, 0, params, &[1; 32]).unwrap();
            let mut short = deal.commitments.clone();
            short.pop();
            assert_eq!(
                t.check_commitments(0, &short),
                Err(FrostError::BadCommitments)
            );
            let mut junk = deal.commitments.clone();
            junk[1] = vec![0xAB; scheme.point_len()];
            assert_eq!(
                t.check_commitments(0, &junk),
                Err(FrostError::BadCommitments)
            );
            assert_eq!(
                t.check_commitments(9, &deal.commitments),
                Err(FrostError::UnknownDealer)
            );
            let s = deal.share_for(2);
            assert!(verify_share(scheme, &deal.commitments, 2, &s));
            assert!(!verify_share(scheme, &deal.commitments, 1, &s));
            let mut bad = s;
            bad[0] ^= 1;
            assert!(!verify_share(scheme, &deal.commitments, 2, &bad));
            // reshare constants must be the dealer's public share
            let (out, shares) = run(scheme, Mode::Fresh, &[], 4);
            let r = Transcript::new(scheme, Mode::Reshare { old: out.group }, params).unwrap();
            let fake = Dealing::fresh(scheme, 0, params, &[9; 32]).unwrap();
            assert_eq!(
                r.check_commitments(0, &fake.commitments),
                Err(FrostError::BadCommitments)
            );
            let real = Dealing::reshare(&shares[0], params, &[9; 32]).unwrap();
            assert!(r.check_commitments(0, &real.commitments).is_ok());
            // a missing dealer set below the threshold fails to finalize
            let mut few = Transcript::new(scheme, Mode::Fresh, params).unwrap();
            few.add_commitments(0, deal.commitments.clone()).unwrap();
            assert!(matches!(
                few.finalize(),
                Err(FrostError::NotEnoughDealers { .. })
            ));
        }
    }

    #[test]
    fn group_key_round_trips_and_rejects_inconsistent_shares() {
        for scheme in Scheme::ALL {
            let (out, shares) = run(scheme, Mode::Fresh, &[], 4);
            let g = out.group;
            assert_eq!(GroupKey::decode(&g.encode()).unwrap(), g);
            let mut bad = g.clone();
            bad.public_shares.swap(0, 1);
            assert!(bad.check().is_err());
            assert!(GroupKey::decode(&g.encode()[..20]).is_err());
            let ks = KeyShare::decode(&shares[2].encode()).unwrap();
            assert_eq!(ks, shares[2]);
            // a share that does not match its public share is refused
            let mut wrong = shares[2].secret;
            wrong[1] ^= 4;
            assert!(KeyShare::new(2, wrong, g.clone()).is_err());
            assert!(KeyShare::new(1, shares[2].secret, g).is_err());
        }
    }
}
