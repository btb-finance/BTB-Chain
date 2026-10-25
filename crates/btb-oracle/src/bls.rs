//! BLS12-381 building blocks over `blst` (Apache-2.0): scalars, G1 public keys, G2 signatures.
//!
//! "min-pk" layout: public keys and Feldman commitments live in G1 (48 bytes compressed),
//! signatures in G2 (96 bytes compressed). Everything here is deterministic and constant-size;
//! decoding is total (never panics) and checks that points are on the curve and in the
//! prime-order subgroup.
//!
//! The `unsafe` blocks only call `blst` C functions on fully initialised, properly aligned
//! values owned by this module.

use blst::*;
use std::fmt;

/// Domain separation tag for hashing beacon messages to G2.
pub const DST: &[u8] = b"BTB_BEACON_BLS12381G2_XMD:SHA-256_SSWU_RO_V1_";

pub const G1_BYTES: usize = 48;
pub const G2_BYTES: usize = 96;
pub const SCALAR_BYTES: usize = 32;

/// An element of the scalar field (the group order `r`), kept in Montgomery form by `blst`.
#[derive(Clone, Copy)]
pub struct Fr(blst_fr);

impl fmt::Debug for Fr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print secrets.
        write!(f, "Fr(..)")
    }
}

impl PartialEq for Fr {
    fn eq(&self, o: &Fr) -> bool {
        self.to_bytes_be() == o.to_bytes_be()
    }
}
impl Eq for Fr {}

impl Fr {
    pub fn zero() -> Fr {
        Fr::from_u64(0)
    }

    pub fn one() -> Fr {
        Fr::from_u64(1)
    }

    pub fn from_u64(v: u64) -> Fr {
        let a = [v, 0, 0, 0];
        let mut r = blst_fr::default();
        // SAFETY: `a` has 4 limbs, `r` is a valid out-pointer.
        unsafe { blst_fr_from_uint64(&mut r, a.as_ptr()) };
        Fr(r)
    }

    /// Interpret `bytes` (any length) as a big-endian integer and reduce it modulo `r`.
    pub fn from_bytes_reduce(bytes: &[u8]) -> Fr {
        let mut s = blst_scalar::default();
        // SAFETY: pointer and length describe `bytes`; `s` is a valid out-pointer.
        unsafe { blst_scalar_from_be_bytes(&mut s, bytes.as_ptr(), bytes.len()) };
        let mut r = blst_fr::default();
        // SAFETY: both are valid.
        unsafe { blst_fr_from_scalar(&mut r, &s) };
        Fr(r)
    }

    /// Canonical 32-byte big-endian encoding; `None` if the value is not below `r`.
    pub fn from_bytes_be(b: &[u8; 32]) -> Option<Fr> {
        let mut s = blst_scalar::default();
        // SAFETY: `b` is 32 readable bytes.
        unsafe { blst_scalar_from_bendian(&mut s, b.as_ptr()) };
        // SAFETY: `s` initialised.
        if !unsafe { blst_scalar_fr_check(&s) } {
            return None;
        }
        let mut r = blst_fr::default();
        // SAFETY: both are valid.
        unsafe { blst_fr_from_scalar(&mut r, &s) };
        Some(Fr(r))
    }

    pub fn to_bytes_be(&self) -> [u8; 32] {
        let mut s = blst_scalar::default();
        // SAFETY: both are valid.
        unsafe { blst_scalar_from_fr(&mut s, &self.0) };
        let mut out = [0u8; 32];
        // SAFETY: `out` has room for 32 bytes.
        unsafe { blst_bendian_from_scalar(out.as_mut_ptr(), &s) };
        out
    }

    fn to_scalar(self) -> blst_scalar {
        let mut s = blst_scalar::default();
        // SAFETY: both are valid.
        unsafe { blst_scalar_from_fr(&mut s, &self.0) };
        s
    }

    pub fn is_zero(&self) -> bool {
        self.to_bytes_be() == [0u8; 32]
    }

    pub fn add(&self, o: &Fr) -> Fr {
        let mut r = blst_fr::default();
        // SAFETY: all pointers valid.
        unsafe { blst_fr_add(&mut r, &self.0, &o.0) };
        Fr(r)
    }

    pub fn sub(&self, o: &Fr) -> Fr {
        let mut r = blst_fr::default();
        // SAFETY: all pointers valid.
        unsafe { blst_fr_sub(&mut r, &self.0, &o.0) };
        Fr(r)
    }

    pub fn mul(&self, o: &Fr) -> Fr {
        let mut r = blst_fr::default();
        // SAFETY: all pointers valid.
        unsafe { blst_fr_mul(&mut r, &self.0, &o.0) };
        Fr(r)
    }

