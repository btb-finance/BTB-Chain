//! Native DEX: limit-order batch auctions with one uniform clearing price per pair.
//!
//! Block producers choose the order of transactions, so on an order-book or AMM DEX they can
//! front-run. Here the order of transactions inside a batch window **cannot matter**: orders
//! are escrowed when they arrive and are only *matched* at the end of the window (`end_block`),
//! all together, at one price per pair that is a function of the *set* of orders. Nothing in
//! the clearing reads an arrival index, a transaction index or a timestamp, and the tie-breaks
//! use only order ids (`SHA256(sender || nonce)`), which the sender fixes before the block is
//! built. See `docs/CHAIN.md` (DEX) and `docs/TOKENOMICS.md` (fees).
//!
//! # Orders
//!
//! An order sells `sell_amount` of one asset for at least `buy_amount` of another (an asset is
//! BTB or a user token of the native token module). The pair is unordered: the asset with the
//! smaller encoding (BTB first, then tokens by id) is the **base**, the other the **quote**; a
//! price is always *quote per base* and is the exact rational `n/d`.
//!
//! * selling the base is an **ask**: `base = sell_amount`, limit `buy_amount / sell_amount`,
//!   it wants a price at or above the limit;
//! * selling the quote is a **bid**: `base = buy_amount`, escrow `sell_amount` of quote, limit
//!   `sell_amount / buy_amount`, it accepts a price at or below the limit and buys *up to*
//!   `base` units.
//!
//! The whole sold amount is escrowed in the DEX vault when the order is placed (BTB in the
//! vault's balance, tokens in the vault's token balance), so a vault balance always equals
//! the sum of open escrows plus nothing else. Amounts are `u128` (the unit of the asset, like
//! token balances; BTB balances are `u64`, so a BTB amount above that is simply more than anyone
//! owns) and at most [`MAX_ORDER_AMOUNT`]; all price math is exact integer math with 256-bit
//! intermediates (cross-multiplied prices, pro-rata shares, fills).
//!
//! # Clearing (every `dex_batch_blocks` blocks, at the end of the block)
//!
//! For a pair, let `S(p)` be the base offered by asks with limit `<= p` and `D(p)` the base
//! wanted by bids with limit `>= p`. The volume at `p` is `min(S, D)`. Candidate prices are the
//! limit prices of the orders in the batch (the volume only changes there). The clearing price
//! is chosen by these rules, in order:
//!
//! 1. **maximum matched volume**;
//! 2. among those, the smallest imbalance `|S - D|`;
//! 3. among those, the **lowest** price.
//!
//! Everything eligible on the short side fills completely; the long side is rationed
//! **pro rata** by remaining base amount (`floor(base * V / L)`), and the `V - sum(floors)`
//! leftover units (fewer than the number of orders) go one each to the orders with the largest
//! remainders, ties by lowest order id. Every fill is at the same price. Rounding is always in
//! the venue's favour and is supply safe: a bid pays `ceil(fill * n / d)` quote, an ask gets
//! `floor(fill * n / d)`; the difference (dust) plus the protocol fee go to the fee sink, so
//! `sum paid == sum received + sink` exactly and no unit is created or destroyed.
//!
//! # Fees
//!
//! `dex_fee_bps` of what each side receives (asks in quote, bids in base) is the clearing fee,
//! paid to whoever does the work: BTB goes to the validator pool, a user token to the fee
//! account [`dex_fees`] (nobody holds its key; a future fee-distribution rule can pay it out).
//!
//! # Lifetime
//!
//! An order lives for `batches` clearings (1 = this window only). After each clearing its
//! unfilled remainder rolls over, until it is fully filled, expired or cancelled; then the
//! remaining escrow is refunded in the same step. `Cancel` refunds at once.
//!
//! | Tag | Key | Value |
//! | --- | --- | --- |
//! | `0x34` order | `34 pair id` | open order |
//! | `0x35` owner index | `35 owner id` | `pair` |
//! | `0x36` pair | `36 pair` | `(base asset, quote asset)` |
//! | `0x37` clearing | `37 pair height:u64be` | `(n, d, base volume, quote volume, fills)` |
//! | `0x38` result | `38 owner id` | how a closed order ended |
//!
//! Only batches with a trade store a clearing record; closed orders keep one small result
//! record (state growth is paid by the placement gas).

use crate::state::{ChainConfig, StateError};
use crate::tx::{Address, Hash};
use crate::{sha256, validator_pool};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};
use btb_runtime::layout;
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub const TAG_ORDER: u8 = 0x34;
pub const TAG_OWNER: u8 = 0x35;
pub const TAG_PAIR: u8 = 0x36;
pub const TAG_CLEARING: u8 = 0x37;
pub const TAG_RESULT: u8 = 0x38;

/// Most open orders one account may have.
pub const MAX_OPEN_ORDERS: usize = 64;
/// Longest order lifetime, in batches.
pub const MAX_BATCHES: u32 = 1 << 20;
/// Highest fee a genesis may set, in basis points.
pub const MAX_FEE_BPS: u64 = 1_000;
/// Largest `sell_amount` or `buy_amount` of one order, `2^100` (1.27e30 units, over a trillion
/// whole tokens at 18 decimals). It keeps the per-pair sums of the clearing in `u128` for any
/// book of fewer than `2^28` orders; larger sums are refused by [`clear`] (no trade) instead of
/// wrapping.
pub const MAX_ORDER_AMOUNT: u128 = 1 << 100;
const BPS: u128 = 10_000;

pub type OrderId = Hash;
pub type PairId = Hash;

/// Holds every escrowed asset. No key and no contract hashes to it and no code path spends
/// from it except clearing, cancel and refund.
pub fn dex_vault() -> Address {
    sha256(&[b"btb.pool.v1", b"dex-vault"])
}

/// Where fees in user tokens accumulate (BTB fees go to the validator pool).
pub fn dex_fees() -> Address {
    sha256(&[b"btb.pool.v1", b"dex-fees"])
}

/// A tradable asset: native BTB or a user token of the native token module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Asset {
    Btb,
    Token([u8; 32]),
}

impl Asset {
    fn put(&self, out: &mut Vec<u8>) {
        match self {
            Asset::Btb => out.push(0),
            Asset::Token(id) => {
                out.push(1);
                out.extend_from_slice(id);
            }
        }
    }
    fn get(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match u8::decode_from(r)? {
            0 => Ok(Asset::Btb),
            1 => Ok(Asset::Token(<[u8; 32]>::decode_from(r)?)),
            _ => Err(DecodeError::TrailingBytes),
        }
    }
}

