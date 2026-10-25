//! Storage placement and challenges are seeded by the randomness beacon when the block has one.
//! Two chains that differ only in their beacon key place the same file on different nodes and
//! challenge different chunks; a chain with the same key reproduces both exactly; and the
//! challenged chunk is the one derived from the beacon seed of the block that opened the round
//! (not from a block hash a proposer could grind).

mod support;
use btb_chain::storage::proof::{self, CHUNK};
use btb_chain::storage::{StorageConfig, StorageOp};
use btb_chain::*;
use btb_oracle::beacon::{beacon_message, Beacon};
use btb_oracle::{combine, trusted_deal, GroupKey, KeyShare, Params};
use btb_runtime::MemKv;
use sha2::{Digest, Sha256};
use support::*;

const RICH: u64 = 1_000_000_000_000;
const NODES: usize = 12;

fn cfg() -> StorageConfig {
    StorageConfig {
        epoch_blocks: 10,
        challenge_blocks: 3,
        repair_blocks: 4,
        price_per_kib_period: 10,
        deposit_per_kib: 100,
        min_deposit: 100_000,
        slash_bps: 5_000,
    }
}

fn file(seed: u8, size: u64, k: u8, m: u8) -> (Hash, Vec<Vec<u8>>) {
    let pl = proof::piece_len(size, k as usize) as usize;
    let pieces: Vec<Vec<u8>> = (0..k + m)
        .map(|i| {
            (0..pl)
                .map(|j| {
                    ((j as u32 ^ (u32::from(i) << 20) ^ (u32::from(seed) << 12))
                        .wrapping_mul(2_654_435_761)
                        >> 24) as u8
                })
                .collect()
        })
        .collect();
    let roots: Vec<Hash> = pieces.iter().map(|p| proof::piece_root(p)).collect();
    (proof::file_id_of_pieces(&roots, size, k, m), pieces)
}

struct World {
    net: Net<MemKv>,
    group: GroupKey,
    shares: Vec<KeyShare>,
    owner: Wallet,
    nodes: Vec<Wallet>,
}

/// A chain with the beacon key dealt from `key_seed`, 4 validators and `NODES` storage nodes.
fn world(key_seed: u8) -> World {
    let (group, shares) = trusted_deal(Params::standard(4), &[key_seed; 32]).unwrap();
    let owner = Wallet::new(1);
    let ops: Vec<Wallet> = (0..4).map(|i| Wallet::new(40 + i as u8)).collect();
    let mut nodes: Vec<Wallet> = (0..NODES).map(|i| Wallet::new(60 + i as u8)).collect();
    let mut config = ChainConfig::new(CHAIN_ID);
    config.storage = cfg();
    config.staking = StakingConfig {
        epoch_len: 1_000,
        max_validators: 8,
        min_stake: 1_000,
        unbonding_blocks: 1_000,
        slash_bps: 2_000,
        reporter_reward_bps: 1_000,
    };
    let g = {
        let mut refs: Vec<&Wallet> = vec![&owner];
        refs.extend(ops.iter());
        refs.extend(nodes.iter());
        genesis_with(&refs, RICH, config)
    };
    let vals: Vec<GenesisValidator> = ops
        .iter()
        .enumerate()
        .map(|(i, w)| GenesisValidator {
            operator: w.addr(),
            consensus_key: SigningKey::from_bytes(&[100 + i as u8; 32])
                .verifying_key()
                .to_bytes(),
            network_addr: Vec::new(),
            stake: 10_000,
        })
        .collect();
    let og = OracleGenesis {
        group_key: group.encode(),
        holders: vals.iter().map(|v| v.consensus_key).collect(),
        pairs: vec![],
        max_age_blocks: 20,
        dkg_window: 20,
    };
    let state = ChainState::from_genesis_full(MemKv::new(), &g, &vals, Some(&og)).unwrap();
    let mut w = World {
        net: Net {
            state,
            ex: Executor::sequential(),
        },
        group,
        shares,
        owner,
        nodes: Vec::new(),
    };
    let regs: Vec<_> = nodes
        .iter_mut()
        .enumerate()
        .map(|(i, n)| {
            n.sign(TxKind::Storage(StorageOp::Register {
                endpoint: format!("http://127.0.0.1:{}", 9000 + i).into_bytes(),
                deposit: 10_000_000,
            }))
        })
        .collect();
    w.nodes = nodes;
    for r in w.run(regs) {
        ok(&r);
    }
    w
}

