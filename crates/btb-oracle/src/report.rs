//! Signed reports for other chains (`docs/ORACLES.md`, section 4).
//!
//! A contract asks for a report with the `report_request` host call. The beacon committee
//! threshold-signs
//!
//! ```text
//! "btb.report.v1" || chain_id:u64be || contract[32] || id:u64be || SHA256(data)
//! ```
//!
//! with the same BLS12-381 group key as the randomness beacon (public key in G1, signature in
//! G2, hash-to-curve `BLS12381G2_XMD:SHA-256_SSWU_RO_` with [`crate::bls::DST`]). The message
//! prefix differs from the beacon's (`btb.beacon.v1`), so a report signature can never be a
//! beacon value and the other way round. The signature binds the chain, the requesting contract
//! and the report id, so a report cannot be replayed for another contract or another chain, and a
//! consumer on the target chain only has to check ONE signature against a 48-byte key.
//!
//! [`verify`] needs nothing but that key and the report; it is the Rust verifier a relayer or a
//! light client uses. `docs/ORACLES.md` has the same check for an EVM verifier.

use crate::bls::{self, G1, G1_BYTES, G2, G2_BYTES};
use sha2::{Digest, Sha256};

pub const REPORT_DOMAIN: &[u8] = b"btb.report.v1";

/// The bytes the committee signs for report `id` of `contract`.
pub fn report_message(chain_id: u64, contract: &[u8; 32], id: u64, data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(REPORT_DOMAIN.len() + 8 + 32 + 8 + 32);
    m.extend_from_slice(REPORT_DOMAIN);
    m.extend_from_slice(&chain_id.to_be_bytes());
    m.extend_from_slice(contract);
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&Sha256::digest(data));
    m
}

/// A report with its group signature, as an RPC serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedReport {
    pub chain_id: u64,
    pub contract: [u8; 32],
    pub id: u64,
    pub data: Vec<u8>,
    pub signature: [u8; G2_BYTES],
}

impl SignedReport {
    /// Is this the committee's signature (group public key `group_key`, 48 bytes compressed)?
    pub fn verify(&self, group_key: &[u8; G1_BYTES]) -> bool {
        verify(
            group_key,
            self.chain_id,
            &self.contract,
            self.id,
            &self.data,
            &self.signature,
        )
    }
}

/// Check a report signature against the committee's group public key. Total: malformed keys and
/// signatures, points outside the subgroup, and the identity element all return `false`.
pub fn verify(
    group_key: &[u8; G1_BYTES],
    chain_id: u64,
    contract: &[u8; 32],
    id: u64,
    data: &[u8],
    signature: &[u8; G2_BYTES],
) -> bool {
    let (Some(pk), Some(sig)) = (G1::from_bytes(group_key), G2::from_bytes(signature)) else {
        return false;
    };
    bls::verify(&pk, &report_message(chain_id, contract, id, data), &sig)
}
