//! The two FROST ciphersuites BTB uses for other chains, and the byte-level helpers that let the
//! rest of the code stay generic over them.
//!
//! * [`Scheme::Taproot`]: FROST(secp256k1, SHA-256) in its BIP-340/BIP-341 form
//!   (`frost-secp256k1-tr`). The DKG output is the *internal key* `P`; signing always applies the
//!   BIP-341 key-path tweak with an empty script tree, so signatures verify under the Taproot
//!   output key `Q = lift_x(P) + H_TapTweak(P) * G` (the key a Bitcoin address commits to) with any
//!   BIP-340 verifier.
//! * [`Scheme::Ed25519`]: FROST(Ed25519, SHA-512) (`frost-ed25519`, RFC 9591). Signatures are
//!   plain RFC 8032 Ed25519 signatures that Solana (and `ed25519-dalek`) verify.
//!
//! Threshold ECDSA for Ethereum is deliberately absent (see `docs/CROSS_CHAIN.md`).

use frost_core::keys::{KeyPackage, PublicKeyPackage};
use frost_core::{Ciphersuite, Field, Group};

/// A signature scheme of an external chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scheme {
    /// Schnorr over secp256k1, BIP-340 (Bitcoin Taproot).
    Taproot,
    /// EdDSA over Edwards25519 (Solana).
    Ed25519,
}

impl Scheme {
    pub const ALL: [Scheme; 2] = [Scheme::Taproot, Scheme::Ed25519];

    pub fn byte(self) -> u8 {
        match self {
            Scheme::Taproot => 0,
            Scheme::Ed25519 => 1,
        }
    }

    pub fn from_byte(b: u8) -> Option<Scheme> {
        match b {
            0 => Some(Scheme::Taproot),
            1 => Some(Scheme::Ed25519),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scheme::Taproot => "taproot",
            Scheme::Ed25519 => "ed25519",
        }
    }

    pub fn from_name(n: &str) -> Option<Scheme> {
        Scheme::ALL.into_iter().find(|s| s.name() == n)
    }

    /// Serialized group element: compressed SEC1 point, or a compressed Edwards point.
    pub fn point_len(self) -> usize {
        match self {
            Scheme::Taproot => 33,
            Scheme::Ed25519 => 32,
        }
    }
}

/// Everything the generic code needs beyond [`Ciphersuite`].
pub trait Suite: Ciphersuite {
    const SCHEME: Scheme;

    /// Apply the scheme's output-key tweak to a signer's key package (BIP-341 for Taproot,
    /// nothing for Ed25519).
    fn tweak_key_package(kp: KeyPackage<Self>) -> KeyPackage<Self>;

    /// The same tweak applied to the public key package.
    fn tweak_public(pk: PublicKeyPackage<Self>) -> PublicKeyPackage<Self>;

    /// The 32 bytes an external verifier or address commits to, from the *tweaked* public key
    /// package: the x-only output key (Taproot) or the Ed25519 public key.
    fn output_key(tweaked: &PublicKeyPackage<Self>) -> Option<[u8; 32]>;
}

impl Suite for frost_secp256k1_tr::Secp256K1Sha256TR {
    const SCHEME: Scheme = Scheme::Taproot;

    fn tweak_key_package(kp: KeyPackage<Self>) -> KeyPackage<Self> {
        use frost_secp256k1_tr::keys::Tweak;
        kp.tweak::<&[u8]>(None)
    }

    fn tweak_public(pk: PublicKeyPackage<Self>) -> PublicKeyPackage<Self> {
        use frost_secp256k1_tr::keys::Tweak;
        pk.tweak::<&[u8]>(None)
    }

    fn output_key(tweaked: &PublicKeyPackage<Self>) -> Option<[u8; 32]> {
        let b = tweaked.verifying_key().serialize().ok()?;
        b.get(1..33)?.try_into().ok()
    }
}

impl Suite for frost_ed25519::Ed25519Sha512 {
    const SCHEME: Scheme = Scheme::Ed25519;

    fn tweak_key_package(kp: KeyPackage<Self>) -> KeyPackage<Self> {
        kp
    }

    fn tweak_public(pk: PublicKeyPackage<Self>) -> PublicKeyPackage<Self> {
        pk
    }

    fn output_key(tweaked: &PublicKeyPackage<Self>) -> Option<[u8; 32]> {
        tweaked.verifying_key().serialize().ok()?.try_into().ok()
    }
}

