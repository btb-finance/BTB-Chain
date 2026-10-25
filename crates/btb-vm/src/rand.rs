//! Per-transaction randomness derived from the beacon seed.
//!
//! ```text
//! tx_seed        = SHA256("btb.rand.tx.v1"  || beacon_seed || tx_id)
//! block j of the c-th random_bytes call (c counts from 0 within the transaction)
//!                = SHA256("btb.rand.out.v1" || tx_seed || c:u64be || j:u32be)
//! random_bytes(len) = first `len` bytes of block 0 || block 1 || ...
//! ```
//!
//! Everything is a hash of values all replicas agree on, so every replica derives the same
//! bytes. Two transactions in one block get unrelated streams (different `tx_id`), and two
//! calls in one transaction get different streams (different `c`). `tx_id` of a cron run is
//! the cron receipt id (`SHA256("btb.cron.v1" || height || contract || function)`).

use sha2::{Digest, Sha256};

pub type Seed = [u8; 32];

/// The seed of one transaction (or cron run).
pub fn tx_seed(beacon_seed: &Seed, tx_id: &[u8; 32]) -> Seed {
    let mut h = Sha256::new();
    h.update(b"btb.rand.tx.v1");
    h.update(beacon_seed);
    h.update(tx_id);
    h.finalize().into()
}

/// The bytes of call number `call` within a transaction whose seed is `seed`.
pub fn stream(seed: &Seed, call: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut j = 0u32;
    while out.len() < len {
        let mut h = Sha256::new();
        h.update(b"btb.rand.out.v1");
        h.update(seed);
        h.update(call.to_be_bytes());
        h.update(j.to_be_bytes());
        out.extend_from_slice(&h.finalize());
        j += 1;
    }
    out.truncate(len);
    out
}

/// The state a host keeps to serve `random_bytes`: the transaction seed and a call counter.
#[derive(Debug, Clone)]
pub struct RandomSource {
    seed: Seed,
    calls: u64,
}

impl RandomSource {
    pub fn new(beacon_seed: &Seed, tx_id: &[u8; 32]) -> Self {
        RandomSource {
            seed: tx_seed(beacon_seed, tx_id),
            calls: 0,
        }
    }

    pub fn next(&mut self, len: usize) -> Vec<u8> {
        let c = self.calls;
        self.calls += 1;
        stream(&self.seed, c, len)
    }
}
