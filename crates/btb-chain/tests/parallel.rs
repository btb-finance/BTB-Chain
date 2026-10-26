//! Parallel (Block-STM style) execution must give exactly what the sequential executor gives.
//!
//! The sequential executor is the oracle. Random blocks (hot accounts, counter, token, dex and
//! todo calls, failing and invalid transactions, deploys followed by calls in the same block,
//! transfers to the fee pools, payment-channel open, top-up, close, timeout and forged vouchers) run on the oracle and on parallel executors with 1, 2, 4 and 8
//! workers (twice, fast path on and off); receipts, state roots and in the end the whole store
//! must match byte for byte.

mod support;
use btb_chain::*;
use btb_codec::Address as CodecAddress;
use btb_codec::Encode;
use btb_runtime::MemKv;
use btb_vm::FunctionKind;
use btb_voucher::{SignedVoucher, Voucher};
use support::*;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

static RELAYED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// try_transfer receipts that report a callee abort: a nested call failed and was reverted alone.
static CALL_OK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static CHANNEL_OK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const RICH: u64 = 10 * UNITS_PER_BTB;

/// Small bond and short unbonding so the random blocks register, unregister and release hosts.
fn host_cfg() -> ChainConfig {
    let mut c = ChainConfig::new(CHAIN_ID);
    c.host_bond = UNITS_PER_BTB;
    c.host_unbonding_blocks = 3;
    c
}
const WALLETS: usize = 14;

#[derive(Default, Debug)]
struct Seen {
    fast_txs: usize,
    stm_txs: usize,
    reexec: u64,
    fell_back: usize,
    blocks: usize,
}

struct Rig {
    label: String,
    net: Net<MemKv>,
    seen: Seen,
}

fn rigs(g: &GenesisConfig) -> Vec<Rig> {
    let mut cfgs: Vec<(String, Executor)> = vec![("sequential".into(), Executor::sequential())];
    for t in [1, 2, 4, 8] {
        cfgs.push((format!("parallel-{t}"), Executor::parallel(t)));
    }
    cfgs.push(("parallel-4-again".into(), Executor::parallel(4)));
    let mut nofast = Executor::parallel(4);
    nofast.fast_path = false;
    cfgs.push(("parallel-4-nofast".into(), nofast));
    cfgs.into_iter()
        .map(|(label, mut ex)| {
            ex.parallel_min_txs = 2; // exercise the engine on small blocks too
            (label, ex)
        })
        .map(|(label, ex)| Rig {
            label,
            net: Net {
                state: ChainState::from_genesis(MemKv::new(), g).unwrap(),
                ex,
            },
            seen: Seen::default(),
        })
        .collect()
}

/// Run `txs` on every rig and require identical receipts and roots; returns the oracle's.
fn step(rigs: &mut [Rig], txs: Vec<Transaction>) -> Vec<Receipt> {
    let mut oracle: Option<(Vec<Receipt>, [u8; 32])> = None;
    for r in rigs.iter_mut() {
        let b = r.net.block_for(txs.clone());
        let (rec, root, st) = r
            .net
            .ex
            .execute_block_stats(&mut r.net.state, &b)
            .unwrap_or_else(|e| panic!("{}: {e:?}", r.label));
        assert_eq!(root, r.net.state.root().unwrap());
        if let Some(s) = st {
            r.seen.fast_txs += s.fast_txs;
            r.seen.stm_txs += s.stm_txs;
            r.seen.reexec += s.executions.saturating_sub(s.stm_txs as u64);
            r.seen.fell_back += usize::from(s.fell_back);
        }
        r.seen.blocks += 1;
        match &oracle {
            None => oracle = Some((rec, root)),
            Some((orc, oroot)) => {
                assert_eq!(&rec, orc, "{}: receipts differ", r.label);
                assert_eq!(&root, oroot, "{}: state root differs", r.label);
            }
        }
    }
    oracle.unwrap().0
}

