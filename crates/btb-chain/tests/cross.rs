//! Cross-chain threshold keys and the native vault (`docs/CROSS_CHAIN.md`). Security-sensitive:
//! every refusal and exclusion path is tested.
//!
//! The harness plays every validator with the real agents (`Agent` for the beacon DKG that
//! the chain requires, `XAgent` for the cross-chain keys), signs their transactions with the
//! consensus keys and runs the executor. FROST signing rounds are run in process with the shares
//! the agents derived from the chain.

mod support;
use btb_chain::cross::CrossError;
use btb_chain::dkg::{self, DkgOp};
use btb_chain::dkg_agent::Agent;
use btb_chain::vault::{self, DepositClaim, MockObserver, Observer, VaultError};
use btb_chain::xdkg::{self, XdkgError};
use btb_chain::xdkg_agent::XAgent;
use btb_chain::xsign::{self, EvidenceOp, Proof, RequestTag, XsignError};
use btb_chain::*;
use btb_oracle::beacon::{beacon_message, Beacon};
use btb_oracle::dkg_crypto;
use btb_oracle::frost::{self, Dealing, Scheme, SigningPackage};
use btb_oracle::{combine, Params};
use btb_runtime::MemKv;
use ed25519_dalek::Signer;
use std::collections::{BTreeMap, BTreeSet};
use support::*;

const E: u64 = 40;
const W: u64 = 12;
const EACH: u64 = 1_000_000_000_000;
const STAKE: u64 = 100_000;

fn ckey(i: usize) -> SigningKey {
    SigningKey::from_bytes(&[100 + i as u8; 32])
}

fn ckey_bytes(i: usize) -> [u8; 32] {
    ckey(i).verifying_key().to_bytes()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Behave {
    Honest,
    /// Posts nothing (neither beacon DKG nor cross-chain DKG messages).
    Silent,
    /// Deals the cross-chain keys with a wrong share for this validator.
    BadShareTo(usize),
}

/// A signing package, the signature shares and the nonces they were made with.
type Rounds = (
    SigningPackage,
    BTreeMap<usize, Vec<u8>>,
    BTreeMap<usize, Vec<u8>>,
);

struct Sim {
    net: Net<MemKv>,
    ops: Vec<Wallet>,
    cw: Vec<Wallet>,
    agents: Vec<Agent>,
    xagents: Vec<XAgent>,
    behave: Vec<Behave>,
    user: Wallet,
    pending: Vec<Transaction>,
    receipts: Vec<(Hash, Receipt)>,
    nonce_seed: u8,
}

fn xerr(r: &Receipt) -> CrossError {
    match &r.status {
        Status::Reverted(Revert::Cross(e)) => e.clone(),
        other => panic!("expected a cross-chain refusal, got {other:?}"),
    }
}

impl Sim {
    fn new(n: usize, wallets: usize) -> Sim {
        let ops: Vec<Wallet> = (0..wallets).map(|i| Wallet::new(10 + i as u8)).collect();
        let cw: Vec<Wallet> = (0..wallets)
            .map(|i| Wallet {
                key: ckey(i),
                nonce: 0,
                chain_id: CHAIN_ID,
            })
            .collect();
        let user = Wallet::new(50);
        let mut all: Vec<&Wallet> = ops.iter().chain(cw.iter()).collect();
        all.push(&user);
        let mut cfg = ChainConfig::new(CHAIN_ID);
        cfg.staking = StakingConfig {
            epoch_len: E,
            max_validators: 8,
            min_stake: 1_000,
            unbonding_blocks: E,
            slash_bps: 2_000,
            reporter_reward_bps: 1_000,
        };
        let g = genesis_with(&all, EACH, cfg);
        let vals: Vec<GenesisValidator> = (0..n)
            .map(|i| GenesisValidator {
                operator: ops[i].addr(),
                consensus_key: ckey_bytes(i),
                network_addr: Vec::new(),
                stake: STAKE,
            })
            .collect();
        let og = OracleGenesis {
            group_key: Vec::new(),
            holders: Vec::new(),
            pairs: vec![],
            max_age_blocks: 20,
            dkg_window: W,
            rules: Default::default(),
        };
        let state = ChainState::from_genesis_full(MemKv::new(), &g, &vals, Some(&og)).unwrap();
        Sim {
            net: Net {
                state,
                ex: Executor::sequential(),
            },
            ops,
            cw,
            agents: (0..wallets)
                .map(|i| Agent::new(CHAIN_ID, ckey(i)))
                .collect(),
            xagents: (0..wallets)
                .map(|i| XAgent::new(CHAIN_ID, ckey(i)))
                .collect(),
            behave: vec![Behave::Honest; wallets],
            user,
            pending: Vec::new(),
            receipts: Vec::new(),
            nonce_seed: 0,
        }
    }

    fn height(&self) -> u64 {
        self.net.state.head().unwrap().height
    }

    fn kv(&self) -> &MemKv {
        self.net.state.store()
    }

    fn epoch(&self) -> u64 {
        self.height() / E // epoch of the next block
    }

    fn xkey(&self, scheme: Scheme, e: u64) -> Option<xdkg::XKey> {
        xdkg::xkey(self.kv(), scheme, e).unwrap()
    }

    fn session(&self, scheme: Scheme, target: u64) -> xdkg::XSession {
        xdkg::session(self.kv(), scheme, target)
            .unwrap()
            .expect("session")
    }

    fn beacon_for_next(&self) -> Option<Beacon> {
        let h = self.height() + 1;
        let e = (h - 1) / E;
        let key = dkg::epoch_key(self.kv(), e).unwrap()?;
        let msg = beacon_message(CHAIN_ID, e, h);
        let partials: Vec<_> = self
            .agents
            .iter()
            .filter_map(|a| a.share_for_epoch(self.kv(), e).unwrap())
            .map(|s| (s.index, s.sign(&msg)))
            .collect();
        let sig = combine(&key.group, &msg, &partials).expect("enough key holders for the beacon");
        Some(Beacon {
            epoch: e,
            height: h,
            signature: sig.to_bytes(),
        })
    }

    /// Run one block with `extra` transactions after the pending ones; returns the receipts of
    /// all of them (pending first).
    fn block(&mut self, extra: Vec<Transaction>) -> Vec<Receipt> {
        let mut txs = std::mem::take(&mut self.pending);
        txs.extend(extra);
        let beacon = self.beacon_for_next();
        let b = self.net.block_for(txs.clone()).with_oracle(OracleInput {
            beacon,
            ..Default::default()
        });
        let (receipts, root) = self
            .net
            .ex
            .execute_block(&mut self.net.state, &b)
            .expect("block executes");
        assert_eq!(root, self.net.state.root().unwrap());
        // BTB is untouched by anything the cross-chain code does
        assert_eq!(
            self.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY)
        );
        for (tx, r) in txs.iter().zip(&receipts) {
            self.receipts.push((tx.id(), r.clone()));
        }
        let h = self.height();
        for i in 0..self.agents.len() {
            let step = self.agents[i].step(self.net.state.store(), h + 1).unwrap();
            let xstep = self.xagents[i].step(self.net.state.store(), h + 1).unwrap();
            if self.behave[i] == Behave::Silent {
                continue;
            }
            for op in step.ops {
                let tx = self.cw[i].sign(TxKind::Dkg(op));
                self.pending.push(tx);
            }
            for op in xstep.ops {
                let op = match self.behave[i] {
                    Behave::BadShareTo(j) => self.tamper(i, op, j),
                    _ => op,
                };
                let tx = self.cw[i].sign(TxKind::Cross(op));
                self.pending.push(tx);
            }
        }
        receipts
    }

    fn run_to(&mut self, height: u64) {
        while self.height() < height {
            self.block(vec![]);
        }
    }

    /// Replace a cross-chain deal by one that encrypts a wrong share for validator `j`.
    fn tamper(&self, who: usize, op: CrossOp, j: usize) -> CrossOp {
        let CrossOp::Deal { scheme, target, .. } = &op else {
            return op;
        };
        let s = self.session(*scheme, *target);
        let j = s
            .committee()
            .iter()
            .position(|k| *k == ckey_bytes(j))
            .unwrap();
        let me = ckey_bytes(who);
        let i = s.dealers.iter().position(|k| *k == me).unwrap();
        let params = s.params();
        let ent = [0xAA ^ who as u8; 32];
        let dealing = match s.from {
            None => Dealing::fresh(*scheme, i, params, &ent).unwrap(),
            Some(from) => {
                let old = self.xkey(*scheme, from).unwrap();
                let share = self.xagents[who]
                    .share_of_origin(*scheme, old.origin)
                    .unwrap();
                Dealing::reshare(share, params, &ent).unwrap()
            }
        };
        let mut shares = dealing.shares(params.n);
        shares[j][0] ^= 1; // a different (maybe non-canonical) scalar
        let d = dkg_crypto::deliver_raw(
            &xdkg::domain(*scheme),
            CHAIN_ID,
            s.target,
            i as u32,
            &[7; 32],
            &s.committee(),
            &shares,
        );
        CrossOp::Deal {
            scheme: *scheme,
            target: s.target,
            eph: d.eph,
            commitments: dealing.commitments.clone(),
            ciphertexts: d.ciphertexts,
        }
    }

    /// Run the FROST rounds for `signers` (holder indices of `key`) in process.
    fn sign_with(&mut self, scheme: Scheme, message: &[u8], signers: &[usize]) -> Rounds {
        let epoch = self.epoch();
        let (key, _) = {
            let (_, k) = xdkg::key_at(self.kv(), scheme, epoch).unwrap().unwrap();
            (k.clone(), ())
        };
        let mut nonces = BTreeMap::new();
        let mut commitments = BTreeMap::new();
        for &h in signers {
            let v = (0..self.xagents.len())
                .find(|v| ckey_bytes(*v) == key.holders[h])
                .unwrap();
            let share = self.xagents[v]
                .share_for_epoch(self.kv(), scheme, epoch)
                .unwrap()
                .expect("share")
                .1;
            self.nonce_seed = self.nonce_seed.wrapping_add(1);
            let (n, c) =
                frost::generate_nonces(&share, &[self.nonce_seed ^ (h as u8 * 17); 32]).unwrap();
            nonces.insert(h, (v, n));
            commitments.insert(h, c);
        }
        let pkg = SigningPackage {
            message: message.to_vec(),
            commitments,
        };
        let mut shares = BTreeMap::new();
        for (h, (v, n)) in &nonces {
            let share = self.xagents[*v]
                .share_for_epoch(self.kv(), scheme, epoch)
                .unwrap()
                .unwrap()
                .1;
            shares.insert(*h, frost::sign_share(&share, &pkg, n).unwrap());
        }
        let nonce_bytes = nonces.into_iter().map(|(h, (_, n))| (h, n)).collect();
        (pkg, shares, nonce_bytes)
    }

    /// A finished signature for request `id` by the attempt-0 signer set.
    fn finish_request(&mut self, id: u64) -> Vec<u8> {
        let r = xsign::get(self.kv(), id).unwrap().unwrap();
        let epoch = self.epoch();
        let key = xdkg::key_at(self.kv(), r.scheme, epoch).unwrap().unwrap().1;
        let excluded = xsign::excluded(self.kv(), r.scheme, epoch).unwrap();
        let signers = xsign::select_signers(&key, r.scheme, &excluded, id, 0);
        let (pkg, shares, _) = self.sign_with(r.scheme, &r.message, &signers);
        frost::aggregate(&key.group, &pkg, &shares).unwrap()
    }

    fn cross(&mut self, v: usize, op: CrossOp) -> Transaction {
        self.cw[v].sign(TxKind::Cross(op))
    }

    /// One block with just `tx`; returns its receipt.
    fn one(&mut self, tx: Transaction) -> Receipt {
        let n = self.pending.len();
        let mut rs = self.block(vec![tx]);
        rs.remove(n)
    }
}

