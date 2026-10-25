//! Security tests for the beacon cryptography: threshold signing, malicious partials, the DKG
//! with misbehaving dealers, resharing, and the stake-weighted median.

use btb_oracle::beacon::{beacon_message, seed_of, Beacon};
use btb_oracle::bls::{Fr, G1, G2};
use btb_oracle::dkg::{self, eval_commitments};
use btb_oracle::threshold::{self, interpolate_g2};
use btb_oracle::*;
use ed25519_dalek::SigningKey;

fn entropy(tag: u8, i: usize) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0] = tag;
    e[1..9].copy_from_slice(&(i as u64).to_le_bytes());
    e
}

fn partials(shares: &[KeyShare], msg: &[u8], who: &[usize]) -> Vec<(usize, G2)> {
    who.iter().map(|i| (*i, shares[*i].sign(msg))).collect()
}

#[test]
fn threshold_values() {
    assert_eq!(threshold_for(1), 1);
    assert_eq!(threshold_for(4), 3);
    assert_eq!(threshold_for(7), 5);
    assert_eq!(threshold_for(10), 7);
    assert_eq!(threshold_for(100), 67);
    for n in 1..=100usize {
        let (t, f) = (threshold_for(n), (n - 1) / 3);
        assert!(n - f >= t, "liveness with f faulty, n={n}");
        assert!(f < t, "f colluders are below the threshold, n={n}");
        // two thirds: any two quorums of size t intersect in more than f holders
        assert!(2 * t > n + f, "quorum intersection, n={n}");
    }
}

#[test]
fn any_threshold_subset_gives_the_same_unique_signature() {
    let (group, shares) = trusted_deal(Params::standard(7), &entropy(1, 0)).unwrap();
    let msg = beacon_message(1, 0, 42);
    let a = combine(&group, &msg, &partials(&shares, &msg, &[0, 1, 2, 3, 4])).unwrap();
    let b = combine(&group, &msg, &partials(&shares, &msg, &[2, 3, 4, 5, 6])).unwrap();
    let c = combine(
        &group,
        &msg,
        &partials(&shares, &msg, &[6, 0, 4, 2, 1, 3, 5]),
    )
    .unwrap();
    assert_eq!(a, b);
    assert_eq!(a, c);
    assert!(btb_oracle::bls::verify(&group.public_key, &msg, &a));
    // and it is the signature of the group secret, which no single share equals
    let beacon = Beacon {
        epoch: 0,
        height: 42,
        signature: a.to_bytes(),
    };
    assert!(beacon.verify(1, &group));
    assert!(!beacon.verify(2, &group), "bound to the chain id");
    let mut other_height = beacon.clone();
    other_height.height = 43;
    assert!(!other_height.verify(1, &group), "bound to the height");
    let mut other_epoch = beacon.clone();
    other_epoch.epoch = 1;
    assert!(!other_epoch.verify(1, &group), "bound to the epoch");
    assert_eq!(beacon.seed(), seed_of(&a.to_bytes()));
}

#[test]
fn fewer_than_threshold_partials_cannot_make_the_beacon() {
    let (group, shares) = trusted_deal(Params::standard(7), &entropy(2, 0)).unwrap();
    let msg = beacon_message(1, 0, 9);
    // t - 1 = 4 partials: refused outright
    assert_eq!(
        combine(&group, &msg, &partials(&shares, &msg, &[0, 1, 2, 3])),
        Err(CombineError::NotEnough)
    );
    // The best an adversary with 4 shares can do is interpolate them as if they were enough
    // (or pad with an invented fifth partial): the result is not the signature.
    let four = partials(&shares, &msg, &[0, 1, 2, 3]);
    let sel: Vec<&(usize, G2)> = four.iter().collect();
    let guess = interpolate_g2(&sel).unwrap();
    assert!(!btb_oracle::bls::verify(&group.public_key, &msg, &guess));
    // padding with a made-up partial (here: someone else's share applied to another message)
    let mut padded = four.clone();
    padded.push((4, shares[4].sign(b"another message")));
    assert_eq!(
        combine(&group, &msg, &padded),
        Err(CombineError::BadAggregate)
    );
    // padding with the identity or with a copy of a known partial
    let mut dup = four.clone();
    dup.push((3, four[3].1));
    assert_eq!(combine(&group, &msg, &dup), Err(CombineError::BadIndex));
    // The 5th real partial completes it.
    assert!(combine(&group, &msg, &partials(&shares, &msg, &[0, 1, 2, 3, 4])).is_ok());
    // Brute force sanity: guessing the 5th partial among many random group elements never works.
    for k in 1..20u64 {
        let fake = G2::hash_to_curve(b"fake").mul(&Fr::from_u64(k));
        let mut p = four.clone();
        p.push((4, fake));
        assert!(combine(&group, &msg, &p).is_err());
    }
}