/// The canonical `(base, quote)` of two assets.
pub fn pair_of(a: &Asset, b: &Asset) -> (Asset, Asset) {
    if a <= b {
        (*a, *b)
    } else {
        (*b, *a)
    }
}

pub fn pair_id(base: &Asset, quote: &Asset) -> PairId {
    let (mut b, mut q) = (Vec::new(), Vec::new());
    base.put(&mut b);
    quote.put(&mut q);
    sha256(&[b"btb.dex.pair.v1", &b, &q])
}

pub fn order_id(owner: &Address, nonce: u64) -> OrderId {
    sha256(&[b"btb.dex.order.v1", owner, &nonce.to_le_bytes()])
}

/// A DEX transaction (transaction tag 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DexOp {
    /// Sell `sell_amount` of `sell` for at least `buy_amount` of `buy`, for `batches` clearings.
    Place {
        sell: Asset,
        buy: Asset,
        sell_amount: u128,
        buy_amount: u128,
        batches: u32,
    },
    /// Cancel the sender's open order and refund its escrow.
    Cancel { order: OrderId },
}

impl DexOp {
    /// Wire form: `sub:u8 || fields` (the transaction tag is written by `tx.rs`).
    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            DexOp::Place {
                sell,
                buy,
                sell_amount,
                buy_amount,
                batches,
            } => {
                out.push(0);
                sell.put(out);
                buy.put(out);
                sell_amount.encode_to(out);
                buy_amount.encode_to(out);
                batches.encode_to(out);
            }
            DexOp::Cancel { order } => {
                out.push(1);
                order.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => DexOp::Place {
                sell: Asset::get(r)?,
                buy: Asset::get(r)?,
                sell_amount: u128::decode_from(r)?,
                buy_amount: u128::decode_from(r)?,
                batches: u32::decode_from(r)?,
            },
            1 => DexOp::Cancel {
                order: <[u8; 32]>::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    pub fn gas(&self) -> u64 {
        match self {
            DexOp::Place { .. } => crate::DEX_PLACE_GAS,
            DexOp::Cancel { .. } => crate::DEX_CANCEL_GAS,
        }
    }

    /// BTB the op takes from the sender's balance on top of the fee (the BTB escrow). An amount
    /// above `u64::MAX` saturates: no balance covers it, so the transaction is refused.
    pub fn debit(&self) -> u64 {
        match self {
            DexOp::Place {
                sell: Asset::Btb,
                sell_amount,
                ..
            } => u64::try_from(*sell_amount).unwrap_or(u64::MAX),
            _ => 0,
        }
    }
}

/// Why a DEX transaction was refused (a revert: nonce used, fee charged, nothing else).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DexError {
    ZeroAmount,
    /// An amount above [`MAX_ORDER_AMOUNT`].
    AmountTooLarge,
    /// Selling an asset for itself.
    SameAsset,
    /// `batches` is 0 or above [`MAX_BATCHES`].
    BadBatches,
    NoSuchToken,
    InsufficientBalance,
    /// The account already has [`MAX_OPEN_ORDERS`] open orders.
    TooManyOrders,
    /// The account has an open order on the other side of this pair (wash trade).
    SelfTrade,
    /// No open order with this id belongs to the sender.
    NoSuchOrder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fail {
    Tx(DexError),
    State(StateError),
}
impl From<DexError> for Fail {
    fn from(e: DexError) -> Self {
        Fail::Tx(e)
    }
}
impl From<StateError> for Fail {
    fn from(e: StateError) -> Self {
        Fail::State(e)
    }
}
impl From<KvError> for Fail {
    fn from(e: KvError) -> Self {
        Fail::State(e.into())
    }
}
impl From<DecodeError> for Fail {
    fn from(e: DecodeError) -> Self {
        Fail::State(e.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Sells the base.
    Ask,
    /// Buys the base with the quote.
    Bid,
}

/// An open order as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    pub owner: Address,
    pub base: Asset,
    pub quote: Asset,
    pub side: Side,
    /// Base units still to trade.
    pub base_rem: u128,
    /// Escrow left: base for an ask, quote for a bid.
    pub escrow: u128,
    /// Limit price `n / d`, quote per base. A bid's limit is tightened to `escrow / base_rem`
    /// after a partial fill so that its escrow can never be overdrawn by rounding.
    pub n: u128,
    pub d: u128,
    pub batches_left: u32,
    pub placed: u64,
    /// Gross base traded and quote traded so far (for the result record).
    pub filled: u128,
    pub quote_traded: u128,
}

impl Order {
    pub fn pair(&self) -> PairId {
        pair_id(&self.base, &self.quote)
    }

    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.owner.encode_to(&mut o);
        o.push(matches!(self.side, Side::Bid) as u8);
        self.base.put(&mut o);
        self.quote.put(&mut o);
        for v in [self.base_rem, self.escrow, self.n, self.d] {
            v.encode_to(&mut o);
        }
        self.batches_left.encode_to(&mut o);
        self.placed.encode_to(&mut o);
        self.filled.encode_to(&mut o);
        self.quote_traded.encode_to(&mut o);
        o
    }

    fn decode(id: OrderId, b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b, 1024);
        let owner = <[u8; 32]>::decode_from(&mut r)?;
        let side = match u8::decode_from(&mut r)? {
            0 => Side::Ask,
            1 => Side::Bid,
            _ => return Err(DecodeError::TrailingBytes),
        };
        Ok(Order {
            id,
            owner,
            side,
            base: Asset::get(&mut r)?,
            quote: Asset::get(&mut r)?,
            base_rem: u128::decode_from(&mut r)?,
            escrow: u128::decode_from(&mut r)?,
            n: u128::decode_from(&mut r)?,
            d: u128::decode_from(&mut r)?,
            batches_left: u32::decode_from(&mut r)?,
            placed: u64::decode_from(&mut r)?,
            filled: u128::decode_from(&mut r)?,
            quote_traded: u128::decode_from(&mut r)?,
        })
    }
}

/// How a closed order ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing left to trade.
    Filled,
    /// The lifetime ran out; the remainder was refunded.
    Expired,
    Cancelled,
}

/// The stored result of a closed order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderResult {
    pub id: OrderId,
    pub outcome: Outcome,
    pub base: Asset,
    pub quote: Asset,
    pub side: Side,
    pub closed_at: u64,
    /// Gross base traded and gross quote traded (before the fee).
    pub filled: u128,
    pub quote_traded: u128,
}

/// A recorded clearing: the uniform price of one batch of one pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clearing {
    pub height: u64,
    pub n: u128,
    pub d: u128,
    pub base_volume: u128,
    pub quote_volume: u128,
    pub orders: u32,
}

