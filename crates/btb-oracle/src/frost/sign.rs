//! FROST signing (RFC 9591): nonce commitments (round 1, preprocessed in batches), signature
//! shares (round 2), aggregation with identification of misbehaving signers.
//!
//! Everything is byte-level and deterministic given its inputs. Randomness enters only through
//! the `entropy` the caller passes to [`NoncePool::top_up`], which must come from the operating
//! system: a nonce derived deterministically from the long-term share would repeat after a lost
//! state file and leak the share.
//!
//! # Nonce discipline
//!
//! A FROST nonce pair must be used for **exactly one** signature share. [`NoncePool::take`]
//! removes the pair from the pool; the host must persist the pool *before* it releases the share
//! computed with the pair, so a crash at any point loses a nonce but can never reuse one. A pair
//! whose share was lost is simply gone; the next attempt uses a fresh one.

use super::keys::{identifier, public_package, FrostError, GroupKey, KeyShare};
use super::suite::{dispatch, Suite};
use frost_core::round1::{SigningCommitments, SigningNonces};
use frost_core::round2::SignatureShare;
use frost_core::{
    CheaterDetection, Error as FrostCoreError, Signature, SigningPackage as FPackage,
};
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use std::collections::BTreeMap;

pub const SIGNATURE_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignError {
    Key(FrostError),
    /// The package holds fewer commitments than the threshold, or an unknown holder.
    BadPackage,
    /// This signer's commitment in the package is not the one of the nonces it holds.
    NotOurCommitment,
    /// Malformed nonces, commitments or shares.
    Malformed,
}

impl From<FrostError> for SignError {
    fn from(e: FrostError) -> Self {
        SignError::Key(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateError {
    /// These holders' shares are invalid (identified with the per-share check).
    Culprits(Vec<usize>),
    NotEnough,
    Malformed,
    /// The aggregate does not verify but no single share is to blame.
    Invalid,
}

/// The public input of round 2: the message and the commitment of every signer in the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningPackage {
    pub message: Vec<u8>,
    /// Holder index -> `SigningCommitments` bytes.
    pub commitments: BTreeMap<usize, Vec<u8>>,
}

impl SigningPackage {
    /// A canonical encoding (the hash of it binds signature shares to the package).
    pub fn encode(&self) -> Vec<u8> {
        let mut o = (self.message.len() as u32).to_le_bytes().to_vec();
        o.extend_from_slice(&self.message);
        o.extend_from_slice(&(self.commitments.len() as u32).to_le_bytes());
        for (i, c) in &self.commitments {
            o.extend_from_slice(&(*i as u32).to_le_bytes());
            o.extend_from_slice(&(c.len() as u32).to_le_bytes());
            o.extend_from_slice(c);
        }
        o
    }

    pub fn decode(b: &[u8]) -> Option<SigningPackage> {
        let mut pos = 0usize;
        let mut take = |n: usize| -> Option<&[u8]> {
            let s = b.get(pos..pos.checked_add(n)?)?;
            pos += n;
            Some(s)
        };
        let ml = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        if ml > 1 << 16 {
            return None;
        }
        let message = take(ml)?.to_vec();
        let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        if n > super::MAX_PARTIES {
            return None;
        }
        let mut commitments = BTreeMap::new();
        for _ in 0..n {
            let i = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let l = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            if l > 256 {
                return None;
            }
            if commitments.insert(i, take(l)?.to_vec()).is_some() {
                return None;
            }
        }
        (pos == b.len()).then_some(SigningPackage {
            message,
            commitments,
        })
    }

    fn to_frost<C: Suite>(&self) -> Result<FPackage<C>, SignError> {
        let mut m = BTreeMap::new();
        for (i, c) in &self.commitments {
            m.insert(
                identifier::<C>(*i)?,
                SigningCommitments::<C>::deserialize(c).map_err(|_| SignError::Malformed)?,
            );
        }
        Ok(FPackage::<C>::new(m, &self.message))
    }
}

// ------------------------------------------------------------------ round 1

/// One fresh nonce pair for `share`: `(secret nonces, public commitments)`, both serialized.
pub fn generate_nonces(
    share: &KeyShare,
    entropy: &[u8; 32],
) -> Result<(Vec<u8>, Vec<u8>), SignError> {
    dispatch!(share.scheme(), S => {
        let kp = share.key_package::<S>()?;
        let mut rng = ChaCha20Rng::from_seed(*entropy);
        let (nonces, commitments) = frost_core::round1::commit::<S, _>(kp.signing_share(), &mut rng);
        Ok((
            nonces.serialize().map_err(|_| SignError::Malformed)?,
            commitments.serialize().map_err(|_| SignError::Malformed)?,
        ))
    })
}

/// A holder's preprocessed nonces, kept until used. Persistence is the host's job: save after
/// every change that removes entries and before the corresponding share leaves the process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoncePool {
    /// Next counter to hand out; counters are never reused, not even after a restart.
    next: u64,
    live: BTreeMap<u64, (Vec<u8>, Vec<u8>)>,
}

impl NoncePool {
    pub fn new() -> NoncePool {
        NoncePool::default()
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    pub fn next_counter(&self) -> u64 {
        self.next
    }

    /// Generate nonces until `target` are available. `entropy` must be fresh OS randomness each
    /// call. Returns the new `(counter, commitments)` to publish as a batch.
    pub fn top_up(
        &mut self,
        share: &KeyShare,
        target: usize,
        mut entropy: impl FnMut() -> [u8; 32],
    ) -> Result<Vec<(u64, Vec<u8>)>, SignError> {
        let mut fresh = Vec::new();
        while self.live.len() < target {
            let (n, c) = generate_nonces(share, &entropy())?;
            let counter = self.next;
            self.next += 1;
            fresh.push((counter, c.clone()));
            self.live.insert(counter, (n, c));
        }
        Ok(fresh)
    }

    /// The public commitment of a live nonce.
    pub fn commitment(&self, counter: u64) -> Option<&[u8]> {
        self.live.get(&counter).map(|(_, c)| c.as_slice())
    }

    /// Live counters, ascending.
    pub fn counters(&self) -> impl Iterator<Item = u64> + '_ {
        self.live.keys().copied()
    }