fn assert_same_stores(rigs: Vec<Rig>) {
    let mut it = rigs.into_iter();
    let first = it.next().unwrap();
    let want = first.net.state.into_store();
    for r in it {
        assert!(
            r.net.state.into_store() == want,
            "{}: stored bytes differ from the sequential executor",
            r.label
        );
    }
}

struct World {
    counter: Address,
    token: Address,
    dex: Address,
    todo: Address,
    /// Two `examples/caller` routers that call each other and the token (cross-contract calls).
    r1: Address,
    r2: Address,
}

fn setup(rigs: &mut [Rig], w: &mut [Wallet]) -> World {
    let todo_m = manifest("todo");
    let r = step(
        rigs,
        vec![
            w[0].deploy(elf("counter"), manifest("counter")),
            w[0].deploy(elf("token"), manifest("token")),
            w[0].deploy(elf("dex"), manifest("dex")),
            w[0].deploy(elf("todo"), todo_m),
            w[0].deploy(elf("caller"), manifest("router1")),
            w[0].deploy(elf("caller"), manifest("router2")),
        ],
    );
    let [counter, token, dex, todo, r1, r2] = [0, 1, 2, 3, 4, 5].map(|i| deployed(&r[i]));
    let mut txs: Vec<Transaction> = [counter, token, dex, todo, r1, r2]
        .iter()
        .map(|c| {
            w[0].transfer(
                *c,
                if [r1, r2].contains(c) {
                    500_000_000
                } else {
                    2_000_000_000
                },
            )
        })
        .collect();
    // two wallets are registered hosting nodes (relayers); the random blocks toggle them
    for h in [12, 13] {
        txs.push(w[h].sign(TxKind::Host(HostOp::Register {
            url: b"http://host".to_vec(),
        })));
    }
    txs.push(w[0].call(token, "init", &(1_000_000u128,)));
    txs.push(w[0].call(token, "transfer", &(CodecAddress(r1), 100_000u128)));
    txs.push(w[0].call(dex, "add_liquidity", &(500_000u128, 500_000u128)));
    let r = step(rigs, txs);
    r.iter().for_each(ok);
    World {
        counter,
        token,
        dex,
        todo,
        r1,
        r2,
    }
}

/// Channels opened so far: (payer wallet, channel id, payee wallet).
type Chans = Vec<(usize, [u8; 32], usize)>;

fn voucher(w: &[Wallet], p: usize, id: [u8; 32], amount: u64, nonce: u64) -> SignedVoucher {
    Voucher {
        chain_id: CHAIN_ID,
        channel_id: id,
        amount,
        nonce,
    }
    .sign(&w[p].key)
}