fn claim(asset: Scheme, n: u8, amount: u64, to: Address) -> DepositClaim {
    DepositClaim {
        asset,
        txid: [n; 32],
        index: 0,
        amount,
        recipient: to,
        height: 100,
        block_hash: [n; 32],
    }
}

// ------------------------------------------------------------------------------ keys

#[test]
fn genesis_generates_one_key_per_scheme_that_every_validator_holds_a_share_of() {
    let mut s = Sim::new(4, 4);
    s.run_to(W + 1);
    let rec0 = s.net.state.epoch_record(0).unwrap().unwrap();
    let committee: Vec<[u8; 32]> = rec0.validators.iter().map(|v| v.consensus_key).collect();
    for scheme in Scheme::ALL {
        let sess = s.session(scheme, 0);
        assert_eq!((sess.window_start, sess.window_end), (1, W));
        assert_eq!(sess.status, xdkg::STATUS_DONE, "{scheme:?}");
        let key = s.xkey(scheme, 0).expect("a key");
        assert_eq!(key.group.threshold, 3);
        assert_eq!(key.holders, committee);
        key.group.check().unwrap();
        for v in 0..4 {
            let (k, sh) = s.xagents[v]
                .share_for_epoch(s.kv(), scheme, 0)
                .unwrap()
                .expect("share");
            assert_eq!(k.group, key.group);
            assert_eq!(
                sh.public_share(),
                key.group.public_shares[sh.index].as_slice()
            );
        }
        // the external key a vault address commits to
        let ok = vault::output_key(s.kv(), scheme, 0).unwrap().unwrap();
        assert_eq!(ok, key.group.output_key().unwrap());
    }
    // different schemes are different keys
    assert_ne!(
        s.xkey(Scheme::Taproot, 0).unwrap().group.public_key[1..],
        s.xkey(Scheme::Ed25519, 0).unwrap().group.public_key[..]
    );
    // every validator dealt once per scheme
    let deals = s
        .receipts
        .iter()
        .filter(|(_, r)| r.status == Status::Success)
        .count();
    assert!(deals >= 8 + 4, "{deals}");
    // any threshold subset signs a verifying signature under each key
    for scheme in Scheme::ALL {
        let msg = [0x5Au8; 32];
        let key = s.xkey(scheme, 0).unwrap();
        let (pkg, shares, _) = s.sign_with(scheme, &msg, &[0, 1, 2]);
        let sig = frost::aggregate(&key.group, &pkg, &shares).unwrap();
        assert!(frost::verify(&key.group, &msg, &sig));
        let (pkg, shares, _) = s.sign_with(scheme, &msg, &[1, 2, 3]);
        let sig = frost::aggregate(&key.group, &pkg, &shares).unwrap();
        assert!(frost::verify(&key.group, &msg, &sig));
    }
}

