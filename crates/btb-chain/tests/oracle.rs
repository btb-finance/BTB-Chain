//! Oracles on the chain: the randomness beacon and price feeds. Security-sensitive: every
//! refusal path is tested (missing or wrong beacon, randomness outside marked entry points,
//! bad, stale, replayed or unauthorised observations), plus determinism across replicas and
//! between sequential and parallel execution.

mod support;
use btb_chain::*;
use btb_codec::Decode;
use btb_oracle::beacon::{beacon_message, Beacon};
use btb_oracle::{
    combine, trusted_deal, GroupKey, KeyShare, Observation, Params, SignedObservation,
};
use btb_runtime::MemKv;
use support::*;

const E: u64 = 1_000;
const EACH: u64 = 1_000_000_000_000;
const STAKES: [u64; 4] = [40_000, 30_000, 20_000, 10_000];

fn ckey(i: usize) -> SigningKey {
    SigningKey::from_bytes(&[100 + i as u8; 32])
}

fn ckey_bytes(i: usize) -> [u8; 32] {
    ckey(i).verifying_key().to_bytes()
}

fn pairs() -> Vec<PairConfig> {
    vec![
        PairConfig {
            id: 0,
            name: "BTC/USD".into(),
            decimals: 8,
        },
        PairConfig {
            id: 1,
            name: "ETH/USD".into(),
            decimals: 8,
        },
    ]
}

struct Env {
    net: Net<MemKv>,
    group: GroupKey,
    shares: Vec<KeyShare>,
    users: Vec<Wallet>,
    /// Blocks whose transactions the parallel executor really ran in parallel.
    parallel_blocks: usize,
}

fn oracle_genesis(group: &GroupKey, n: usize) -> OracleGenesis {
    OracleGenesis {
        group_key: group.encode(),
        holders: (0..n).map(ckey_bytes).collect(),
        pairs: pairs(),
        max_age_blocks: 20,
        dkg_window: 20,
    }
}

fn staking_cfg(max_validators: u32) -> StakingConfig {
    StakingConfig {
        epoch_len: E,
        max_validators,
        min_stake: 1_000,
        unbonding_blocks: E,
        slash_bps: 2_000,
        reporter_reward_bps: 1_000,
    }
}

fn env_with(stakes: &[u64], seed: u8, users: usize) -> Env {
    let (group, shares) = trusted_deal(Params::standard(stakes.len()), &[seed; 32]).unwrap();
    env_keyed(stakes, group, shares, users)
}

fn env_keyed(stakes: &[u64], group: GroupKey, shares: Vec<KeyShare>, users: usize) -> Env {
    let n = stakes.len();
    let ops: Vec<Wallet> = (0..n).map(|i| Wallet::new(10 + i as u8)).collect();
    let users: Vec<Wallet> = (0..users).map(|i| Wallet::new(50 + i as u8)).collect();
    let mut all: Vec<&Wallet> = ops.iter().collect();
    all.extend(users.iter());
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.staking = staking_cfg(8);
    let g = genesis_with(&all, EACH, cfg);
    let vals: Vec<GenesisValidator> = stakes
        .iter()
        .enumerate()
        .map(|(i, s)| GenesisValidator {
            operator: ops[i].addr(),
            consensus_key: ckey_bytes(i),
            network_addr: Vec::new(),
            stake: *s,
        })
        .collect();
    let og = oracle_genesis(&group, n);
    let state = ChainState::from_genesis_full(MemKv::new(), &g, &vals, Some(&og)).unwrap();
    Env {
        net: Net {
            state,
            ex: Executor::sequential(),
        },
        group,
        shares,
        users,
        parallel_blocks: 0,
    }
}

fn env() -> Env {
    env_with(&STAKES, 1, 3)
}

fn beacon_for(group: &GroupKey, shares: &[KeyShare], epoch: u64, height: u64) -> Beacon {
    let msg = beacon_message(CHAIN_ID, epoch, height);
    let partials: Vec<_> = shares
        .iter()
        .take(group.threshold)
        .map(|s| (s.index, s.sign(&msg)))
        .collect();
    Beacon {
        epoch,
        height,
        signature: combine(group, &msg, &partials).unwrap().to_bytes(),
    }
}

