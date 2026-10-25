//! Price observations and the stake-weighted median.
//!
//! A validator signs `(pair, price, height)` with its consensus key. The chain keeps each
//! validator's latest fresh observation per pair and, every block that brought news, takes the
//! **stake-weighted median** of them. If the weights of the reporting validators sum to at
//! least two thirds of the total stake, then validators holding less than one third of the
//! total stake hold less than one half of the reporting weight, so the median lies between two
//! values reported by honest validators: a minority cannot move the price outside the range
//! the honest validators saw.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

pub const OBS_DOMAIN: &[u8] = b"btb.oracle.obs.v1";
pub const OBS_BYTES: usize = 32 + 4 + 16 + 8 + 64;

/// What a validator saw: `price` (an integer in the pair's decimals) at chain height `height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub pair: u32,
    pub price: u128,
    /// The chain height the validator had executed when it made the observation.
    pub height: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedObservation {
    /// The validator's Ed25519 consensus key.
    pub key: [u8; 32],
    pub obs: Observation,
    pub sig: [u8; 64],
}

fn digest(chain_id: u64, o: &Observation) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(OBS_DOMAIN);
    h.update(chain_id.to_be_bytes());
    h.update(o.pair.to_be_bytes());
    h.update(o.price.to_be_bytes());
    h.update(o.height.to_be_bytes());
    h.finalize().into()
}

impl SignedObservation {
    pub fn sign(chain_id: u64, key: &SigningKey, obs: Observation) -> SignedObservation {
        SignedObservation {
            key: key.verifying_key().to_bytes(),
            obs,
            sig: key.sign(&digest(chain_id, &obs)).to_bytes(),
        }
    }

    /// Strict Ed25519 verification under `key` over a digest bound to the chain id.
    pub fn verify(&self, chain_id: u64) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(&self.key) else {
            return false;
        };
        vk.verify_strict(
            &digest(chain_id, &self.obs),
            &Signature::from_bytes(&self.sig),
        )
        .is_ok()
    }

    /// `key[32] || pair:u32 LE || price:u128 LE || height:u64 LE || sig[64]`.
    pub fn encode(&self) -> [u8; OBS_BYTES] {
        let mut o = [0u8; OBS_BYTES];
        o[..32].copy_from_slice(&self.key);
        o[32..36].copy_from_slice(&self.obs.pair.to_le_bytes());
        o[36..52].copy_from_slice(&self.obs.price.to_le_bytes());
        o[52..60].copy_from_slice(&self.obs.height.to_le_bytes());
        o[60..].copy_from_slice(&self.sig);
        o
    }

    pub fn decode(b: &[u8]) -> Option<SignedObservation> {
        if b.len() != OBS_BYTES {
            return None;
        }
        Some(SignedObservation {
            key: b[..32].try_into().ok()?,
            obs: Observation {
                pair: u32::from_le_bytes(b[32..36].try_into().ok()?),
                price: u128::from_le_bytes(b[36..52].try_into().ok()?),
                height: u64::from_le_bytes(b[52..60].try_into().ok()?),
            },
            sig: b[60..].try_into().ok()?,
        })
    }
}

/// Lower weighted median of `(value, weight)`: sort by value, then the first value at which
/// the cumulative weight reaches half of the total (`2 * cumulative >= total`). Zero-weight
/// entries are ignored; `None` if nothing weighs anything. Ties in value are irrelevant.
pub fn weighted_median(items: &[(u128, u64)]) -> Option<u128> {
    let mut v: Vec<(u128, u64)> = items.iter().copied().filter(|(_, w)| *w > 0).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by_key(|(p, _)| *p);
    let total: u128 = v.iter().map(|(_, w)| u128::from(*w)).sum();
    let mut cum = 0u128;
    for (p, w) in &v {
        cum += u128::from(*w);
        if cum * 2 >= total {
            return Some(*p);
        }
    }
    v.last().map(|(p, _)| *p)
}