#[test]
fn resharing_keeps_each_group_key_and_external_address_through_epochs_with_a_validator_change() {
    // Five operators; validators 0..4 at genesis, then 3 leaves and 4 joins before the tail.
    let mut s = Sim::new(4, 5);
    s.run_to(W + 1);
    let before: Vec<(Scheme, Vec<u8>, [u8; 32])> = Scheme::ALL
        .iter()
        .map(|sc| {
            let k = s.xkey(*sc, 0).unwrap();
            (
                *sc,
                k.group.public_key.clone(),
                k.group.output_key().unwrap(),
            )
        })
        .collect();
    let unstake = s.ops[3].sign(TxKind::Unstake { amount: STAKE });
    let reg = s.ops[4].sign(TxKind::RegisterValidator {
        consensus_key: ckey_bytes(4),
        network_addr: Vec::new(),
    });
    let stake_key = s.ops[4].key.verifying_key().to_bytes();
    let stake = s.ops[4].sign(TxKind::Stake {
        validator: stake_key,
        amount: STAKE,
    });
    for r in s.block(vec![unstake, reg, stake]) {
        ok(&r);
    }
    s.run_to(E + 2);
    let rec1 = s.net.state.epoch_record(1).unwrap().unwrap();
    let keys1: Vec<[u8; 32]> = rec1.validators.iter().map(|v| v.consensus_key).collect();
    assert!(keys1.contains(&ckey_bytes(4)) && !keys1.contains(&ckey_bytes(3)));
    for (scheme, pk, out) in &before {
        let sess = s.session(*scheme, 1);
        assert_eq!(sess.from, Some(0));
        assert_eq!(sess.status, xdkg::STATUS_DONE);
        let k1 = s.xkey(*scheme, 1).unwrap();
        assert_eq!(&k1.group.public_key, pk, "resharing keeps the group key");
        assert_eq!(
            &k1.group.output_key().unwrap(),
            out,
            "and the external address"
        );
        assert_eq!(k1.holders, keys1);
        assert_eq!(k1.origin, 1);
        assert_ne!(
            k1.group.public_shares,
            s.xkey(*scheme, 0).unwrap().group.public_shares
        );
        // the new committee signs for the old address; the removed validator has no new share
        let msg = [0x77u8; 32];
        let (pkg, shares, _) = s.sign_with(*scheme, &msg, &[0, 1, 3]);
        let sig = frost::aggregate(&k1.group, &pkg, &shares).unwrap();
        assert!(frost::verify(&k1.group, &msg, &sig));
        assert!(s.xagents[3]
            .share_for_epoch(s.kv(), *scheme, 1)
            .unwrap()
            .is_none());
        assert!(s.xagents[4]
            .share_for_epoch(s.kv(), *scheme, 0)
            .unwrap()
            .is_none());
    }
    // two more epochs of resharing
    s.run_to(3 * E + 3);
    for (scheme, pk, out) in &before {
        let k3 = s.xkey(*scheme, 3).unwrap();
        assert_eq!(k3.origin, 3);
        assert_eq!(&k3.group.public_key, pk);
        assert_eq!(&k3.group.output_key().unwrap(), out);
        assert_eq!(s.session(*scheme, 3).from, Some(2));
    }
}

#[test]
fn a_dealer_with_a_bad_share_is_complained_against_and_excluded_but_the_key_forms() {
    let mut s = Sim::new(4, 4);
    s.behave[1] = Behave::BadShareTo(2);
    s.run_to(W + 2);
    for scheme in Scheme::ALL {
        let sess = s.session(scheme, 0);
        assert_eq!(sess.status, xdkg::STATUS_DONE);
        let deal = xdkg::deal(s.kv(), scheme, 0, 1).unwrap().unwrap();
        assert!(deal.disqualified, "{scheme:?}");
        let (_, out) = xdkg::outcome(s.kv(), &sess).unwrap().unwrap();
        assert_eq!(out.qualified, vec![0, 2, 3]);
        // validator 2 still got a usable share (the qualified dealers' shares were fine)
        assert!(s.xagents[2]
            .share_for_epoch(s.kv(), scheme, 0)
            .unwrap()
            .is_some());
    }
    // the upheld complaints succeeded
    let ok_complaints = s
        .receipts
        .iter()
        .filter(|(_, r)| r.output == vec![1])
        .count();
    assert!(ok_complaints >= 2, "{ok_complaints}");
}

#[test]
fn a_silent_minority_still_forms_the_key_but_too_many_silent_dealers_fail_it() {
    let mut s = Sim::new(4, 4);
    s.behave[3] = Behave::Silent;
    s.run_to(W + 2);
    for scheme in Scheme::ALL {
        assert_eq!(s.session(scheme, 0).status, xdkg::STATUS_DONE);
        let (_, out) = xdkg::outcome(s.kv(), &s.session(scheme, 0))
            .unwrap()
            .unwrap();
        assert_eq!(out.qualified, vec![0, 1, 2]);
    }
    let mut s = Sim::new(4, 4);
    s.behave[2] = Behave::Silent;
    s.behave[3] = Behave::Silent;
    s.run_to(W + 2);
    for scheme in Scheme::ALL {
        assert_eq!(s.session(scheme, 0).status, xdkg::STATUS_FAILED);
        assert!(s.xkey(scheme, 0).is_none());
    }
    // recovery: the next tail deals a fresh key
    s.behave[2] = Behave::Honest;
    s.behave[3] = Behave::Honest;
    s.run_to(E + 3);
    for scheme in Scheme::ALL {
        assert_eq!(s.session(scheme, 1).from, None);
        assert!(s.xkey(scheme, 1).is_some());
    }
}

#[test]
fn a_failed_resharing_keeps_the_previous_key_and_signing_continues() {
    let mut s = Sim::new(4, 4);
    s.run_to(W + 1);
    let k0 = s.xkey(Scheme::Ed25519, 0).unwrap();
    s.behave[0] = Behave::Silent;
    s.behave[1] = Behave::Silent;
    s.run_to(E + 4);
    for scheme in Scheme::ALL {
        let sess = s.session(scheme, 1);
        assert_eq!(sess.from, Some(0));
        assert_eq!(sess.status, xdkg::STATUS_FAILED);
    }
    let k1 = s.xkey(Scheme::Ed25519, 1).expect("carried over");
    assert_eq!(k1, k0);
    // the old holders still sign
    s.behave[0] = Behave::Honest;
    s.behave[1] = Behave::Honest;
    let msg = [3u8; 32];
    let (pkg, shares, _) = s.sign_with(Scheme::Ed25519, &msg, &[0, 1, 2]);
    let sig = frost::aggregate(&k1.group, &pkg, &shares).unwrap();
    assert!(frost::verify(&k1.group, &msg, &sig));
    // with everybody back, the next epoch's reshare works from the carried key
    s.run_to(2 * E + 4);
    let sess = s.session(Scheme::Ed25519, 2);
    assert_eq!(sess.from, Some(1));
    assert_eq!(sess.status, xdkg::STATUS_DONE);
    assert_eq!(
        s.xkey(Scheme::Ed25519, 2).unwrap().group.public_key,
        k0.group.public_key
    );
}