/// Run `$body` with `$S` bound to the ciphersuite of `$scheme`.
macro_rules! dispatch {
    ($scheme:expr, $S:ident => $body:expr) => {
        match $scheme {
            $crate::frost::Scheme::Taproot => {
                type $S = frost_secp256k1_tr::Secp256K1Sha256TR;
                $body
            }
            $crate::frost::Scheme::Ed25519 => {
                type $S = frost_ed25519::Ed25519Sha512;
                $body
            }
        }
    };
}
pub(crate) use dispatch;

pub(crate) type Sc<C> = <<<C as Ciphersuite>::Group as Group>::Field as Field>::Scalar;
pub(crate) type El<C> = <<C as Ciphersuite>::Group as Group>::Element;

/// A scalar from its 32-byte serialization (`None`: not canonical).
pub(crate) fn sc<C: Ciphersuite>(b: &[u8]) -> Option<Sc<C>> {
    let ser =
        <<<C as Ciphersuite>::Group as Group>::Field as Field>::Serialization::try_from(b).ok()?;
    <<<C as Ciphersuite>::Group as Group>::Field as Field>::deserialize(&ser).ok()
}

pub(crate) fn sc_bytes<C: Ciphersuite>(s: &Sc<C>) -> [u8; 32] {
    let mut out = [0u8; 32];
    let ser = <<<C as Ciphersuite>::Group as Group>::Field as Field>::serialize(s);
    let b = ser.as_ref();
    out[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
    out
}

/// A group element from its serialization (`None`: not a valid, prime-order, non-identity point).
pub(crate) fn el<C: Ciphersuite>(b: &[u8]) -> Option<El<C>> {
    let ser = <<C as Ciphersuite>::Group as Group>::Serialization::try_from(b).ok()?;
    let e = <<C as Ciphersuite>::Group as Group>::deserialize(&ser).ok()?;
    (e != <<C as Ciphersuite>::Group as Group>::identity()).then_some(e)
}

/// `None` for the identity (which has no serialization in either ciphersuite).
pub(crate) fn el_bytes<C: Ciphersuite>(e: &El<C>) -> Option<Vec<u8>> {
    <<C as Ciphersuite>::Group as Group>::serialize(e)
        .ok()
        .map(|s| s.as_ref().to_vec())
}

pub(crate) fn sc_from_u64<C: Ciphersuite>(mut x: u64) -> Sc<C> {
    type F<C> = <<C as Ciphersuite>::Group as Group>::Field;
    let (mut acc, mut pow) = (F::<C>::zero(), F::<C>::one());
    while x > 0 {
        if x & 1 == 1 {
            acc = acc + pow;
        }
        pow = pow + pow;
        x >>= 1;
    }
    acc
}

pub(crate) fn generator<C: Ciphersuite>() -> El<C> {
    <<C as Ciphersuite>::Group as Group>::generator()
}

pub(crate) fn identity<C: Ciphersuite>() -> El<C> {
    <<C as Ciphersuite>::Group as Group>::identity()
}

pub(crate) fn zero<C: Ciphersuite>() -> Sc<C> {
    <<<C as Ciphersuite>::Group as Group>::Field as Field>::zero()
}

pub(crate) fn one<C: Ciphersuite>() -> Sc<C> {
    <<<C as Ciphersuite>::Group as Group>::Field as Field>::one()
}

pub(crate) fn invert<C: Ciphersuite>(s: &Sc<C>) -> Option<Sc<C>> {
    <<<C as Ciphersuite>::Group as Group>::Field as Field>::invert(s).ok()
}

/// Lagrange coefficient of `xs[k]` at `x` over the distinct x-coordinates `xs`.
pub(crate) fn lagrange<C: Ciphersuite>(xs: &[u64], k: usize, x: u64) -> Option<Sc<C>> {
    let (fx, fk) = (sc_from_u64::<C>(x), sc_from_u64::<C>(xs[k]));
    let (mut num, mut den) = (one::<C>(), one::<C>());
    for (j, xj) in xs.iter().enumerate() {
        if j == k {
            continue;
        }
        let fj = sc_from_u64::<C>(*xj);
        num = num * (fx - fj);
        den = den * (fk - fj);
    }
    Some(num * invert::<C>(&den)?)
}

/// `sum_k commitments[k] * x^k`.
pub(crate) fn eval_commitments<C: Ciphersuite>(c: &[El<C>], x: u64) -> El<C> {
    let fx = sc_from_u64::<C>(x);
    c.iter().rev().fold(identity::<C>(), |acc, p| acc * fx + *p)
}