#[test]
fn malicious_partials_are_rejected_and_do_not_block_liveness() {
    let n = 10;
    let (group, shares) = trusted_deal(Params::standard(n), &entropy(3, 0)).unwrap();
    let t = group.threshold;
    assert_eq!(t, 7);
    let msg = beacon_message(1, 0, 100);
    // f = 3 faulty holders send garbage of several kinds; 7 honest ones remain.
    let mut c = Collector::new(group.clone(), msg.clone());
    let wrong_msg = shares[0].sign(b"not the beacon message");
    let wrong_key = shares[9].sign(&msg); // honest partial of 9 submitted under index 0
    let rand_pt = G2::hash_to_curve(b"junk");
    assert!(c.add(0, wrong_key).is_none());
    assert!(c.add(1, wrong_msg).is_none());
    assert!(c.add(2, rand_pt).is_none());
    assert!(c.add(3, shares[3].sign(&msg)).is_none());
    assert!(c.add(4, shares[4].sign(&msg)).is_none());
    assert!(c.add(5, shares[5].sign(&msg)).is_none());
    assert!(c.add(6, shares[6].sign(&msg)).is_none());
    assert!(c.add(7, shares[7].sign(&msg)).is_none());
    assert!(c.add(8, shares[8].sign(&msg)).is_none());
    // 9 partials held, 3 invalid: the 7th valid one completes the beacon
    let sig = c
        .add(9, shares[9].sign(&msg))
        .expect("liveness with f faulty");
    assert!(btb_oracle::bls::verify(&group.public_key, &msg, &sig));
    assert_eq!(c.rejected(), vec![0, 1, 2]);
    // identical to the honest-only result
    let honest = combine(
        &group,
        &msg,
        &partials(&shares, &msg, &[3, 4, 5, 6, 7, 8, 9]),
    )
    .unwrap();
    assert_eq!(sig, honest);

    // individual partial verification
    assert!(verify_partial(&group, 3, &msg, &shares[3].sign(&msg)));
    assert!(!verify_partial(&group, 3, &msg, &shares[4].sign(&msg)));
    assert!(!verify_partial(&group, 3, b"x", &shares[3].sign(&msg)));
    assert!(!verify_partial(&group, 99, &msg, &shares[3].sign(&msg)));

    // a rejected holder is ignored afterwards; the slow path happens once per holder
    let mut c2 = Collector::new(group.clone(), msg.clone());
    for (i, share) in shares.iter().enumerate().take(7) {
        c2.add(i, if i == 2 { rand_pt } else { share.sign(&msg) });
    }
    assert_eq!(c2.rejected(), vec![2]);
    assert!(c2.signature().is_none());
    assert!(c2.add(2, shares[2].sign(&msg)).is_none(), "stays rejected");
    // six valid partials are held; the seventh (from anyone else) completes it
    assert!(c2.add(7, shares[7].sign(&msg)).is_some());
}