/// A listed pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairInfo {
    pub id: PairId,
    pub base: Asset,
    pub quote: Asset,
}

fn k2(tag: u8, a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + a.len() + b.len());
    k.push(tag);
    k.extend_from_slice(a);
    k.extend_from_slice(b);
    k
}

// ------------------------------------------------------------------ exact arithmetic

fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    let (a1, a0) = (a >> 64, a & u64::MAX as u128);
    let (b1, b0) = (b >> 64, b & u64::MAX as u128);
    let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    let mid = (p00 >> 64) + (p01 & u64::MAX as u128) + (p10 & u64::MAX as u128);
    let lo = (p00 & u64::MAX as u128) | (mid << 64);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (hi, lo)
}

/// `(floor(a * b / c), a * b mod c)` with a 256-bit intermediate, or `None` if the quotient
/// does not fit in 128 bits. Panics on `c == 0`.
pub fn mul_div_checked(a: u128, b: u128, c: u128) -> Option<(u128, u128)> {
    assert!(c != 0, "division by zero");
    let (hi, _) = mul_wide(a, b);
    (hi < c).then(|| mul_div(a, b, c))
}

/// `(floor(a * b / c), a * b mod c)` with a 256-bit intermediate. The quotient must fit in
/// 128 bits (callers use `b <= c`; otherwise see [`mul_div_checked`]). Panics on `c == 0`.
pub fn mul_div(a: u128, b: u128, c: u128) -> (u128, u128) {
    assert!(c != 0, "division by zero");
    if let Some(p) = a.checked_mul(b) {
        return (p / c, p % c);
    }
    let (hi, lo) = mul_wide(a, b);
    let (mut q, mut rem) = (0u128, 0u128);
    for i in (0..256).rev() {
        let bit = if i >= 128 {
            (hi >> (i - 128)) & 1
        } else {
            (lo >> i) & 1
        };
        let carry = rem >> 127;
        rem = (rem << 1) | bit;
        q <<= 1;
        if carry == 1 || rem >= c {
            rem = rem.wrapping_sub(c);
            q |= 1;
        }
    }
    (q, rem)
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Compare the prices `a.0 / a.1` and `b.0 / b.1` exactly (cross-multiplied in 256 bits).
pub fn cmp_price(a: (u128, u128), b: (u128, u128)) -> Ordering {
    mul_wide(a.0, b.1).cmp(&mul_wide(b.0, a.1))
}

// ------------------------------------------------------------------ the clearing function

/// One side's order as the clearing sees it: `base` units at limit `n / d` (all non-zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lim {
    pub id: OrderId,
    pub base: u128,
    pub n: u128,
    pub d: u128,
}

/// The outcome of one pair's batch. Fills are sorted by order id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleared {
    pub n: u128,
    pub d: u128,
    pub volume: u128,
    pub ask_fills: Vec<(OrderId, u128)>,
    pub bid_fills: Vec<(OrderId, u128)>,
}

/// Split `v <= sum(base)` over `items` pro rata (largest remainder, ties by lowest id). The sum
/// of the bases must fit in `u128` (`clear` checks it).
fn ration(items: &[Lim], v: u128) -> Vec<u128> {
    let total: u128 = items.iter().map(|l| l.base).sum();
    if v >= total {
        return items.iter().map(|l| l.base).collect();
    }
    let mut fills = Vec::with_capacity(items.len());
    let mut rems = Vec::with_capacity(items.len());
    let mut given = 0u128;
    for (i, l) in items.iter().enumerate() {
        let (q, r) = mul_div(l.base, v, total);
        given += q;
        fills.push(q);
        rems.push((std::cmp::Reverse(r), l.id, i));
    }
    rems.sort();
    for (_, _, i) in rems.into_iter().take((v - given) as usize) {
        fills[i] += 1;
    }
    fills
}

/// Clear one pair. The result depends only on the *set* of orders (the inputs are sorted by
/// price and id inside), never on their order in the slices. `None` if nothing trades, or if
/// the bases of one side sum past `u128` (not reachable below `2^28` orders, see
/// [`MAX_ORDER_AMOUNT`]).
pub fn clear(asks: &[Lim], bids: &[Lim]) -> Option<Cleared> {
    let key = |l: &Lim| ((l.n, l.d), l.id);
    let sorted = |v: &[Lim]| {
        let mut v: Vec<Lim> = v
            .iter()
            .filter(|l| l.base > 0 && l.n > 0 && l.d > 0)
            .copied()
            .collect();
        v.sort_by(|x, y| cmp_price(key(x).0, key(y).0).then_with(|| x.id.cmp(&y.id)));
        v
    };
    let (a, b) = (sorted(asks), sorted(bids));
    // prefix[i] = base of a[..i]; suffix[i] = base of b[i..]
    let mut prefix = vec![0u128];
    for l in &a {
        prefix.push(prefix.last().unwrap().checked_add(l.base)?);
    }
    let mut suffix = vec![0u128; b.len() + 1];
    for i in (0..b.len()).rev() {
        suffix[i] = suffix[i + 1].checked_add(b[i].base)?;
    }
    let mut cands: Vec<(u128, u128)> = a.iter().chain(b.iter()).map(|l| (l.n, l.d)).collect();
    cands.sort_by(|x, y| cmp_price(*x, *y));
    cands.dedup_by(|x, y| cmp_price(*x, *y) == Ordering::Equal);

    // (price, volume, imbalance, eligible asks, first eligible bid)
    type Best = ((u128, u128), u128, u128, usize, usize);
    let mut best: Option<Best> = None;
    for &p in &cands {
        let ia = a.partition_point(|x| cmp_price((x.n, x.d), p) != Ordering::Greater);
        let ib = b.partition_point(|x| cmp_price((x.n, x.d), p) == Ordering::Less);
        let (s, d) = (prefix[ia], suffix[ib]);
        let vol = s.min(d);
        if vol == 0 {
            continue;
        }
        let imb = s.abs_diff(d);
        let better = match &best {
            None => true,
            Some((_, bv, bi, _, _)) => vol > *bv || (vol == *bv && imb < *bi),
        };
        if better {
            best = Some((p, vol, imb, ia, ib));
        }
    }
    let ((n, d), volume, _, ia, ib) = best?;
    let g = gcd(n, d);
    let (n, d) = (n / g, d / g);
    let (ea, eb) = (&a[..ia], &b[ib..]);
    let (fa, fb) = (ration(ea, volume), ration(eb, volume));
    let mut ask_fills: Vec<_> = ea.iter().zip(fa).map(|(l, f)| (l.id, f)).collect();
    let mut bid_fills: Vec<_> = eb.iter().zip(fb).map(|(l, f)| (l.id, f)).collect();
    ask_fills.sort();
    bid_fills.sort();
    Some(Cleared {
        n,
        d,
        volume,
        ask_fills,
        bid_fills,
    })
}

