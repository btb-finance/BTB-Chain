//! Threshold Schnorr keys for other chains: FROST over secp256k1 (BIP-340, Bitcoin Taproot) and
//! over Ed25519 (Solana). Design and threat model: `docs/CROSS_CHAIN.md`.
//!
//! * [`suite`]: the two ciphersuites ([`Scheme`]) and generic byte helpers.
//! * [`keys`]: distributed key generation and resharing (joint-Feldman with publicly decidable
//!   complaints, the transport of `crate::dkg_crypto`), [`GroupKey`], [`KeyShare`].
//! * [`sign`]: nonce pools, signature shares, aggregation with blame, verification.
//! * [`addr`]: external addresses derived from group keys (Taproot bech32m, Solana base58).
//!
//! Nothing here reads a clock or the network.

pub mod addr;
pub mod keys;
pub mod sign;
pub mod suite;

pub use keys::{
    valid_point, verify_share, Dealing, FrostError, GroupKey, KeyShare, Mode, Outcome, Transcript,
};
pub use sign::{
    aggregate, generate_nonces, sign_share, valid_commitments, verify,
    verify_share as verify_signature_share, AggregateError, NoncePool, SignError, SigningPackage,
    SIGNATURE_BYTES,
};
pub use suite::Scheme;

/// Most dealers, recipients or signers in one key (`crate::threshold::MAX_HOLDERS`).
pub const MAX_PARTIES: usize = crate::threshold::MAX_HOLDERS;