#[test]
fn key_generation_transactions_are_refused_outside_their_rules() {
    let mut s = Sim::new(4, 5);
    // nobody deals for us: the checks below need an open window with empty boards
    s.behave = vec![Behave::Silent; 5];
    s.run_to(2);
    let sess = s.session(Scheme::Ed25519, 0);
    let params = sess.params();
    let dealing = Dealing::fresh(Scheme::Ed25519, 0, params, &[9; 32]).unwrap();
    let delivery = dkg_crypto::deliver_raw(
        &xdkg::domain(Scheme::Ed25519),
        CHAIN_ID,
        0,
        0,
        &[1; 32],
        &sess.committee(),
        &dealing.shares(params.n),
    );
    let deal = |scheme, target, commitments: Vec<Vec<u8>>| CrossOp::Deal {
        scheme,
        target,
        eph: delivery.eph,
        commitments,
        ciphertexts: delivery.ciphertexts.clone(),
    };
    // no session for a future epoch
    let tx = s.cross(1, deal(Scheme::Ed25519, 9, dealing.commitments.clone()));
    assert_eq!(xerr(&s.one(tx)), CrossError::Dkg(XdkgError::NoSession));
    // a non-validator (operator account key 4 is not a dealer) is refused
    let mut outsider = Wallet {
        key: ckey(4),
        nonce: 0,
        chain_id: CHAIN_ID,
    };
    let tx = outsider.sign(TxKind::Cross(deal(
        Scheme::Ed25519,
        0,
        dealing.commitments.clone(),
    )));
    assert_eq!(xerr(&s.one(tx)), CrossError::Dkg(XdkgError::NotDealer));
    // wrong number of commitments
    let mut short = dealing.commitments.clone();
    short.pop();
    let tx = s.cross(1, deal(Scheme::Ed25519, 0, short));
    assert_eq!(xerr(&s.one(tx)), CrossError::Dkg(XdkgError::BadShape));
    // an invalid point
    let mut junk = dealing.commitments.clone();
    junk[1] = vec![0xFF; 32];
    let tx = s.cross(1, deal(Scheme::Ed25519, 0, junk));
    assert_eq!(xerr(&s.one(tx)), CrossError::Dkg(XdkgError::BadCommitments));
    // a complaint without a deal, and by a non-member
    let reveal = dkg_crypto::reveal(&ckey(0), &delivery.eph).unwrap();
    let complaint = |dealer| CrossOp::Complain {
        scheme: Scheme::Ed25519,
        target: 0,
        dealer,
        shared: reveal.shared,
        proof: reveal.proof,
    };
    let tx = s.cross(0, complaint(0));
    let r = s.one(tx);
    // validators dealt in the same blocks, so dealer 0's deal may exist: then the proof (made for
    // another ephemeral key) fails; otherwise there is no deal. Either way it is refused.
    assert!(matches!(
        xerr(&r),
        CrossError::Dkg(XdkgError::NoSuchDeal) | CrossError::Dkg(XdkgError::BadProof)
    ));
    let tx = outsider.sign(TxKind::Cross(complaint(0)));
    assert_eq!(xerr(&s.one(tx)), CrossError::Dkg(XdkgError::NotMember));
    // after the window closes nothing is accepted and a double deal is refused
    s.run_to(W + 1);
    let tx = s.cross(1, deal(Scheme::Ed25519, 0, dealing.commitments.clone()));
    let e = xerr(&s.one(tx));
    assert!(
        matches!(
            e,
            CrossError::Dkg(XdkgError::NoSession) | CrossError::Dkg(XdkgError::WrongPhase)
        ),
        "{e:?}"
    );
}

// ------------------------------------------------------------------------------ vault

fn vault_sim() -> Sim {
    let mut s = Sim::new(4, 4);
    s.run_to(W + 1);
    s
}

#[test]
fn a_deposit_is_credited_only_when_two_thirds_of_the_stake_voted_for_the_same_claim() {
    let mut s = vault_sim();
    let me = s.user.addr();
    let c = claim(Scheme::Taproot, 1, 50_000, me);
    let id = c.id();
    let attest = |s: &mut Sim, v: usize, c: &DepositClaim| {
        let tx = s.cross(
            v,
            CrossOp::Attest {
                claims: vec![c.clone()],
            },
        );
        s.one(tx)
    };
    let r = attest(&mut s, 0, &c);
    ok(&r);
    assert_eq!(r.output, vec![0]);
    assert_eq!(vault::balance_of(s.kv(), Scheme::Taproot, &me).unwrap(), 0);
    // voting twice does not count twice
    let r = attest(&mut s, 0, &c);
    ok(&r);
    assert_eq!(vault::claim_votes(s.kv(), &id).unwrap().len(), 1);
    // a validator that saw a different amount votes for a different claim: no quorum for either
    let other = claim(Scheme::Taproot, 1, 60_000, me);
    ok(&attest(&mut s, 1, &other));
    assert_eq!(vault::balance_of(s.kv(), Scheme::Taproot, &me).unwrap(), 0);
    // the second vote for the real claim: two of four is not two thirds
    ok(&attest(&mut s, 2, &c));
    assert_eq!(vault::balance_of(s.kv(), Scheme::Taproot, &me).unwrap(), 0);
    assert!(!vault::is_credited(s.kv(), &id).unwrap());
    // the third is
    let r = attest(&mut s, 3, &c);
    ok(&r);
    assert_eq!(r.output, vec![1]);
    assert_eq!(
        vault::balance_of(s.kv(), Scheme::Taproot, &me).unwrap(),
        50_000
    );
    assert!(vault::is_credited(s.kv(), &id).unwrap());
    // replaying the deposit (even by everybody) credits nothing more
    for v in 0..4 {
        ok(&attest(&mut s, v, &c));
    }
    assert_eq!(
        vault::balance_of(s.kv(), Scheme::Taproot, &me).unwrap(),
        50_000
    );
    // the other chain's token is a different asset
    assert_eq!(vault::balance_of(s.kv(), Scheme::Ed25519, &me).unwrap(), 0);
    let (credited, burned, supply) = vault::accounting(s.kv(), Scheme::Taproot).unwrap();
    assert_eq!((credited, burned, supply), (50_000, 0, 50_000));
    // the wrapped token is a user token with the vault as sole authority
    let info = s
        .net
        .state
        .token_info(&vault::token_id(Scheme::Taproot))
        .unwrap()
        .unwrap();
    assert_eq!(info.symbol, "wBTC");
    assert_eq!(info.mint_authority, Some(vault::vault_address()));
    assert_eq!(info.supply, 50_000);
}

#[test]
fn the_same_external_output_in_another_block_is_not_credited_twice() {
    let mut s = vault_sim();
    let me = s.user.addr();
    let a = claim(Scheme::Taproot, 1, 50_000, me);
    // the same output after a reorg: another block, another claim id
    let mut b = a.clone();
    b.height += 1;
    b.block_hash = [0x77; 32];
    assert_ne!(a.id(), b.id());
    assert_eq!(a.outpoint_id(), b.outpoint_id());
    for v in 0..3 {
        let tx = s.cross(
            v,
            CrossOp::Attest {
                claims: vec![a.clone()],
            },
        );
        ok(&s.one(tx));
    }
    assert_eq!(
        vault::balance_of(s.kv(), Scheme::Taproot, &me).unwrap(),
        50_000
    );
    for v in 0..4 {
        let tx = s.cross(
            v,
            CrossOp::Attest {
                claims: vec![b.clone()],
            },
        );
        ok(&s.one(tx));
    }
    assert_eq!(
        vault::balance_of(s.kv(), Scheme::Taproot, &me).unwrap(),
        50_000
    );
    // the credited output is a spendable vault UTXO, once
    assert_eq!(vault::utxo(s.kv(), &a.txid, 0).unwrap(), Some(50_000));
    assert_eq!(vault::utxos(s.kv(), 10).unwrap().len(), 1);
}

