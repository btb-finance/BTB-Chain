//! Share delivery for the on-chain DKG: encryption to a validator's consensus key, and a
//! publicly checkable proof of what a recipient decrypted.
//!
//! The DKG (`crate::dkg`) needs a private channel from every dealer to every recipient. On
//! chain, the channel is a ciphertext in the dealer's transaction, one per recipient, so the
//! whole transcript is public and auditable and no side channel is needed.
//!
//! * **Key.** Every validator's Ed25519 consensus key `A = a*B` doubles as its delivery key
//!   (`a` is the clamped scalar of its secret seed, `SigningKey::to_scalar`): nothing extra is
//!   registered. Using one key for signing and for Diffie-Hellman is acceptable here because the
//!   DH output is only ever hashed together with a domain, the dealer, the recipient and the
//!   epoch, and never used as a signature nonce or exposed (see "On using the same key pair for
//!   Ed25519 and an X25519 based KEM", Kim et al. 2021).
//! * **Encryption.** The dealer draws an ephemeral scalar `e`, publishes `E = e*B` and uses
//!   `S_j = e*A_j` (the same point the recipient computes as `a_j*E`) to derive a ChaCha20-Poly1305
//!   key `SHA256("btb.dkg.key.v1" || S_j || context)`; the plaintext is the 32-byte share. The
//!   context (chain id, epoch, dealer, recipient) is also the associated data, so a ciphertext
//!   cannot be moved to another slot. Each key encrypts exactly one message, so a fixed nonce
//!   is safe.
//! * **Complaints.** A recipient that finds its share wrong cannot reveal its secret key. It
//!   reveals `S_j` together with a Chaum-Pedersen proof that `log_B(A_j) = log_E(S_j)`
//!   (a Schnorr-style DLEQ proof, Fiat-Shamir over SHA-512). Anyone can then decrypt the same
//!   ciphertext and check the share against the dealer's Feldman commitments, so the chain
//!   decides the complaint by itself. The reveal exposes only that one share to the public; a
//!   false complaint therefore costs the accuser its own fee and leaks nothing the dealer, who
//!   knows the share anyway, did not know.
//!
//! The Edwards form is used (rather than X25519) because the DLEQ proof needs the group
//! operation on points; the primitives are the same.

use crate::bls::Fr;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use curve25519_dalek::constants::ED25519_BASEPOINT_POINT as BASE;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::IsIdentity;
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256, Sha512};

/// One encrypted share: 32 bytes of ciphertext and the 16-byte Poly1305 tag.
pub const CT_BYTES: usize = 48;
/// `R1[32] || R2[32] || z[32]`.
pub const PROOF_BYTES: usize = 96;

/// Where a ciphertext lives: it is bound to this slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareCtx {
    pub chain_id: u64,
    /// The epoch the key generation is for.
    pub target: u64,
    /// Index of the dealer in the dealing committee, of the recipient in the new committee.
    pub dealer: u32,
    pub recipient: u32,
}

impl ShareCtx {
    fn bytes(&self) -> [u8; 24] {
        let mut o = [0u8; 24];
        o[..8].copy_from_slice(&self.chain_id.to_le_bytes());
        o[8..16].copy_from_slice(&self.target.to_le_bytes());
        o[16..20].copy_from_slice(&self.dealer.to_le_bytes());
        o[20..].copy_from_slice(&self.recipient.to_le_bytes());
        o
    }
}

/// A recipient's proof of what it decrypted: the shared point and a DLEQ proof for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reveal {
    pub shared: [u8; 32],
    pub proof: [u8; PROOF_BYTES],
}

/// A point of the prime-order subgroup that is not the identity (the only points keys and
/// ephemeral values may be).
fn point(b: &[u8; 32]) -> Option<EdwardsPoint> {
    let p = CompressedEdwardsY(*b).decompress()?;
    (!p.is_identity() && p.is_torsion_free()).then_some(p)
}

/// Is `b` a usable delivery key / ephemeral key?
pub fn valid_point(b: &[u8; 32]) -> bool {
    point(b).is_some()
}

fn scalar_wide(parts: &[&[u8]]) -> Scalar {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    Scalar::from_hash(h)
}

fn sym_key(shared: &EdwardsPoint, ctx: &ShareCtx) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"btb.dkg.key.v1");
    h.update(shared.compress().as_bytes());
    h.update(ctx.bytes());
    h.finalize().into()
}

