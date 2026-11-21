//! Paid `btb_query` on the node's RPC (proposal 0004, phase B0).
//!
//! A query costs the node real work, so beyond a small free quota it is paid with a
//! payment-channel voucher (`btb-voucher`, the same scheme edge nodes use, `docs/EDGE.md`):
//!
//! * Each client gets `free_queries` per `window_ms` without a voucher.
//! * Beyond that the request carries `X-BTB-Voucher` (hex of the 120-byte [`SignedVoucher`]) for a
//!   channel whose **payee is this node's account** (the account of its consensus key). The
//!   voucher must strictly raise amount and nonce ([`Ledger::accept`]); the query runs when the
//!   amount earned on the channel covers everything charged so far plus `min_charge`.
//! * The query is then charged `max(min_charge, ceil(gas_used * price_per_mgas / 1_000_000))`.
//!   A charge above what was covered leaves a debt the next voucher must pay first; it is bounded
//!   by the query gas cap.
//! * The node settles channels that earned at least `redeem_min` with a `ChannelOp::Close` signed
//!   by its own key ([`QueryPay::take_redeemable`]): the BTB goes to the node that served.
//!
//! A request with a voucher is always paid (it does not use the free quota) and a bad voucher is
//! refused even inside the quota, so a wallet learns at once that it is out of sync.
//! Nothing here touches consensus: charging is local bookkeeping, settlement is an ordinary
//! transaction, and no unit is created (every unit comes from a payer's channel deposit).

use btb_voucher::{ChannelId, Ledger, SignedVoucher};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// `node.json` section `query_pay`. Absent: queries are free (the old behaviour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPayConfig {
    /// Queries per client and window that need no voucher.
    pub free_queries: u64,
    pub window_ms: u64,
    /// Base units charged per 1,000,000 gas a query uses.
    pub price_per_mgas: u64,
    /// Smallest charge for one paid query, and the credit a voucher must leave before one runs.
    pub min_charge: u64,
    /// Settle a channel on chain once it has earned at least this much.
    pub redeem_min: u64,
}

impl QueryPayConfig {
    /// Defaults for a chain with gas price `gas_price`: a query costs a tenth of what the same
    /// gas costs in a transaction.
    pub fn for_gas_price(gas_price: u64) -> Self {
        QueryPayConfig {
            free_queries: 100,
            window_ms: 3_600_000,
            price_per_mgas: gas_price.saturating_mul(100_000),
            min_charge: gas_price.saturating_mul(1_000).max(1),
            redeem_min: gas_price.saturating_mul(10_000_000).max(1),
        }
    }

    pub fn from_json(v: &serde_json::Value, gas_price: u64) -> Result<Self, String> {
        let d = Self::for_gas_price(gas_price);
        let num = |k: &str, def: u64| -> Result<u64, String> {
            match v.get(k) {
                None => Ok(def),
                Some(x) => x
                    .as_u64()
                    .ok_or_else(|| format!("query_pay.{k} must be a non-negative integer")),
            }
        };
        let c = QueryPayConfig {
            free_queries: num("free_queries", d.free_queries)?,
            window_ms: num("window_ms", d.window_ms)?,
            price_per_mgas: num("price_per_mgas", d.price_per_mgas)?,
            min_charge: num("min_charge", d.min_charge)?,
            redeem_min: num("redeem_min", d.redeem_min)?,
        };
        if c.window_ms == 0 {
            return Err("query_pay.window_ms must be positive".into());
        }
        if c.min_charge == 0 {
            return Err("query_pay.min_charge must be positive".into());
        }
        Ok(c)
    }

    /// What a query that used `gas` costs.
    pub fn cost(&self, gas: u64) -> u64 {
        let v = (gas as u128 * self.price_per_mgas as u128).div_ceil(1_000_000);
        u64::try_from(v).unwrap_or(u64::MAX).max(self.min_charge)
    }
}