#[test]
fn a_bitcoin_withdrawal_plan_is_checked_by_the_chain_and_spends_each_utxo_once() {
    use btb_chain::vault_plan::BtcTx;
    use btb_oracle::frost::addr;
    let mut s = vault_sim();
    let me = s.user.addr();
    let dep = claim(Scheme::Taproot, 1, 50_000, me);
    for v in 0..3 {
        let tx = s.cross(
            v,
            CrossOp::Attest {
                claims: vec![dep.clone()],
            },
        );
        ok(&s.one(tx));
    }
    let vkey = vault::output_key(s.kv(), Scheme::Taproot, 0)
        .unwrap()
        .unwrap();
    let vscript = addr::taproot_script_pubkey(&vkey);
    let mut dest = vec![0x51, 0x20];
    dest.extend_from_slice(&[0xAB; 32]);
    let tx = s.user.sign(TxKind::Cross(CrossOp::Withdraw {
        asset: Scheme::Taproot,
        amount: 20_000,
        destination: dest.clone(),
    }));
    let r = s.one(tx);
    ok(&r);
    let wid = u64::from_le_bytes(r.output.clone().try_into().unwrap());
    let mk = |txid: [u8; 32], pay: u64, to: &[u8], change: u64| BtcTx {
        version: 2,
        inputs: vec![(txid, 0, 0xFFFF_FFFD)],
        outputs: vec![(pay, to.to_vec()), (change, vscript.clone())],
        lock_time: 0,
    };
    let propose = |s: &mut Sim, v: usize, t: &BtcTx, sh: [u8; 32]| {
        let tx = s.cross(
            v,
            CrossOp::Prepare {
                withdrawal: wid,
                digest: t.txid(),
                sighashes: vec![sh],
                plan: t.serialize(),
            },
        );
        s.one(tx)
    };
    let sighash =
        |t: &BtcTx, amount: u64| t.taproot_sighash(0, &[(amount, vscript.clone())]).unwrap();
    // wrong destination, over-payment, wrong sighash: BadPlan
    let bad_dest = mk(dep.txid, 19_000, &[0x51, 0x20, 1], 31_000);
    assert_eq!(
        xerr(&propose(&mut s, 0, &bad_dest, sighash(&bad_dest, 50_000))),
        CrossError::Vault(VaultError::BadPlan)
    );
    let overpay = mk(dep.txid, 21_000, &dest, 29_000);
    assert_eq!(
        xerr(&propose(&mut s, 0, &overpay, sighash(&overpay, 50_000))),
        CrossError::Vault(VaultError::BadPlan)
    );
    let good = mk(dep.txid, 19_000, &dest, 30_000);
    assert_eq!(
        xerr(&propose(&mut s, 0, &good, [9; 32])),
        CrossError::Vault(VaultError::BadPlan)
    );
    // an input the vault does not hold
    let ghost = mk([0xEE; 32], 19_000, &dest, 30_000);
    assert_eq!(
        xerr(&propose(&mut s, 0, &ghost, sighash(&ghost, 50_000))),
        CrossError::Vault(VaultError::UtxoUnavailable)
    );
    // the good plan: three votes make it final, the input is spent and the change appears
    let sh = sighash(&good, 50_000);
    for v in 0..3 {
        ok(&propose(&mut s, v, &good, sh));
    }
    let w = vault::withdrawal(s.kv(), wid).unwrap().unwrap();
    assert_eq!(w.status, vault::STATUS_PREPARED);
    assert_eq!(w.plan, good.serialize());
    assert_eq!(vault::utxo(s.kv(), &dep.txid, 0).unwrap(), None);
    assert_eq!(vault::utxo(s.kv(), &good.txid(), 1).unwrap(), Some(30_000));
    let req = xsign::get(s.kv(), w.requests[0]).unwrap().unwrap();
    assert_eq!(req.message, sh.to_vec());
    // a second withdrawal cannot spend the consumed output
    let tx = s.user.sign(TxKind::Cross(CrossOp::Withdraw {
        asset: Scheme::Taproot,
        amount: 10_000,
        destination: dest.clone(),
    }));
    let r = s.one(tx);
    ok(&r);
    let wid2 = u64::from_le_bytes(r.output.try_into().unwrap());
    let again = mk(dep.txid, 9_000, &dest, 40_000);
    let tx = s.cross(
        0,
        CrossOp::Prepare {
            withdrawal: wid2,
            digest: again.txid(),
            sighashes: vec![sighash(&again, 50_000)],
            plan: again.serialize(),
        },
    );
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Vault(VaultError::UtxoUnavailable)
    );
}

#[test]
fn only_current_validators_can_attest_and_claims_are_validated() {
    let mut s = vault_sim();
    let me = s.user.addr();
    let c = claim(Scheme::Ed25519, 4, 1_000, me);
    // an ordinary account is not a validator
    let tx = s.user.sign(TxKind::Cross(CrossOp::Attest {
        claims: vec![c.clone()],
    }));
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Vault(VaultError::NotValidator)
    );
    // zero amounts and empty batches are refused
    let tx = s.cross(
        0,
        CrossOp::Attest {
            claims: vec![claim(Scheme::Ed25519, 5, 0, me)],
        },
    );
    assert_eq!(xerr(&s.one(tx)), CrossError::Vault(VaultError::BadShape));
    let tx = s.cross(0, CrossOp::Attest { claims: vec![] });
    let r = s.one(tx);
    assert_eq!(r.status, Status::Invalid(Invalid::TooLarge));
    s.cw[0].nonce -= 1; // an invalid transaction does not use its nonce
                        // a batch with one bad claim changes nothing for its good claims
    let tx = s.cross(
        0,
        CrossOp::Attest {
            claims: vec![c.clone(), claim(Scheme::Ed25519, 6, 0, me)],
        },
    );
    assert_eq!(xerr(&s.one(tx)), CrossError::Vault(VaultError::BadShape));
    assert!(vault::claim_votes(s.kv(), &c.id()).unwrap().is_empty());
}

fn fund(s: &mut Sim, scheme: Scheme, amount: u64) -> Address {
    let me = s.user.addr();
    let c = claim(scheme, 9, amount, me);
    for v in 0..3 {
        let tx = s.cross(
            v,
            CrossOp::Attest {
                claims: vec![c.clone()],
            },
        );
        ok(&s.one(tx));
    }
    assert_eq!(
        vault::balance_of(s.kv(), scheme, &me).unwrap(),
        u128::from(amount)
    );
    me
}