    /// Multiplicative inverse; `None` for zero.
    pub fn inverse(&self) -> Option<Fr> {
        if self.is_zero() {
            return None;
        }
        let mut r = blst_fr::default();
        // SAFETY: all pointers valid; input non-zero.
        unsafe { blst_fr_eucl_inverse(&mut r, &self.0) };
        Some(Fr(r))
    }
}

/// A point of G1 (public keys, Feldman commitments).
#[derive(Clone, Copy)]
pub struct G1(blst_p1);

impl fmt::Debug for G1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "G1({})", hex8(&self.to_bytes()))
    }
}

impl PartialEq for G1 {
    fn eq(&self, o: &G1) -> bool {
        self.to_bytes() == o.to_bytes()
    }
}
impl Eq for G1 {}

fn hex8(b: &[u8]) -> String {
    b.iter().take(4).map(|x| format!("{x:02x}")).collect()
}

impl G1 {
    pub fn identity() -> G1 {
        G1(blst_p1::default())
    }

    pub fn generator() -> G1 {
        // SAFETY: returns a pointer to a static.
        G1(unsafe { *blst_p1_generator() })
    }

    pub fn is_identity(&self) -> bool {
        // SAFETY: valid pointer.
        unsafe { blst_p1_is_inf(&self.0) }
    }

    pub fn mul(&self, k: &Fr) -> G1 {
        let s = k.to_scalar();
        let mut r = blst_p1::default();
        // SAFETY: `s.b` is 32 bytes (256 bits).
        unsafe { blst_p1_mult(&mut r, &self.0, s.b.as_ptr(), 255) };
        G1(r)
    }

    /// Multiply by a small public integer (cheap: only as many bit steps as `k` has bits).
    pub fn mul_u64(&self, k: u64) -> G1 {
        let bits = (64 - k.leading_zeros()).max(1) as usize;
        let bytes = k.to_le_bytes();
        let mut r = blst_p1::default();
        // SAFETY: `bytes` has 8 bytes (64 bits) and `bits <= 64`.
        unsafe { blst_p1_mult(&mut r, &self.0, bytes.as_ptr(), bits) };
        G1(r)
    }

    pub fn add(&self, o: &G1) -> G1 {
        let mut r = blst_p1::default();
        // SAFETY: all pointers valid; `add_or_double` handles equal inputs.
        unsafe { blst_p1_add_or_double(&mut r, &self.0, &o.0) };
        G1(r)
    }

    pub fn to_bytes(&self) -> [u8; G1_BYTES] {
        let mut out = [0u8; G1_BYTES];
        // SAFETY: `out` has 48 bytes.
        unsafe { blst_p1_compress(out.as_mut_ptr(), &self.0) };
        out
    }

    /// Decode a compressed point; checks curve and subgroup membership. The identity is
    /// accepted (callers that must not accept it check `is_identity`).
    pub fn from_bytes(b: &[u8; G1_BYTES]) -> Option<G1> {
        let mut aff = blst_p1_affine::default();
        // SAFETY: `b` is 48 bytes.
        if unsafe { blst_p1_uncompress(&mut aff, b.as_ptr()) } != BLST_ERROR::BLST_SUCCESS {
            return None;
        }
        // SAFETY: `aff` initialised.
        if !unsafe { blst_p1_affine_in_g1(&aff) } {
            return None;
        }
        let mut p = blst_p1::default();
        // SAFETY: valid.
        unsafe { blst_p1_from_affine(&mut p, &aff) };
        Some(G1(p))
    }

    fn affine(&self) -> blst_p1_affine {
        let mut a = blst_p1_affine::default();
        // SAFETY: valid.
        unsafe { blst_p1_to_affine(&mut a, &self.0) };
        a
    }
}

/// A point of G2 (signatures).
#[derive(Clone, Copy)]
pub struct G2(blst_p2);

impl fmt::Debug for G2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "G2({})", hex8(&self.to_bytes()))
    }
}

impl PartialEq for G2 {
    fn eq(&self, o: &G2) -> bool {
        self.to_bytes() == o.to_bytes()
    }
}
impl Eq for G2 {}

impl G2 {
    pub fn identity() -> G2 {
        G2(blst_p2::default())
    }

    pub fn is_identity(&self) -> bool {
        // SAFETY: valid pointer.
        unsafe { blst_p2_is_inf(&self.0) }
    }

    /// Hash a message to G2 (RFC 9380 `hash_to_curve`, suite `BLS12381G2_XMD:SHA-256_SSWU_RO_`).
    pub fn hash_to_curve(msg: &[u8]) -> G2 {
        let mut p = blst_p2::default();
        // SAFETY: pointers and lengths describe `msg` and `DST`; no augmentation.
        unsafe {
            blst_hash_to_g2(
                &mut p,
                msg.as_ptr(),
                msg.len(),
                DST.as_ptr(),
                DST.len(),
                std::ptr::null(),
                0,
            )
        };
        G2(p)
    }