/// What the chain says about a channel (looked up by the caller in the head state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelView {
    pub payer_key: [u8; 32],
    pub payee: [u8; 32],
    pub deposit: u64,
    pub closing: bool,
}

/// Why a query was not admitted. The RPC answers with error code [`PAYMENT_REQUIRED`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayError {
    QuotaUsed,
    BadVoucher(String),
    /// The voucher is valid but does not cover what is owed plus `min_charge`.
    Insufficient {
        earned: u64,
        needed: u64,
    },
}

impl std::fmt::Display for PayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayError::QuotaUsed => {
                write!(f, "free quota used up: send a voucher in X-BTB-Voucher")
            }
            PayError::BadVoucher(m) => write!(f, "voucher refused: {m}"),
            PayError::Insufficient { earned, needed } => write!(
                f,
                "voucher too small: channel pays {earned}, {needed} needed"
            ),
        }
    }
}

/// JSON-RPC error code for a query that needs (more) payment.
pub const PAYMENT_REQUIRED: i64 = -32402;

/// A query let through, to be charged with [`QueryPay::charge`] once it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    Free,
    Paid(ChannelId),
}

struct Chan {
    ledger: Ledger,
    deposit: u64,
    charged: u64,
}

/// Per-node payment state for queries (in memory; lost on restart, like the edge's).
pub struct QueryPay {
    pub cfg: QueryPayConfig,
    chain_id: u64,
    payee: [u8; 32],
    clients: HashMap<String, (Instant, u64)>,
    channels: HashMap<ChannelId, Chan>,
    pub free_served: u64,
    pub paid_served: u64,
    pub refused: u64,
    pub charged_total: u64,
    pub redeemed_total: u64,
}

impl QueryPay {
    pub fn new(cfg: QueryPayConfig, chain_id: u64, payee: [u8; 32]) -> Self {
        QueryPay {
            cfg,
            chain_id,
            payee,
            clients: HashMap::new(),
            channels: HashMap::new(),
            free_served: 0,
            paid_served: 0,
            refused: 0,
            charged_total: 0,
            redeemed_total: 0,
        }
    }

    pub fn payee(&self) -> [u8; 32] {
        self.payee
    }

    /// Decide whether a query from `client` may run. `lookup` reads a channel from the head state.
    pub fn admit(
        &mut self,
        now: Instant,
        client: &str,
        voucher_hex: Option<&str>,
        lookup: impl FnOnce(&ChannelId) -> Option<ChannelView>,
    ) -> Result<Admit, PayError> {
        let r = self.admit_inner(now, client, voucher_hex, lookup);
        if r.is_err() {
            self.refused += 1;
        }
        r
    }