#[test]
fn liveness_with_exactly_f_offline() {
    for n in [4usize, 7, 10, 13] {
        let (group, shares) = trusted_deal(Params::standard(n), &entropy(4, n)).unwrap();
        let f = (n - 1) / 3;
        let msg = beacon_message(1, 0, n as u64);
        let alive: Vec<usize> = (f..n).collect();
        assert!(
            combine(&group, &msg, &partials(&shares, &msg, &alive)).is_ok(),
            "n={n}"
        );
        // one more offline and it stalls instead of producing something wrong
        let fewer: Vec<usize> = (f + 1..n).collect();
        assert_eq!(
            combine(&group, &msg, &partials(&shares, &msg, &fewer)),
            Err(CombineError::NotEnough)
        );
    }
}

#[test]
fn beacon_outputs_look_random() {
    // Statistical sanity (not a proof): over many heights the seeds never repeat, every bit
    // position is balanced, and consecutive seeds are unrelated.
    let (group, shares) = trusted_deal(Params::standard(4), &entropy(5, 0)).unwrap();
    let all: Vec<usize> = vec![0, 1, 2];
    let mut seeds = Vec::new();
    for h in 1..=300u64 {
        let msg = beacon_message(1, 0, h);
        let sig = combine(&group, &msg, &partials(&shares, &msg, &all)).unwrap();
        seeds.push(seed_of(&sig.to_bytes()));
    }
    let mut uniq = seeds.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), seeds.len());
    let mut ones = [0u32; 256];
    for s in &seeds {
        for (b, slot) in ones.iter_mut().enumerate() {
            *slot += u32::from((s[b / 8] >> (b % 8)) & 1);
        }
    }
    // 300 fair coins: mean 150, sd ~8.7; allow 5.5 sd
    for (b, c) in ones.iter().enumerate() {
        assert!((100..=200).contains(c), "bit {b} had {c} ones of 300");
    }
    let mut diff = 0u32;
    for w in seeds.windows(2) {
        diff += w[0]
            .iter()
            .zip(&w[1])
            .map(|(a, b)| (a ^ b).count_ones())
            .sum::<u32>();
    }
    let avg = f64::from(diff) / 299.0;
    assert!((118.0..138.0).contains(&avg), "avg hamming distance {avg}");
}

#[test]
fn group_key_validation_and_codec() {
    let (group, _) = trusted_deal(Params::standard(7), &entropy(6, 0)).unwrap();
    let bytes = group.encode();
    assert_eq!(GroupKey::decode(&bytes).unwrap(), group);
    assert_eq!(GroupKey::decode_unchecked(&bytes).unwrap(), group);
    // a swapped public share breaks consistency
    let mut shares = group.public_shares.clone();
    shares.swap(1, 2);
    assert_eq!(
        GroupKey::new(group.threshold, group.public_key, shares).unwrap_err(),
        KeyError::Inconsistent
    );
    // a group key unrelated to the shares
    let other = G1::generator().mul(&Fr::from_u64(77));
    assert_eq!(
        GroupKey::new(group.threshold, other, group.public_shares.clone()).unwrap_err(),
        KeyError::Inconsistent
    );
    // threshold 0 and above n, truncation, trailing bytes
    assert!(GroupKey::new(0, group.public_key, group.public_shares.clone()).is_err());
    assert!(GroupKey::new(8, group.public_key, group.public_shares.clone()).is_err());
    assert!(GroupKey::decode(&bytes[..bytes.len() - 1]).is_err());
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(GroupKey::decode(&longer).is_err());
    // identity share
    let mut with_id = group.public_shares.clone();
    with_id[0] = G1::identity();
    assert_eq!(
        GroupKey::new(group.threshold, group.public_key, with_id).unwrap_err(),
        KeyError::IdentityKey
    );
    // KeyShare codec
    let (_, sh) = trusted_deal(Params::standard(4), &entropy(6, 1)).unwrap();
    assert_eq!(KeyShare::decode(&sh[2].encode()).unwrap(), sh[2]);
    assert!(KeyShare::decode(&[0xff; 36]).is_none());
}

// ------------------------------------------------------------------------------------ DKG

/// Run a complete DKG among `n` participants. `behaviour(dealer, recipient)` can corrupt a
/// private share (returning a replacement) or drop it (`None`); `justifies(dealer)` says
/// whether a dealer answers complaints honestly.
struct Run {
    outcome: Outcome,
    shares: Vec<Option<KeyShare>>,
}

