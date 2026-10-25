//! Native DEX (`dex.rs`): batch auctions with a uniform clearing price.
//!
//! Security-sensitive: escrow, rounding, supply conservation, order-independence (the
//! front-running property) and sequential / parallel equivalence are all tested here; the
//! clearing function itself is checked against a brute-force reference in the unit tests of
//! `src/dex.rs`.

mod support;
use btb_chain::dex::{pair_id, DexError, Outcome, Side, MAX_OPEN_ORDERS};
use btb_chain::*;
use btb_codec::Address as CodecAddress;
use btb_runtime::{KvStore, MemKv};
use support::*;

type MemNet = Net<MemKv>;
const RICH: u64 = 100 * UNITS_PER_BTB;
const FAUCET: u128 = 1_000_000;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn place(
    w: &mut Wallet,
    sell: Asset,
    buy: Asset,
    sell_amount: u64,
    buy_amount: u64,
    batches: u32,
) -> Transaction {
    w.sign(TxKind::Dex(DexOp::Place {
        sell,
        buy,
        sell_amount,
        buy_amount,
        batches,
    }))
}

fn cancel(w: &mut Wallet, order: [u8; 32]) -> Transaction {
    w.sign(TxKind::Dex(DexOp::Cancel { order }))
}

fn revert(r: &Receipt) -> DexError {
    match &r.status {
        Status::Reverted(Revert::Dex(e)) => *e,
        s => panic!("expected a DEX revert, got {s:?}"),
    }
}

struct World {
    net: MemNet,
    wallets: Vec<Wallet>,
    /// Two user tokens, every trader holds `FAUCET` of each.
    gold: Asset,
    silver: Asset,
    tokens: Vec<[u8; 32]>,
    launcher: Address,
}

impl World {
    fn new(traders: usize, cfg: impl FnOnce(&mut ChainConfig)) -> World {
        Self::with_exec(traders, cfg, Executor::new())
    }

    fn with_exec(traders: usize, cfg: impl FnOnce(&mut ChainConfig), ex: Executor) -> World {
        let mut wallets: Vec<Wallet> = (1..=traders as u8 + 1).map(Wallet::new).collect();
        let mut config = ChainConfig::new(CHAIN_ID);
        cfg(&mut config);
        let g = {
            let refs: Vec<&Wallet> = wallets.iter().collect();
            genesis_with(&refs, RICH, config)
        };
        let mut net = Net::new(MemKv::new(), &g);
        net.ex = ex;
        // wallet 0 is the issuer: deploys the launcher and creates two tokens.
        let launcher = deployed(
            &net.run(vec![
                wallets[0].deploy(elf("launcher"), manifest("launcher"))
            ])[0],
        );
        let r = net.run(vec![wallets[0].transfer(launcher, UNITS_PER_BTB)]);
        ok(&r[0]);
        let mut tokens = vec![];
        for (name, sym) in [("Gold", "GLD"), ("Silver", "SLV")] {
            let r = net.run(vec![wallets[0].call(
                launcher,
                "launch",
                &(
                    name.to_string(),
                    sym.to_string(),
                    9u8,
                    FAUCET * 100,
                    FAUCET,
                    false,
                ),
            )]);
            ok(&r[0]);
            tokens.push(<[u8; 32]>::try_from(r[0].output.as_slice()).unwrap());
        }
        for t in &tokens {
            let txs: Vec<_> = wallets[1..]
                .iter_mut()
                .map(|w| w.call(launcher, "claim", &CodecAddress(*t)))
                .collect();
            for r in net.run(txs) {
                ok(&r);
            }
        }
        World {
            net,
            wallets,
            gold: Asset::Token(tokens[0]),
            silver: Asset::Token(tokens[1]),
            tokens,
            launcher,
        }
    }

    fn tok(&self, t: &Asset, who: &Address) -> u128 {
        match t {
            Asset::Btb => u128::from(self.net.bal(who)),
            Asset::Token(id) => token_balance(self.net.state.store(), id, who),
        }
    }

    /// Everything a test may compare: BTB and token balances of every wallet and the vault.
    fn snapshot(&self) -> Vec<u128> {
        let mut out = vec![];
        let mut who: Vec<Address> = self.wallets.iter().map(|w| w.addr()).collect();
        who.push(dex_vault());
        who.push(dex_fees());
        who.push(validator_pool());
        for a in &who {
            out.push(self.tok(&Asset::Btb, a));
            for t in &self.tokens {
                out.push(self.tok(&Asset::Token(*t), a));
            }
        }
        out
    }

    /// Supply conservation: BTB is the fixed supply, every token's holders sum to its supply,
    /// and the vault holds exactly the open escrows.
    fn check_conservation(&self) {
        assert_eq!(
            self.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY)
        );
        for t in &self.tokens {
            let sum = token_total(self.net.state.store(), t);
            assert_eq!(sum, FAUCET * 100, "token supply changed");
        }
        let mut escrow = std::collections::BTreeMap::<Asset, u128>::new();
        for p in self.net.state.dex_pairs().unwrap() {
            for o in self.net.state.dex_book(&p.id).unwrap() {
                let sold = if o.side == Side::Ask { o.base } else { o.quote };
                *escrow.entry(sold).or_default() += u128::from(o.escrow);
                assert!(o.base_rem > 0 && o.escrow > 0 && o.batches_left > 0);
            }
        }
        for a in [Asset::Btb, self.gold, self.silver] {
            let held = self.net.state.dex_vault_balance(&a).unwrap();
            assert_eq!(
                held,
                escrow.get(&a).copied().unwrap_or(0),
                "vault != escrow for {a:?}"
            );
        }
    }
}

fn token_balance(kv: &MemKv, id: &[u8; 32], who: &Address) -> u128 {
    kv.get(&btb_runtime::layout::token_balance_key(id, who))
        .unwrap()
        .map_or(0, |b| u128::from_le_bytes(b.try_into().unwrap()))
}