// ------------------------------------------------------------------ balances

fn tok_get(kv: &impl KvStore, id: &[u8; 32], who: &Address) -> Result<u128, KvError> {
    Ok(kv
        .get(&layout::token_balance_key(id, who))?
        .and_then(|b| <[u8; 16]>::try_from(b).ok())
        .map_or(0, u128::from_le_bytes))
}

fn tok_set(kv: &mut impl KvStore, id: &[u8; 32], who: &Address, v: u128) -> Result<(), KvError> {
    let key = layout::token_balance_key(id, who);
    if v == 0 {
        kv.delete(&key)
    } else {
        kv.put(&key, &v.to_le_bytes())
    }
}

fn balance_of(kv: &impl KvStore, asset: &Asset, who: &Address) -> Result<u128, KvError> {
    match asset {
        Asset::Btb => Ok(u128::from(crate::state::read_u64(
            kv,
            &layout::balance_key(who),
        )?)),
        Asset::Token(id) => tok_get(kv, id, who),
    }
}

fn set_balance(
    kv: &mut impl KvStore,
    asset: &Asset,
    who: &Address,
    v: u128,
) -> Result<(), StateError> {
    match asset {
        Asset::Btb => {
            let v = u64::try_from(v).map_err(|_| StateError("dex: BTB balance overflow".into()))?;
            Ok(kv.put(&layout::balance_key(who), &v.to_le_bytes())?)
        }
        Asset::Token(id) => Ok(tok_set(kv, id, who, v)?),
    }
}

/// Move `amount` between two accounts of one asset. A shortfall is a bug, not a user error.
fn transfer(
    kv: &mut impl KvStore,
    asset: &Asset,
    from: &Address,
    to: &Address,
    amount: u128,
) -> Result<(), StateError> {
    if amount == 0 || from == to {
        return Ok(());
    }
    let f = balance_of(kv, asset, from)?
        .checked_sub(amount)
        .ok_or_else(|| StateError("dex: escrow shortfall".into()))?;
    set_balance(kv, asset, from, f)?;
    let t = balance_of(kv, asset, to)?
        .checked_add(amount)
        .ok_or_else(|| StateError("dex: balance overflow".into()))?;
    set_balance(kv, asset, to, t)
}

/// Pay a fee or dust: BTB to the validator pool, a token to the fee account.
fn sink(kv: &mut impl KvStore, asset: &Asset, amount: u128) -> Result<(), StateError> {
    let to = match asset {
        Asset::Btb => validator_pool(),
        Asset::Token(_) => dex_fees(),
    };
    transfer(kv, asset, &dex_vault(), &to, amount)
}

// ------------------------------------------------------------------ order storage

fn order_key(pair: &PairId, id: &OrderId) -> Vec<u8> {
    k2(TAG_ORDER, pair, id)
}

fn load_order(kv: &impl KvStore, pair: &PairId, id: &OrderId) -> Result<Option<Order>, StateError> {
    match kv.get(&order_key(pair, id))? {
        None => Ok(None),
        Some(b) => Ok(Some(Order::decode(*id, &b)?)),
    }
}

fn put_order(kv: &mut impl KvStore, o: &Order) -> Result<(), KvError> {
    kv.put(&order_key(&o.pair(), &o.id), &o.encode())
}

fn result_bytes(r: &OrderResult) -> Vec<u8> {
    let mut o = vec![match r.outcome {
        Outcome::Filled => 0,
        Outcome::Expired => 1,
        Outcome::Cancelled => 2,
    }];
    r.base.put(&mut o);
    r.quote.put(&mut o);
    o.push(matches!(r.side, Side::Bid) as u8);
    r.closed_at.encode_to(&mut o);
    r.filled.encode_to(&mut o);
    r.quote_traded.encode_to(&mut o);
    o
}

fn decode_result(id: OrderId, b: &[u8]) -> Result<OrderResult, DecodeError> {
    let mut r = Reader::new(b, 256);
    let outcome = match u8::decode_from(&mut r)? {
        0 => Outcome::Filled,
        1 => Outcome::Expired,
        2 => Outcome::Cancelled,
        _ => return Err(DecodeError::TrailingBytes),
    };
    let base = Asset::get(&mut r)?;
    let quote = Asset::get(&mut r)?;
    let side = if u8::decode_from(&mut r)? == 1 {
        Side::Bid
    } else {
        Side::Ask
    };
    Ok(OrderResult {
        id,
        outcome,
        base,
        quote,
        side,
        closed_at: u64::decode_from(&mut r)?,
        filled: u128::decode_from(&mut r)?,
        quote_traded: u128::decode_from(&mut r)?,
    })
}

/// Close an order: refund its escrow, remove it and record how it ended.
fn close(
    kv: &mut impl KvStore,
    o: &Order,
    outcome: Outcome,
    height: u64,
) -> Result<(), StateError> {
    let sold = match o.side {
        Side::Ask => &o.base,
        Side::Bid => &o.quote,
    };
    transfer(kv, sold, &dex_vault(), &o.owner, o.escrow)?;
    kv.delete(&order_key(&o.pair(), &o.id))?;
    kv.delete(&k2(TAG_OWNER, &o.owner, &o.id))?;
    let res = OrderResult {
        id: o.id,
        outcome,
        base: o.base,
        quote: o.quote,
        side: o.side,
        closed_at: height,
        filled: o.filled,
        quote_traded: o.quote_traded,
    };
    kv.put(&k2(TAG_RESULT, &o.owner, &o.id), &result_bytes(&res))?;
    Ok(())
}

// ------------------------------------------------------------------ transactions