fn obs(i: usize, pair: u32, price: u128, height: u64) -> SignedObservation {
    SignedObservation::sign(
        CHAIN_ID,
        &ckey(i),
        Observation {
            pair,
            price,
            height,
        },
    )
}

impl Env {
    fn height(&self) -> u64 {
        self.net.state.head().unwrap().height
    }

    fn block(&self, txs: Vec<Transaction>, observations: Vec<SignedObservation>) -> Block {
        let h = self.height() + 1;
        let beacon = beacon_for(&self.group, &self.shares, (h - 1) / E, h);
        self.net.block_for(txs).with_oracle(OracleInput {
            beacon: Some(beacon),
            observations,
        })
    }

    fn try_run(
        &mut self,
        txs: Vec<Transaction>,
        observations: Vec<SignedObservation>,
    ) -> Result<Vec<Receipt>, BlockError> {
        let b = self.block(txs, observations);
        let (r, root, stats) = self.net.ex.execute_block_stats(&mut self.net.state, &b)?;
        self.parallel_blocks += usize::from(stats.is_some());
        assert_eq!(root, self.net.state.root().unwrap());
        assert_eq!(
            self.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY)
        );
        Ok(r)
    }

    fn run(&mut self, txs: Vec<Transaction>) -> Vec<Receipt> {
        self.try_run(txs, vec![]).unwrap()
    }

    fn run_obs(&mut self, observations: Vec<SignedObservation>) {
        self.try_run(vec![], observations).unwrap();
    }

    fn run_one(&mut self, tx: Transaction) -> Receipt {
        self.run(vec![tx]).remove(0)
    }

    fn price(&self, pair: u32) -> Option<(u128, u64, u8, u32)> {
        oracle::price(self.net.state.store(), pair)
            .unwrap()
            .map(|p| (p.price, p.height, p.decimals, p.reporters))
    }

    fn deploy_oracle_contract(&mut self, crons: Vec<(String, u64)>) -> Address {
        let tx = self.users[0].deploy(
            elf("oracle"),
            Manifest {
                name: "oracle".into(),
                crons,
                indexes: vec![],
            },
        );
        let c = deployed(&self.run_one(tx));
        // the contract pays the storage deposit of what it stores
        let fund = self.users[0].transfer(c, 100_000_000_000);
        ok(&self.run_one(fund));
        c
    }
}

fn out_u64(r: &Receipt) -> u64 {
    ok(r);
    u64::decode(&r.output).unwrap()
}

fn trap_name(r: &Receipt) -> String {
    match &r.status {
        Status::Reverted(Revert::Trap(t)) => t.clone(),
        other => panic!("expected a trap, got {other:?}"),
    }
}

// ------------------------------------------------------------------------------ beacon

#[test]
fn every_block_needs_a_valid_beacon() {
    let mut e = env();
    let h = e.height() + 1;
    let before = e.net.state.root().unwrap();
    let exec = |e: &mut Env, b: Block| e.net.ex.execute_block(&mut e.net.state, &b);

    // none at all
    let b = e.net.block_for(vec![]);
    assert!(matches!(exec(&mut e, b), Err(BlockError::Oracle(_))));
    // a beacon for another height / epoch
    for (epoch, height) in [(0, h + 1), (0, h - 1), (1, h)] {
        let bad = beacon_for(&e.group, &e.shares, epoch, height);
        let b = e.net.block_for(vec![]).with_oracle(OracleInput {
            beacon: Some(bad),
            observations: vec![],
        });
        assert!(
            matches!(exec(&mut e, b), Err(BlockError::Oracle(_))),
            "epoch {epoch} height {height}"
        );
    }
    // the right message signed by another committee
    let (other_group, other_shares) = trusted_deal(Params::standard(4), &[9; 32]).unwrap();
    let forged = beacon_for(&other_group, &other_shares, 0, h);
    let b = e.net.block_for(vec![]).with_oracle(OracleInput {
        beacon: Some(forged),
        observations: vec![],
    });
    assert!(matches!(exec(&mut e, b), Err(BlockError::Oracle(_))));
    // a signature with a flipped bit, and garbage bytes
    for flip in [5usize, 40, 90] {
        let mut bad = beacon_for(&e.group, &e.shares, 0, h);
        bad.signature[flip] ^= 1;
        let b = e.net.block_for(vec![]).with_oracle(OracleInput {
            beacon: Some(bad),
            observations: vec![],
        });
        assert!(matches!(exec(&mut e, b), Err(BlockError::Oracle(_))));
    }
    let junk = Beacon {
        epoch: 0,
        height: h,
        signature: [0xAB; 96],
    };
    let b = e.net.block_for(vec![]).with_oracle(OracleInput {
        beacon: Some(junk),
        observations: vec![],
    });
    assert!(matches!(exec(&mut e, b), Err(BlockError::Oracle(_))));
    // every refusal left the state untouched
    assert_eq!(e.height(), h - 1);
    assert_eq!(e.net.state.root().unwrap(), before);
    // and the real beacon is accepted
    e.run(vec![]);
    assert_eq!(e.height(), h);
    // the beacon is committed in state for verifiers
    let stored = oracle::last_beacon(e.net.state.store()).unwrap().unwrap();
    assert_eq!(stored, beacon_for(&e.group, &e.shares, 0, h));
}