fn seal(shared: &EdwardsPoint, ctx: &ShareCtx, share: &Fr) -> Option<[u8; CT_BYTES]> {
    let key = sym_key(shared, ctx);
    let c = ChaCha20Poly1305::new((&key).into());
    let aad = ctx.bytes();
    let ct = c
        .encrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload {
                msg: &share.to_bytes_be(),
                aad: &aad,
            },
        )
        .ok()?;
    ct.try_into().ok()
}

fn open(shared: &EdwardsPoint, ctx: &ShareCtx, ct: &[u8; CT_BYTES]) -> Option<Fr> {
    let key = sym_key(shared, ctx);
    let c = ChaCha20Poly1305::new((&key).into());
    let aad = ctx.bytes();
    let pt = c
        .decrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload {
                msg: ct.as_slice(),
                aad: &aad,
            },
        )
        .ok()?;
    Fr::from_bytes_be(&pt.try_into().ok()?)
}

/// What a dealer publishes besides its commitments: the ephemeral public key and one
/// ciphertext per recipient (in committee order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub eph: [u8; 32],
    pub ciphertexts: Vec<[u8; CT_BYTES]>,
}

/// Encrypt `shares[j]` to `recipients[j]`. A recipient whose key is not a valid point gets an
/// all-zero ciphertext (it can never decrypt and never complain; it simply gets no share).
pub fn deliver(
    chain_id: u64,
    target: u64,
    dealer: u32,
    entropy: &[u8; 32],
    recipients: &[[u8; 32]],
    shares: &[Fr],
) -> Delivery {
    let e = scalar_wide(&[b"btb.dkg.eph.v1", entropy, &target.to_le_bytes()]);
    let eph = (e * BASE).compress().to_bytes();
    let ciphertexts = recipients
        .iter()
        .zip(shares)
        .enumerate()
        .map(|(j, (key, share))| {
            let ctx = ShareCtx {
                chain_id,
                target,
                dealer,
                recipient: j as u32,
            };
            point(key)
                .and_then(|a| seal(&(e * a), &ctx, share))
                .unwrap_or([0u8; CT_BYTES])
        })
        .collect();
    Delivery { eph, ciphertexts }
}

/// The recipient decrypts its ciphertext with its consensus key. `None`: undecryptable or not a
/// canonical scalar (the dealer cheated or the ciphertext was damaged): complain.
pub fn decrypt(
    key: &SigningKey,
    eph: &[u8; 32],
    ct: &[u8; CT_BYTES],
    ctx: &ShareCtx,
) -> Option<Fr> {
    let e = point(eph)?;
    open(&(key.to_scalar() * e), ctx, ct)
}

fn challenge(a: &EdwardsPoint, e: &EdwardsPoint, s: &EdwardsPoint, r1: &[u8], r2: &[u8]) -> Scalar {
    scalar_wide(&[
        b"btb.dkg.dleq.v1",
        a.compress().as_bytes(),
        e.compress().as_bytes(),
        s.compress().as_bytes(),
        r1,
        r2,
    ])
}

/// Prove (publicly) what `key`'s owner computed for the dealer ephemeral key `eph`. `None` if
/// `eph` is not a valid point.
pub fn reveal(key: &SigningKey, eph: &[u8; 32]) -> Option<Reveal> {
    let e = point(eph)?;
    let a = key.to_scalar();
    let apub = a * BASE;
    let s = a * e;
    let k = scalar_wide(&[
        b"btb.dkg.dleq.nonce.v1",
        &key.to_scalar_bytes(),
        eph,
        s.compress().as_bytes(),
    ]);
    let (r1, r2) = ((k * BASE).compress(), (k * e).compress());
    let c = challenge(&apub, &e, &s, r1.as_bytes(), r2.as_bytes());
    let z = k + c * a;
    let mut proof = [0u8; PROOF_BYTES];
    proof[..32].copy_from_slice(r1.as_bytes());
    proof[32..64].copy_from_slice(r2.as_bytes());
    proof[64..].copy_from_slice(z.as_bytes());
    Some(Reveal {
        shared: s.compress().to_bytes(),
        proof,
    })
}

/// A checked reveal, ready to open ciphertexts.
pub struct Opened(EdwardsPoint);