impl World {
    fn beacon(&self, height: u64) -> Beacon {
        let msg = beacon_message(CHAIN_ID, (height - 1) / 1_000, height);
        let partials: Vec<_> = self
            .shares
            .iter()
            .take(self.group.threshold)
            .map(|s| (s.index, s.sign(&msg)))
            .collect();
        Beacon {
            epoch: (height - 1) / 1_000,
            height,
            signature: combine(&self.group, &msg, &partials).unwrap().to_bytes(),
        }
    }

    fn run(&mut self, txs: Vec<Transaction>) -> Vec<Receipt> {
        let h = self.net.state.head().unwrap().height + 1;
        let b = self.net.block_for(txs).with_oracle(OracleInput {
            beacon: Some(self.beacon(h)),
            ..Default::default()
        });
        let (r, _) = self.net.ex.execute_block(&mut self.net.state, &b).unwrap();
        r
    }

    fn store(&mut self, f: &Hash, size: u64, k: u8, m: u8) {
        let tx = self.owner.sign(TxKind::Storage(StorageOp::Store {
            root: *f,
            size,
            k,
            m,
            periods: 3,
        }));
        ok(&self.run(vec![tx])[0]);
    }

    fn holders(&self, f: &Hash) -> Vec<Address> {
        self.net
            .state
            .storage_pieces(f)
            .unwrap()
            .iter()
            .map(|p| p.node)
            .collect()
    }

    /// Run empty blocks until a piece of `f` is challenged; returns `(height, index, chunk)`.
    fn first_challenge(&mut self, f: &Hash) -> (u64, u8, u64) {
        for _ in 0..40 {
            self.run(vec![]);
            let found = self
                .net
                .state
                .storage_pieces(f)
                .unwrap()
                .into_iter()
                .find_map(|p| p.challenge.map(|c| (p.index, c.chunk)));
            if let Some((index, chunk)) = found {
                return (self.net.state.head().unwrap().height, index, chunk);
            }
        }
        panic!("no challenge opened");
    }
}

#[test]
fn storage_placement_follows_the_beacon_seed() {
    let size = 64 * 1024;
    let (k, m) = (3u8, 2u8);
    let files: Vec<(Hash, Vec<Vec<u8>>)> = (1..=6).map(|s| file(s, size, k, m)).collect();

    let place = |key_seed: u8| -> Vec<Vec<Address>> {
        let mut w = world(key_seed);
        files
            .iter()
            .map(|(id, _)| {
                w.store(id, size, k, m);
                w.holders(id)
            })
            .collect()
    };
    let a = place(1);
    let a_again = place(1);
    let b = place(2);
    assert_eq!(a, a_again, "same beacons, same placement");
    assert_ne!(a, b, "another beacon key gives another placement");
    let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
    assert!(
        differing >= 3,
        "only {differing} of 6 files were placed differently"
    );
    // pieces of one file still go to distinct nodes while enough exist
    for holders in &a {
        let mut d = holders.clone();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), holders.len());
    }
}

#[test]
fn challenged_chunks_come_from_the_beacon_of_the_block_that_opens_the_round() {
    let size = 64 * 1024;
    let (k, m) = (3u8, 2u8);
    let (id, _) = file(9, size, k, m);
    let chunks = proof::piece_len(size, k as usize) / CHUNK as u64;
    assert!(chunks >= 16);

    let mut seen = Vec::new();
    for key_seed in [1u8, 2] {
        let mut w = world(key_seed);
        w.store(&id, size, k, m);
        let (height, index, chunk) = w.first_challenge(&id);
        // the seed is derived from the beacon of the block in which the round opened
        let seed: Hash = Sha256::new()
            .chain_update(b"btb.storage.seed.v1")
            .chain_update(w.beacon(height).seed())
            .finalize()
            .into();
        let expect = proof::challenge_chunk(&seed, &id, index, 1, chunks);
        assert_eq!(
            chunk, expect,
            "key {key_seed}: chunk is not the beacon-derived one"
        );
        seen.push((index, chunk));
    }
    assert_ne!(seen[0], seen[1], "different beacons challenge differently");
}