#[test]
fn a_withdrawal_burns_first_is_prepared_once_and_signed_for_exactly_one_transaction() {
    let mut s = vault_sim();
    let me = fund(&mut s, Scheme::Ed25519, 5_000);
    let dest = vec![0xD5u8; 32];
    // too much, wrong destination, zero
    let tx = s.user.sign(TxKind::Cross(CrossOp::Withdraw {
        asset: Scheme::Ed25519,
        amount: 5_001,
        destination: dest.clone(),
    }));
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Vault(VaultError::InsufficientBalance)
    );
    let tx = s.user.sign(TxKind::Cross(CrossOp::Withdraw {
        asset: Scheme::Ed25519,
        amount: 10,
        destination: vec![1; 31],
    }));
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Vault(VaultError::BadDestination)
    );
    let tx = s.user.sign(TxKind::Cross(CrossOp::Withdraw {
        asset: Scheme::Ed25519,
        amount: 0,
        destination: dest.clone(),
    }));
    assert_eq!(xerr(&s.one(tx)), CrossError::Vault(VaultError::BadShape));
    assert_eq!(
        vault::balance_of(s.kv(), Scheme::Ed25519, &me).unwrap(),
        5_000
    );

    // the burn happens in the withdrawal transaction, before any signing
    let tx = s.user.sign(TxKind::Cross(CrossOp::Withdraw {
        asset: Scheme::Ed25519,
        amount: 3_000,
        destination: dest.clone(),
    }));
    let r = s.one(tx);
    ok(&r);
    let wid = u64::from_le_bytes(r.output.clone().try_into().unwrap());
    assert_eq!(
        vault::balance_of(s.kv(), Scheme::Ed25519, &me).unwrap(),
        2_000
    );
    assert_eq!(
        vault::accounting(s.kv(), Scheme::Ed25519).unwrap(),
        (5_000, 3_000, 2_000)
    );
    let w = vault::withdrawal(s.kv(), wid).unwrap().unwrap();
    assert_eq!(w.status, vault::STATUS_BURNED);
    assert!(w.requests.is_empty());
    assert!(
        xsign::pending_from(s.kv(), 0, 10).unwrap().is_empty(),
        "nothing is signed before the burn is prepared"
    );

    // validators agree on the unsigned transaction: 2 of 4 is not enough
    let obs = MockObserver::new();
    let vkey = vault::output_key(s.kv(), Scheme::Ed25519, 0)
        .unwrap()
        .unwrap();
    let prepared = obs.prepare(&w, &vkey).unwrap();
    let prep = |s: &mut Sim, v: usize, digest: [u8; 32], sighashes: Vec<[u8; 32]>| {
        let tx = s.cross(
            v,
            CrossOp::Prepare {
                withdrawal: wid,
                digest,
                sighashes,
                plan: vec![],
            },
        );
        s.one(tx)
    };
    ok(&prep(
        &mut s,
        0,
        prepared.digest,
        prepared.sighashes.clone(),
    ));
    // a different proposal does not add to it
    let evil = [0xEEu8; 32];
    ok(&prep(&mut s, 1, [1; 32], vec![evil]));
    ok(&prep(
        &mut s,
        2,
        prepared.digest,
        prepared.sighashes.clone(),
    ));
    assert_eq!(
        vault::withdrawal(s.kv(), wid).unwrap().unwrap().status,
        vault::STATUS_BURNED
    );
    assert!(xsign::pending_from(s.kv(), 0, 10).unwrap().is_empty());
    // the third vote for the same proposal makes it final: one signing request per sighash
    let r = prep(&mut s, 3, prepared.digest, prepared.sighashes.clone());
    ok(&r);
    assert_eq!(r.output, vec![1]);
    let w = vault::withdrawal(s.kv(), wid).unwrap().unwrap();
    assert_eq!(w.status, vault::STATUS_PREPARED);
    assert_eq!(w.requests.len(), 1);
    let req = xsign::get(s.kv(), w.requests[0]).unwrap().unwrap();
    assert_eq!(req.message, prepared.sighashes[0]);
    assert_eq!(req.scheme, Scheme::Ed25519);
    assert_eq!(req.tag, RequestTag::Withdrawal { id: wid, index: 0 });
    // a later, different proposal (even a quorum of it) cannot create a second signing
    let tx = s.cross(
        0,
        CrossOp::Prepare {
            withdrawal: wid,
            digest: [1; 32],
            sighashes: vec![evil],
            plan: vec![],
        },
    );
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Vault(VaultError::AlreadyPrepared)
    );
    assert_eq!(
        xsign::next_id(s.kv()).unwrap(),
        1,
        "exactly one signing request exists"
    );

    // signing: a bad signature is refused, the real one is stored once
    let rid = req.id;
    let bad = s.user.sign(TxKind::Cross(CrossOp::Signature {
        request: rid,
        signature: vec![7; 64],
    }));
    assert_eq!(
        xerr(&s.one(bad)),
        CrossError::Sign(XsignError::BadSignature)
    );
    let sig = s.finish_request(rid);
    let tx = s.user.sign(TxKind::Cross(CrossOp::Signature {
        request: rid,
        signature: sig.clone(),
    }));
    ok(&s.one(tx));
    let done = xsign::get(s.kv(), rid).unwrap().unwrap();
    assert_eq!(done.signature.as_deref(), Some(sig.as_slice()));
    assert_eq!(
        vault::withdrawal(s.kv(), wid).unwrap().unwrap().status,
        vault::STATUS_SIGNED
    );
    // the signature is a standard Ed25519 signature of the sighash under the vault key
    use ed25519_dalek::{Signature, VerifyingKey};
    VerifyingKey::from_bytes(&vkey)
        .unwrap()
        .verify_strict(
            &prepared.sighashes[0],
            &Signature::from_slice(&sig).unwrap(),
        )
        .unwrap();
    // no second signature for the same request, and a second one for a fresh nonce neither
    let again = s.user.sign(TxKind::Cross(CrossOp::Signature {
        request: rid,
        signature: sig,
    }));
    assert_eq!(
        xerr(&s.one(again)),
        CrossError::Sign(XsignError::AlreadySigned)
    );
    // supply of the wrapped token == observed deposits - withdrawals
    assert_eq!(
        vault::accounting(s.kv(), Scheme::Ed25519).unwrap(),
        (5_000, 3_000, 2_000)
    );
}

#[test]
fn wrapped_supply_always_equals_deposits_minus_withdrawals_and_matches_the_balances() {
    let mut s = vault_sim();
    let alice = s.user.addr();
    // per asset: (deposits that reached the quorum, burned by withdrawals)
    let mut totals: BTreeMap<Scheme, (u128, u128)> = BTreeMap::new();
    // a deterministic pseudo-random walk of deposits and withdrawals on both assets
    let mut x = 0x9E37_79B9u32;
    let mut next = |m: u32| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x % m
    };
    for step in 0..30u32 {
        let scheme = if next(2) == 0 {
            Scheme::Taproot
        } else {
            Scheme::Ed25519
        };
        if next(3) != 0 {
            let amount = u64::from(next(10_000) + 1);
            let c = claim(scheme, step as u8 + 100, amount, alice);
            // a fresh random subset of validators votes; the claim is credited iff >= 3 voted
            let voters: Vec<usize> = (0..4).filter(|_| next(4) != 0).collect();
            for v in &voters {
                let tx = s.cross(
                    *v,
                    CrossOp::Attest {
                        claims: vec![c.clone()],
                    },
                );
                ok(&s.one(tx));
            }
            if voters.len() >= 3 {
                totals.entry(scheme).or_default().0 += u128::from(amount);
            }
        } else {
            let have = vault::balance_of(s.kv(), scheme, &alice).unwrap();
            let amount = (have as u64).min(u64::from(next(5_000) + 1));
            if amount > 0 {
                let dest = if scheme == Scheme::Taproot {
                    vec![
                        0x51, 0x20, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
                        19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
                    ]
                } else {
                    vec![5; 32]
                };
                let tx = s.user.sign(TxKind::Cross(CrossOp::Withdraw {
                    asset: scheme,
                    amount,
                    destination: dest,
                }));
                ok(&s.one(tx));
                totals.entry(scheme).or_default().1 += u128::from(amount);
            }
        }
        for sc in Scheme::ALL {
            let (credited, burned, supply) = vault::accounting(s.kv(), sc).unwrap();
            assert_eq!(supply, credited - burned, "{sc:?} step {step}");
            // the only holder is alice, so the balance is the whole supply
            assert_eq!(vault::balance_of(s.kv(), sc, &alice).unwrap(), supply);
        }
    }
    for sc in Scheme::ALL {
        let (deposited, withdrawn) = totals.get(&sc).copied().unwrap_or_default();
        let (credited, burned, supply) = vault::accounting(s.kv(), sc).unwrap();
        assert_eq!((credited, burned), (deposited, withdrawn), "{sc:?}");
        assert_eq!(supply, deposited - withdrawn);
        assert!(deposited > 0, "{sc:?} saw deposits");
    }
}

