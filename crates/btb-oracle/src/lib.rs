//! BTB oracles: the threshold-BLS randomness beacon and price-feed aggregation.
//!
//! * [`bls`]: BLS12-381 over `blst`.
//! * [`threshold`]: Shamir-shared keys, partial signatures, aggregation, the partial collector.
//! * [`dkg`]: joint-Feldman distributed key generation with complaints, resharing, and the
//!   trusted-dealer bootstrap.
//! * [`dkg_crypto`]: encryption of DKG shares to consensus keys, and publicly checkable
//!   decryption proofs for complaints.
//! * [`frost`]: threshold Schnorr keys for other chains (FROST over secp256k1/BIP-340 and Ed25519).
//! * [`beacon`]: the per-height beacon value and seed.
//! * [`price`]: signed price observations and the stake-weighted median.
//!
//! Design and security argument: `docs/ORACLES.md`. Nothing here reads a clock or the network.

pub mod beacon;
pub mod bls;
pub mod dkg;
pub mod dkg_crypto;
pub mod frost;
pub mod price;
pub mod report;
pub mod threshold;

pub use beacon::{beacon_message, seed_of, Beacon};
pub use dkg::{trusted_deal, Dealing, Mode, Outcome, Params, Participant, Transcript};
pub use price::{weighted_median, Observation, SignedObservation};
pub use threshold::{
    combine, threshold_for, verify_partial, Collector, CombineError, GroupKey, KeyError, KeyShare,
};