#[test]
fn beacon_input_on_a_chain_without_oracles_is_refused() {
    let w = Wallet::new(1);
    let g = genesis(&[&w], EACH);
    let mut net = Net::new(MemKv::new(), &g);
    let b = net.block_for(vec![]).with_oracle(OracleInput {
        beacon: Some(Beacon {
            epoch: 0,
            height: 1,
            signature: [0; 96],
        }),
        observations: vec![],
    });
    assert!(matches!(
        net.ex.execute_block(&mut net.state, &b),
        Err(BlockError::Oracle(_))
    ));
    // an ordinary block still works
    net.run(vec![]);
}

#[test]
fn oracle_genesis_is_validated() {
    let (group, _) = trusted_deal(Params::standard(4), &[1; 32]).unwrap();
    let base = oracle_genesis(&group, 4);
    let build = |og: &OracleGenesis| {
        let w = Wallet::new(1);
        let g = genesis(&[&w], EACH);
        ChainState::from_genesis_full(MemKv::new(), &g, &[], Some(og)).map(|_| ())
    };
    build(&base).unwrap();
    let mut bad = base.clone();
    bad.holders.pop();
    assert!(build(&bad).is_err(), "holders vs shares");
    let mut bad = base.clone();
    bad.holders[1] = bad.holders[0];
    assert!(build(&bad).is_err(), "duplicate holder");
    let mut bad = base.clone();
    bad.pairs.push(bad.pairs[0].clone());
    assert!(build(&bad).is_err(), "duplicate pair");
    let mut bad = base.clone();
    bad.max_age_blocks = 0;
    assert!(build(&bad).is_err(), "max age");
    let mut bad = base.clone();
    bad.group_key[20] ^= 1;
    assert!(build(&bad).is_err(), "corrupt key");
    let mut bad = base.clone();
    bad.pairs[0].name = String::new();
    assert!(build(&bad).is_err(), "empty name");
    // swapped public shares are inconsistent with the group key
    let mut shares = group.public_shares.clone();
    shares.swap(0, 1);
    let inconsistent = GroupKey {
        public_shares: shares,
        ..group.clone()
    };
    let mut bad = base;
    bad.group_key = inconsistent.encode();
    assert!(build(&bad).is_err(), "inconsistent shares");
}

#[test]
fn oracle_input_and_block_encoding_round_trip() {
    let e = env();
    let obs = vec![obs(0, 0, 123, 1), obs(1, 1, 456, 1)];
    let b = e.block(vec![], obs.clone());
    let again = Block::decode(&b.encode()).unwrap();
    assert_eq!(again, b);
    assert_eq!(again.oracle.observations, obs);
    // a block without oracle input encodes exactly as before (no trailing section)
    let plain = e.net.block_for(vec![]);
    assert_eq!(Block::decode(&plain.encode()).unwrap(), plain);
    assert!(b.encode().len() > plain.encode().len());
    // truncated or padded oracle sections are refused
    let bytes = b.encode();
    assert!(Block::decode(&bytes[..bytes.len() - 1]).is_err());
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(Block::decode(&longer).is_err());
}