// ------------------------------------------------------------------------------ signing evidence

fn prepared_request(s: &mut Sim, scheme: Scheme) -> u64 {
    let id = {
        // a native module asks for a signature; for tests the state is written through a
        // withdrawal so the real path (quorum, request) is used
        let me = fund(s, scheme, 1_000);
        let dest = if scheme == Scheme::Taproot {
            vec![0x51u8; 34]
        } else {
            vec![5u8; 32]
        };
        let tx = s.user.sign(TxKind::Cross(CrossOp::Withdraw {
            asset: scheme,
            amount: 10,
            destination: dest,
        }));
        let r = s.one(tx);
        ok(&r);
        let wid = u64::from_le_bytes(r.output.try_into().unwrap());
        let w = vault::withdrawal(s.kv(), wid).unwrap().unwrap();
        assert_eq!(w.owner, me);
        let sighash = MockObserver::sighash_of(&w);
        for v in 0..3 {
            let tx = s.cross(
                v,
                CrossOp::Prepare {
                    withdrawal: wid,
                    digest: [wid as u8; 32],
                    sighashes: vec![sighash],
                    plan: vec![],
                },
            );
            ok(&s.one(tx));
        }
        vault::withdrawal(s.kv(), wid).unwrap().unwrap().requests[0]
    };
    id
}

fn bad_share_evidence(s: &mut Sim, id: u64, culprit: usize, flip: bool) -> (EvidenceOp, Vec<u8>) {
    let r = xsign::get(s.kv(), id).unwrap().unwrap();
    let epoch = s.epoch();
    let key = xdkg::key_at(s.kv(), r.scheme, epoch).unwrap().unwrap().1;
    let signers = vec![0usize, 1, 2];
    let (pkg, mut shares, _) = s.sign_with(r.scheme, &r.message, &signers);
    if flip {
        let sh = shares.get_mut(&culprit).unwrap();
        let last = sh.len() - 1;
        sh[last] ^= 1;
    }
    let attempt = 0u32;
    let pkg_bytes = pkg.encode();
    let digest = xsign::share_digest(
        CHAIN_ID,
        r.scheme,
        key.origin,
        id,
        attempt,
        &xsign::package_hash(&pkg_bytes),
        &shares[&culprit],
    );
    let holder_key = key.holders[culprit];
    let v = (0..4).find(|v| ckey_bytes(*v) == holder_key).unwrap();
    let sig = ckey(v).sign(&digest).to_bytes();
    // the aggregate names the culprit
    let res = frost::aggregate(&key.group, &pkg, &shares);
    if flip {
        assert_eq!(res, Err(frost::AggregateError::Culprits(vec![culprit])));
    }
    (
        EvidenceOp {
            request: id,
            attempt,
            origin: key.origin,
            holder: culprit as u32,
            proof: Proof::BadShare {
                package: pkg_bytes,
                share: shares[&culprit].clone(),
                sig,
            },
        },
        digest.to_vec(),
    )
}

#[test]
fn a_provably_bad_signature_share_is_recorded_and_the_signer_leaves_the_signer_sets() {
    let mut s = vault_sim();
    let id = prepared_request(&mut s, Scheme::Taproot);
    let key = s.xkey(Scheme::Taproot, 0).unwrap();
    // before the evidence the culprit is in some signer sets
    let culprit = 1usize;
    let in_sets = |attempts: std::ops::Range<u32>, ex: &BTreeSet<[u8; 32]>| {
        attempts
            .filter(|a| xsign::select_signers(&key, Scheme::Taproot, ex, id, *a).contains(&culprit))
            .count()
    };
    assert!(in_sets(0..40, &BTreeSet::new()) > 10);
    // a *valid* share is no offence
    let (good, _) = bad_share_evidence(&mut s, id, culprit, false);
    let tx = s.user.sign(TxKind::Cross(CrossOp::Evidence(good)));
    assert_eq!(xerr(&s.one(tx)), CrossError::Sign(XsignError::NoOffence));
    // an unsigned (forged) accusation is refused
    let (mut forged, _) = bad_share_evidence(&mut s, id, culprit, true);
    if let Proof::BadShare { sig, .. } = &mut forged.proof {
        sig[5] ^= 1;
    }
    let tx = s.user.sign(TxKind::Cross(CrossOp::Evidence(forged)));
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Sign(XsignError::NotSignedByAccused)
    );
    // the real evidence is recorded
    let (ev, _) = bad_share_evidence(&mut s, id, culprit, true);
    let tx = s.user.sign(TxKind::Cross(CrossOp::Evidence(ev.clone())));
    ok(&s.one(tx));
    let off = xsign::offender(s.kv(), Scheme::Taproot, &key.holders[culprit])
        .unwrap()
        .unwrap();
    assert_eq!(off.count, 1);
    assert_eq!(off.last_request, id);
    // recording it twice is refused
    let tx = s.user.sign(TxKind::Cross(CrossOp::Evidence(ev)));
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Sign(XsignError::AlreadyRecorded)
    );
    // and the offender is excluded from every signer set from now on
    let ex = xsign::excluded(s.kv(), Scheme::Taproot, s.epoch()).unwrap();
    assert!(ex.contains(&key.holders[culprit]));
    assert_eq!(in_sets(0..40, &ex), 0);
    // other schemes are not affected
    assert!(xsign::excluded(s.kv(), Scheme::Ed25519, s.epoch())
        .unwrap()
        .is_empty());
    // retrying without them: the new set signs and the chain accepts the signature
    let attempt1 = xsign::select_signers(&key, Scheme::Taproot, &ex, id, 1);
    assert!(!attempt1.contains(&culprit));
    let r = xsign::get(s.kv(), id).unwrap().unwrap();
    let (pkg, shares, _) = s.sign_with(Scheme::Taproot, &r.message, &attempt1);
    let sig = frost::aggregate(&key.group, &pkg, &shares).unwrap();
    let tx = s.user.sign(TxKind::Cross(CrossOp::Signature {
        request: id,
        signature: sig,
    }));
    ok(&s.one(tx));
}