fn run_dkg(
    n: usize,
    silent_dealers: &[usize],
    mut tamper: impl FnMut(usize, usize, Fr) -> Option<Fr>,
    lying_accusers: &[(usize, usize)],
    justifies: impl Fn(usize) -> bool,
) -> Result<Run, dkg::DkgError> {
    let params = Params::standard(n);
    let mut t = Transcript::new(Mode::Fresh, params)?;
    let dealings: Vec<Dealing> = (0..n)
        .map(|i| Dealing::fresh(i, params, &entropy(10, i)).unwrap())
        .collect();
    for d in &dealings {
        if silent_dealers.contains(&d.dealer) {
            continue;
        }
        t.add_commitments(d.dealer, d.commitments.clone())?;
    }
    let mut parts: Vec<Participant> = (0..n).map(Participant::new).collect();
    for d in &dealings {
        if silent_dealers.contains(&d.dealer) {
            continue;
        }
        for p in parts.iter_mut() {
            if let Some(s) = tamper(d.dealer, p.index, d.share_for(p.index)) {
                p.receive(&t, d.dealer, s);
            }
        }
    }
    // complaints
    let mut complaints: Vec<(usize, usize)> = Vec::new();
    for p in &parts {
        for d in p.complaints(&t) {
            complaints.push((p.index, d));
        }
    }
    for (acc, dealer) in lying_accusers {
        if !complaints.contains(&(*acc, *dealer)) {
            complaints.push((*acc, *dealer));
        }
    }
    for (acc, dealer) in &complaints {
        t.add_complaint(*acc, *dealer)?;
    }
    for (acc, dealer) in &complaints {
        if justifies(*dealer) {
            t.add_justification(*dealer, *acc, dealings[*dealer].share_for(*acc));
        }
    }
    let outcome = t.finalize()?;
    let shares = parts
        .iter()
        .map(|p| p.key_share(&t, &outcome).ok())
        .collect();
    Ok(Run { outcome, shares })
}

fn check_run_signs(r: &Run, n: usize) {
    let all: Vec<KeyShare> = r.shares.iter().map(|s| s.clone().unwrap()).collect();
    assert_eq!(all.len(), n);
    let msg = beacon_message(1, 0, 5);
    let sig = combine(
        &r.outcome.group,
        &msg,
        &partials(
            &all,
            &msg,
            &(0..r.outcome.group.threshold).collect::<Vec<_>>(),
        ),
    )
    .unwrap();
    // a different subset gives the same signature
    let alt: Vec<usize> = (n - r.outcome.group.threshold..n).collect();
    assert_eq!(
        sig,
        combine(&r.outcome.group, &msg, &partials(&all, &msg, &alt)).unwrap()
    );
}

#[test]
fn honest_dkg_gives_consistent_keys() {
    for n in [1usize, 2, 4, 7] {
        let r = run_dkg(n, &[], |_, _, s| Some(s), &[], |_| true).unwrap();
        assert_eq!(r.outcome.qualified, (0..n).collect::<Vec<_>>());
        assert_eq!(r.outcome.group.threshold, threshold_for(n));
        r.outcome.group.check().unwrap();
        check_run_signs(&r, n);
        // every participant's share matches the public share everyone computes from the board
        for (j, s) in r.shares.iter().enumerate() {
            assert_eq!(
                s.as_ref().unwrap().public_share(),
                r.outcome.group.public_shares[j]
            );
        }
    }
}

#[test]
fn dkg_is_deterministic_for_observers() {
    let a = run_dkg(4, &[], |_, _, s| Some(s), &[], |_| true).unwrap();
    let b = run_dkg(4, &[], |_, _, s| Some(s), &[], |_| true).unwrap();
    assert_eq!(a.outcome.group, b.outcome.group);
    assert_eq!(a.outcome.group.encode(), b.outcome.group.encode());
}