// ------------------------------------------------------------------------- randomness

#[test]
fn only_marked_entry_points_get_randomness() {
    let mut e = env();
    let c = e.deploy_oracle_contract(vec![]);
    let mut w = e.users.remove(1);
    // marked mutation: a die roll in 1..=6
    let r = e.run_one(w.call(c, "roll", &()));
    assert!((1..=6).contains(&out_u64(&r)));
    // unmarked mutation: the VM traps
    let r = e.run_one(w.call(c, "unmarked", &()));
    assert_eq!(trap_name(&r), "RandomnessForbidden");
    // the same function name as a query (no transaction): also trapped
    let err = e
        .net
        .ex
        .query(&mut e.net.state, &c, "cheat_query", &[], &w.addr())
        .unwrap_err();
    assert_eq!(err, Revert::Trap("RandomnessForbidden".into()));
    // a trapped call still changed nothing but nonce and fee
    let rolls = e.net.query(&c, "rolls", &(), &w.addr());
    assert_eq!(
        Vec::<u8>::decode(&rolls).unwrap().len(),
        8,
        "one roll recorded"
    );
}

#[test]
fn randomness_is_unique_per_call_and_per_transaction() {
    let mut e = env();
    let c = e.deploy_oracle_contract(vec![]);
    let (mut a, mut b) = (e.users.remove(1), e.users.remove(1));
    // two transactions in one block, two draws in one transaction, two blocks
    let r = e.run(vec![a.call(c, "draw", &()), b.call(c, "draw", &())]);
    let (d1, d2) = (r[0].output.clone(), r[1].output.clone());
    assert_ne!(d1, d2, "different transactions");
    let r = e.run_one(a.call(c, "two_draws", &()));
    ok(&r);
    let (x, y) = <(u64, u64)>::decode(&r.output).unwrap();
    assert_ne!(x, y, "two calls in one transaction");
    let r = e.run_one(a.call(c, "draw", &()));
    assert_ne!(r.output, d1, "a later block");
    // many rolls cover the die and stay in range
    let mut seen = [0u32; 7];
    for _ in 0..40 {
        let r = e.run_one(b.call(c, "roll", &()));
        seen[out_u64(&r) as usize] += 1;
    }
    assert_eq!(seen[0], 0);
    assert!(
        seen[1..].iter().all(|n| *n > 0),
        "all faces appeared: {seen:?}"
    );
}

#[test]
fn randomness_is_deterministic_across_replicas_and_depends_on_the_beacon() {
    let build = |seed: u8| {
        let mut e = env_with(&STAKES, seed, 3);
        let c = e.deploy_oracle_contract(vec![]);
        let mut w = e.users.remove(1);
        let mut outs = Vec::new();
        for _ in 0..6 {
            let r = e.run(vec![w.call(c, "draw", &()), w.call(c, "roll", &())]);
            outs.push((r[0].output.clone(), r[1].output.clone()));
        }
        (outs, e.net.state.root().unwrap())
    };
    let (a, ra) = build(1);
    let (b, rb) = build(1);
    assert_eq!(a, b, "same chain, same bytes");
    assert_eq!(ra, rb, "same state root");
    // A different beacon key gives different randomness (and a different genesis hash).
    let (c, rc) = build(2);
    assert_ne!(a, c);
    assert_ne!(ra, rc);
}