    /// The lowest live counter (the next one to hand out).
    pub fn first(&self) -> Option<u64> {
        self.live.keys().next().copied()
    }

    /// Remove and return the secret nonces of `counter`. The pool must be persisted before the
    /// returned nonces are used. `None` if the counter was never issued or is already spent.
    pub fn take(&mut self, counter: u64) -> Option<Vec<u8>> {
        self.live.remove(&counter).map(|(n, _)| n)
    }

    /// Drop everything (the key was replaced; the commitments are useless).
    pub fn clear(&mut self) {
        self.live.clear();
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut o = self.next.to_le_bytes().to_vec();
        o.extend_from_slice(&(self.live.len() as u32).to_le_bytes());
        for (k, (n, c)) in &self.live {
            o.extend_from_slice(&k.to_le_bytes());
            o.extend_from_slice(&(n.len() as u32).to_le_bytes());
            o.extend_from_slice(n);
            o.extend_from_slice(&(c.len() as u32).to_le_bytes());
            o.extend_from_slice(c);
        }
        o
    }

    pub fn decode(b: &[u8]) -> Option<NoncePool> {
        let mut pos = 0usize;
        let mut take = |n: usize| -> Option<&[u8]> {
            let s = b.get(pos..pos.checked_add(n)?)?;
            pos += n;
            Some(s)
        };
        let next = u64::from_le_bytes(take(8)?.try_into().ok()?);
        let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        if n > 1 << 16 {
            return None;
        }
        let mut live = BTreeMap::new();
        for _ in 0..n {
            let k = u64::from_le_bytes(take(8)?.try_into().ok()?);
            let nl = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let nonces = take(nl)?.to_vec();
            let cl = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let comm = take(cl)?.to_vec();
            if k >= next {
                return None; // a counter the pool never issued
            }
            live.insert(k, (nonces, comm));
        }
        (pos == b.len()).then_some(NoncePool { next, live })
    }
}

// ------------------------------------------------------------------ round 2

/// The signature share of `share`'s holder over `pkg` with the (single-use) `nonces`.
pub fn sign_share(
    share: &KeyShare,
    pkg: &SigningPackage,
    nonces: &[u8],
) -> Result<Vec<u8>, SignError> {
    dispatch!(share.scheme(), S => {
        if pkg.commitments.len() < share.group.threshold
            || pkg.commitments.keys().any(|i| *i >= share.group.n())
        {
            return Err(SignError::BadPackage);
        }
        let kp = S::tweak_key_package(share.key_package::<S>()?);
        let nonces = SigningNonces::<S>::deserialize(nonces).map_err(|_| SignError::Malformed)?;
        let sp = pkg.to_frost::<S>()?;
        let id = identifier::<S>(share.index)?;
        if sp.signing_commitment(&id).as_ref() != Some(nonces.commitments()) {
            return Err(SignError::NotOurCommitment);
        }
        let out = frost_core::round2::sign::<S>(&sp, &nonces, &kp).map_err(|_| SignError::BadPackage)?;
        Ok(out.serialize())
    })
}

/// Is `b` a well-formed nonce commitment pair of `scheme`?
pub fn valid_commitments(scheme: super::suite::Scheme, b: &[u8]) -> bool {
    dispatch!(scheme, S => SigningCommitments::<S>::deserialize(b).is_ok())
}

/// Check one signature share against the public data (what evidence of a bad share rests on).
pub fn verify_share(group: &GroupKey, pkg: &SigningPackage, holder: usize, share: &[u8]) -> bool {
    dispatch!(group.scheme, S => (|| -> Option<bool> {
        let pk = public_package::<S>(group, true).ok()?;
        let id = identifier::<S>(holder).ok()?;
        let vs = pk.verifying_shares().get(&id)?;
        let share = SignatureShare::<S>::deserialize(share).ok()?;
        let sp = pkg.to_frost::<S>().ok()?;
        Some(
            frost_core::verify_signature_share::<S>(id, vs, &share, &sp, pk.verifying_key())
                .is_ok(),
        )
    })()
    .unwrap_or(false))
}

/// Combine the shares of every signer in `pkg` into a 64-byte signature, or name the holders
/// whose shares are invalid.
pub fn aggregate(
    group: &GroupKey,
    pkg: &SigningPackage,
    shares: &BTreeMap<usize, Vec<u8>>,
) -> Result<Vec<u8>, AggregateError> {
    dispatch!(group.scheme, S => {
        let pk = public_package::<S>(group, true).map_err(|_| AggregateError::Malformed)?;
        let sp = pkg.to_frost::<S>().map_err(|_| AggregateError::Malformed)?;
        let mut map = BTreeMap::new();
        let mut back = BTreeMap::new();
        for (i, s) in shares {
            let id = identifier::<S>(*i).map_err(|_| AggregateError::Malformed)?;
            map.insert(
                id,
                SignatureShare::<S>::deserialize(s).map_err(|_| AggregateError::Malformed)?,
            );
            back.insert(id, *i);
        }
        match frost_core::aggregate_custom::<S>(&sp, &map, &pk, CheaterDetection::AllCheaters) {
            Ok(sig) => sig.serialize().map_err(|_| AggregateError::Malformed),
            Err(FrostCoreError::InvalidSignatureShare { culprits }) => {
                let mut v: Vec<usize> =
                    culprits.iter().filter_map(|c| back.get(c).copied()).collect();
                v.sort_unstable();
                v.dedup();
                Err(AggregateError::Culprits(v))
            }
            Err(FrostCoreError::IncorrectNumberOfShares) => Err(AggregateError::NotEnough),
            Err(FrostCoreError::InvalidSignature) => Err(AggregateError::Invalid),
            Err(_) => Err(AggregateError::Malformed),
        }
    })
}

/// Verify a signature the way an external chain would: BIP-340 under the Taproot output key, or
/// Ed25519 under the group key.
pub fn verify(group: &GroupKey, message: &[u8], sig: &[u8]) -> bool {
    dispatch!(group.scheme, S => (|| -> Option<bool> {
        let pk = public_package::<S>(group, true).ok()?;
        let sig = Signature::<S>::deserialize(sig).ok()?;
        Some(pk.verifying_key().verify(message, &sig).is_ok())
    })()
    .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::super::keys::{Dealing, Mode, Transcript};
    use super::super::suite::Scheme;
    use super::*;
    use crate::dkg::Params;

    /// A fresh `n`-holder key by running the DKG in memory.
    pub(crate) fn keygen(scheme: Scheme, n: usize) -> Vec<KeyShare> {
        let params = Params::standard(n);
        let mut t = Transcript::new(scheme, Mode::Fresh, params).unwrap();
        let deals: Vec<Dealing> = (0..n)
            .map(|d| Dealing::fresh(scheme, d, params, &[d as u8 + 1; 32]).unwrap())
            .collect();
        for d in &deals {
            t.add_commitments(d.dealer, d.commitments.clone()).unwrap();
        }
        let out = t.finalize().unwrap();
        (0..n)
            .map(|j| {
                let got = (0..n).map(|d| (d, deals[d].share_for(j))).collect();
                out.derive_share(j, &got).unwrap()
            })
            .collect()
    }

    fn entropy(seed: u8) -> impl FnMut() -> [u8; 32] {
        let mut c = 0u8;
        move || {
            c = c.wrapping_add(1);
            [seed ^ c; 32]
        }
    }

    pub(crate) fn pool_and_package(
        keys: &[KeyShare],
        signers: &[usize],
        msg: &[u8],
    ) -> (Vec<(usize, Vec<u8>)>, SigningPackage) {
        let mut nonces = Vec::new();
        let mut commitments = BTreeMap::new();
        for &i in signers {
            let (n, c) = generate_nonces(&keys[i], &[i as u8 + 40; 32]).unwrap();
            commitments.insert(i, c);
            nonces.push((i, n));
        }
        (
            nonces,
            SigningPackage {
                message: msg.to_vec(),
                commitments,
            },
        )
    }

    #[test]
    fn threshold_signatures_verify_for_both_schemes() {
        for scheme in Scheme::ALL {
            let keys = keygen(scheme, 4);
            let group = keys[0].group.clone();
            let msg = [0x42u8; 32];
            let (nonces, pkg) = pool_and_package(&keys, &[0, 2, 3], &msg);
            let shares: BTreeMap<usize, Vec<u8>> = nonces
                .iter()
                .map(|(i, n)| (*i, sign_share(&keys[*i], &pkg, n).unwrap()))
                .collect();
            for (i, s) in &shares {
                assert!(verify_share(&group, &pkg, *i, s));
            }
            let sig = aggregate(&group, &pkg, &shares).unwrap();
            assert_eq!(sig.len(), SIGNATURE_BYTES);
            assert!(verify(&group, &msg, &sig));
            assert!(!verify(&group, &[0x43u8; 32], &sig));
            // package encoding round-trips
            assert_eq!(SigningPackage::decode(&pkg.encode()).unwrap(), pkg);
        }
    }

    #[test]
    fn fewer_than_threshold_signers_cannot_sign() {
        for scheme in Scheme::ALL {
            let keys = keygen(scheme, 4);
            let group = keys[0].group.clone();
            let (nonces, pkg) = pool_and_package(&keys, &[0, 1], &[7u8; 32]);
            // an honest signer refuses a package below the threshold
            assert_eq!(
                sign_share(&keys[0], &pkg, &nonces[0].1),
                Err(SignError::BadPackage)
            );
            // and so does aggregation
            assert!(aggregate(&group, &pkg, &BTreeMap::new()).is_err());
        }
    }

    /// Even a coalition that bypasses every check (key packages whose `min_signers` is lowered to
    /// `t - 1`) only produces an invalid signature: `t - 1` shares reveal nothing about the key.
    #[test]
    fn t_minus_one_colluding_signers_produce_no_valid_signature() {
        use frost_core::keys::KeyPackage;
        use frost_ed25519::Ed25519Sha512 as S;
        let keys = keygen(Scheme::Ed25519, 4);
        let group = keys[0].group.clone();
        let signers = [0usize, 1]; // t = 3, so t - 1 = 2
        let msg = [5u8; 32];
        let (nonces, pkg) = pool_and_package(&keys, &signers, &msg);
        let sp = pkg.to_frost::<S>().unwrap();
        let mut shares = BTreeMap::new();
        for (i, n) in &nonces {
            let kp = keys[*i].key_package::<S>().unwrap();
            let kp = KeyPackage::<S>::new(
                *kp.identifier(),
                *kp.signing_share(),
                *kp.verifying_share(),
                *kp.verifying_key(),
                2,
            );
            let nonces = SigningNonces::<S>::deserialize(n).unwrap();
            let share = frost_core::round2::sign::<S>(&sp, &nonces, &kp).unwrap();
            shares.insert(identifier::<S>(*i).unwrap(), share);
        }
        let mut pk = public_package::<S>(&group, true).unwrap();
        pk = frost_core::keys::PublicKeyPackage::<S>::new(
            pk.verifying_shares().clone(),
            *pk.verifying_key(),
            Some(2),
        );
        // the sum of the two shares is not a signature under the group key
        let r = frost_core::aggregate_custom::<S>(&sp, &shares, &pk, CheaterDetection::Disabled);
        assert!(r.is_err(), "t-1 shares must not verify");
    }

    #[test]
    fn a_malicious_signature_share_is_identified() {
        for scheme in Scheme::ALL {
            let keys = keygen(scheme, 4);
            let group = keys[0].group.clone();
            let (nonces, pkg) = pool_and_package(&keys, &[1, 2, 3], &[9u8; 32]);
            let mut shares: BTreeMap<usize, Vec<u8>> = nonces
                .iter()
                .map(|(i, n)| (*i, sign_share(&keys[*i], &pkg, n).unwrap()))
                .collect();
            // holder 2 sends the share of holder 3's key package instead (a wrong share)
            let wrong = sign_share(&keys[3], &pkg, &nonces[1].1);
            assert_eq!(wrong, Err(SignError::NotOurCommitment));
            // or flips bits in a valid one
            let s = shares.get_mut(&2).unwrap();
            let last = s.len() - 1;
            s[last] ^= 0x01;
            let bad = shares[&2].clone();
            assert!(!verify_share(&group, &pkg, 2, &bad));
            assert_eq!(
                aggregate(&group, &pkg, &shares),
                Err(AggregateError::Culprits(vec![2]))
            );
            // retrying without the culprit, with fresh nonces, works
            let (nonces2, pkg2) = pool_and_package(&keys, &[0, 1, 3], &[9u8; 32]);
            let good: BTreeMap<usize, Vec<u8>> = nonces2
                .iter()
                .map(|(i, n)| (*i, sign_share(&keys[*i], &pkg2, n).unwrap()))
                .collect();
            assert!(verify(
                &group,
                &[9u8; 32],
                &aggregate(&group, &pkg2, &good).unwrap()
            ));
        }
    }

    #[test]
    fn nonce_pool_never_hands_out_a_nonce_twice_even_across_a_restart() {
        let keys = keygen(Scheme::Ed25519, 4);
        let mut pool = NoncePool::new();
        let fresh = pool.top_up(&keys[0], 4, entropy(3)).unwrap();
        assert_eq!(fresh.len(), 4);
        assert_eq!(pool.len(), 4);
        let first = pool.first().unwrap();
        let n = pool.take(first).unwrap();
        // a second take of the same counter is impossible
        assert!(pool.take(first).is_none());
        // a restart from the saved pool does not bring it back
        let mut restarted = NoncePool::decode(&pool.encode()).unwrap();
        assert!(restarted.take(first).is_none());
        assert_eq!(restarted.len(), 3);
        // new counters continue after the old ones
        let more = restarted.top_up(&keys[0], 4, entropy(4)).unwrap();
        assert_eq!(more[0].0, 4);
        assert!(restarted.commitment(first).is_none());
        // taking removes the secret: the bytes we got are the only copy
        assert!(!n.is_empty());
        // a damaged or invented pool is refused
        assert!(NoncePool::decode(&pool.encode()[..10]).is_none());
        let mut bad = pool.encode();
        bad[..8].copy_from_slice(&0u64.to_le_bytes()); // counters above `next`
        assert!(NoncePool::decode(&bad).is_none());
    }

    #[test]
    fn a_share_needs_its_own_commitment_in_the_package() {
        let keys = keygen(Scheme::Taproot, 4);
        let (nonces, pkg) = pool_and_package(&keys, &[0, 1, 2], &[1u8; 32]);
        // holder 0 with holder 1's nonces: the package commitment is not theirs
        assert_eq!(
            sign_share(&keys[0], &pkg, &nonces[1].1),
            Err(SignError::NotOurCommitment)
        );
        // a holder that is not in the package cannot sign
        assert_eq!(
            sign_share(&keys[3], &pkg, &nonces[0].1),
            Err(SignError::NotOurCommitment)
        );
    }
}
