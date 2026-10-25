//! FROST signatures checked by independent verifiers: a BIP-340 Schnorr verifier (`k256`) under
//! the Taproot output key, and `ed25519-dalek` (what Solana runs) under the group key.

use btb_oracle::dkg::Params;
use btb_oracle::frost::{
    self, addr, aggregate, generate_nonces, sign_share, Dealing, GroupKey, KeyShare, Mode, Scheme,
    SigningPackage, Transcript,
};
use std::collections::BTreeMap;

fn keygen(scheme: Scheme, n: usize) -> Vec<KeyShare> {
    let params = Params::standard(n);
    let mut t = Transcript::new(scheme, Mode::Fresh, params).unwrap();
    let deals: Vec<Dealing> = (0..n)
        .map(|d| Dealing::fresh(scheme, d, params, &[d as u8 + 1; 32]).unwrap())
        .collect();
    for d in &deals {
        t.add_commitments(d.dealer, d.commitments.clone()).unwrap();
    }
    let out = t.finalize().unwrap();
    (0..n)
        .map(|j| {
            let got = (0..n).map(|d| (d, deals[d].share_for(j))).collect();
            out.derive_share(j, &got).unwrap()
        })
        .collect()
}

fn reshare(old: &[KeyShare], n: usize, salt: u8) -> Vec<KeyShare> {
    let scheme = old[0].scheme();
    let params = Params::standard(n);
    let mut t = Transcript::new(
        scheme,
        Mode::Reshare {
            old: old[0].group.clone(),
        },
        params,
    )
    .unwrap();
    let deals: Vec<Dealing> = old
        .iter()
        .map(|k| Dealing::reshare(k, params, &[k.index as u8 + salt; 32]).unwrap())
        .collect();
    for d in &deals {
        t.add_commitments(d.dealer, d.commitments.clone()).unwrap();
    }
    let out = t.finalize().unwrap();
    (0..n)
        .map(|j| {
            let got = out
                .needed()
                .into_iter()
                .map(|d| (d, deals[d].share_for(j)))
                .collect();
            out.derive_share(j, &got).unwrap()
        })
        .collect()
}

fn sign(keys: &[KeyShare], signers: &[usize], msg: &[u8]) -> Vec<u8> {
    let group = &keys[0].group;
    let mut nonces = BTreeMap::new();
    let mut commitments = BTreeMap::new();
    for &i in signers {
        let (n, c) =
            generate_nonces(&keys[i], &[(i * 7 + msg[0] as usize) as u8 ^ 0x5a; 32]).unwrap();
        nonces.insert(i, n);
        commitments.insert(i, c);
    }
    let pkg = SigningPackage {
        message: msg.to_vec(),
        commitments,
    };
    let shares: BTreeMap<usize, Vec<u8>> = signers
        .iter()
        .map(|&i| (i, sign_share(&keys[i], &pkg, &nonces[&i]).unwrap()))
        .collect();
    aggregate(group, &pkg, &shares).unwrap()
}

fn bip340_verify(output_key: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(vk) = k256::schnorr::VerifyingKey::from_bytes(output_key) else {
        return false;
    };
    let Ok(sig) = k256::schnorr::Signature::try_from(sig) else {
        return false;
    };
    vk.verify_raw(msg, &sig).is_ok()
}

fn ed25519_verify(key: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) else {
        return false;
    };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(sig) else {
        return false;
    };
    vk.verify_strict(msg, &sig).is_ok()
}

#[test]
fn taproot_signatures_verify_with_a_bip340_verifier_under_the_output_key() {
    let keys = keygen(Scheme::Taproot, 4);
    let group = &keys[0].group;
    let q = group.output_key().unwrap();
    // the output key is the tweaked key, not the internal key
    assert_ne!(&q[..], &group.public_key[1..]);
    for (k, signers) in [[0usize, 1, 2], [1, 2, 3], [0, 2, 3], [0, 1, 3]]
        .iter()
        .enumerate()
    {
        let msg = [k as u8 + 1; 32];
        let sig = sign(&keys, signers, &msg);
        assert!(bip340_verify(&q, &msg, &sig), "signers {signers:?}");
        assert!(!bip340_verify(&q, &[0xEE; 32], &sig));
        // the internal key is not what the signature verifies under
        let internal: [u8; 32] = group.public_key[1..33].try_into().unwrap();
        assert!(!bip340_verify(&internal, &msg, &sig));
        let mut bad = sig.clone();
        bad[40] ^= 1;
        assert!(!bip340_verify(&q, &msg, &bad));
    }
    // and the address commits to the same key
    let a = addr::taproot_address("bc", &q);
    assert!(a.starts_with("bc1p"));
    assert_eq!(addr::parse_taproot_address("bc", &a), Some(q));
}