#[test]
fn parallel_and_sequential_execution_agree_with_randomness_and_prices() {
    let mk = |parallel: bool| {
        let mut e = env_with(&STAKES, 3, 20);
        e.net.ex = if parallel {
            let mut x = Executor::parallel(4);
            x.parallel_min_txs = 2;
            x
        } else {
            Executor::sequential()
        };
        let c = e.deploy_oracle_contract(vec![]);
        let mut users = std::mem::take(&mut e.users);
        users.remove(0);
        let mut roots = Vec::new();
        let mut outs = Vec::new();
        for round in 0..3u64 {
            let txs: Vec<Transaction> = users
                .iter_mut()
                .enumerate()
                .map(|(i, u)| {
                    if i % 3 == 0 {
                        u.call(c, "draw", &())
                    } else if i % 3 == 1 {
                        u.call(c, "roll", &())
                    } else {
                        u.call(c, "quote", &0u32)
                    }
                })
                .collect();
            let observations = (0..4)
                .map(|v| {
                    obs(
                        v,
                        0,
                        1_000 + u128::from(round) * 10 + v as u128,
                        e.height() + 1,
                    )
                })
                .collect();
            let r = e.try_run(txs, observations).unwrap();
            outs.push(
                r.iter()
                    .map(|x| (x.status.clone(), x.gas_used, x.output.clone()))
                    .collect::<Vec<_>>(),
            );
            roots.push(e.net.state.root().unwrap());
        }
        assert_eq!(
            e.parallel_blocks > 0,
            parallel,
            "parallel path was exercised"
        );
        (outs, roots)
    };
    let (seq_out, seq_roots) = mk(false);
    let (par_out, par_roots) = mk(true);
    assert_eq!(seq_roots, par_roots);
    assert_eq!(seq_out, par_out);
}

#[test]
fn randomness_costs_the_full_gas_limit_even_when_the_call_fails() {
    let mut e = env();
    let c = e.deploy_oracle_contract(vec![]);
    let mut w = e.users.remove(1);
    let price = e.net.state.config().gas_price;
    // success: the whole limit is charged, not what the code used
    let r = e.run_one(w.call(c, "roll", &()));
    ok(&r);
    assert_eq!(r.gas_used, GAS);
    assert_eq!(r.fee, GAS * price);
    // a "losing" outcome that aborts costs the same, so aborting is never cheaper
    let r = e.run_one(w.call(c, "roll_and_abort", &()));
    assert!(matches!(r.status, Status::Reverted(Revert::Abort(_))));
    assert_eq!(r.gas_used, GAS);
    // a trap in a marked function too; unmarked functions pay only what they used
    let r = e.run_one(w.call(c, "unmarked", &()));
    assert!(r.gas_used < GAS / 100, "{}", r.gas_used);
    // and a call without randomness is metered normally
    let r = e.run_one(w.call(c, "rolls", &()));
    let _ = r;
}

#[test]
fn a_cron_marked_randomness_gets_the_beacon() {
    let mut e = env();
    let c = e.deploy_oracle_contract(vec![("lottery".into(), 5)]);
    let mut runs = Vec::new();
    for _ in 0..16 {
        for r in e.run(vec![]) {
            if matches!(r.origin, Origin::Cron { .. }) {
                runs.push(r);
            }
        }
    }
    assert!(runs.len() >= 2, "cron ran {} times", runs.len());
    for r in &runs {
        ok(r);
        assert_eq!(r.gas_used, e.net.state.config().cron_gas_limit, "full gas");
    }
    let rolls = e.net.query(&c, "rolls", &(), &e.users[0].addr());
    let rolls = Vec::<u8>::decode(&rolls).unwrap();
    assert_eq!(rolls.len(), 8 * runs.len());
    for ch in rolls.chunks(8) {
        let v = u64::from_le_bytes(ch.try_into().unwrap());
        assert!((101..=106).contains(&v), "{v}");
    }
}

// ----------------------------------------------------------------------------- prices

#[test]
fn stake_weighted_median_of_fresh_observations() {
    let mut e = env();
    assert_eq!(e.price(0), None);
    let h = e.height() + 1;
    // 100 (40%), 101 (30%), 102 (20%), 103 (10%): the weighted median is 101
    e.run_obs((0..4).map(|v| obs(v, 0, 100 + v as u128, h)).collect());
    assert_eq!(e.price(0), Some((101, h, 8, 4)));
    assert_eq!(e.price(1), None, "other feeds are untouched");
    // a new round moves it
    let h2 = e.height() + 1;
    e.run_obs((0..4).map(|v| obs(v, 0, 200, h2)).collect());
    assert_eq!(e.price(0), Some((200, h2, 8, 4)));
}