/// Run one DEX op for `sender` at `height`. Everything is checked before anything is written,
/// so a refusal changes nothing. Returns the receipt output (the order id for `Place`).
pub(crate) fn apply(
    kv: &mut impl KvStore,
    sender: &Address,
    nonce: u64,
    height: u64,
    op: &DexOp,
) -> Result<Vec<u8>, Fail> {
    match op {
        DexOp::Place {
            sell,
            buy,
            sell_amount,
            buy_amount,
            batches,
        } => {
            if *sell_amount == 0 || *buy_amount == 0 {
                return Err(DexError::ZeroAmount.into());
            }
            if *sell_amount > MAX_ORDER_AMOUNT || *buy_amount > MAX_ORDER_AMOUNT {
                return Err(DexError::AmountTooLarge.into());
            }
            if sell == buy {
                return Err(DexError::SameAsset.into());
            }
            if !(1..=MAX_BATCHES).contains(batches) {
                return Err(DexError::BadBatches.into());
            }
            for a in [sell, buy] {
                if let Asset::Token(id) = a {
                    if kv.get(&layout::token_key(id))?.is_none() {
                        return Err(DexError::NoSuchToken.into());
                    }
                }
            }
            let (base, quote) = pair_of(sell, buy);
            let pair = pair_id(&base, &quote);
            let (side, base_rem, escrow, n, d) = if *sell == base {
                (
                    Side::Ask,
                    *sell_amount,
                    *sell_amount,
                    *buy_amount,
                    *sell_amount,
                )
            } else {
                (
                    Side::Bid,
                    *buy_amount,
                    *sell_amount,
                    *sell_amount,
                    *buy_amount,
                )
            };
            // Open-order cap and wash-trade guard.
            let prefix = k2(TAG_OWNER, sender, &[]);
            let end = layout::prefix_successor(&prefix);
            let mine = kv.scan(&prefix, end.as_deref(), false, MAX_OPEN_ORDERS)?;
            if mine.len() >= MAX_OPEN_ORDERS {
                return Err(DexError::TooManyOrders.into());
            }
            for (k, v) in &mine {
                let (Ok(p), Ok(id)) = (
                    <PairId>::try_from(v.as_slice()),
                    <OrderId>::try_from(&k[prefix.len()..]),
                ) else {
                    continue;
                };
                if p == pair {
                    if let Some(o) = load_order(kv, &p, &id)? {
                        if o.side != side {
                            return Err(DexError::SelfTrade.into());
                        }
                    }
                }
            }
            if balance_of(kv, sell, sender)? < *sell_amount {
                return Err(DexError::InsufficientBalance.into());
            }
            // Checked: write.
            transfer(kv, sell, sender, &dex_vault(), *sell_amount)?;
            let id = order_id(sender, nonce);
            let o = Order {
                id,
                owner: *sender,
                base,
                quote,
                side,
                base_rem,
                escrow,
                n,
                d,
                batches_left: *batches,
                placed: height,
                filled: 0,
                quote_traded: 0,
            };
            put_order(kv, &o)?;
            kv.put(&k2(TAG_OWNER, sender, &id), &pair)?;
            let mut meta = Vec::new();
            base.put(&mut meta);
            quote.put(&mut meta);
            kv.put(&k2(TAG_PAIR, &pair, &[]), &meta)?;
            Ok(id.to_vec())
        }
        DexOp::Cancel { order } => {
            let Some(pair) = kv.get(&k2(TAG_OWNER, sender, order))? else {
                return Err(DexError::NoSuchOrder.into());
            };
            let pair: PairId = pair
                .try_into()
                .map_err(|_| StateError("dex: corrupt owner index".into()))?;
            let o = load_order(kv, &pair, order)?
                .ok_or_else(|| StateError("dex: owner index without order".into()))?;
            close(kv, &o, Outcome::Cancelled, height)?;
            Ok(o.escrow.to_le_bytes().to_vec())
        }
    }
}

// ------------------------------------------------------------------ end of block

fn load_open(kv: &impl KvStore) -> Result<Vec<Order>, StateError> {
    let end = [TAG_ORDER + 1];
    let mut from = vec![TAG_ORDER];
    let mut out = Vec::new();
    loop {
        let page = kv.scan(&from, Some(&end), false, 1024)?;
        for (k, v) in &page {
            let id: OrderId = k
                .get(33..65)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(|| StateError("dex: bad order key".into()))?;
            out.push(Order::decode(id, v)?);
        }
        match page.last() {
            Some((k, _)) if page.len() == 1024 => {
                from = k.clone();
                from.push(0);
            }
            _ => return Ok(out),
        }
    }
}

/// Clear every pair if `height` ends a batch window. Runs sequentially after the crons.
pub(crate) fn end_block(
    kv: &mut impl KvStore,
    cfg: &ChainConfig,
    height: u64,
) -> Result<(), StateError> {
    if cfg.dex_batch_blocks == 0 || !height.is_multiple_of(cfg.dex_batch_blocks) {
        return Ok(());
    }
    let mut pairs: BTreeMap<PairId, Vec<Order>> = BTreeMap::new();
    for o in load_open(kv)? {
        pairs.entry(o.pair()).or_default().push(o);
    }
    for (pair, orders) in pairs {
        clear_pair(kv, cfg, height, &pair, orders)?;
    }
    Ok(())
}

