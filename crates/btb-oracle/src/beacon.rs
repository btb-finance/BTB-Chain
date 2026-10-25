//! The randomness beacon: one threshold BLS signature per block height.
//!
//! `beacon(h) = BLS_group_sign("btb.beacon.v1" || chain_id || epoch || height)`. It is a
//! deterministic function of `(chain, epoch, height)` and the group key, so there is exactly
//! one valid value per height; it can be produced only by combining `threshold` partial
//! signatures, and each validator releases its partial for height `h` only after block `h` is
//! committed, i.e. after the transactions that will consume the value are fixed.
//!
//! The 32-byte *seed* is `SHA256("btb.beacon.seed.v1" || signature)`. Per-transaction
//! randomness is derived from it in `btb-runtime` as
//! `H(seed, tx id, call index)`.

use crate::bls::{self, G2, G2_BYTES};
use crate::threshold::GroupKey;
use sha2::{Digest, Sha256};

pub const MSG_DOMAIN: &[u8] = b"btb.beacon.v1";
pub const SEED_DOMAIN: &[u8] = b"btb.beacon.seed.v1";

/// The message validators sign for height `height` of `epoch`.
pub fn beacon_message(chain_id: u64, epoch: u64, height: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(MSG_DOMAIN.len() + 24);
    m.extend_from_slice(MSG_DOMAIN);
    m.extend_from_slice(&chain_id.to_be_bytes());
    m.extend_from_slice(&epoch.to_be_bytes());
    m.extend_from_slice(&height.to_be_bytes());
    m
}

/// The seed derived from a beacon signature.
pub fn seed_of(signature: &[u8; G2_BYTES]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(SEED_DOMAIN);
    h.update(signature);
    h.finalize().into()
}

/// A beacon value with what is needed to verify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    pub epoch: u64,
    pub height: u64,
    pub signature: [u8; G2_BYTES],
}

impl Beacon {
    /// Is this the group's signature on `(chain_id, epoch, height)`?
    pub fn verify(&self, chain_id: u64, group: &GroupKey) -> bool {
        let Some(sig) = G2::from_bytes(&self.signature) else {
            return false;
        };
        bls::verify(
            &group.public_key,
            &beacon_message(chain_id, self.epoch, self.height),
            &sig,
        )
    }

    pub fn seed(&self) -> [u8; 32] {
        seed_of(&self.signature)
    }

    /// `epoch:u64 LE || height:u64 LE || signature[96]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(16 + G2_BYTES);
        o.extend_from_slice(&self.epoch.to_le_bytes());
        o.extend_from_slice(&self.height.to_le_bytes());
        o.extend_from_slice(&self.signature);
        o
    }

    pub fn decode(b: &[u8]) -> Option<Beacon> {
        if b.len() != 16 + G2_BYTES {
            return None;
        }
        Some(Beacon {
            epoch: u64::from_le_bytes(b[..8].try_into().ok()?),
            height: u64::from_le_bytes(b[8..16].try_into().ok()?),
            signature: b[16..].try_into().ok()?,
        })
    }
}