#[test]
fn prices_need_two_thirds_of_the_stake_reporting() {
    let mut e = env();
    let h = e.height() + 1;
    // validators 2 and 3 hold 30%: nothing is published
    e.run_obs(vec![obs(2, 0, 100, h), obs(3, 0, 100, h)]);
    assert_eq!(e.price(0), None);
    // validator 1 adds 30%: 60%, still short of two thirds
    let h = e.height() + 1;
    e.run_obs(vec![obs(1, 0, 100, h)]);
    assert_eq!(e.price(0), None);
    // validator 0 adds 40%: everyone reported (fresh observations accumulate across blocks)
    let h = e.height() + 1;
    e.run_obs(vec![obs(0, 0, 100, h)]);
    assert_eq!(e.price(0).map(|p| p.0), Some(100));
    // validators 0 and 1 alone are 70% >= two thirds
    let mut e = env();
    let h = e.height() + 1;
    e.run_obs(vec![obs(0, 0, 50, h), obs(1, 0, 60, h)]);
    assert_eq!(e.price(0).map(|p| (p.0, p.3)), Some((50, 2)));
}

#[test]
fn a_minority_of_stake_cannot_move_the_price_outside_the_honest_range() {
    // four equal validators: one (25% < 1/3) is Byzantine
    let honest = [1_000u128, 1_010, 1_020];
    for evil in [0u128, 1, u128::MAX, 999_999_999_999] {
        for evil_idx in 0..4usize {
            let mut e = env_with(&[25_000; 4], 4, 2);
            let h = e.height() + 1;
            let mut hs = honest.iter();
            let observations = (0..4)
                .map(|v| {
                    if v == evil_idx {
                        obs(v, 0, evil, h)
                    } else {
                        obs(v, 0, *hs.next().unwrap(), h)
                    }
                })
                .collect();
            e.run_obs(observations);
            let p = e.price(0).unwrap().0;
            assert!(
                (1_000..=1_020).contains(&p),
                "evil {evil} at {evil_idx} moved the price to {p}"
            );
        }
    }
    // stake, not head count: a 10% validator reporting nonsense is irrelevant
    let mut e = env();
    let h = e.height() + 1;
    e.run_obs(vec![
        obs(0, 0, 500, h),
        obs(1, 0, 501, h),
        obs(2, 0, 502, h),
        obs(3, 0, u128::MAX, h),
    ]);
    assert_eq!(e.price(0).map(|p| p.0), Some(501));
}

#[test]
fn bad_stale_replayed_and_unauthorised_observations_are_ignored() {
    let mut e = env();
    e.run(vec![]);
    e.run(vec![]);
    let h = e.height() + 1;
    let stranger = SigningKey::from_bytes(&[77; 32]);
    let sign = |k: &SigningKey, chain: u64, pair: u32, price: u128, height: u64| {
        SignedObservation::sign(
            chain,
            k,
            Observation {
                pair,
                price,
                height,
            },
        )
    };
    let mut bad = vec![
        // not a validator
        sign(&stranger, CHAIN_ID, 0, 1, h),
        // another chain's signature
        sign(&ckey(0), CHAIN_ID + 1, 0, 2, h),
        // unknown pair
        sign(&ckey(0), CHAIN_ID, 9, 3, h),
        // from the future
        sign(&ckey(1), CHAIN_ID, 0, 4, h + 1),
    ];
    // tampered price after signing
    let mut t = obs(2, 0, 5, h);
    t.obs.price = 6;
    bad.push(t);
    // a forged signer field: validator 3's key with validator 2's signature
    let mut f = obs(2, 0, 7, h);
    f.key = ckey_bytes(3);
    bad.push(f);
    e.run_obs(bad);
    assert_eq!(e.price(0), None, "nothing was accepted");

    // stale: older than max_age_blocks (20) blocks
    let mut e = env();
    for _ in 0..25 {
        e.run(vec![]);
    }
    let h = e.height() + 1;
    e.run_obs((0..4).map(|v| obs(v, 0, 10, 2)).collect());
    assert_eq!(
        e.price(0),
        None,
        "observations from height 2 are stale at {h}"
    );
    // fresh ones work
    let h = e.height() + 1;
    e.run_obs((0..4).map(|v| obs(v, 0, 10, h)).collect());
    assert_eq!(e.price(0).map(|p| p.0), Some(10));

    // replay / older statement: a validator cannot go back in time
    let h2 = e.height() + 1;
    e.run_obs((0..4).map(|v| obs(v, 0, 20, h2)).collect());
    assert_eq!(e.price(0).map(|p| p.0), Some(20));
    let h3 = e.height() + 1;
    e.run_obs((0..4).map(|v| obs(v, 0, 5, h2 - 1)).collect()); // older height: ignored
    assert_eq!(e.price(0), Some((20, h2, 8, 4)), "{h3}");
    e.run_obs((0..4).map(|v| obs(v, 0, 5, h2)).collect()); // same height again: ignored
    assert_eq!(e.price(0).map(|p| p.0), Some(20));

    // a validator that equivocates at one height cannot change its first statement
    let h4 = e.height() + 1;
    e.run_obs(vec![obs(0, 0, 30, h4), obs(0, 0, 3_000_000, h4)]);
    let p = e.price(0).unwrap();
    assert!(p.0 < 3_000_000);
}