fn random_block(
    rng: &mut Lcg,
    w: &mut [Wallet],
    c: &World,
    chans: &mut Chans,
    mode: usize,
) -> Vec<Transaction> {
    let n = 4 + rng.below(36);
    let hot = [w[1].addr(), w[2].addr(), w[3].addr()];
    let mut txs = Vec::new();
    for _ in 0..n {
        let i = rng.below(WALLETS);
        let j = rng.below(WALLETS);
        let to_hot = hot[rng.below(3)];
        let amt = rng.next() % 50_000_000 + 1;
        let k = if mode == 0 {
            // transfer-only block: hot recipients and disjoint pairs (fast path candidates)
            rng.below(3)
        } else {
            rng.below(42)
        };
        let tx = match k {
            0 => w[i].transfer(to_hot, amt),
            1 => w[i].transfer(w[j].addr(), amt),
            2 => {
                // disjoint pair per wallet index (wallets 8..14 pair up with 14 - 1 - i)
                let p = 8 + (i % 6);
                let q = 13 - (i % 6);
                w[p].transfer(w[q].addr(), amt)
            }
            3 | 4 => w[i].call(c.counter, "increment", &(rng.next() % 100,)),
            5 => w[i].call(c.counter, "increment", &(u64::MAX,)), // aborts
            6 => w[i].call_with(
                c.counter,
                FunctionKind::Mutation,
                "increment",
                &(1u64,),
                2_100,
            ), // out of gas
            7 | 8 => {
                let to = CodecAddress(w[j].addr());
                w[i].call(c.token, "transfer", &(to, (rng.next() % 5_000) as u128))
            }
            9 => w[i].call(
                c.dex,
                "swap_a_for_b",
                &((rng.next() % 5_000 + 1) as u128, 0u128),
            ),
            10 => w[i].call(
                c.dex,
                "swap_a_for_b",
                &((rng.next() % 5_000 + 1) as u128, u128::MAX),
            ), // slippage abort
            11 => w[i].call(
                c.dex,
                "add_liquidity",
                &(
                    (rng.next() % 900 + 1) as u128,
                    (rng.next() % 900 + 1) as u128,
                ),
            ),
            12 | 13 => w[i].call(c.todo, "add", &(format!("item {}", rng.next() % 1000),)),
            14 => {
                // deploy, fund and use the new contract in the same block
                let me = w[i].addr();
                let at = w[i].nonce;
                let d = w[i].deploy(elf("counter"), manifest("fresh"));
                let new = contract_address(&me, at);
                txs.push(d);
                txs.push(w[i].transfer(new, 100_000_000));
                w[i].call(new, "increment", &(7u64,))
            }
            15 => w[i].deploy(elf("counter"), manifest("again")),
            16 => w[i].transfer(w[j].addr(), u64::MAX / 2), // cannot afford: invalid
            17 => {
                let mut t = w[i].transfer(to_hot, 1);
                t.signature[3] ^= 1; // forged: invalid, no nonce used
                t
            }
            18 => {
                let mut t = w[i].transfer(to_hot, 1);
                t.body.nonce += 5; // gap: invalid
                w[i].nonce -= 1;
                t
            }
            19 => w[i].transfer(c.counter, amt), // fund a contract
            20 if rng.below(4) == 0 => w[i].transfer(validator_pool(), 1_000), // pool: fallback
            20 => w[i].call(c.counter, "nope", &()), // no such function
            21 => w[i].transfer(w[i].addr(), 5), // self transfer
            24 => {
                // open a channel (its id is known from the payer and its nonce)
                let id = channel_id(&w[i].addr(), w[i].nonce);
                chans.push((i, id, j));
                w[i].sign(TxKind::Channel(ChannelOp::Open {
                    payee: w[j].addr(),
                    deposit: amt,
                    expiry_blocks: 1 + rng.next() % 4,
                }))
            }
            25..=30 if chans.is_empty() => w[i].transfer(to_hot, amt),
            25..=30 => {
                let (p, id, q) = chans[rng.below(chans.len())];
                let claim = rng.next() % (2 * amt);
                match k {
                    25 => w[p].sign(TxKind::Channel(ChannelOp::TopUp {
                        channel: id,
                        amount: amt,
                    })),
                    26 | 27 => {
                        let v = voucher(w, p, id, claim, rng.next());
                        w[q].sign(TxKind::Channel(ChannelOp::Close { voucher: v }))
                    }
                    28 => {
                        let v = (rng.below(2) == 0).then(|| voucher(w, p, id, claim, 1));
                        w[p].sign(TxKind::Channel(ChannelOp::StartClose {
                            channel: id,
                            voucher: v,
                        }))
                    }
                    29 => w[p].sign(TxKind::Channel(ChannelOp::Finalize { channel: id })),
                    _ => {
                        // forged: signed by a wallet that is not the payer
                        let v = voucher(w, (p + 1) % WALLETS, id, claim, 1);
                        w[q].sign(TxKind::Channel(ChannelOp::Close { voucher: v }))
                    }
                }
            }
            31..=33 => {
                // a relayed transfer or call: registered host, unregistered, the sender itself,
                // a hot account (deferred credits must not be visible to its other txs)
                let relayer = [
                    w[12].addr(),
                    w[13].addr(),
                    w[i].addr(),
                    treasury(),
                    w[1].addr(),
                    w[12].addr(),
                ][rng.below(6)];
                let kind = if k == 33 {
                    TxKind::Call {
                        contract: c.counter,
                        kind: FunctionKind::Mutation,
                        function: "increment".into(),
                        args: (3u64,).encode(),
                        gas_limit: GAS,
                        max_fee: u64::MAX,
                    }
                } else {
                    TxKind::Transfer {
                        to: if k == 31 { to_hot } else { w[j].addr() },
                        amount: amt,
                    }
                };
                w[i].sign_relayed(kind, Some(relayer))
            }
            34 => {
                let h = 12 + rng.below(2);
                w[h].sign(TxKind::Host(if rng.below(3) == 0 {
                    HostOp::Unregister
                } else {
                    HostOp::Register {
                        url: vec![b'u'; rng.below(300)],
                    }
                }))
            }
            35 => w[i].transfer(w[12 + rng.below(2)].addr(), amt), // pay a relayer directly
            36 => {
                // router moves tokens it holds: the token sees the router as caller
                let to = CodecAddress(w[j].addr());
                w[i].call(
                    c.r1,
                    "forward_transfer",
                    &(CodecAddress(c.token), to, (rng.next() % 3_000) as u128),
                )
            }
            37 => {
                // a callee abort (insufficient balance) must only revert the callee
                let to = CodecAddress(w[j].addr());
                let (from, amt) = if rng.below(2) == 0 {
                    (c.r1, (rng.next() % 3_000) as u128)
                } else {
                    (c.r2, 5u128)
                };
                w[i].call(from, "try_transfer", &(CodecAddress(c.token), to, amt))
            }
            38 => w[i].call(c.r1, "chain", &(vec![CodecAddress(c.r2)],)),
            // re-entrancy refused in the middle of a chain
            39 => w[i].call(
                c.r2,
                "chain",
                &(vec![CodecAddress(c.r1), CodecAddress(c.r2)],),
            ),
            40 => {
                let n = (rng.next() % 800) as u32;
                if rng.below(2) == 0 {
                    w[i].call(c.r1, "grow_via", &(CodecAddress(c.r2), n))
                } else {
                    w[i].call(c.r2, "ident_m_via", &(CodecAddress(c.r1),))
                }
            }
            41 => {
                // gas forwarded to a callee, sometimes too little for it
                let gas = if rng.below(2) == 0 { u64::MAX } else { 20_000 };
                let (from, to) = if rng.below(2) == 0 {
                    (c.r1, c.r2)
                } else {
                    (c.r2, c.r1)
                };
                w[i].call(
                    from,
                    "call_burn",
                    &(CodecAddress(to), rng.next() % 3_000, gas),
                )
            }
            _ => w[i].call(c.counter, "increment", &(1u64,)),
        };
        txs.push(tx);
    }
    txs
}

