//! Cross-language check for payment-channel vouchers (`sdk-ts/src/voucher.ts`).
//!
//! `sdk-ts/test/fixtures/voucher.json` holds vouchers that `btb-voucher` signed. Ed25519 is
//! deterministic, so the TypeScript side must reproduce the digest, the signature and the wire
//! bytes exactly, and must verify the Rust signatures. Regenerate with
//! `BTB_UPDATE_FIXTURES=1 cargo test -p btb-tests --test ts_voucher_fixture`.

use btb_voucher::{SignedVoucher, Voucher};
use ed25519_dalek::SigningKey;
use std::path::PathBuf;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The group order L of edwards25519, little endian.
const L: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

fn add_le(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let (mut out, mut carry) = ([0u8; 32], 0u16);
    for i in 0..32 {
        let t = a[i] as u16 + b[i] as u16 + carry;
        out[i] = t as u8;
        carry = t >> 8;
    }
    assert_eq!(carry, 0);
    out
}

/// Encodings of small-order points, canonical and not.
fn point(name: &str) -> [u8; 32] {
    let mut p = [0xffu8; 32];
    p[31] = 0x7f;
    let mut b = [0u8; 32];
    match name {
        "identity" => b[0] = 1,
        // y = p + 1: the identity, y not reduced.
        "identity_noncanonical" => {
            p[0] = 0xee;
            return p;
        }
        // x = 0 with the sign bit set ("negative zero").
        "identity_negative_zero" => {
            b[0] = 1;
            b[31] = 0x80;
        }
        // (0, -1), order 2.
        "order2" => {
            p[0] = 0xec;
            return p;
        }
        // y = 0, order 4.
        "order4" => {}
        _ => panic!("{name}"),
    }
    b
}

/// Signatures and keys that RFC 8032 style verification may accept but `verify_strict`
/// rejects. Each is a full voucher (the `basic` case with another signature) plus the key to
/// check it against; Rust must reject every one, and so must `sdk-ts`.
fn rejected(key: &SigningKey) -> Vec<(&'static str, [u8; 32], Vec<u8>)> {
    let good_pk = key.verifying_key().to_bytes();
    let v = Voucher {
        chain_id: 7,
        channel_id: [0xab; 32],
        amount: 1_000,
        nonce: 1,
    };
    let sv = v.sign(key);
    let sig = |r: [u8; 32], s: [u8; 32]| -> [u8; 64] {
        let mut o = [0u8; 64];
        o[..32].copy_from_slice(&r);
        o[32..].copy_from_slice(&s);
        o
    };
    let s_of = |sg: &[u8; 64]| -> [u8; 32] { sg[32..].try_into().unwrap() };
    let r_of = |sg: &[u8; 64]| -> [u8; 32] { sg[..32].try_into().unwrap() };
    let zero = [0u8; 32];
    let cases: Vec<(&'static str, [u8; 32], [u8; 64])> = vec![
        // S + L is the same scalar, so the equation still holds, but S >= L.
        (
            "malleated_s",
            good_pk,
            sig(r_of(&sv.signature), add_le(&s_of(&sv.signature), &L)),
        ),
        // A of small order: with R = identity and S = 0 the plain equation holds for any message.
        (
            "small_order_key_identity",
            point("identity"),
            sig(point("identity"), zero),
        ),
        (
            "small_order_key_order2",
            point("order2"),
            sig(point("identity"), zero),
        ),
        (
            "small_order_key_order4",
            point("order4"),
            sig(point("identity"), zero),
        ),
        (
            "noncanonical_small_order_key",
            point("identity_noncanonical"),
            sig(point("identity"), zero),
        ),
        (
            "negative_zero_key",
            point("identity_negative_zero"),
            sig(point("identity"), zero),
        ),
        // R of small order or not canonical, with the real key.
        ("small_order_r", good_pk, sig(point("identity"), zero)),
        (
            "small_order_r_order4",
            good_pk,
            sig(point("order4"), s_of(&sv.signature)),
        ),
        (
            "noncanonical_r",
            good_pk,
            sig(point("identity_noncanonical"), s_of(&sv.signature)),
        ),
        (
            "negative_zero_r",
            good_pk,
            sig(point("identity_negative_zero"), s_of(&sv.signature)),
        ),
    ];
    cases
        .into_iter()
        .map(|(name, pk, signature)| {
            let bad = SignedVoucher {
                voucher: sv.voucher,
                signature,
            };
            assert!(!bad.verify(&pk), "{name}: Rust must reject");
            (name, pk, bad.to_bytes().to_vec())
        })
        .collect()
}

fn fixture() -> String {
    let seed = [7u8; 32];
    let key = SigningKey::from_bytes(&seed);
    let cases = [
        ("basic", 7u64, [0xabu8; 32], 1_000u64, 1u64),
        ("zero_amount", 7, [0x01; 32], 0, 0),
        ("large", u64::MAX, [0xff; 32], u64::MAX, u64::MAX),
        ("other_chain", 8, [0xab; 32], 1_000, 1),
    ];
    let mut out = format!(
        "{{\n  \"seed\": \"{}\",\n  \"public_key\": \"{}\",\n  \"vouchers\": [\n",
        hex(&seed),
        hex(&key.verifying_key().to_bytes())
    );
    for (i, (name, chain_id, channel_id, amount, nonce)) in cases.iter().enumerate() {
        let v = Voucher {
            chain_id: *chain_id,
            channel_id: *channel_id,
            amount: *amount,
            nonce: *nonce,
        };
        let sv = v.sign(&key);
        assert!(sv.verify(&key.verifying_key().to_bytes()));
        let sep = if i + 1 == cases.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{\"name\": \"{name}\", \"chain_id\": \"{chain_id}\", \"channel_id\": \"{}\", \"amount\": \"{amount}\", \"nonce\": \"{nonce}\", \"digest\": \"{}\", \"signature\": \"{}\", \"bytes\": \"{}\"}}{sep}\n",
            hex(channel_id),
            hex(&v.digest()),
            hex(&sv.signature),
            hex(&sv.to_bytes()),
        ));
    }
    out.push_str("  ],\n  \"rejected\": [\n");
    let bad = rejected(&key);
    for (i, (name, pk, bytes)) in bad.iter().enumerate() {
        let sep = if i + 1 == bad.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{\"name\": \"{name}\", \"public_key\": \"{}\", \"bytes\": \"{}\"}}{sep}\n",
            hex(pk),
            hex(bytes)
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

#[test]
fn ts_voucher_fixture_is_current() {
    let json = fixture();
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk-ts/test/fixtures/voucher.json");
    if std::env::var_os("BTB_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, &json).unwrap();
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        on_disk, json,
        "sdk-ts/test/fixtures/voucher.json is stale; run with BTB_UPDATE_FIXTURES=1"
    );
}