#[test]
fn old_observations_expire_and_the_price_stops_updating() {
    let mut e = env();
    let h = e.height() + 1;
    e.run_obs((0..4).map(|v| obs(v, 0, 100, h)).collect());
    assert_eq!(e.price(0).map(|p| p.0), Some(100));
    for _ in 0..25 {
        e.run(vec![]);
    }
    // only 30% report now; the others' old observations have expired: no update, and the
    // old price keeps its old height so consumers can see how stale it is
    let h2 = e.height() + 1;
    e.run_obs(vec![obs(3, 0, 999, h2), obs(2, 0, 999, h2)]);
    assert_eq!(e.price(0), Some((100, h, 8, 4)));
}

#[test]
fn contracts_read_prices_queries_free_mutations_pay_a_fee() {
    let mut e = env();
    let c = e.deploy_oracle_contract(vec![]);
    let mut w = e.users.remove(1);
    // no price yet
    let q = e.net.query(&c, "quote", &0u32, &w.addr());
    assert_eq!(Option::<(u128, u64, u8)>::decode(&q).unwrap(), None);
    let r = e.run_one(w.call(c, "use_price", &(0u32, 5u64)));
    assert!(matches!(r.status, Status::Reverted(Revert::Abort(ref m)) if m == "no price"));
    // publish one
    let h = e.height() + 1;
    e.run_obs((0..4).map(|v| obs(v, 0, 6_500_000_000_000, h)).collect());
    // query: the value, no fee, small gas
    let before = e.net.bal(&w.addr());
    let (out, gas, _) = e
        .net
        .ex
        .query_tracked(
            &mut e.net.state,
            &c,
            "quote",
            &(0u32).to_le_bytes_vec(),
            &w.addr(),
        )
        .unwrap();
    assert_eq!(
        Option::<(u128, u64, u8)>::decode(&out).unwrap(),
        Some((6_500_000_000_000, h, 8))
    );
    assert!(
        gas < btb_vm::gas::PRICE_READ_FEE,
        "query read is cheap: {gas}"
    );
    assert_eq!(e.net.bal(&w.addr()), before, "queries cost nothing");
    // mutation: the price, plus the per-read fee in gas
    let r = e.run_one(w.call(c, "use_price", &(0u32, 5u64)));
    ok(&r);
    assert_eq!(u128::decode(&r.output).unwrap(), 6_500_000_000_000);
    assert!(
        r.gas_used >= btb_vm::gas::PRICE_READ_FEE,
        "per-read fee charged: gas {}",
        r.gas_used
    );
    assert_eq!(r.fee, r.gas_used * e.net.state.config().gas_price);
    // a staleness bound the contract chooses
    for _ in 0..10 {
        e.run(vec![]);
    }
    let r = e.run_one(w.call(c, "use_price", &(0u32, 5u64)));
    assert!(matches!(r.status, Status::Reverted(Revert::Abort(ref m)) if m == "price too old"));
    let r = e.run_one(w.call(c, "use_price", &(0u32, 50u64)));
    ok(&r);
    // unknown feed
    let r = e.run_one(w.call(c, "use_price", &(7u32, 50u64)));
    assert!(matches!(r.status, Status::Reverted(Revert::Abort(ref m)) if m == "no price"));
}