    fn admit_inner(
        &mut self,
        now: Instant,
        client: &str,
        voucher_hex: Option<&str>,
        lookup: impl FnOnce(&ChannelId) -> Option<ChannelView>,
    ) -> Result<Admit, PayError> {
        let Some(h) = voucher_hex else {
            let window = Duration::from_millis(self.cfg.window_ms);
            if self.clients.len() > 10_000 {
                self.clients
                    .retain(|_, (t, _)| now.duration_since(*t) < window);
            }
            let e = self.clients.entry(client.to_string()).or_insert((now, 0));
            if now.duration_since(e.0) >= window {
                *e = (now, 0);
            }
            if e.1 >= self.cfg.free_queries {
                return Err(PayError::QuotaUsed);
            }
            e.1 += 1;
            return Ok(Admit::Free);
        };
        let bad = |m: &str| PayError::BadVoucher(m.to_string());
        let sv = crate::hex::decode(h.trim())
            .ok()
            .and_then(|b| SignedVoucher::from_bytes(&b))
            .ok_or_else(|| bad("X-BTB-Voucher is not a 120-byte voucher in hex"))?;
        let id = sv.voucher.channel_id;
        if sv.voucher.chain_id != self.chain_id {
            return Err(bad("voucher for another chain"));
        }
        let known = self.channels.get(&id).map(|c| c.deposit);
        if known.is_none_or(|d| sv.voucher.amount > d) {
            let v = lookup(&id).ok_or_else(|| bad("unknown channel (never opened, or closed)"))?;
            if v.payee != self.payee {
                return Err(bad("the channel pays another account"));
            }
            if v.closing {
                return Err(bad("the channel is closing"));
            }
            match self.channels.get_mut(&id) {
                Some(c) => {
                    if v.deposit > c.deposit {
                        c.ledger
                            .add_deposit(v.deposit - c.deposit)
                            .map_err(|e| PayError::BadVoucher(format!("{e:?}")))?;
                        c.deposit = v.deposit;
                    }
                }
                None => {
                    self.channels.insert(
                        id,
                        Chan {
                            ledger: Ledger::new(self.chain_id, id, v.payer_key, v.deposit),
                            deposit: v.deposit,
                            charged: 0,
                        },
                    );
                }
            }
        }
        let c = self.channels.get_mut(&id).expect("inserted above");
        c.ledger
            .accept(&sv)
            .map_err(|e| PayError::BadVoucher(format!("{e:?}")))?;
        let earned = c.ledger.earned();
        let needed = c.charged.saturating_add(self.cfg.min_charge);
        if earned < needed {
            return Err(PayError::Insufficient { earned, needed });
        }
        Ok(Admit::Paid(id))
    }

    /// Charge a query that ran (or trapped) after using `gas`. Returns the charge (0 if free).
    pub fn charge(&mut self, admit: Admit, gas: u64) -> u64 {
        match admit {
            Admit::Free => {
                self.free_served += 1;
                0
            }
            Admit::Paid(id) => {
                let cost = self.cfg.cost(gas);
                if let Some(c) = self.channels.get_mut(&id) {
                    c.charged = c.charged.saturating_add(cost);
                }
                self.paid_served += 1;
                self.charged_total = self.charged_total.saturating_add(cost);
                cost
            }
        }
    }

    /// Unsettled amount across open channels.
    pub fn unsettled(&self) -> u64 {
        self.channels.values().map(|c| c.ledger.earned()).sum()
    }

    /// Best vouchers of channels that earned at least `redeem_min`. They are forgotten here: the
    /// caller closes them on chain, after which further vouchers for them are refused (the
    /// channel is gone).
    pub fn take_redeemable(&mut self) -> Vec<SignedVoucher> {
        let min = self.cfg.redeem_min;
        let ids: Vec<ChannelId> = self
            .channels
            .iter()
            .filter(|(_, c)| c.ledger.earned() >= min && c.ledger.best().is_some())
            .map(|(id, _)| *id)
            .collect();
        let mut out = Vec::new();
        for id in ids {
            if let Some(c) = self.channels.remove(&id) {
                self.redeemed_total = self.redeemed_total.saturating_add(c.ledger.earned());
                out.extend(c.ledger.best().cloned());
            }
        }
        out
    }

    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "payee": crate::hex::encode(&self.payee),
            "free_queries": self.cfg.free_queries,
            "window_ms": self.cfg.window_ms,
            "price_per_mgas": self.cfg.price_per_mgas.to_string(),
            "min_charge": self.cfg.min_charge.to_string(),
            "redeem_min": self.cfg.redeem_min.to_string(),
            "free_served": self.free_served,
            "paid_served": self.paid_served,
            "refused": self.refused,
            "charged_total": self.charged_total.to_string(),
            "unsettled": self.unsettled().to_string(),
            "redeemed_total": self.redeemed_total.to_string(),
            "open_channels": self.channels.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_voucher::{Spender, Voucher};
    use ed25519_dalek::SigningKey;

    const CHAIN: u64 = 7;
    const NODE: [u8; 32] = [9; 32];
    const CH: ChannelId = [5; 32];