impl Opened {
    /// Decrypt `ct` with the proven shared point.
    pub fn open(&self, ct: &[u8; CT_BYTES], ctx: &ShareCtx) -> Option<Fr> {
        open(&self.0, ctx, ct)
    }
}

/// Check that `r.shared` is the Diffie-Hellman value of the owner of `recipient_key` and the
/// ephemeral key `eph`.
pub fn verify_reveal(recipient_key: &[u8; 32], eph: &[u8; 32], r: &Reveal) -> Option<Opened> {
    let a = point(recipient_key)?;
    let e = point(eph)?;
    let s = point(&r.shared)?;
    let r1 = CompressedEdwardsY(r.proof[..32].try_into().ok()?).decompress()?;
    let r2 = CompressedEdwardsY(r.proof[32..64].try_into().ok()?).decompress()?;
    let z = Option::<Scalar>::from(Scalar::from_canonical_bytes(r.proof[64..].try_into().ok()?))?;
    let c = challenge(&a, &e, &s, &r.proof[..32], &r.proof[32..64]);
    let ok = z * BASE == r1 + c * a && z * e == r2 + c * s;
    ok.then_some(Opened(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn ctx(j: u32) -> ShareCtx {
        ShareCtx {
            chain_id: 7,
            target: 3,
            dealer: 1,
            recipient: j,
        }
    }

    #[test]
    fn delivery_round_trips_and_binds_the_slot() {
        let keys: Vec<SigningKey> = (1..=3).map(key).collect();
        let pubs: Vec<[u8; 32]> = keys.iter().map(|k| k.verifying_key().to_bytes()).collect();
        let shares: Vec<Fr> = (1..=3).map(Fr::from_u64).collect();
        let d = deliver(7, 3, 1, &[5; 32], &pubs, &shares);
        for j in 0..3 {
            let got = decrypt(&keys[j], &d.eph, &d.ciphertexts[j], &ctx(j as u32));
            assert_eq!(got, Some(shares[j]));
            // wrong key, wrong slot, damaged ciphertext
            assert!(decrypt(
                &keys[(j + 1) % 3],
                &d.eph,
                &d.ciphertexts[j],
                &ctx(j as u32)
            )
            .is_none());
            assert!(decrypt(&keys[j], &d.eph, &d.ciphertexts[j], &ctx(j as u32 + 1)).is_none());
            let mut bad = d.ciphertexts[j];
            bad[3] ^= 1;
            assert!(decrypt(&keys[j], &d.eph, &bad, &ctx(j as u32)).is_none());
        }
        assert!(!valid_point(&[0; 32]));
        // an invalid recipient key gets the zero ciphertext
        let mut identity = [0u8; 32];
        identity[0] = 1; // the neutral element
        assert!(!valid_point(&identity));
        let d = deliver(7, 3, 1, &[5; 32], &[identity], &shares[..1]);
        assert_eq!(d.ciphertexts[0], [0u8; CT_BYTES]);
    }

    #[test]
    fn reveal_is_checkable_by_anyone_and_unforgeable() {
        let (a, b) = (key(1), key(2));
        let (pa, pb) = (a.verifying_key().to_bytes(), b.verifying_key().to_bytes());
        let share = Fr::from_u64(99);
        let d = deliver(7, 3, 1, &[5; 32], &[pa, pb], &[share, share]);
        let r = reveal(&a, &d.eph).unwrap();
        let o = verify_reveal(&pa, &d.eph, &r).expect("valid proof");
        assert_eq!(o.open(&d.ciphertexts[0], &ctx(0)), Some(share));
        // the proof belongs to a's key and this ephemeral key only
        assert!(verify_reveal(&pb, &d.eph, &r).is_none());
        let other = deliver(7, 3, 2, &[6; 32], &[pa], &[share]);
        assert!(verify_reveal(&pa, &other.eph, &r).is_none());
        // a wrong shared point with a copied proof, and damaged proofs
        let mut wrong = r;
        wrong.shared = reveal(&b, &d.eph).unwrap().shared;
        assert!(verify_reveal(&pa, &d.eph, &wrong).is_none());
        for i in [0usize, 40, 70] {
            let mut bad = r;
            bad.proof[i] ^= 1;
            assert!(verify_reveal(&pa, &d.eph, &bad).is_none(), "byte {i}");
        }
        // b cannot produce a proof that passes for a's key
        assert!(verify_reveal(&pa, &d.eph, &reveal(&b, &d.eph).unwrap()).is_none());
    }
}