#[test]
fn dealer_sending_a_bad_share_and_not_justifying_is_disqualified() {
    // dealer 1 sends participant 2 a wrong share, then stays silent about the complaint
    let r = run_dkg(
        7,
        &[],
        |d, j, s| {
            Some(if d == 1 && j == 2 {
                s.add(&Fr::one())
            } else {
                s
            })
        },
        &[],
        |d| d != 1,
    )
    .unwrap();
    assert_eq!(r.outcome.qualified, vec![0, 2, 3, 4, 5, 6]);
    assert!(!r.outcome.qualified.contains(&1));
    check_run_signs(&r, 7);
}

#[test]
fn dealer_sending_a_bad_share_but_justifying_correctly_stays() {
    // a glitch (or a lying accuser) is cleared by the public justification
    let r = run_dkg(
        4,
        &[],
        |d, j, s| {
            Some(if d == 0 && j == 3 {
                s.add(&Fr::one())
            } else {
                s
            })
        },
        &[],
        |_| true,
    )
    .unwrap();
    assert_eq!(r.outcome.qualified, vec![0, 1, 2, 3]);
    check_run_signs(&r, 4);
}

#[test]
fn missing_share_is_a_complaint_and_the_justification_repairs_it() {
    let r = run_dkg(
        4,
        &[],
        |d, j, s| if d == 2 && j == 1 { None } else { Some(s) },
        &[],
        |_| true,
    )
    .unwrap();
    assert_eq!(r.outcome.qualified.len(), 4);
    check_run_signs(&r, 4);
}

#[test]
fn false_complaints_do_not_disqualify_honest_dealers() {
    let r = run_dkg(
        7,
        &[],
        |_, _, s| Some(s),
        &[(3, 0), (3, 1), (4, 0)],
        |_| true,
    )
    .unwrap();
    assert_eq!(r.outcome.qualified.len(), 7);
    check_run_signs(&r, 7);
}

#[test]
fn silent_and_equivocating_dealers_are_excluded() {
    // dealers 0 and 5 post nothing at all (f = 2 of 7)
    let r = run_dkg(7, &[0, 5], |_, _, s| Some(s), &[], |_| true).unwrap();
    assert_eq!(r.outcome.qualified, vec![1, 2, 3, 4, 6]);
    check_run_signs(&r, 7);
    // too many silent dealers: fewer than t qualified -> the DKG fails instead of producing a
    // key controlled by a few
    let params = Params::standard(7);
    assert!(matches!(
        run_dkg(7, &[0, 1, 2], |_, _, s| Some(s), &[], |_| true),
        Err(dkg::DkgError::NotEnoughDealers { have: 4, need: 5 })
    ));
    let _ = params;
}

#[test]
fn dkg_input_validation() {
    let params = Params::standard(4);
    let mut t = Transcript::new(Mode::Fresh, params).unwrap();
    let d = Dealing::fresh(0, params, &entropy(11, 0)).unwrap();
    // wrong number of commitments disqualifies
    assert_eq!(
        t.add_commitments(0, d.commitments[..2].to_vec()),
        Err(dkg::DkgError::BadCommitments)
    );
    assert!(t.is_disqualified(0));
    assert_eq!(
        t.add_commitments(9, d.commitments.clone()),
        Err(dkg::DkgError::UnknownDealer)
    );
    // complaints need commitments and a valid accuser
    assert_eq!(t.add_complaint(1, 3), Err(dkg::DkgError::BadComplaint));
    let d1 = Dealing::fresh(1, params, &entropy(11, 1)).unwrap();
    t.add_commitments(1, d1.commitments.clone()).unwrap();
    assert_eq!(
        t.add_commitments(1, d1.commitments.clone()),
        Err(dkg::DkgError::DuplicateCommitments)
    );
    assert_eq!(t.add_complaint(77, 1), Err(dkg::DkgError::BadComplaint));
    // a justification without a complaint is ignored
    assert!(!t.add_justification(1, 2, d1.share_for(2)));
    // bad params
    assert!(Transcript::new(Mode::Fresh, Params { n: 3, t: 4 }).is_err());
    assert!(Dealing::fresh(0, Params { n: 3, t: 0 }, &entropy(0, 0)).is_err());
    // a share with a wrong value does not verify
    assert!(dkg::verify_share(&d1.commitments, 2, &d1.share_for(2)));
    assert!(!dkg::verify_share(&d1.commitments, 3, &d1.share_for(2)));
    assert_eq!(
        eval_commitments(&d1.commitments, 1),
        G1::generator().mul(&d1.share_for(0))
    );
}