#[test]
fn equivocating_about_the_nonce_is_provable_and_a_malformed_accusation_is_not() {
    let mut s = vault_sim();
    let id = prepared_request(&mut s, Scheme::Ed25519);
    let key = s.xkey(Scheme::Ed25519, 0).unwrap();
    let culprit = 2usize;
    // holders are in committee order; find the validator that owns holder `culprit`
    let owner = (0..4)
        .find(|v| ckey_bytes(*v) == key.holders[culprit])
        .unwrap();
    let other = (culprit + 1) % 4;
    let sign_use = |counter: u64| {
        let d = xsign::use_digest(CHAIN_ID, Scheme::Ed25519, key.origin, id, 3, counter);
        ckey(owner).sign(&d).to_bytes()
    };
    let ev = |a: u64, b: u64, holder: u32| EvidenceOp {
        request: id,
        attempt: 3,
        origin: key.origin,
        holder,
        proof: Proof::Equivocation {
            counter_a: a,
            sig_a: sign_use(a),
            counter_b: b,
            sig_b: sign_use(b),
        },
    };
    // the same counter twice is not equivocation
    let tx = s
        .user
        .sign(TxKind::Cross(CrossOp::Evidence(ev(5, 5, culprit as u32))));
    assert_eq!(xerr(&s.one(tx)), CrossError::Sign(XsignError::NoOffence));
    // two announcements signed by someone else are not evidence against this holder
    let tx = s
        .user
        .sign(TxKind::Cross(CrossOp::Evidence(ev(5, 6, other as u32))));
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Sign(XsignError::NotSignedByAccused)
    );
    // a holder index outside the key
    let tx = s.user.sign(TxKind::Cross(CrossOp::Evidence(ev(5, 6, 99))));
    assert_eq!(xerr(&s.one(tx)), CrossError::Sign(XsignError::BadEvidence));
    // an unknown request
    let mut bad = ev(5, 6, culprit as u32);
    bad.request = 77;
    let tx = s.user.sign(TxKind::Cross(CrossOp::Evidence(bad)));
    assert_eq!(
        xerr(&s.one(tx)),
        CrossError::Sign(XsignError::NoSuchRequest)
    );
    // the real thing
    let tx = s
        .user
        .sign(TxKind::Cross(CrossOp::Evidence(ev(5, 6, culprit as u32))));
    ok(&s.one(tx));
    assert!(xsign::excluded(s.kv(), Scheme::Ed25519, s.epoch())
        .unwrap()
        .contains(&key.holders[culprit]));
}

#[test]
fn signer_sets_are_deterministic_threshold_sized_and_rotate_with_the_attempt() {
    let mut s = vault_sim();
    s.run_to(W + 2);
    let key = s.xkey(Scheme::Taproot, 0).unwrap();
    let none = BTreeSet::new();
    let a0 = xsign::select_signers(&key, Scheme::Taproot, &none, 0, 0);
    assert_eq!(a0.len(), key.group.threshold);
    assert_eq!(
        a0,
        xsign::select_signers(&key, Scheme::Taproot, &none, 0, 0)
    );
    let mut seen = BTreeSet::new();
    for attempt in 0..64 {
        let set = xsign::select_signers(&key, Scheme::Taproot, &none, 0, attempt);
        assert_eq!(set.len(), 3);
        assert!(set.windows(2).all(|w| w[0] < w[1]));
        seen.insert(set);
    }
    assert_eq!(seen.len(), 4, "all four 3-of-4 sets come up");
    // exclusions that would make signing impossible are ignored (liveness first)
    let two: BTreeSet<[u8; 32]> = key.holders[..2].iter().copied().collect();
    assert_eq!(
        xsign::select_signers(&key, Scheme::Taproot, &two, 0, 0).len(),
        3
    );
    // different requests get different sets
    let sets: BTreeSet<_> = (0..32)
        .map(|r| xsign::select_signers(&key, Scheme::Taproot, &none, r, 0))
        .collect();
    assert!(sets.len() > 1);
}

#[test]
fn cross_transactions_round_trip_through_the_wire_format() {
    let mut w = Wallet::new(77);
    let ops = vec![
        CrossOp::Signature {
            request: 3,
            signature: vec![1; 64],
        },
        CrossOp::Attest {
            claims: vec![
                claim(Scheme::Taproot, 1, 5, [3; 32]),
                claim(Scheme::Ed25519, 2, 6, [4; 32]),
            ],
        },
        CrossOp::Prepare {
            withdrawal: 9,
            digest: [8; 32],
            sighashes: vec![[1; 32], [2; 32]],
            plan: vec![],
        },
        CrossOp::Withdraw {
            asset: Scheme::Ed25519,
            amount: 5,
            destination: vec![7; 32],
        },
        CrossOp::Evidence(EvidenceOp {
            request: 1,
            attempt: 2,
            origin: 3,
            holder: 4,
            proof: Proof::Equivocation {
                counter_a: 1,
                sig_a: [5; 64],
                counter_b: 2,
                sig_b: [6; 64],
            },
        }),
        CrossOp::Evidence(EvidenceOp {
            request: 1,
            attempt: 2,
            origin: 3,
            holder: 4,
            proof: Proof::BadShare {
                package: vec![1, 2, 3],
                share: vec![4, 5],
                sig: [9; 64],
            },
        }),
        CrossOp::Deal {
            scheme: Scheme::Taproot,
            target: 4,
            eph: [1; 32],
            commitments: vec![vec![2; 33], vec![3; 33]],
            ciphertexts: vec![[4; 48]],
        },
        CrossOp::Complain {
            scheme: Scheme::Ed25519,
            target: 1,
            dealer: 2,
            shared: [3; 32],
            proof: [4; 96],
        },
    ];
    for op in ops {
        let tx = w.sign(TxKind::Cross(op.clone()));
        let back = Transaction::decode(&tx.encode()).unwrap();
        assert_eq!(back, tx);
        assert!(back.verify_signature());
        assert_eq!(back.body.kind, TxKind::Cross(op));
        // every strict prefix fails to decode
        let bytes = tx.encode();
        for cut in [bytes.len() - 1, bytes.len() / 2, 10] {
            assert!(Transaction::decode(&bytes[..cut]).is_err());
        }
    }
}

#[test]
fn the_parallel_executor_reaches_the_same_state_for_keys_and_vault_flows() {
    let run = |parallel: bool| {
        let mut s = Sim::new(4, 4);
        if parallel {
            s.net.ex = Executor::parallel(4);
            s.net.ex.parallel_min_txs = 2;
        }
        s.behave[1] = Behave::BadShareTo(2);
        s.run_to(W + 1);
        let me = s.user.addr();
        let c = claim(Scheme::Taproot, 1, 777, me);
        let txs: Vec<Transaction> = (0..4)
            .map(|v| {
                s.cross(
                    v,
                    CrossOp::Attest {
                        claims: vec![c.clone()],
                    },
                )
            })
            .collect();
        s.block(txs);
        s.run_to(E + 3);
        assert_eq!(
            vault::balance_of(s.kv(), Scheme::Taproot, &me).unwrap(),
            777
        );
        (
            s.net.state.root().unwrap(),
            s.xkey(Scheme::Taproot, 1).unwrap().group.public_key,
        )
    };
    let (a, ka) = run(false);
    let (b, kb) = run(true);
    assert_eq!(a, b, "state roots differ between executors");
    assert_eq!(ka, kb);
}

#[allow(dead_code)]
fn unused(_: Params, _: DkgOp) {}