    pub fn mul(&self, k: &Fr) -> G2 {
        let s = k.to_scalar();
        let mut r = blst_p2::default();
        // SAFETY: `s.b` is 32 bytes (256 bits).
        unsafe { blst_p2_mult(&mut r, &self.0, s.b.as_ptr(), 255) };
        G2(r)
    }

    pub fn add(&self, o: &G2) -> G2 {
        let mut r = blst_p2::default();
        // SAFETY: all pointers valid.
        unsafe { blst_p2_add_or_double(&mut r, &self.0, &o.0) };
        G2(r)
    }

    pub fn to_bytes(&self) -> [u8; G2_BYTES] {
        let mut out = [0u8; G2_BYTES];
        // SAFETY: `out` has 96 bytes.
        unsafe { blst_p2_compress(out.as_mut_ptr(), &self.0) };
        out
    }

    pub fn from_bytes(b: &[u8; G2_BYTES]) -> Option<G2> {
        let mut aff = blst_p2_affine::default();
        // SAFETY: `b` is 96 bytes.
        if unsafe { blst_p2_uncompress(&mut aff, b.as_ptr()) } != BLST_ERROR::BLST_SUCCESS {
            return None;
        }
        // SAFETY: `aff` initialised.
        if !unsafe { blst_p2_affine_in_g2(&aff) } {
            return None;
        }
        let mut p = blst_p2::default();
        // SAFETY: valid.
        unsafe { blst_p2_from_affine(&mut p, &aff) };
        Some(G2(p))
    }

    fn affine(&self) -> blst_p2_affine {
        let mut a = blst_p2_affine::default();
        // SAFETY: valid.
        unsafe { blst_p2_to_affine(&mut a, &self.0) };
        a
    }
}

/// Verify `sig` as a BLS signature on `msg` under `pk`: `e(pk, H(msg)) == e(g1, sig)`.
/// Rejects the identity key and the identity signature.
pub fn verify(pk: &G1, msg: &[u8], sig: &G2) -> bool {
    if pk.is_identity() || sig.is_identity() {
        return false;
    }
    let (pk, sig) = (pk.affine(), sig.affine());
    // SAFETY: affine points are initialised, `msg` and `DST` pointers are valid for their lengths.
    let r = unsafe {
        blst_core_verify_pk_in_g1(
            &pk,
            &sig,
            true,
            msg.as_ptr(),
            msg.len(),
            DST.as_ptr(),
            DST.len(),
            std::ptr::null(),
            0,
        )
    };
    r == BLST_ERROR::BLST_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_arithmetic() {
        let a = Fr::from_u64(7);
        let b = Fr::from_u64(5);
        assert_eq!(a.add(&b), Fr::from_u64(12));
        assert_eq!(a.sub(&b), Fr::from_u64(2));
        assert_eq!(a.mul(&b), Fr::from_u64(35));
        let inv = a.inverse().unwrap();
        assert_eq!(a.mul(&inv), Fr::one());
        assert!(Fr::zero().inverse().is_none());
        // negative wraps modulo r, and round-trips through bytes
        let neg = Fr::zero().sub(&Fr::one());
        assert_eq!(Fr::from_bytes_be(&neg.to_bytes_be()), Some(neg));
        assert_eq!(neg.add(&Fr::one()), Fr::zero());
        // a non-canonical encoding (all ones) is rejected
        assert!(Fr::from_bytes_be(&[0xff; 32]).is_none());
        // reduction accepts it
        assert!(!Fr::from_bytes_reduce(&[0xff; 64]).is_zero());
    }

    #[test]
    fn group_and_signature() {
        let sk = Fr::from_u64(123_456_789);
        let pk = G1::generator().mul(&sk);
        let msg = b"hello";
        let sig = G2::hash_to_curve(msg).mul(&sk);
        assert!(verify(&pk, msg, &sig));
        assert!(!verify(&pk, b"other", &sig));
        let other = G1::generator().mul(&Fr::from_u64(5));
        assert!(!verify(&other, msg, &sig));
        // encodings round trip, garbage is rejected
        assert_eq!(G1::from_bytes(&pk.to_bytes()), Some(pk));
        assert_eq!(G2::from_bytes(&sig.to_bytes()), Some(sig));
        assert!(G1::from_bytes(&[0xAB; 48]).is_none());
        assert!(G2::from_bytes(&[0xAB; 96]).is_none());
        // linearity: (a+b)G = aG + bG
        let a = Fr::from_u64(11);
        let b = Fr::from_u64(31);
        assert_eq!(
            G1::generator().mul(&a.add(&b)),
            G1::generator().mul(&a).add(&G1::generator().mul(&b))
        );
        // identity is never a valid key or signature
        assert!(!verify(&G1::identity(), msg, &sig));
        assert!(!verify(&pk, msg, &G2::identity()));
    }
}
