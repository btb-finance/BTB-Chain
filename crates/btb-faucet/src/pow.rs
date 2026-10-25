//! Optional proof of work: the client gets a single-use challenge and must find a `nonce` such
//! that `SHA256(challenge || address || nonce_le)` has `bits` leading zero bits. Binding the
//! address stops one solution from being spent on another address; single use stops replay.
//! This is a speed bump against scripts, not a captcha.

use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(300);
const MAX_PENDING: usize = 20_000;

pub fn leading_zero_bits(h: &[u8]) -> u32 {
    let mut n = 0;
    for b in h {
        if *b == 0 {
            n += 8;
        } else {
            return n + b.leading_zeros();
        }
    }
    n
}

pub fn digest(challenge: &[u8; 16], addr: &[u8; 32], nonce: u64) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(challenge);
    h.update(addr);
    h.update(nonce.to_le_bytes());
    h.finalize().into()
}

/// Find a nonce (used by tests and the CLI self-check; browsers do the same in the page).
pub fn solve(challenge: &[u8; 16], addr: &[u8; 32], bits: u32) -> u64 {
    (0u64..)
        .find(|n| leading_zero_bits(&digest(challenge, addr, *n)) >= bits)
        .expect("a nonce exists")
}

#[derive(Default)]
pub struct Issuer {
    pending: HashMap<[u8; 16], Instant>,
}

impl Issuer {
    /// A fresh challenge, or `None` when too many are outstanding.
    pub fn issue(&mut self, now: Instant) -> Option<[u8; 16]> {
        if self.pending.len() >= MAX_PENDING {
            self.pending.retain(|_, t| now < *t);
            if self.pending.len() >= MAX_PENDING {
                return None;
            }
        }
        let mut c = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut c);
        self.pending.insert(c, now + TTL);
        Some(c)
    }

    /// Check a solution. The challenge is used up whether or not the solution is right.
    pub fn verify(
        &mut self,
        challenge: &[u8; 16],
        addr: &[u8; 32],
        nonce: u64,
        bits: u32,
        now: Instant,
    ) -> bool {
        match self.pending.remove(challenge) {
            Some(exp) if now < exp => leading_zero_bits(&digest(challenge, addr, nonce)) >= bits,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_bits() {
        assert_eq!(leading_zero_bits(&[0, 0, 0x10]), 19);
        assert_eq!(leading_zero_bits(&[0x80]), 0);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
    }

    #[test]
    fn solve_verify_single_use_and_address_bound() {
        let mut i = Issuer::default();
        let now = Instant::now();
        let c = i.issue(now).unwrap();
        let a = [4u8; 32];
        let n = solve(&c, &a, 8);
        // wrong address fails (and burns the challenge)
        assert!(!i.verify(&c, &[5; 32], n, 8, now));
        assert!(!i.verify(&c, &a, n, 8, now), "challenge already used");
        let c2 = i.issue(now).unwrap();
        let n2 = solve(&c2, &a, 8);
        assert!(i.verify(&c2, &a, n2, 8, now));
        assert!(!i.verify(&c2, &a, n2, 8, now), "single use");
        // unknown and expired challenges
        assert!(!i.verify(&[9; 16], &a, 0, 0, now));
        let c3 = i.issue(now).unwrap();
        assert!(!i.verify(&c3, &a, solve(&c3, &a, 4), 4, now + TTL));
    }
}
