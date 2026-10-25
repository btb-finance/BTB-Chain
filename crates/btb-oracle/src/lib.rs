//! BTB oracles: the threshold-BLS randomness beacon and price-feed aggregation.
//!
//! * [`bls`]: BLS12-381 over `blst`.
//! * [`threshold`]: Shamir-shared keys, partial signatures, aggregation, the partial collector.
//! * [`dkg`]: joint-Feldman distributed key generation with complaints, resharing, and the
//!   trusted-dealer bootstrap.
//! * [`beacon`]: the per-height beacon value and seed.
//! * [`price`]: signed price observations and the stake-weighted median.
//!
//! Design and security argument: `docs/ORACLES.md`. Nothing here reads a clock or the network.

pub mod beacon;
pub mod bls;
pub mod dkg;
pub mod price;
pub mod threshold;

pub use beacon::{beacon_message, seed_of, Beacon};
pub use dkg::{trusted_deal, Dealing, Mode, Outcome, Params, Participant, Transcript};
pub use price::{weighted_median, Observation, SignedObservation};
pub use threshold::{
    combine, threshold_for, verify_partial, Collector, CombineError, GroupKey, KeyError, KeyShare,
};