    fn cfg() -> QueryPayConfig {
        QueryPayConfig {
            free_queries: 2,
            window_ms: 1_000,
            price_per_mgas: 1_000_000, // 1 unit per gas
            min_charge: 100,
            redeem_min: 1_000,
        }
    }

    fn payer() -> SigningKey {
        SigningKey::from_bytes(&[3; 32])
    }

    fn view(deposit: u64) -> ChannelView {
        ChannelView {
            payer_key: payer().verifying_key().to_bytes(),
            payee: NODE,
            deposit,
            closing: false,
        }
    }

    fn hex_of(sv: &SignedVoucher) -> String {
        crate::hex::encode(&sv.to_bytes())
    }

    #[test]
    fn free_quota_then_payment_required_then_reset() {
        let mut p = QueryPay::new(cfg(), CHAIN, NODE);
        let t = Instant::now();
        let none = |_: &ChannelId| None;
        assert_eq!(p.admit(t, "a", None, none), Ok(Admit::Free));
        assert_eq!(p.admit(t, "a", None, none), Ok(Admit::Free));
        assert_eq!(p.admit(t, "a", None, none), Err(PayError::QuotaUsed));
        assert_eq!(p.admit(t, "b", None, none), Ok(Admit::Free), "per client");
        let later = t + Duration::from_millis(1_000);
        assert_eq!(
            p.admit(later, "a", None, none),
            Ok(Admit::Free),
            "new window"
        );
        assert_eq!(p.refused, 1);
    }

    #[test]
    fn paid_query_is_charged_by_gas_and_debt_blocks_the_next() {
        let mut p = QueryPay::new(cfg(), CHAIN, NODE);
        let mut s = Spender::new(CHAIN, CH, 10_000);
        let t = Instant::now();
        let v1 = s.pay(500, &payer()).unwrap();
        let a = p.admit(t, "a", Some(&hex_of(&v1)), |_| Some(view(10_000)));
        assert_eq!(a, Ok(Admit::Paid(CH)));
        assert_eq!(p.charge(a.unwrap(), 700), 700, "1 unit per gas");
        // Owed 700, earned 500: the next voucher must cover 700 + min_charge.
        let v2 = s.pay(250, &payer()).unwrap(); // cumulative 750
        assert_eq!(
            p.admit(t, "a", Some(&hex_of(&v2)), |_| None),
            Err(PayError::Insufficient {
                earned: 750,
                needed: 800
            })
        );
        let v3 = s.pay(100, &payer()).unwrap(); // 850
        let a = p.admit(t, "a", Some(&hex_of(&v3)), |_| None).unwrap();
        assert_eq!(p.charge(a, 10), 100, "min charge");
        assert_eq!(p.charged_total, 800);
        assert_eq!(p.unsettled(), 850);
    }

    #[test]
    fn bad_vouchers_are_refused_even_inside_the_quota() {
        let mut p = QueryPay::new(cfg(), CHAIN, NODE);
        let t = Instant::now();
        let mut s = Spender::new(CHAIN, CH, 10_000);
        let v1 = s.pay(500, &payer()).unwrap();
        let ok = |_: &ChannelId| Some(view(10_000));
        assert!(p.admit(t, "a", Some(&hex_of(&v1)), ok).is_ok());
        // Replay: not strictly greater.
        assert!(matches!(
            p.admit(t, "a", Some(&hex_of(&v1)), ok),
            Err(PayError::BadVoucher(_))
        ));
        // Garbage.
        assert!(matches!(
            p.admit(t, "a", Some("zz"), ok),
            Err(PayError::BadVoucher(_))
        ));
        // Wrong chain.
        let other = Voucher {
            chain_id: CHAIN + 1,
            channel_id: [6; 32],
            amount: 1_000,
            nonce: 1,
        }
        .sign(&payer());
        assert!(matches!(
            p.admit(t, "a", Some(&hex_of(&other)), ok),
            Err(PayError::BadVoucher(_))
        ));
        // Channel paying someone else, closing, or unknown.
        let mut s2 = Spender::new(CHAIN, [7; 32], 10_000);
        let v = s2.pay(500, &payer()).unwrap();
        let elsewhere = |_: &ChannelId| {
            Some(ChannelView {
                payee: [1; 32],
                ..view(10_000)
            })
        };
        assert!(p.admit(t, "a", Some(&hex_of(&v)), elsewhere).is_err());
        let closing = |_: &ChannelId| {
            Some(ChannelView {
                closing: true,
                ..view(10_000)
            })
        };
        assert!(p.admit(t, "a", Some(&hex_of(&v)), closing).is_err());
        assert!(p.admit(t, "a", Some(&hex_of(&v)), |_| None).is_err());
        // Signed by another key.
        let mut s3 = Spender::new(CHAIN, [8; 32], 10_000);
        let forged = s3.pay(500, &SigningKey::from_bytes(&[4; 32])).unwrap();
        assert!(p
            .admit(t, "a", Some(&hex_of(&forged)), |_| Some(ChannelView {
                ..view(10_000)
            }))
            .is_err());
        // None of this used the free quota.
        assert_eq!(p.admit(t, "a", None, |_| None), Ok(Admit::Free));
        assert_eq!(p.paid_served, 0);
    }