#[test]
fn resharing_moves_the_key_to_a_new_committee() {
    // old committee: 4 holders (t = 3). New committee: 7 holders (t = 5), overlapping or not.
    let (old, old_shares) = trusted_deal(Params::standard(4), &entropy(20, 0)).unwrap();
    let new_params = Params::standard(7);
    let mut t = Transcript::new(Mode::Reshare { old: old.clone() }, new_params).unwrap();
    // old holder 3 is offline (f = 1); holders 0..3 deal
    let dealings: Vec<Dealing> = (0..3)
        .map(|i| Dealing::reshare(&old_shares[i], new_params, &entropy(21, i)).unwrap())
        .collect();
    for d in &dealings {
        t.add_commitments(d.dealer, d.commitments.clone()).unwrap();
    }
    // A dishonest old holder whose constant term is not its real share is rejected at once.
    let liar = Dealing::fresh(3, new_params, &entropy(22, 3)).unwrap();
    assert_eq!(
        t.add_commitments(3, liar.commitments.clone()),
        Err(dkg::DkgError::BadCommitments)
    );
    let mut parts: Vec<Participant> = (0..7).map(Participant::new).collect();
    for d in &dealings {
        for p in parts.iter_mut() {
            assert!(p.receive(&t, d.dealer, d.share_for(p.index)));
        }
    }
    let out = t.finalize().unwrap();
    assert_eq!(
        out.group.public_key, old.public_key,
        "same group key after resharing"
    );
    assert_eq!(out.group.threshold, 5);
    assert_eq!(out.group.n(), 7);
    let new_shares: Vec<KeyShare> = parts
        .iter()
        .map(|p| p.key_share(&t, &out).unwrap())
        .collect();
    let msg = beacon_message(1, 1, 601);
    let sig_new = combine(
        &out.group,
        &msg,
        &partials(&new_shares, &msg, &[2, 3, 4, 5, 6]),
    )
    .unwrap();
    let sig_old = combine(&old, &msg, &partials(&old_shares, &msg, &[0, 1, 2])).unwrap();
    assert_eq!(sig_new, sig_old, "old and new committees sign identically");
    // the old shares and the new shares are different values
    assert_ne!(old_shares[0].secret, new_shares[0].secret);
    // the old committee alone cannot be mixed with the new one: 2 old + 3 new partials fail
    // (they are shares of different polynomials)
    let mix = [
        (0usize, old_shares[0].sign(&msg)),
        (1, old_shares[1].sign(&msg)),
        (2, new_shares[2].sign(&msg)),
        (3, new_shares[3].sign(&msg)),
        (4, new_shares[4].sign(&msg)),
    ];
    assert!(combine(&out.group, &msg, &mix).is_err());
    // resharing with too few old dealers fails
    let mut t2 = Transcript::new(Mode::Reshare { old: old.clone() }, new_params).unwrap();
    for d in &dealings[..2] {
        t2.add_commitments(d.dealer, d.commitments.clone()).unwrap();
    }
    assert!(matches!(
        t2.finalize(),
        Err(dkg::DkgError::NotEnoughDealers { have: 2, need: 3 })
    ));
}

#[test]
fn threshold_module_helpers() {
    // lagrange coefficients sum to 1 at x = 0 for any set (interpolating the constant 1)
    let xs = [2u64, 5, 7, 11];
    let mut sum = Fr::zero();
    for k in 0..xs.len() {
        sum = sum.add(&threshold::lagrange(&xs, k, 0).unwrap());
    }
    assert_eq!(sum, Fr::one());
    // duplicate x-coordinates: no coefficient
    assert!(threshold::lagrange(&[3, 3], 0, 0).is_none());
}