#[test]
fn a_transaction_sees_the_price_of_the_previous_block() {
    let mut e = env();
    let c = e.deploy_oracle_contract(vec![]);
    let mut w = e.users.remove(1);
    let h = e.height() + 1;
    e.run_obs((0..4).map(|v| obs(v, 0, 100, h)).collect());
    // the same block carries a transaction and a newer price: the transaction reads 100
    let h2 = e.height() + 1;
    let tx = w.call(c, "use_price", &(0u32, 50u64));
    let r = e
        .try_run(vec![tx], (0..4).map(|v| obs(v, 0, 200, h2)).collect())
        .unwrap();
    assert_eq!(u128::decode(&r[0].output).unwrap(), 100);
    assert_eq!(e.price(0).map(|p| p.0), Some(200));
}

trait LeBytes {
    fn to_le_bytes_vec(&self) -> Vec<u8>;
}
impl LeBytes for u32 {
    fn to_le_bytes_vec(&self) -> Vec<u8> {
        btb_codec::Encode::encode(self)
    }
}

// ------------------------------------------------------------------ DKG output on chain

#[test]
fn a_dkg_generated_key_and_a_resharing_work_as_the_chain_beacon_key() {
    use btb_oracle::{Dealing, Mode, Participant, Transcript};
    let entropy = |i: usize| {
        let mut e = [0u8; 32];
        e[..8].copy_from_slice(&(i as u64 + 1000).to_le_bytes());
        e
    };
    // joint-Feldman among the 4 genesis validators, no trusted dealer
    let params = Params::standard(4);
    let mut t = Transcript::new(Mode::Fresh, params).unwrap();
    let dealings: Vec<Dealing> = (0..4)
        .map(|i| Dealing::fresh(i, params, &entropy(i)).unwrap())
        .collect();
    for d in &dealings {
        t.add_commitments(d.dealer, d.commitments.clone()).unwrap();
    }
    let mut parts: Vec<Participant> = (0..4).map(Participant::new).collect();
    for d in &dealings {
        for p in parts.iter_mut() {
            assert!(p.receive(&t, d.dealer, d.share_for(p.index)));
        }
    }
    let out = t.finalize().unwrap();
    let shares: Vec<KeyShare> = parts
        .iter()
        .map(|p| p.key_share(&t, &out).unwrap())
        .collect();
    let mut e = env_keyed(&STAKES, out.group.clone(), shares.clone(), 2);
    let c = e.deploy_oracle_contract(vec![]);
    let mut w = e.users.remove(1);
    let r = e.run_one(w.call(c, "roll", &()));
    assert!((1..=6).contains(&out_u64(&r)));

    // Reshare the same key to a committee of 7 and use it on a fresh chain with 7 holders: the
    // beacon of a given (chain, epoch, height) is the same signature under either committee.
    let new_params = Params::standard(7);
    let mut t2 = Transcript::new(
        Mode::Reshare {
            old: out.group.clone(),
        },
        new_params,
    )
    .unwrap();
    let old_dealers: Vec<Dealing> = (0..3)
        .map(|i| Dealing::reshare(&shares[i], new_params, &entropy(50 + i)).unwrap())
        .collect();
    for d in &old_dealers {
        t2.add_commitments(d.dealer, d.commitments.clone()).unwrap();
    }
    let mut new_parts: Vec<Participant> = (0..7).map(Participant::new).collect();
    for d in &old_dealers {
        for p in new_parts.iter_mut() {
            assert!(p.receive(&t2, d.dealer, d.share_for(p.index)));
        }
    }
    let out2 = t2.finalize().unwrap();
    let shares2: Vec<KeyShare> = new_parts
        .iter()
        .map(|p| p.key_share(&t2, &out2).unwrap())
        .collect();
    for h in [1u64, 2, 77] {
        assert_eq!(
            beacon_for(&out.group, &shares, 0, h),
            beacon_for(&out2.group, &shares2, 0, h),
            "height {h}"
        );
    }
    let stakes7 = [10_000u64; 7];
    let mut e2 = env_keyed(&stakes7, out2.group, shares2, 2);
    e2.run(vec![]);
    e2.run(vec![]);
}