fn token_total(kv: &MemKv, id: &[u8; 32]) -> u128 {
    let p = btb_runtime::layout::token_balance_prefix(id);
    let end = btb_runtime::layout::prefix_successor(&p);
    kv.scan(&p, end.as_deref(), false, usize::MAX)
        .unwrap()
        .iter()
        .map(|(_, v)| u128::from_le_bytes(v.clone().try_into().unwrap()))
        .sum()
}

// A hand-checked batch ---------------------------------------------------------------

#[test]
fn hand_checked_batch_btb_gold() {
    let mut w = World::new(4, |_| {});
    let gold = w.gold;
    let [_, mut alice, mut bob, mut carol, mut dave] =
        <[Wallet; 5]>::try_from(std::mem::take(&mut w.wallets))
            .ok()
            .unwrap();
    // BTB < token, so BTB is the base, price = GLD per BTB unit.
    let txs = vec![
        place(&mut alice, Asset::Btb, gold, 100, 1_000, 1), // ask: >= 10
        place(&mut bob, Asset::Btb, gold, 100, 1_500, 1),   // ask: >= 15
        place(&mut carol, gold, Asset::Btb, 1_600, 100, 1), // bid: <= 16
        place(&mut dave, gold, Asset::Btb, 1_200, 100, 1),  // bid: <= 12
    ];
    let before: Vec<u128> = [&alice, &bob, &carol, &dave]
        .iter()
        .map(|x| w.tok(&gold, &x.addr()))
        .collect();
    let btb_before: Vec<u128> = [&alice, &bob, &carol, &dave]
        .iter()
        .map(|x| w.tok(&Asset::Btb, &x.addr()))
        .collect();
    let r = w.net.run(txs);
    r.iter().for_each(ok);
    // Volume is 100 at every candidate; imbalance ties too; the lowest price (10) wins.
    let pair = pair_id(&Asset::Btb, &gold);
    let c = w
        .net
        .state
        .dex_last_price(&pair)
        .unwrap()
        .expect("a clearing");
    assert_eq!(
        (c.n, c.d, c.base_volume, c.quote_volume, c.orders),
        (10, 1, 100, 1_000, 3)
    );
    assert_eq!(c.height, w.net.state.head().unwrap().height);
    let fee = w.net.state.config().dex_fee_bps;
    assert_eq!(fee, 10);
    let fees: Vec<u64> = r.iter().map(|x| x.fee).collect();
    // Alice (ask, fully filled): 100 BTB out, 1000 GLD gross, 1 GLD fee.
    assert_eq!(w.tok(&gold, &alice.addr()), before[0] + 999);
    assert_eq!(
        w.tok(&Asset::Btb, &alice.addr()),
        (btb_before[0] - 100 - u128::from(fees[0])) as u128
    );
    // Bob's limit 15 > 10: not eligible, refunded after his single batch.
    assert_eq!(
        w.tok(&Asset::Btb, &bob.addr()),
        btb_before[1] - u128::from(fees[1])
    );
    // Carol and Dave are rationed 50 / 50 of the 100 on the short side, each pays 500 GLD.
    assert_eq!(w.tok(&gold, &carol.addr()), before[2] - 500);
    assert_eq!(w.tok(&gold, &dave.addr()), before[3] - 500);
    assert_eq!(
        w.tok(&Asset::Btb, &carol.addr()),
        btb_before[2] + 50 - u128::from(fees[2])
    );
    assert_eq!(
        w.tok(&Asset::Btb, &dave.addr()),
        btb_before[3] + 50 - u128::from(fees[3])
    );
    // The fee (1 GLD) sits in the fee account; nothing is left in the vault.
    assert_eq!(w.net.state.dex_fee_balance(&w.tokens[0]).unwrap(), 1);
    assert_eq!(w.net.state.dex_vault_balance(&Asset::Btb).unwrap(), 0);
    assert_eq!(w.net.state.dex_vault_balance(&gold).unwrap(), 0);
    // Results: Alice filled, Carol and Dave expired with 50 filled.
    let res = w
        .net
        .state
        .dex_result(&alice.addr(), &bytes32(&r[0].output))
        .unwrap()
        .unwrap();
    assert_eq!(
        (res.outcome, res.filled, res.quote_traded),
        (Outcome::Filled, 100, 1_000)
    );
    let res = w
        .net
        .state
        .dex_result(&carol.addr(), &bytes32(&r[2].output))
        .unwrap()
        .unwrap();
    assert_eq!(
        (res.outcome, res.filled, res.quote_traded),
        (Outcome::Expired, 50, 500)
    );
    assert_eq!(
        w.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
}

fn bytes32(b: &[u8]) -> [u8; 32] {
    b.try_into().unwrap()
}

#[test]
fn btb_fee_goes_to_the_validator_pool() {
    let mut w = World::new(2, |c| c.dex_fee_bps = 1_000); // 10%
    let gold = w.gold;
    let (mut a, mut b) = (w.wallets.remove(1), w.wallets.remove(1));
    let pool0 = w.net.state.validator_pool_balance().unwrap();
    let r = w.net.run(vec![
        place(&mut a, Asset::Btb, gold, 1_000, 5_000, 1), // ask 1000 BTB units >= 5
        place(&mut b, gold, Asset::Btb, 6_000, 1_000, 1), // bid up to 6
    ]);
    r.iter().for_each(ok);
    let c = w
        .net
        .state
        .dex_last_price(&pair_id(&Asset::Btb, &gold))
        .unwrap()
        .unwrap();
    assert_eq!((c.n, c.d, c.base_volume), (5, 1, 1_000)); // lowest tied price
                                                          // The bid receives 1000 BTB units less 10% = 900; the 100 BTB fee goes to the validator pool
                                                          // next to the validator share of the two transaction fees.
    let validator_share: u64 = r
        .iter()
        .map(|x| x.fee - x.fee * RELAY_SHARE_BPS / 10_000)
        .sum();
    let pool1 = w.net.state.validator_pool_balance().unwrap();
    assert_eq!(pool1 - pool0, 100 + validator_share);
    assert_eq!(w.tok(&gold, &a.addr()), FAUCET + 4_500); // 5000 gross less 10%
    assert_eq!(w.tok(&gold, &b.addr()), FAUCET - 5_000);
    assert_eq!(w.net.state.dex_fee_balance(&w.tokens[0]).unwrap(), 500);
    assert_eq!(
        w.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
}

// Pro rata, rolling orders, windows ---------------------------------------------------

#[test]
fn partial_fills_are_pro_rata_with_largest_remainder() {
    let mut w = World::new(4, |c| c.dex_fee_bps = 0);
    let gold = w.gold;
    let [_, mut ask, mut b1, mut b2, mut b3] =
        <[Wallet; 5]>::try_from(std::mem::take(&mut w.wallets))
            .ok()
            .unwrap();
    let r = w.net.run(vec![
        place(&mut b1, gold, Asset::Btb, 100, 10, 1),
        place(&mut b2, gold, Asset::Btb, 200, 20, 1),
        place(&mut b3, gold, Asset::Btb, 300, 30, 1),
        place(&mut ask, Asset::Btb, gold, 25, 250, 1),
    ]);
    r.iter().for_each(ok);
    let filled = |w: &World, who: &Wallet, r: &Receipt| {
        w.net
            .state
            .dex_result(&who.addr(), &bytes32(&r.output))
            .unwrap()
            .unwrap()
            .filled
    };
    // 25 over weights 10:20:30 -> floors 4, 8, 12 (24), the spare unit goes to the largest
    // remainder (30/60 -> the third bid).
    assert_eq!(filled(&w, &b1, &r[0]), 4);
    assert_eq!(filled(&w, &b2, &r[1]), 8);
    assert_eq!(filled(&w, &b3, &r[2]), 13);
    assert_eq!(filled(&w, &ask, &r[3]), 25);
    let c = w
        .net
        .state
        .dex_last_price(&pair_id(&Asset::Btb, &gold))
        .unwrap()
        .unwrap();
    assert_eq!((c.n, c.d, c.base_volume, c.quote_volume), (10, 1, 25, 250));
    // Dust is zero here (price 10, whole units); everything unspent was refunded.
    assert_eq!(w.tok(&gold, &b3.addr()), FAUCET - 130);
    assert_eq!(w.net.state.dex_vault_balance(&gold).unwrap(), 0);
}

#[test]
fn rounding_dust_goes_to_the_sink_and_nothing_is_created() {
    // Price 1/3 quote per base: 7 base -> 7/3 = 2.33 quote. Bid pays ceil = 3, ask gets floor = 2.
    let mut w = World::new(2, |c| c.dex_fee_bps = 0);
    let gold = w.gold;
    let (mut ask, mut bid) = (w.wallets.remove(1), w.wallets.remove(1));
    let r = w.net.run(vec![
        place(&mut ask, Asset::Btb, gold, 7, 2, 1), // limit 2/7 <= 1/3
        place(&mut bid, gold, Asset::Btb, 10, 30, 1), // limit 1/3, buys up to 30 base
    ]);
    r.iter().for_each(ok);
    let c = w
        .net
        .state
        .dex_last_price(&pair_id(&Asset::Btb, &gold))
        .unwrap()
        .unwrap();
    // Candidates: 2/7 (S=7, D=0 since 2/7 < 1/3 means the bid is eligible: D=30) and 1/3.
    assert_eq!((c.base_volume, c.n * 7 / c.d >= 2), (7, true));
    let paid = FAUCET - w.tok(&gold, &bid.addr());
    let got = w.tok(&gold, &ask.addr()) - FAUCET;
    assert!(paid >= got && paid - got <= 1, "paid {paid} got {got}");
    assert_eq!(
        w.net.state.dex_fee_balance(&w.tokens[0]).unwrap(),
        paid - got
    );
    w.wallets = vec![];
    assert_eq!(token_total(w.net.state.store(), &w.tokens[0]), FAUCET * 100);
    assert_eq!(
        w.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
}

#[test]
fn orders_roll_over_until_expiry_and_refund_the_rest() {
    let mut w = World::new(3, |c| c.dex_fee_bps = 0);
    let gold = w.gold;
    let (mut bid, mut ask1, mut ask2) = (
        w.wallets.remove(1),
        w.wallets.remove(1),
        w.wallets.remove(1),
    );
    let start = w.tok(&gold, &bid.addr());
    // Bid: up to 100 BTB units at <= 10 GLD, lives 3 batches.
    let r = w
        .net
        .run(vec![place(&mut bid, gold, Asset::Btb, 1_000, 100, 3)]);
    ok(&r[0]);
    let id = bytes32(&r[0].output);
    assert_eq!(w.net.state.dex_orders_of(&bid.addr()).unwrap().len(), 1);
    // Batch 2: an ask for 30 at 8 -> clears at 8; the bid pays 240 and keeps 70 base.
    let r2 = w
        .net
        .run(vec![place(&mut ask1, Asset::Btb, gold, 30, 240, 1)]);
    ok(&r2[0]);
    let o = &w.net.state.dex_orders_of(&bid.addr()).unwrap()[0];
    assert_eq!((o.base_rem, o.escrow, o.batches_left), (70, 760, 1));
    // The limit of the remainder was tightened to escrow/base_rem = 760/70 (a bid's escrow can
    // never be overdrawn by rounding).
    assert_eq!((o.n, o.d), (760, 70));
    // Batch 3: a second ask for 30 at 9. Last batch of the bid: it ends here.
    let r3 = w
        .net
        .run(vec![place(&mut ask2, Asset::Btb, gold, 30, 270, 1)]);
    ok(&r3[0]);
    assert!(w.net.state.dex_orders_of(&bid.addr()).unwrap().is_empty());
    let res = w.net.state.dex_result(&bid.addr(), &id).unwrap().unwrap();
    assert_eq!(
        (res.outcome, res.filled, res.quote_traded),
        (Outcome::Expired, 60, 240 + 270)
    );
    assert_eq!(w.tok(&gold, &bid.addr()), start - 510);
    assert_eq!(w.net.state.dex_vault_balance(&gold).unwrap(), 0);
    assert_eq!(
        w.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
}

#[test]
fn unmatched_orders_expire_and_are_refunded_exactly() {
    let mut w = World::new(1, |_| {});
    let (gold, mut a) = (w.gold, w.wallets.remove(1));
    let (btb0, gld0) = (w.tok(&Asset::Btb, &a.addr()), w.tok(&gold, &a.addr()));
    let r = w.net.run(vec![
        place(&mut a, Asset::Btb, gold, 5_000, 50_000, 3),
        place(&mut a, gold, Asset::Btb, 10, 10_000_000, 2), // bid on the same side? no: other pair side
    ]);
    ok(&r[0]);
    // The second order is on the other side of the same pair: wash-trade guard.
    assert_eq!(revert(&r[1]), DexError::SelfTrade);
    let fee0 = r[0].fee + r[1].fee;
    assert_eq!(
        w.tok(&Asset::Btb, &a.addr()),
        btb0 - 5_000 - u128::from(fee0)
    );
    assert_eq!(w.net.state.dex_vault_balance(&Asset::Btb).unwrap(), 5_000);
    // Batch 1 ran in the placing block; one more empty block is batch 2: still alive.
    w.net.empty_blocks(1);
    assert_eq!(
        w.net.state.dex_orders_of(&a.addr()).unwrap().len(),
        1,
        "alive for 3 batches"
    );
    w.net.empty_blocks(1);
    assert!(w.net.state.dex_orders_of(&a.addr()).unwrap().is_empty());
    assert_eq!(w.tok(&Asset::Btb, &a.addr()), btb0 - u128::from(fee0));
    assert_eq!(w.tok(&gold, &a.addr()), gld0);
    let res = w
        .net
        .state
        .dex_result(&a.addr(), &bytes32(&r[0].output))
        .unwrap()
        .unwrap();
    assert_eq!((res.outcome, res.filled), (Outcome::Expired, 0));
    assert_eq!(w.net.state.dex_vault_balance(&Asset::Btb).unwrap(), 0);
}

#[test]
fn batch_window_of_two_blocks_clears_orders_from_both_blocks_together() {
    let mut w = World::new(2, |c| c.dex_batch_blocks = 2);
    let gold = w.gold;
    let (mut ask, mut bid) = (w.wallets.remove(1), w.wallets.remove(1));
    while (w.net.state.head().unwrap().height + 1) % 2 != 1 {
        w.net.run(vec![]);
    }
    let r = w
        .net
        .run(vec![place(&mut ask, Asset::Btb, gold, 10, 100, 1)]); // odd block
    ok(&r[0]);
    assert_eq!(
        w.net.state.dex_orders_of(&ask.addr()).unwrap().len(),
        1,
        "not cleared mid-window"
    );
    let r = w
        .net
        .run(vec![place(&mut bid, gold, Asset::Btb, 100, 10, 1)]); // even block ends it
    ok(&r[0]);
    let c = w
        .net
        .state
        .dex_last_price(&pair_id(&Asset::Btb, &gold))
        .unwrap()
        .unwrap();
    assert_eq!((c.base_volume, c.height % 2), (10, 0));
    assert!(w.net.state.dex_orders_of(&ask.addr()).unwrap().is_empty());
    w.net.run(vec![]);
    assert_eq!(
        w.net
            .state
            .dex_clearings(&pair_id(&Asset::Btb, &gold), 10)
            .unwrap()
            .len(),
        1
    );
}

// Cancellation -----------------------------------------------------------------------

#[test]
fn cancel_refunds_and_only_the_owner_can_cancel() {
    let mut w = World::new(2, |_| {});
    let gold = w.gold;
    let (mut a, mut b) = (w.wallets.remove(1), w.wallets.remove(1));
    let (btb0, gld0) = (w.tok(&Asset::Btb, &a.addr()), w.tok(&gold, &a.addr()));
    let silver0 = w.tok(&w.silver, &a.addr());
    let r = w.net.run(vec![
        place(&mut a, Asset::Btb, gold, 7_000, 70_000, 100),
        place(&mut a, gold, gold_other(&w), 500, 500, 100),
    ]);
    ok(&r[0]);
    ok(&r[1]);
    let mut fees = u128::from(r[0].fee + r[1].fee);
    let (id0, id1) = (bytes32(&r[0].output), bytes32(&r[1].output));
    assert_eq!(w.net.state.dex_vault_balance(&Asset::Btb).unwrap(), 7_000);
    // Someone else cannot cancel it, nor an unknown id.
    let r = w
        .net
        .run(vec![cancel(&mut b, id0), cancel(&mut b, [9; 32])]);
    assert_eq!(revert(&r[0]), DexError::NoSuchOrder);
    assert_eq!(revert(&r[1]), DexError::NoSuchOrder);
    assert_eq!(w.net.state.dex_orders_of(&a.addr()).unwrap().len(), 2);
    assert!(
        r.iter().all(|x| x.fee > 0),
        "a refused cancel still pays gas"
    );
    // The owner can; a second cancel fails; the result is recorded.
    let r = w.net.run(vec![
        cancel(&mut a, id0),
        cancel(&mut a, id0),
        cancel(&mut a, id1),
    ]);
    ok(&r[0]);
    assert_eq!(revert(&r[1]), DexError::NoSuchOrder);
    ok(&r[2]);
    fees += r.iter().map(|x| u128::from(x.fee)).sum::<u128>();
    assert!(w.net.state.dex_orders_of(&a.addr()).unwrap().is_empty());
    // Only gas was spent: the escrow came back to the unit.
    assert_eq!(w.tok(&Asset::Btb, &a.addr()), btb0 - fees);
    assert_eq!(w.tok(&gold, &a.addr()), gld0);
    assert_eq!(w.tok(&w.silver, &a.addr()), silver0);
    assert_eq!(w.net.state.dex_vault_balance(&Asset::Btb).unwrap(), 0);
    let res = w.net.state.dex_result(&a.addr(), &id0).unwrap().unwrap();
    assert_eq!((res.outcome, res.filled), (Outcome::Cancelled, 0));
    w.wallets = vec![];
    w.check_conservation();
}

fn gold_other(w: &World) -> Asset {
    w.silver
}

#[test]
fn cancel_in_the_same_block_prevents_the_fill_and_a_cleared_order_cannot_be_cancelled() {
    let mut w = World::new(2, |_| {});
    let gold = w.gold;
    let (mut ask, mut bid) = (w.wallets.remove(1), w.wallets.remove(1));
    let rb = w
        .net
        .run(vec![place(&mut bid, gold, Asset::Btb, 100, 10, 1)]);
    // The bid already expired (1 batch); cancelling it is too late.
    let r = w.net.run(vec![cancel(&mut bid, bytes32(&rb[0].output))]);
    assert_eq!(revert(&r[0]), DexError::NoSuchOrder);
    // Place and cancel an ask in one block, a bid next to it: no trade.
    let ra = place(&mut ask, Asset::Btb, gold, 10, 100, 1);
    let id = btb_chain::dex::order_id(&ask.addr(), ra.body.nonce);
    let rc = cancel(&mut ask, id);
    let rb = place(&mut bid, gold, Asset::Btb, 100, 10, 1);
    let r = w.net.run(vec![ra, rb, rc]);
    r.iter().for_each(ok);
    assert!(w
        .net
        .state
        .dex_last_price(&pair_id(&Asset::Btb, &gold))
        .unwrap()
        .is_none());
}

// Malicious and degenerate input ---------------------------------------------------

#[test]
fn malformed_and_hostile_orders_are_refused_without_effect() {
    let mut w = World::new(2, |_| {});
    let (gold, silver) = (w.gold, w.silver);
    let (mut a, mut b) = (w.wallets.remove(1), w.wallets.remove(1));
    let fake = Asset::Token([0xAB; 32]);
    let snap = w.snapshot_of(&[a.addr(), b.addr()]);
    let txs = vec![
        place(&mut a, Asset::Btb, gold, 0, 5, 1),       // zero sell
        place(&mut a, Asset::Btb, gold, 5, 0, 1),       // zero buy
        place(&mut a, gold, gold, 5, 5, 1),             // same asset
        place(&mut a, Asset::Btb, Asset::Btb, 5, 5, 1), // same asset (BTB)
        place(&mut a, Asset::Btb, gold, 5, 5, 0),       // zero lifetime
        place(&mut a, Asset::Btb, gold, 5, 5, (1 << 20) + 1), // too long
        place(&mut a, fake, gold, 5, 5, 1),             // token that does not exist
        place(&mut a, gold, fake, 5, 5, 1),
        place(&mut a, gold, silver, (FAUCET + 1) as u64, 5, 1), // more than the balance
    ];
    let r = w.net.run(txs);
    let want = [
        DexError::ZeroAmount,
        DexError::ZeroAmount,
        DexError::SameAsset,
        DexError::SameAsset,
        DexError::BadBatches,
        DexError::BadBatches,
        DexError::NoSuchToken,
        DexError::NoSuchToken,
        DexError::InsufficientBalance,
    ];
    for (rc, e) in r.iter().zip(want) {
        assert_eq!(revert(rc), e);
    }
    assert!(
        w.net.state.dex_pairs().unwrap().is_empty(),
        "no order, no pair"
    );
    let after = w.snapshot_of(&[a.addr(), b.addr()]);
    // Only gas was spent.
    let gas: u128 = r.iter().map(|x| u128::from(x.fee)).sum();
    assert_eq!(snap[0] - after[0], gas);
    assert_eq!(snap[1..], after[1..]);
    // Not enough BTB for escrow + fee is invalid (never enters a block's effects).
    let r = w.net.run(vec![place(&mut b, Asset::Btb, gold, RICH, 1, 1)]);
    assert!(
        matches!(
            r[0].status,
            Status::Invalid(Invalid::InsufficientBalance { .. })
        ),
        "{:?}",
        r[0].status
    );
    // A forged signature is invalid.
    let mut forged = place(&mut b, Asset::Btb, gold, 5, 5, 1);
    forged.signature[0] ^= 1;
    let r = w.net.run(vec![forged]);
    assert!(matches!(
        r[0].status,
        Status::Invalid(Invalid::BadSignature)
    ));
    w.wallets = vec![];
    w.check_conservation();
}

impl World {
    fn snapshot_of(&self, who: &[Address]) -> Vec<u128> {
        let mut out = vec![];
        for a in who {
            out.push(self.tok(&Asset::Btb, a));
            for t in &self.tokens {
                out.push(self.tok(&Asset::Token(*t), a));
            }
        }
        out
    }
}

#[test]
fn self_trade_and_open_order_limit() {
    let mut w = World::new(2, |_| {});
    let (gold, silver) = (w.gold, w.silver);
    let (mut a, mut b) = (w.wallets.remove(1), w.wallets.remove(1));
    let r = w.net.run(vec![
        place(&mut a, gold, silver, 10, 10, 50),
        place(&mut a, silver, gold, 10, 10, 50), // other side of the same pair: refused
        place(&mut a, gold, silver, 10, 12, 50), // same side again is fine
        place(&mut b, silver, gold, 10, 10, 50), // a different account may take the other side
    ]);
    assert_eq!(r.iter().filter(|x| x.status == Status::Success).count(), 3);
    assert_eq!(revert(&r[1]), DexError::SelfTrade);
    // b's order crosses a's resting orders: they trade at once (different owners).
    assert!(
        w.net
            .state
            .dex_last_price(&pair_id(&gold, &silver))
            .unwrap()
            .is_some()
            || {
                let (x, y) = btb_chain::dex::pair_of(&gold, &silver);
                w.net
                    .state
                    .dex_last_price(&pair_id(&x, &y))
                    .unwrap()
                    .is_some()
            }
    );
    // Fill up to the cap with orders that never trade.
    let mut txs = vec![];
    for _ in 0..MAX_OPEN_ORDERS + 2 {
        txs.push(place(&mut a, gold, silver, 1, 1_000_000, 1000));
    }
    let r = w.net.run(txs);
    let open = w.net.state.dex_orders_of(&a.addr()).unwrap().len();
    assert_eq!(open, MAX_OPEN_ORDERS);
    assert!(r.iter().any(|x| matches!(
        &x.status,
        Status::Reverted(Revert::Dex(DexError::TooManyOrders))
    )));
    w.wallets = vec![];
    w.check_conservation();
}

#[test]
fn extreme_amounts_and_prices_do_not_overflow_or_create_tokens() {
    let mut w = World::new(2, |c| c.dex_fee_bps = 1_000);
    let mut issuer = w.wallets.remove(0);
    let (mut a, mut b) = (w.wallets.remove(0), w.wallets.remove(0));
    // Two huge tokens, so u64::MAX escrows are possible.
    let mut ids = vec![];
    for (n, s) in [("Big", "BIG"), ("Huge", "HGE")] {
        let r = w.net.run(vec![issuer.call(
            w.launcher,
            "launch",
            &(
                n.to_string(),
                s.to_string(),
                0u8,
                1u128 << 100,
                1u128 << 70,
                false,
            ),
        )]);
        ok(&r[0]);
        ids.push(bytes32(&r[0].output));
    }
    for t in &ids {
        let txs = vec![
            a.call(w.launcher, "claim", &CodecAddress(*t)),
            b.call(w.launcher, "claim", &CodecAddress(*t)),
        ];
        w.net.run(txs).iter().for_each(ok);
    }
    let (x, y) = (Asset::Token(ids[0]), Asset::Token(ids[1]));
    let m = u64::MAX;
    let r = w.net.run(vec![
        place(&mut a, x, y, m, m, 2),
        place(&mut b, y, x, m, 1, 2), // pays up to u64::MAX of y for a single unit of x
    ]);
    r.iter().for_each(ok);
    // Next, absurd prices both ways on the same pair with fresh accounts' leftovers.
    let r = w.net.run(vec![
        place(&mut a, x, y, 1, m, 3),
        place(&mut b, y, x, m, m, 3),
    ]);
    // `a` already has an ask on this pair and b a bid: same sides, so both are accepted.
    r.iter().for_each(ok);
    w.net.empty_blocks(5);
    for t in &ids {
        assert_eq!(
            token_total(w.net.state.store(), t),
            1u128 << 100,
            "supply of an extreme token changed"
        );
    }
    assert_eq!(
        w.net.state.total_supply().unwrap(),
        u128::from(TOTAL_SUPPLY)
    );
    assert!(w.net.state.dex_orders_of(&a.addr()).unwrap().is_empty());
    assert_eq!(w.net.state.dex_vault_balance(&x).unwrap(), 0);
    assert_eq!(w.net.state.dex_vault_balance(&y).unwrap(), 0);
    // A burst of tiny prices and huge volumes through the pure clearing code path.
    let r = w.net.run(vec![
        place(&mut a, x, y, m, 1, 1),
        place(&mut b, y, x, 1, m, 1),
    ]);
    r.iter().for_each(ok);
    w.net.empty_blocks(1);
    for t in &ids {
        assert_eq!(token_total(w.net.state.store(), t), 1u128 << 100);
    }
}

// Front-running: order within a batch is irrelevant ---------------------------------------

fn random_orders(w: &mut World, rng: &mut Lcg, traders: usize, per: usize) -> Vec<Transaction> {
    let assets = [Asset::Btb, w.gold, w.silver];
    let mut txs = vec![];
    for _ in 0..per {
        for t in 1..=traders {
            if rng.below(4) == 0 {
                continue;
            }
            let (si, bi) = {
                let s = rng.below(3) as usize;
                (s, (s + 1 + rng.below(2) as usize) % 3)
            };
            let price = 1 + rng.below(20);
            let amt = 10 + rng.below(500);
            // sell `amt` units for `amt * price / 8` (varied limit prices on both sides)
            let sell_amount = amt;
            let buy_amount = (amt * price / 8).max(1);
            let batches = 1 + rng.below(3) as u32;
            txs.push(place(
                &mut w.wallets[t],
                assets[si],
                assets[bi],
                sell_amount,
                buy_amount,
                batches,
            ));
        }
    }
    txs
}

fn observable(w: &World) -> Vec<String> {
    let mut v = vec![format!("{:?}", w.snapshot())];
    for p in w.net.state.dex_pairs().unwrap() {
        v.push(format!("{:?}", w.net.state.dex_book(&p.id).unwrap()));
        v.push(format!(
            "{:?}",
            w.net.state.dex_clearings(&p.id, 100).unwrap()
        ));
    }
    for wl in &w.wallets {
        v.push(format!(
            "{:?}",
            w.net.state.dex_results_of(&wl.addr(), 1000).unwrap()
        ));
    }
    v
}

#[test]
fn reordering_the_orders_of_a_batch_changes_nobodys_outcome() {
    let traders = 9;
    for seed in 0..6u64 {
        let mut rng = Lcg(seed + 100);
        let mut base = World::new(traders, |_| {});
        let txs = random_orders(&mut base, &mut rng, traders, 3);
        assert!(txs.len() > 10);
        // Reference: the natural order.
        let mut reference = World::new(traders, |_| {});
        let rc = reference.net.run(txs.clone());
        let trades = reference
            .net
            .state
            .dex_pairs()
            .unwrap()
            .iter()
            .any(|p| reference.net.state.dex_last_price(&p.id).unwrap().is_some());
        let want = observable(&reference);
        for perm in 0..12 {
            let mut t2 = txs.clone();
            // Any permutation that keeps each sender's own nonce order (a producer can do no more).
            let mut r2 = Lcg(seed * 1000 + perm);
            for i in (1..t2.len()).rev() {
                t2.swap(i, r2.below(i as u64 + 1) as usize);
            }
            t2.sort_by_key(|t| (t.sender(), t.body.nonce)); // restore per-sender order ...
            let mut t3 = vec![];
            let mut buckets: std::collections::BTreeMap<Address, Vec<Transaction>> =
                Default::default();
            for t in t2 {
                buckets.entry(t.sender()).or_default().push(t);
            }
            let mut order: Vec<Address> = buckets.keys().copied().collect();
            for i in (1..order.len()).rev() {
                order.swap(i, r2.below(i as u64 + 1) as usize);
            }
            // ... then interleave senders randomly
            while t3.len() < txs.len() {
                let k = order[r2.below(order.len() as u64) as usize];
                if !buckets[&k].is_empty() {
                    t3.push(buckets.get_mut(&k).unwrap().remove(0));
                }
            }
            let mut w2 = World::new(traders, |_| {});
            let rc2 = w2.net.run(t3.clone());
            assert_eq!(observable(&w2), want, "seed {seed} perm {perm}");
            // Same fees, and every transaction has the same status wherever it landed.
            let by_id = |rs: &[Receipt]| {
                let mut m: Vec<_> = rs.iter().map(|r| (r.id, r.status.clone(), r.fee)).collect();
                m.sort_by_key(|x| x.0);
                m
            };
            assert_eq!(by_id(&rc), by_id(&rc2));
        }
        assert!(trades || seed > 0, "the generator should produce trades");
    }
}

#[test]
fn an_attacker_gains_nothing_by_being_first_or_last() {
    // A victim places a big bid; an attacker (a producer inserting its own order) places an
    // ask just before or after it. The attacker's outcome is the same either way.
    let outcome = |attacker_first: bool| {
        let mut w = World::new(3, |_| {});
        let gold = w.gold;
        let (mut attacker, mut victim, mut maker) = (
            w.wallets.remove(1),
            w.wallets.remove(1),
            w.wallets.remove(1),
        );
        let a = place(&mut attacker, Asset::Btb, gold, 40, 300, 1);
        let v = place(&mut victim, gold, Asset::Btb, 2_000, 100, 1);
        let m = place(&mut maker, Asset::Btb, gold, 100, 900, 1);
        let txs = if attacker_first {
            vec![a, v, m]
        } else {
            vec![m, v, a]
        };
        w.net.run(txs).iter().for_each(ok);
        (
            w.tok(&gold, &attacker.addr()),
            w.tok(&Asset::Btb, &attacker.addr()),
            w.tok(&gold, &victim.addr()),
            w.tok(&Asset::Btb, &victim.addr()),
            w.tok(&gold, &maker.addr()),
        )
    };
    assert_eq!(outcome(true), outcome(false));
}

#[test]
fn every_fill_in_a_batch_is_at_the_one_clearing_price() {
    let mut w = World::new(10, |c| c.dex_fee_bps = 0);
    let mut rng = Lcg(5);
    let gold = w.gold;
    let mut checked = 0;
    for _ in 0..25 {
        let mut txs = vec![];
        for t in 1..=10 {
            if rng.below(3) == 0 {
                continue;
            }
            let ask = rng.below(2) == 0;
            let amt = 5 + rng.below(300);
            let price = 3 + rng.below(8);
            txs.push(if ask {
                place(
                    &mut w.wallets[t],
                    Asset::Btb,
                    gold,
                    amt,
                    amt * price + rng.below(3),
                    1,
                )
            } else {
                place(
                    &mut w.wallets[t],
                    gold,
                    Asset::Btb,
                    amt * price + rng.below(5),
                    amt,
                    1,
                )
            });
        }
        let r = w.net.run(txs.clone());
        let h = w.net.state.head().unwrap().height;
        let pair = pair_id(&Asset::Btb, &gold);
        let c = w.net.state.dex_clearings(&pair, 1).unwrap();
        let Some(c) = c.first().filter(|c| c.height == h) else {
            continue;
        };
        for (t, rc) in txs.iter().zip(&r) {
            if rc.status != Status::Success {
                continue;
            }
            let res = w
                .net
                .state
                .dex_result(&t.sender(), &bytes32(&rc.output))
                .unwrap()
                .unwrap();
            if res.filled == 0 {
                continue;
            }
            checked += 1;
            let v = res.filled * u128::from(c.n);
            let (fl, ce) = (v / u128::from(c.d), v.div_ceil(u128::from(c.d)));
            match res.side {
                Side::Ask => assert_eq!(res.quote_traded, fl),
                Side::Bid => assert_eq!(res.quote_traded, ce),
            }
        }
        w.check_conservation_loose();
    }
    assert!(checked > 20, "too few fills checked ({checked})");
}

impl World {
    fn check_conservation_loose(&self) {
        assert_eq!(
            self.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY)
        );
        for t in &self.tokens {
            assert_eq!(token_total(self.net.state.store(), t), FAUCET * 100);
        }
    }
}

#[test]
fn random_traffic_conserves_every_asset_and_empties_the_vault() {
    for (seed, batch) in [(1u64, 1u64), (2, 3), (3, 2)] {
        let traders = 8;
        let mut w = World::new(traders, |c| {
            c.dex_batch_blocks = batch;
            c.dex_fee_bps = 25;
        });
        let mut rng = Lcg(seed * 77);
        let mut open: Vec<(usize, [u8; 32])> = vec![];
        for _ in 0..40 {
            let mut txs = random_orders(&mut w, &mut rng, traders, 1);
            // sprinkle cancels of known ids (some stale)
            for _ in 0..2 {
                if let Some(&(who, id)) = open.get(rng.below(open.len() as u64 + 1) as usize) {
                    txs.push(cancel(&mut w.wallets[who], id));
                }
            }
            let senders: Vec<_> = txs.iter().map(|t| t.sender()).collect();
            let r = w.net.run(txs);
            for (s, rc) in senders.iter().zip(&r) {
                if rc.status == Status::Success && rc.output.len() == 32 {
                    let who = w.wallets.iter().position(|x| x.addr() == *s).unwrap();
                    open.push((who, bytes32(&rc.output)));
                }
            }
            w.check_conservation();
        }
        w.net.empty_blocks(4 * batch);
        w.check_conservation();
        // Every order closed: nothing escrowed, nothing owed.
        for a in [Asset::Btb, w.gold, w.silver] {
            assert_eq!(w.net.state.dex_vault_balance(&a).unwrap(), 0, "seed {seed}");
        }
        assert!(w
            .wallets
            .iter()
            .all(|x| w.net.state.dex_orders_of(&x.addr()).unwrap().is_empty()));
        // Fee accounts only ever grow from fills; with a fee > 0 and trades there is some.
        let total_fees: u128 = w
            .tokens
            .iter()
            .map(|t| w.net.state.dex_fee_balance(t).unwrap())
            .sum();
        let traded = w
            .net
            .state
            .dex_pairs()
            .unwrap()
            .iter()
            .any(|p| w.net.state.dex_last_price(&p.id).unwrap().is_some());
        assert!(traded, "seed {seed} produced no trade");
        let _ = total_fees;
    }
}

// Determinism and parallel equivalence --------------------------------------------------

fn run_script(ex: Executor) -> Vec<(Vec<Receipt>, [u8; 32])> {
    let mut w = World::with_exec(10, |_| {}, ex);
    let mut rng = Lcg(2024);
    let mut out = vec![];
    for _ in 0..15 {
        let mut txs = random_orders(&mut w, &mut rng, 10, 2);
        // plain transfers and a hot account in the same blocks
        let a = w.wallets[1].addr();
        txs.push(w.wallets[2].transfer(a, 1_000));
        txs.push(w.wallets[3].transfer(dex_vault(), 77)); // a gift to the vault is dust, never spendable
        let b = w.net.block_for(txs.clone());
        let (receipts, root) = w.net.ex.execute_block(&mut w.net.state, &b).unwrap();
        out.push((receipts, root));
    }
    out
}

#[test]
fn dex_blocks_are_deterministic_and_parallel_equals_sequential() {
    let seq = run_script(Executor::sequential());
    let again = run_script(Executor::sequential());
    assert_eq!(seq, again, "sequential runs differ");
    for threads in [1, 2, 4, 8] {
        for fast in [true, false] {
            let mut ex = Executor::parallel(threads);
            ex.parallel_min_txs = 2;
            ex.fast_path = fast;
            let par = run_script(ex);
            assert_eq!(par, seq, "threads {threads} fast_path {fast}");
        }
    }
}

// Wire format and config ---------------------------------------------------------------

#[test]
fn dex_transactions_round_trip_and_unknown_sub_tags_are_rejected() {
    let mut w = Wallet::new(1);
    for t in [
        place(&mut w, Asset::Btb, Asset::Token([3; 32]), 5, 6, 7),
        place(
            &mut w,
            Asset::Token([9; 32]),
            Asset::Token([3; 32]),
            u64::MAX,
            1,
            u32::MAX,
        ),
        cancel(&mut w, [4; 32]),
    ] {
        let bytes = t.encode();
        assert_eq!(Transaction::decode(&bytes).unwrap(), t);
        assert!(t.verify_signature());
        assert_eq!(bytes[16], 10, "tag 10");
    }
    let t = cancel(&mut w, [4; 32]);
    let mut bytes = t.encode();
    bytes[17] = 7; // unknown sub-tag
    assert_eq!(Transaction::decode(&bytes), Err(TxError::BadTag(7)));
    let t = place(&mut w, Asset::Btb, Asset::Btb, 1, 1, 1);
    let mut bytes = t.encode();
    bytes[18] = 2; // unknown asset tag
    assert!(Transaction::decode(&bytes).is_err());
}

#[test]
fn dex_parameters_are_genesis_config() {
    let a = Wallet::new(1);
    for bad in [
        |c: &mut ChainConfig| c.dex_batch_blocks = 0,
        |c: &mut ChainConfig| c.dex_fee_bps = 10_001,
    ] {
        let mut cfg = ChainConfig::new(CHAIN_ID);
        bad(&mut cfg);
        assert!(ChainState::from_genesis(MemKv::new(), &genesis_with(&[&a], RICH, cfg)).is_err());
    }
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.dex_batch_blocks = 5;
    cfg.dex_fee_bps = 33;
    let g = genesis_with(&[&a], RICH, cfg);
    let st = ChainState::from_genesis(MemKv::new(), &g).unwrap();
    let st = ChainState::open(st.into_store()).unwrap();
    assert_eq!(
        (st.config().dex_batch_blocks, st.config().dex_fee_bps),
        (5, 33)
    );
    // The parameters are part of the genesis hash.
    let mut cfg2 = cfg;
    cfg2.dex_batch_blocks = 6;
    assert_ne!(g.hash(), genesis_with(&[&a], RICH, cfg2).hash());
}

#[test]
fn pair_identity_is_symmetric() {
    let (g, s) = (Asset::Token([1; 32]), Asset::Token([2; 32]));
    let (b1, q1) = btb_chain::dex::pair_of(&g, &s);
    let (b2, q2) = btb_chain::dex::pair_of(&s, &g);
    assert_eq!((b1, q1), (b2, q2));
    assert_eq!(pair_id(&b1, &q1), pair_id(&b2, &q2));
    assert_eq!(btb_chain::dex::pair_of(&g, &Asset::Btb).0, Asset::Btb);
}