// ---------------------------------------------------------------------------------- prices

#[test]
fn signed_observations() {
    let k = SigningKey::from_bytes(&[7; 32]);
    let o = Observation {
        pair: 3,
        price: 6_500_012_345_678,
        height: 99,
    };
    let s = SignedObservation::sign(1, &k, o);
    assert!(s.verify(1));
    assert!(!s.verify(2), "bound to the chain id");
    assert_eq!(SignedObservation::decode(&s.encode()), Some(s));
    for i in [0usize, 33, 40, 55, 70] {
        let mut b = s.encode();
        b[i] ^= 1;
        let t = SignedObservation::decode(&b).unwrap();
        assert!(!t.verify(1), "tamper at byte {i}");
    }
    assert!(SignedObservation::decode(&s.encode()[..100]).is_none());
}

#[test]
fn weighted_median_basics() {
    assert_eq!(weighted_median(&[]), None);
    assert_eq!(weighted_median(&[(5, 0)]), None);
    assert_eq!(weighted_median(&[(5, 10)]), Some(5));
    // equal weights: lower median
    assert_eq!(weighted_median(&[(1, 1), (2, 1), (3, 1)]), Some(2));
    assert_eq!(weighted_median(&[(1, 1), (2, 1), (3, 1), (4, 1)]), Some(2));
    // stake decides, not the count
    assert_eq!(weighted_median(&[(1, 1), (2, 1), (100, 10)]), Some(100));
    // order of input is irrelevant
    assert_eq!(weighted_median(&[(100, 10), (2, 1), (1, 1)]), Some(100));
}

#[test]
fn weighted_median_is_robust_to_less_than_a_third_of_the_stake() {
    // Exhaustive-ish: honest validators report values in [lo, hi]; byzantine validators with
    // strictly less than 1/3 of the total stake report anything. If the reporting weight is at
    // least 2/3 of the total, the median is always inside [lo, hi].
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..3000 {
        let n = 4 + (next() % 12) as usize;
        let weights: Vec<u64> = (0..n).map(|_| 1 + next() % 100).collect();
        let total: u64 = weights.iter().sum();
        // pick byzantine validators greedily while strictly below a third of the total
        let mut byz = vec![false; n];
        let mut bw = 0u64;
        for i in 0..n {
            if next() % 2 == 0 && (bw + weights[i]) * 3 < total {
                byz[i] = true;
                bw += weights[i];
            }
        }
        // some honest validators do not report, but at least 2/3 of the total stake does
        let mut reporting = vec![true; n];
        let mut missing = 0u64;
        for i in 0..n {
            if !byz[i] && next() % 4 == 0 && (missing + weights[i]) * 3 < total {
                reporting[i] = false;
                missing += weights[i];
            }
        }
        let (lo, hi) = (
            1_000u128 + u128::from(next() % 50),
            1_100 + u128::from(next() % 50),
        );
        let mut items = Vec::new();
        let mut honest_vals = Vec::new();
        for i in 0..n {
            if !reporting[i] {
                continue;
            }
            let v = if byz[i] {
                [0u128, 1, u128::MAX, hi + 1_000_000, lo - 900][(next() % 5) as usize]
            } else {
                let v = lo + u128::from(next()) % (hi - lo + 1);
                honest_vals.push(v);
                v
            };
            items.push((v, weights[i]));
        }
        let rep_w: u64 = items.iter().map(|(_, w)| w).sum();
        assert!(rep_w * 3 >= total * 2);
        let m = weighted_median(&items).unwrap();
        let (hmin, hmax) = (
            *honest_vals.iter().min().unwrap(),
            *honest_vals.iter().max().unwrap(),
        );
        assert!(
            (hmin..=hmax).contains(&m),
            "median {m} outside honest range {hmin}..={hmax} (byz {bw}/{total})"
        );
    }
    // And the bound is tight: with 1/2 of the reporting weight a minority CAN move it.
    assert_eq!(
        weighted_median(&[(10, 1), (1_000_000, 1), (11, 1), (1_000_001, 1)]),
        Some(11)
    );
}