fn resync(rigs: &[Rig], w: &mut [Wallet]) {
    for x in w.iter_mut() {
        x.nonce = rigs[0].net.state.nonce(&x.addr()).unwrap();
    }
}

fn run_seed(seed: u64, blocks: usize) -> Vec<Rig> {
    let mut w: Vec<Wallet> = (1..=WALLETS as u8).map(Wallet::new).collect();
    let refs: Vec<&Wallet> = w.iter().collect();
    let g = genesis_with(&refs, RICH, host_cfg());
    let mut rigs = rigs(&g);
    let c = setup(&mut rigs, &mut w);
    let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut chans = Chans::new();
    for b in 0..blocks {
        resync(&rigs, &mut w);
        let mode = usize::from(b % 4 != 0);
        let txs = random_block(&mut rng, &mut w, &c, &mut chans, mode);
        let kinds: Vec<bool> = txs
            .iter()
            .map(|t| matches!(t.body.kind, TxKind::Channel(_)))
            .collect();
        let rec = step(&mut rigs, txs);
        CALL_OK.fetch_add(
            rec.iter()
                .filter(|r| r.status == Status::Success && r.output == vec![3])
                .count(),
            std::sync::atomic::Ordering::Relaxed,
        );
        RELAYED.fetch_add(
            rec.iter().filter(|r| r.relayer.is_some()).count(),
            std::sync::atomic::Ordering::Relaxed,
        );
        for (r, is) in rec.iter().zip(kinds) {
            if is && r.status == Status::Success {
                CHANNEL_OK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        for r in &rigs {
            assert_eq!(
                r.net.state.total_supply().unwrap(),
                u128::from(TOTAL_SUPPLY),
                "{}",
                r.label
            );
        }
    }
    rigs
}

#[test]
fn parallel_equals_sequential_on_random_blocks() {
    let mut conflicts = 0;
    let mut fast = 0;
    let mut fell_back = 0;
    for seed in 1..=6 {
        let rigs = run_seed(seed, 20);
        for r in &rigs {
            if r.label == "parallel-4" {
                conflicts += r.seen.reexec;
                fast += r.seen.fast_txs;
                fell_back += r.seen.fell_back;
            }
        }
        assert_same_stores(rigs);
    }
    // The workloads really exercised the machinery.
    assert!(conflicts > 50, "re-executions: {conflicts}");
    assert!(fast > 100, "fast-path txs: {fast}");
    assert!(fell_back > 0, "pool-touching blocks must fall back");
    let relayed = RELAYED.load(std::sync::atomic::Ordering::Relaxed);
    assert!(relayed > 20, "relayer-paying txs: {relayed}");
    let reverted_callees = CALL_OK.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        reverted_callees > 5,
        "reverted nested calls: {reverted_callees}"
    );
    let ok = CHANNEL_OK.load(std::sync::atomic::Ordering::Relaxed);
    assert!(ok > 20, "successful channel txs: {ok}");
}

/// Everyone pays the same hot account and calls the same counter: maximal conflicts, and the
/// chain of dependencies must still resolve to the sequential result.
#[test]
fn fully_conflicting_block_matches_sequential() {
    let mut w: Vec<Wallet> = (1..=WALLETS as u8).map(Wallet::new).collect();
    let refs: Vec<&Wallet> = w.iter().collect();
    let g = genesis_with(&refs, RICH, host_cfg());
    let mut rigs = rigs(&g);
    let c = setup(&mut rigs, &mut w);
    resync(&rigs, &mut w);
    let hot = w[0].addr();
    let mut txs = Vec::new();
    for round in 0..8u64 {
        for x in w.iter_mut().skip(1) {
            txs.push(x.transfer(hot, 1_000 + round));
            txs.push(x.call(c.counter, "increment", &(1u64,)));
        }
    }
    let r = step(&mut rigs, txs);
    assert!(r.iter().all(|x| x.status == Status::Success));
    assert_same_stores(rigs);
}

#[test]
fn parallel_run_is_repeatable() {
    // Same blocks, three independent runs with different thread timing: same roots.
    let roots: Vec<_> = (0..3)
        .map(|_| {
            let rigs = run_seed(77, 10);
            rigs.last().unwrap().net.state.root().unwrap()
        })
        .collect();
    assert!(roots.windows(2).all(|p| p[0] == p[1]));
}

/// A genuine state error in a block is reported like the sequential executor does, and the
/// state stays untouched in both.
#[test]
fn block_errors_match() {
    let mut w: Vec<Wallet> = (1..=3u8).map(Wallet::new).collect();
    let refs: Vec<&Wallet> = w.iter().collect();
    let g = genesis_with(&refs, RICH, host_cfg());
    let mut rigs = rigs(&g);
    let (a1, a2) = (w[1].addr(), w[2].addr());
    let t1 = w[0].transfer(a1, 5);
    let t2 = w[1].transfer(a2, 5);
    let mut blk = rigs[0].net.block_for(vec![t1, t2]);
    blk.header.state_root = [9; 32]; // wrong root
    let errs: Vec<_> = rigs
        .iter_mut()
        .map(|r| r.net.ex.execute_block(&mut r.net.state, &blk).unwrap_err())
        .collect();
    assert!(errs.windows(2).all(|p| p[0] == p[1]));
    let roots: Vec<_> = rigs.iter().map(|r| r.net.state.root().unwrap()).collect();
    assert!(roots.windows(2).all(|p| p[0] == p[1]));
}