    #[test]
    fn voucher_above_known_deposit_rereads_the_channel() {
        let mut p = QueryPay::new(cfg(), CHAIN, NODE);
        let t = Instant::now();
        let mut s = Spender::new(CHAIN, CH, 1_000);
        let v = s.pay(500, &payer()).unwrap();
        assert!(p
            .admit(t, "a", Some(&hex_of(&v)), |_| Some(view(1_000)))
            .is_ok());
        s.add_deposit(5_000).unwrap();
        let v = s.pay(3_000, &payer()).unwrap();
        // Without a re-read the ledger would refuse an amount above the old deposit.
        assert!(p
            .admit(t, "a", Some(&hex_of(&v)), |_| Some(view(6_000)))
            .is_ok());
    }

    #[test]
    fn redeem_takes_only_channels_above_the_minimum_and_forgets_them() {
        let mut p = QueryPay::new(cfg(), CHAIN, NODE);
        let t = Instant::now();
        let mut big = Spender::new(CHAIN, CH, 10_000);
        let mut small = Spender::new(CHAIN, [6; 32], 10_000);
        let vb = big.pay(1_500, &payer()).unwrap();
        let vs = small.pay(200, &payer()).unwrap();
        p.admit(t, "a", Some(&hex_of(&vb)), |_| Some(view(10_000)))
            .unwrap();
        p.admit(t, "a", Some(&hex_of(&vs)), |_| Some(view(10_000)))
            .unwrap();
        let out = p.take_redeemable();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], vb, "the best voucher of the big channel");
        assert_eq!(p.redeemed_total, 1_500);
        assert_eq!(p.unsettled(), 200);
        assert!(p.take_redeemable().is_empty());
    }

    #[test]
    fn cost_rounds_up_and_respects_the_minimum() {
        let c = QueryPayConfig {
            price_per_mgas: 3,
            min_charge: 1,
            ..cfg()
        };
        assert_eq!(c.cost(0), 1);
        assert_eq!(c.cost(1), 1);
        assert_eq!(c.cost(1_000_000), 3);
        assert_eq!(c.cost(1_000_001), 4);
        assert_eq!(
            c.cost(u64::MAX) as u128,
            (u64::MAX as u128 * 3).div_ceil(1_000_000)
        );
        let d = QueryPayConfig::for_gas_price(1);
        assert_eq!(
            d.cost(331_987),
            33_199,
            "a full stock() call at g/10 per gas"
        );
        assert!(QueryPayConfig::from_json(&serde_json::json!({"window_ms": 0}), 1).is_err());
        assert_eq!(
            QueryPayConfig::from_json(&serde_json::json!({"free_queries": 5}), 1)
                .unwrap()
                .free_queries,
            5
        );
    }
}