fn clear_pair(
    kv: &mut impl KvStore,
    cfg: &ChainConfig,
    height: u64,
    pair: &PairId,
    mut orders: Vec<Order>,
) -> Result<(), StateError> {
    let lim = |o: &Order| Lim {
        id: o.id,
        base: o.base_rem,
        n: o.n,
        d: o.d,
    };
    let asks: Vec<Lim> = orders
        .iter()
        .filter(|o| o.side == Side::Ask)
        .map(lim)
        .collect();
    let bids: Vec<Lim> = orders
        .iter()
        .filter(|o| o.side == Side::Bid)
        .map(lim)
        .collect();
    let (base, quote) = (orders[0].base, orders[0].quote);
    let vault = dex_vault();
    if let Some(c) = clear(&asks, &bids) {
        let fills: BTreeMap<OrderId, u128> = c
            .ask_fills
            .iter()
            .chain(c.bid_fills.iter())
            .copied()
            .collect();
        let (mut quote_paid, mut quote_out, mut base_out) = (0u128, 0u128, 0u128);
        let (mut base_fee, mut base_in) = (0u128, 0u128);
        let mut count = 0u32;
        for o in orders.iter_mut() {
            let f = fills.get(&o.id).copied().unwrap_or(0);
            if f == 0 {
                continue;
            }
            count += 1;
            let overflow = || StateError("dex: amount overflow".into());
            let (floor, rem) = mul_div_checked(f, c.n, c.d).ok_or_else(overflow)?;
            // The result record is statistics: saturate rather than stop the chain.
            o.filled = o.filled.saturating_add(f);
            o.base_rem -= f;
            match o.side {
                Side::Ask => {
                    // Receives quote: floor(f n / d) less the fee.
                    let fee = mul_div(floor, u128::from(cfg.dex_fee_bps), BPS).0;
                    let net = floor - fee;
                    o.escrow -= f;
                    o.quote_traded = o.quote_traded.saturating_add(floor);
                    quote_out = quote_out.checked_add(net).ok_or_else(overflow)?;
                    base_out = base_out.checked_add(f).ok_or_else(overflow)?;
                    transfer(kv, &quote, &vault, &o.owner, net)?;
                }
                Side::Bid => {
                    // Pays ceil(f n / d) quote and receives f base less the fee.
                    let pay = floor
                        .checked_add(u128::from(rem != 0))
                        .filter(|p| *p <= o.escrow)
                        .ok_or_else(|| StateError("dex: bid pays more than its escrow".into()))?;
                    o.escrow -= pay;
                    o.quote_traded = o.quote_traded.saturating_add(pay);
                    quote_paid = quote_paid.checked_add(pay).ok_or_else(overflow)?;
                    let fee = mul_div(f, u128::from(cfg.dex_fee_bps), BPS).0;
                    transfer(kv, &base, &vault, &o.owner, f - fee)?;
                    base_fee += fee;
                    base_in = base_in.checked_add(f).ok_or_else(overflow)?;
                }
            }
            if o.side == Side::Bid && o.base_rem > 0 && o.escrow > 0 {
                (o.n, o.d) = (o.escrow, o.base_rem);
            }
        }
        // Base side: asks gave `base_out` (= volume); bids were paid `volume - fees`.
        sink(kv, &base, base_fee)?;
        // Quote side: bids paid `quote_paid`; asks got `quote_out`; the rest (fee and rounding
        // dust, never negative: sum of ceilings >= floor of the sum >= sum of floors) is the sink.
        let quote_sink = quote_paid
            .checked_sub(quote_out)
            .ok_or_else(|| StateError("dex: quote paid below quote received".into()))?;
        sink(kv, &quote, quote_sink)?;
        if base_out != c.volume || base_in != c.volume {
            return Err(StateError("dex: base volume mismatch".into()));
        }
        let rec = (c.n, c.d, c.volume, quote_paid, count);
        let mut v = Vec::new();
        rec.0.encode_to(&mut v);
        rec.1.encode_to(&mut v);
        rec.2.encode_to(&mut v);
        rec.3.encode_to(&mut v);
        rec.4.encode_to(&mut v);
        kv.put(&k2(TAG_CLEARING, pair, &height.to_be_bytes()), &v)?;
    }
    for o in orders {
        let mut o = o;
        o.batches_left -= 1;
        if o.base_rem == 0 || o.escrow == 0 {
            let outcome = if o.base_rem == 0 {
                Outcome::Filled
            } else {
                Outcome::Expired
            };
            close(kv, &o, outcome, height)?;
        } else if o.batches_left == 0 {
            close(kv, &o, Outcome::Expired, height)?;
        } else {
            put_order(kv, &o)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ queries

fn scan_prefix(
    kv: &impl KvStore,
    prefix: &[u8],
    reverse: bool,
    limit: usize,
) -> Result<Vec<btb_runtime::kv::Entry>, KvError> {
    let end = layout::prefix_successor(prefix);
    kv.scan(prefix, end.as_deref(), reverse, limit)
}

fn decode_clearing(height: u64, b: &[u8]) -> Result<Clearing, DecodeError> {
    let mut r = Reader::new(b, 128);
    Ok(Clearing {
        height,
        n: u128::decode_from(&mut r)?,
        d: u128::decode_from(&mut r)?,
        base_volume: u128::decode_from(&mut r)?,
        quote_volume: u128::decode_from(&mut r)?,
        orders: u32::decode_from(&mut r)?,
    })
}

impl<S: KvStore> crate::state::ChainState<S> {
    /// Every pair that ever had an order, in pair-id order.
    pub fn dex_pairs(&self) -> Result<Vec<PairInfo>, StateError> {
        let mut out = Vec::new();
        for (k, v) in scan_prefix(&self.kv, &[TAG_PAIR], false, usize::MAX)? {
            let id: PairId = k[1..]
                .try_into()
                .map_err(|_| StateError("dex: bad pair key".into()))?;
            let mut r = Reader::new(&v, 128);
            out.push(PairInfo {
                id,
                base: Asset::get(&mut r)?,
                quote: Asset::get(&mut r)?,
            });
        }
        Ok(out)
    }

    /// The open orders of a pair (the order book), in order-id order.
    pub fn dex_book(&self, pair: &PairId) -> Result<Vec<Order>, StateError> {
        let prefix = k2(TAG_ORDER, pair, &[]);
        scan_prefix(&self.kv, &prefix, false, usize::MAX)?
            .into_iter()
            .map(|(k, v)| {
                let id: OrderId = k[33..]
                    .try_into()
                    .map_err(|_| StateError("dex: bad order key".into()))?;
                Ok(Order::decode(id, &v)?)
            })
            .collect()
    }

    /// An account's open orders.
    pub fn dex_orders_of(&self, owner: &Address) -> Result<Vec<Order>, StateError> {
        let prefix = k2(TAG_OWNER, owner, &[]);
        let mut out = Vec::new();
        for (k, v) in scan_prefix(&self.kv, &prefix, false, usize::MAX)? {
            let (Ok(id), Ok(pair)) = (
                <OrderId>::try_from(&k[prefix.len()..]),
                <PairId>::try_from(v.as_slice()),
            ) else {
                continue;
            };
            if let Some(o) = load_order(&self.kv, &pair, &id)? {
                out.push(o);
            }
        }
        Ok(out)
    }

    /// How a closed order of `owner` ended.
    pub fn dex_result(
        &self,
        owner: &Address,
        id: &OrderId,
    ) -> Result<Option<OrderResult>, StateError> {
        match self.kv.get(&k2(TAG_RESULT, owner, id))? {
            None => Ok(None),
            Some(b) => Ok(Some(decode_result(*id, &b)?)),
        }
    }

    /// An account's closed orders, in order-id order, at most `limit`.
    pub fn dex_results_of(
        &self,
        owner: &Address,
        limit: usize,
    ) -> Result<Vec<OrderResult>, StateError> {
        let prefix = k2(TAG_RESULT, owner, &[]);
        scan_prefix(&self.kv, &prefix, false, limit)?
            .into_iter()
            .map(|(k, v)| {
                let id: OrderId = k[prefix.len()..]
                    .try_into()
                    .map_err(|_| StateError("dex: bad result key".into()))?;
                Ok(decode_result(id, &v)?)
            })
            .collect()
    }

    /// The most recent clearings of a pair (batches with a trade), newest first.
    pub fn dex_clearings(&self, pair: &PairId, limit: usize) -> Result<Vec<Clearing>, StateError> {
        let prefix = k2(TAG_CLEARING, pair, &[]);
        scan_prefix(&self.kv, &prefix, true, limit)?
            .into_iter()
            .map(|(k, v)| {
                let h = <[u8; 8]>::try_from(&k[prefix.len()..])
                    .map_err(|_| StateError("dex: bad clearing key".into()))?;
                Ok(decode_clearing(u64::from_be_bytes(h), &v)?)
            })
            .collect()
    }

    /// The last clearing price of a pair.
    pub fn dex_last_price(&self, pair: &PairId) -> Result<Option<Clearing>, StateError> {
        Ok(self.dex_clearings(pair, 1)?.pop())
    }

    /// The info record of a user token, if it exists.
    pub fn token_info(
        &self,
        id: &[u8; 32],
    ) -> Result<Option<btb_vm::token::TokenInfo>, StateError> {
        Ok(self
            .kv
            .get(&layout::token_key(id))?
            .and_then(|b| btb_vm::token::TokenInfo::decode(&b)))
    }

    /// A user-token balance of `who`.
    pub fn token_balance(&self, id: &[u8; 32], who: &Address) -> Result<u128, StateError> {
        Ok(tok_get(&self.kv, id, who)?)
    }

    /// Up to `limit` user tokens in id order, with their info (for pickers).
    pub fn tokens(
        &self,
        limit: usize,
    ) -> Result<Vec<([u8; 32], btb_vm::token::TokenInfo)>, StateError> {
        let mut out = Vec::new();
        for (k, v) in scan_prefix(&self.kv, &[layout::TAG_TOKEN], false, limit)? {
            if let (Ok(id), Some(info)) = (
                <[u8; 32]>::try_from(&k[1..]),
                btb_vm::token::TokenInfo::decode(&v),
            ) {
                out.push((id, info));
            }
        }
        Ok(out)
    }

    /// What the DEX vault holds of `asset`: equals the escrow of all open orders in it.
    pub fn dex_vault_balance(&self, asset: &Asset) -> Result<u128, StateError> {
        Ok(balance_of(&self.kv, asset, &dex_vault())?)
    }

    /// Fees collected in a user token (BTB fees are paid to the validator pool).
    pub fn dex_fee_balance(&self, token: &[u8; 32]) -> Result<u128, StateError> {
        Ok(tok_get(&self.kv, token, &dex_fees())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// 256-bit `q * c + r` as (hi, lo) by schoolbook on 32-bit limbs, independent of `mul_wide`.
    fn limbs(x: u128) -> [u64; 4] {
        [0, 1, 2, 3].map(|i| ((x >> (32 * i)) & 0xFFFF_FFFF) as u64)
    }
    fn mul256(a: u128, b: u128) -> [u64; 8] {
        let (a, b) = (limbs(a), limbs(b));
        let mut out = [0u64; 8];
        for i in 0..4 {
            let mut carry = 0u64;
            for j in 0..4 {
                let cur = out[i + j] + a[i] * b[j] + carry;
                out[i + j] = cur & 0xFFFF_FFFF;
                carry = cur >> 32;
            }
            out[i + 4] += carry;
        }
        out
    }
    fn add256(mut x: [u64; 8], y: u128) -> [u64; 8] {
        let y = limbs(y);
        let mut carry = 0;
        for i in 0..8 {
            let cur = x[i] + if i < 4 { y[i] } else { 0 } + carry;
            x[i] = cur & 0xFFFF_FFFF;
            carry = cur >> 32;
        }
        x
    }

    #[test]
    fn mul_div_is_exact_on_wide_values() {
        let mut r = Rng(0x1234_5678_9abc_def1);
        for _ in 0..2_000 {
            let wide = |r: &mut Rng| ((r.next() as u128) << 64) | r.next() as u128;
            let a = wide(&mut r) >> r.below(100);
            let c = (wide(&mut r) >> r.below(100)).max(1);
            let b = if r.below(2) == 0 { c } else { wide(&mut r) % c };
            let (q, rem) = mul_div(a, b, c);
            assert!(rem < c);
            assert_eq!(add256(mul256(q, c), rem), mul256(a, b));
        }
        assert_eq!(mul_div(u128::MAX, 5, 5), (u128::MAX, 0));
        assert_eq!(mul_div(7, 3, 2), (10, 1));
    }

    fn id(i: u32) -> OrderId {
        let mut h = [0u8; 32];
        h[..4].copy_from_slice(&i.to_be_bytes());
        h
    }

    /// `p <= q` by the schoolbook 256-bit product, independent of `mul_wide`.
    fn le(p: (u128, u128), q: (u128, u128)) -> bool {
        ref_cmp(p, q) != Ordering::Greater
    }

    fn ref_cmp(p: (u128, u128), q: (u128, u128)) -> Ordering {
        mul256(p.0, q.1)
            .iter()
            .rev()
            .cmp(mul256(q.0, p.1).iter().rev())
    }

    #[test]
    fn cmp_price_is_exact_on_wide_values() {
        let mut r = Rng(99);
        let wide = |r: &mut Rng| (((r.next() as u128) << 64) | r.next() as u128) >> r.below(120);
        for _ in 0..5_000 {
            let (p, q) = (
                (wide(&mut r), wide(&mut r).max(1)),
                (wide(&mut r), wide(&mut r).max(1)),
            );
            assert_eq!(cmp_price(p, q), ref_cmp(p, q));
            assert_eq!(cmp_price(p, p), Ordering::Equal);
        }
        // 2^127/3 against 2^127/3 + epsilon: a 128-bit product would wrap.
        let big = 1u128 << 127;
        assert_eq!(cmp_price((big, 3), (big + 1, 3)), Ordering::Less);
        assert_eq!(cmp_price((big, 1), (big - 1, 1)), Ordering::Greater);
    }

    /// Obvious reference: for each candidate price recompute S and D from scratch.
    fn reference(asks: &[Lim], bids: &[Lim]) -> Option<((u128, u128), u128, u128)> {
        let mut cands: Vec<(u128, u128)> = asks.iter().chain(bids).map(|l| (l.n, l.d)).collect();
        cands.sort_by(|a, b| ref_cmp(*a, *b));
        let vol_at = |p: (u128, u128)| {
            let s: u128 = asks
                .iter()
                .filter(|l| le((l.n, l.d), p))
                .map(|l| l.base)
                .sum();
            let d: u128 = bids
                .iter()
                .filter(|l| le(p, (l.n, l.d)))
                .map(|l| l.base)
                .sum();
            (s.min(d), s.abs_diff(d))
        };
        let mut best: Option<((u128, u128), u128, u128)> = None;
        for p in cands {
            let (v, imb) = vol_at(p);
            if v == 0 {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, bv, bi)) => v > bv || (v == bv && imb < bi),
            };
            if better {
                best = Some((p, v, imb));
            }
        }
        best
    }

    fn random_book(r: &mut Rng, n: usize) -> (Vec<Lim>, Vec<Lim>) {
        random_book_scaled(r, n, 0)
    }

    /// Like `random_book`, but every amount and price term is shifted left by a random
    /// 0..=`max_shift` bits, so books reach 2^100 and need the full 256-bit comparisons.
    fn random_book_scaled(r: &mut Rng, n: usize, max_shift: u64) -> (Vec<Lim>, Vec<Lim>) {
        let (mut asks, mut bids) = (vec![], vec![]);
        for i in 0..n {
            let shift = |r: &mut Rng| {
                if max_shift == 0 {
                    0
                } else {
                    r.below(max_shift + 1)
                }
            };
            let (sb, sn, sd) = (shift(r), shift(r), shift(r));
            let l = Lim {
                id: id(i as u32),
                base: u128::from(1 + r.below(50)) << sb,
                n: u128::from(1 + r.below(30)) << sn,
                d: u128::from(1 + r.below(6)) << sd,
            };
            if r.below(2) == 0 {
                asks.push(l)
            } else {
                bids.push(l)
            }
        }
        (asks, bids)
    }

    #[test]
    fn clearing_matches_bruteforce_on_random_books() {
        bruteforce(42, 0);
    }

    /// Same, with amounts and prices up to 2^100 (256-bit cross products and shares).
    #[test]
    fn clearing_matches_bruteforce_on_wide_random_books() {
        bruteforce(43, 94);
        bruteforce(44, 40);
    }

    fn bruteforce(seed: u64, max_shift: u64) {
        let mut r = Rng(seed);
        let mut traded = 0;
        for round in 0..3_000 {
            let n = 1 + r.below(14) as usize;
            let (asks, bids) = random_book_scaled(&mut r, n, max_shift);
            let want = reference(&asks, &bids);
            let got = clear(&asks, &bids);
            match (want, &got) {
                (None, None) => continue,
                (Some((p, v, _)), Some(c)) => {
                    traded += 1;
                    assert_eq!(cmp_price((c.n, c.d), p), Ordering::Equal, "round {round}");
                    assert_eq!(c.volume, v, "round {round}");
                    // No *other* price (a fine rational grid, not just limit prices) has more volume.
                    for num in 1..=120u128 {
                        let q = (num, 4);
                        let s: u128 = asks
                            .iter()
                            .filter(|l| le((l.n, l.d), q))
                            .map(|l| l.base)
                            .sum();
                        let d: u128 = bids
                            .iter()
                            .filter(|l| le(q, (l.n, l.d)))
                            .map(|l| l.base)
                            .sum();
                        assert!(
                            s.min(d) <= v,
                            "round {round}: price {q:?} beats the clearing"
                        );
                    }
                    check_fills(&asks, &bids, c);
                }
                other => panic!("round {round}: {other:?}"),
            }
        }
        assert!(
            traded > 1_000,
            "the generator should trade often ({traded})"
        );
    }

    fn check_fills(asks: &[Lim], bids: &[Lim], c: &Cleared) {
        let p = (c.n, c.d);
        for (side, fills, ok) in [(asks, &c.ask_fills, true), (bids, &c.bid_fills, false)] {
            let elig: Vec<&Lim> = side
                .iter()
                .filter(|l| {
                    if ok {
                        le((l.n, l.d), p)
                    } else {
                        le(p, (l.n, l.d))
                    }
                })
                .collect();
            assert_eq!(elig.len(), fills.len());
            let total: u128 = elig.iter().map(|l| l.base).sum();
            let sum: u128 = fills.iter().map(|f| f.1).sum();
            assert_eq!(sum, c.volume);
            for (id, f) in fills {
                let l = elig.iter().find(|l| l.id == *id).expect("eligible");
                assert!(*f <= l.base);
                if total == c.volume {
                    assert_eq!(*f, l.base, "short side fills fully");
                } else {
                    // pro rata: floor or floor + 1 of base * V / L
                    let q = mul_div(l.base, c.volume, total).0;
                    assert!(*f == q || *f == q + 1, "pro rata {f} vs {q}");
                }
            }
        }
    }

    #[test]
    fn clearing_ignores_input_order() {
        let mut r = Rng(7);
        for _ in 0..500 {
            let n = 2 + r.below(12) as usize;
            let (asks, bids) = random_book(&mut r, n);
            let want = clear(&asks, &bids);
            let (mut a2, mut b2) = (asks.clone(), bids.clone());
            for i in (1..a2.len()).rev() {
                a2.swap(i, r.below(i as u64 + 1) as usize);
            }
            for i in (1..b2.len()).rev() {
                b2.swap(i, r.below(i as u64 + 1) as usize);
            }
            assert_eq!(clear(&a2, &b2), want);
        }
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let m = MAX_ORDER_AMOUNT;
        let a = [
            Lim {
                id: id(1),
                base: m,
                n: m,
                d: 1,
            },
            Lim {
                id: id(2),
                base: m,
                n: 1,
                d: m,
            },
        ];
        let b = [
            Lim {
                id: id(3),
                base: m,
                n: m,
                d: 1,
            },
            Lim {
                id: id(4),
                base: m,
                n: m,
                d: m,
            },
        ];
        let c = clear(&a, &b).expect("trades");
        assert!(c.volume <= 2 * m);
        assert_eq!(c.ask_fills.iter().map(|f| f.1).sum::<u128>(), c.volume);
        assert!(clear(&[], &b).is_none());
        assert!(clear(&a, &[]).is_none());
        // zero-sized or zero-price entries are ignored, never divide by zero
        assert!(clear(
            &[Lim {
                id: id(9),
                base: 0,
                n: 1,
                d: 1
            }],
            &b
        )
        .is_none());
    }

    #[test]
    fn sums_past_u128_are_refused_not_wrapped() {
        let big = |i, base| Lim {
            id: id(i),
            base,
            n: 1,
            d: 1,
        };
        // Two bids that together want more than u128::MAX base: no clearing.
        let bids = [big(1, u128::MAX), big(2, u128::MAX)];
        assert!(clear(&[big(3, 10)], &bids).is_none());
        // One of them alone is fine.
        let c = clear(&[big(3, 10)], &bids[..1]).expect("trades");
        assert_eq!(c.volume, 10);
    }

    #[test]
    fn mul_div_checked_reports_quotient_overflow() {
        assert_eq!(mul_div_checked(u128::MAX, 2, 2), Some((u128::MAX, 0)));
        assert_eq!(mul_div_checked(u128::MAX, 2, 1), None);
        assert_eq!(mul_div_checked(1 << 100, 1 << 100, 1 << 70), None);
        assert_eq!(
            mul_div_checked(1 << 100, 1 << 100, 1 << 73),
            Some((1 << 127, 0))
        );
    }
}