#[test]
fn ed25519_signatures_verify_with_dalek_under_the_group_key() {
    let keys = keygen(Scheme::Ed25519, 7);
    let group = &keys[0].group;
    assert_eq!(group.threshold, 5);
    let pk = group.output_key().unwrap();
    assert_eq!(&pk[..], &group.public_key[..]);
    for signers in [[0usize, 1, 2, 3, 4], [2, 3, 4, 5, 6], [0, 2, 4, 5, 6]] {
        let msg = b"a solana transaction message".to_vec();
        let sig = sign(&keys, &signers, &msg);
        assert!(ed25519_verify(&pk, &msg, &sig), "{signers:?}");
        assert!(!ed25519_verify(&pk, b"another message", &sig));
    }
    assert_eq!(
        addr::parse_solana_address(&addr::solana_address(&pk)),
        Some(pk)
    );
}

#[test]
fn resharing_keeps_the_group_key_and_the_external_address() {
    for scheme in Scheme::ALL {
        let keys = keygen(scheme, 4);
        let before = keys[0].group.output_key().unwrap();
        let keys2 = reshare(&keys, 7, 90); // epoch change: bigger committee, higher threshold
        assert_eq!(keys2[0].group.threshold, 5);
        assert_eq!(keys2[0].group.public_key, keys[0].group.public_key);
        assert_eq!(keys2[0].group.output_key().unwrap(), before);
        let keys3 = reshare(&keys2, 5, 120);
        assert_eq!(keys3[0].group.output_key().unwrap(), before);
        // the new committee signs for the old address, with either verifier
        let msg = [0x33u8; 32];
        let sig = sign(&keys3, &[0, 1, 2, 4], &msg);
        match scheme {
            Scheme::Taproot => assert!(bip340_verify(&before, &msg, &sig)),
            Scheme::Ed25519 => assert!(ed25519_verify(&before, &msg, &sig)),
        }
        // old shares and new shares combined (a "mixed epoch" coalition) are not a quorum of
        // either key: the old committee's threshold of old shares still signs under the same key,
        // which is why honest holders must erase old shares (docs/CROSS_CHAIN.md)
        let sig_old = sign(&keys, &[0, 1, 2], &msg);
        match scheme {
            Scheme::Taproot => assert!(bip340_verify(&before, &msg, &sig_old)),
            Scheme::Ed25519 => assert!(ed25519_verify(&before, &msg, &sig_old)),
        }
    }
}

#[test]
fn a_reshare_dealer_that_lies_about_its_share_breaks_the_key_check() {
    let keys = keygen(Scheme::Ed25519, 4);
    let params = Params::standard(4);
    let t = Transcript::new(
        Scheme::Ed25519,
        Mode::Reshare {
            old: keys[0].group.clone(),
        },
        params,
    )
    .unwrap();
    // dealer 2 deals a random constant instead of its share: refused by the board
    let liar = Dealing::fresh(Scheme::Ed25519, 2, params, &[77; 32]).unwrap();
    assert!(t.check_commitments(2, &liar.commitments).is_err());
}

#[test]
fn bip86_output_key_vector() {
    // BIP-86 test vector: internal key -> output key (key-path-only taproot).
    let internal = unhex("cc8a4bc64d897bddc5fbc2f670f7a8ba0b386779106cf1223c6fc5d7cd6fc115");
    let want = unhex("a60869f0dbcf1dc659c9cecbaf8050135ea9e8cdc487053f1dc6880949dc684c");
    let mut compressed = vec![0x02];
    compressed.extend_from_slice(&internal);
    // a one-holder "group": threshold 1, the key and its single public share are the same point
    let g = GroupKey::new(Scheme::Taproot, 1, compressed.clone(), vec![compressed]).unwrap();
    assert_eq!(g.output_key().unwrap().to_vec(), want);
    assert_eq!(
        addr::taproot_address("bc", &g.output_key().unwrap()),
        "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr"
    );
    let _ = frost::MAX_PARTIES;
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}
