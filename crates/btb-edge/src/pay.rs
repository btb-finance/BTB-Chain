//! Per-request payment with payment-channel vouchers (`docs/TOKENOMICS.md`).
//!
//! Each client address gets `free_requests` per `window`. Beyond that a request needs a
//! voucher in `X-BTB-Voucher` (hex of the 120-byte [`SignedVoucher`]): signed by the channel's
//! payer, for a channel whose payee is this edge, with a **cumulative** amount above every
//! earlier one ([`Ledger`] refuses anything else). The request is served when the amount
//! earned so far covers everything charged so far plus `price`. The edge keeps the best voucher
//! per channel and settles it with a `Close` transaction ([`Pay::redeem`]).
//!
//! A request that carries a voucher is always a paid request (it does not use the free quota),
//! and a bad voucher is refused even inside the quota: the wallet learns it is out of sync.

use crate::chain::{Rpc, Submitter};
use crate::metrics::Metrics;
use btb_voucher::{ChannelId, Ledger, SignedVoucher, VoucherError};
use serde_json::json;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct PayConfig {
    /// The account vouchers must pay (the edge's own address).
    pub payee: [u8; 32],
    /// Chain id vouchers must name. `None`: ask the node once (`btb_status`).
    pub chain_id: Option<u64>,
    /// Requests per client address and `window` that need no voucher.
    pub free_requests: u64,
    pub window: Duration,
    /// Base units charged per request beyond the quota.
    pub price: u64,
    /// Do not settle a channel on chain before it has earned at least this much.
    pub redeem_min: u64,
}

struct Chan {
    ledger: Ledger,
    deposit: u64,
    charged: u64,
}

#[derive(Default)]
struct State {
    clients: HashMap<IpAddr, (Instant, u64)>,
    channels: HashMap<ChannelId, Chan>,
}

/// Result of settling one channel.
#[derive(Debug)]
pub struct Redeemed {
    pub channel: ChannelId,
    pub amount: u64,
    pub result: Result<(), String>,
}

pub struct Pay {
    cfg: PayConfig,
    rpc: Arc<dyn Rpc>,
    submitter: Option<Arc<dyn Submitter>>,
    metrics: Arc<Metrics>,
    chain_id: Mutex<Option<u64>>,
    st: Mutex<State>,
}

fn n(v: &serde_json::Value, k: &str) -> Result<u64, String> {
    match &v[k] {
        serde_json::Value::Number(x) => x.as_u64().ok_or_else(|| format!("bad number {k}")),
        serde_json::Value::String(x) => x.parse().map_err(|_| format!("bad number {k}")),
        _ => Err(format!("missing field {k}")),
    }
}

impl Pay {
    pub fn new(
        cfg: PayConfig,
        rpc: Arc<dyn Rpc>,
        submitter: Option<Arc<dyn Submitter>>,
        metrics: Arc<Metrics>,
    ) -> Pay {
        Pay {
            chain_id: Mutex::new(cfg.chain_id),
            cfg,
            rpc,
            submitter,
            metrics,
            st: Mutex::new(State::default()),
        }
    }

    pub fn price(&self) -> u64 {
        self.cfg.price
    }

    /// Total earned across open channels, not yet settled.
    pub fn unsettled(&self) -> u64 {
        let s = self.st.lock().unwrap_or_else(|e| e.into_inner());
        s.channels.values().map(|c| c.ledger.earned()).sum()
    }

    fn chain_id(&self) -> Result<u64, String> {
        let mut c = self.chain_id.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = *c {
            return Ok(id);
        }
        let id = self.rpc.call("btb_status", json!([]))?["chain_id"]
            .as_u64()
            .ok_or("status has no chain_id")?;
        *c = Some(id);
        Ok(id)
    }

    /// Make sure the channel is known and its deposit is current for a voucher of `amount`.
    fn ensure_channel(&self, sv: &SignedVoucher) -> Result<(), String> {
        let id = sv.voucher.channel_id;
        {
            let s = self.st.lock().unwrap_or_else(|e| e.into_inner());
            if s.channels
                .get(&id)
                .is_some_and(|c| sv.voucher.amount <= c.deposit)
            {
                return Ok(());
            }
        }
        let v = self
            .rpc
            .call("btb_channel", json!([btb_storage::hex(&id)]))
            .map_err(|e| format!("cannot look up the channel: {e}"))?;
        if v.is_null() {
            return Err("unknown channel (never opened, or already closed)".into());
        }
        let payee = btb_storage::unhex32(v["payee"].as_str().ok_or("channel has no payee")?)?;
        if payee != self.cfg.payee {
            return Err("the channel pays another account".into());
        }
        if !v["closing"].is_null() {
            return Err("the channel is closing".into());
        }
        let deposit = n(&v, "deposit")?;
        let key = btb_storage::unhex32(v["payer_key"].as_str().ok_or("channel has no key")?)?;
        let chain_id = self.chain_id()?;
        let mut s = self.st.lock().unwrap_or_else(|e| e.into_inner());
        match s.channels.get_mut(&id) {
            Some(c) => {
                if deposit > c.deposit {
                    c.ledger
                        .add_deposit(deposit - c.deposit)
                        .map_err(|e| format!("{e:?}"))?;
                    c.deposit = deposit;
                }
            }
            None => {
                s.channels.insert(
                    id,
                    Chan {
                        ledger: Ledger::new(chain_id, id, key, deposit),
                        deposit,
                        charged: 0,
                    },
                );
            }
        }
        Ok(())
    }

    /// Decide whether a request from `ip` may be served. `Err` is the reason for the 402.
    pub fn admit(&self, ip: IpAddr, voucher_hex: Option<&str>) -> Result<(), String> {
        let r = self.admit_inner(ip, voucher_hex);
        if r.is_err() {
            self.metrics.inc("payment_required");
        }
        r
    }

    fn admit_inner(&self, ip: IpAddr, voucher_hex: Option<&str>) -> Result<(), String> {
        let Some(h) = voucher_hex else {
            let now = Instant::now();
            let mut s = self.st.lock().unwrap_or_else(|e| e.into_inner());
            if s.clients.len() > 10_000 {
                let w = self.cfg.window;
                s.clients.retain(|_, (t, _)| now.duration_since(*t) < w);
            }
            let e = s.clients.entry(ip).or_insert((now, 0));
            if now.duration_since(e.0) >= self.cfg.window {
                *e = (now, 0);
            }
            if e.1 < self.cfg.free_requests {
                e.1 += 1;
                drop(s);
                self.metrics.inc("free_served");
                return Ok(());
            }
            return Err("free quota used up: send a voucher in X-BTB-Voucher".into());
        };
        let sv = btb_storage::unhex(h)
            .ok()
            .and_then(|b| SignedVoucher::from_bytes(&b))
            .ok_or_else(|| "X-BTB-Voucher is not a 120-byte voucher in hex".to_string());
        let sv = sv.inspect_err(|_| self.metrics.inc("vouchers_rejected"))?;
        self.ensure_channel(&sv)
            .inspect_err(|_| self.metrics.inc("vouchers_rejected"))?;
        let mut s = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let c = s
            .channels
            .get_mut(&sv.voucher.channel_id)
            .ok_or("channel was settled")?;
        match c.ledger.accept(&sv) {
            Ok(_) => self.metrics.inc("vouchers_accepted"),
            Err(e) => {
                self.metrics.inc("vouchers_rejected");
                return Err(format!("voucher refused: {}", describe(e)));
            }
        }
        let need = c.charged.saturating_add(self.cfg.price);
        if c.ledger.earned() < need {
            return Err(format!(
                "voucher pays {} in total but {need} is owed: raise the amount",
                c.ledger.earned()
            ));
        }
        c.charged = need;
        drop(s);
        self.metrics.inc("paid_served");
        Ok(())
    }

    /// Settle every channel that earned at least `redeem_min` with a `Close` transaction.
    pub fn redeem(&self) -> Vec<Redeemed> {
        let Some(sub) = &self.submitter else {
            return Vec::new();
        };
        let due: Vec<(ChannelId, SignedVoucher)> = {
            let s = self.st.lock().unwrap_or_else(|e| e.into_inner());
            s.channels
                .iter()
                .filter_map(|(id, c)| {
                    let b = c.ledger.best()?;
                    (c.ledger.earned() >= self.cfg.redeem_min.max(1)).then_some((*id, *b))
                })
                .collect()
        };
        let mut out = Vec::new();
        for (id, v) in due {
            let result = sub.close(&v);
            match &result {
                Ok(()) => {
                    // Closed on chain: the channel no longer exists.
                    self.st
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .channels
                        .remove(&id);
                    self.metrics.inc("redeem_txs");
                    self.metrics.add("redeemed_units", v.voucher.amount);
                }
                Err(_) => self.metrics.inc("redeem_failures"),
            }
            out.push(Redeemed {
                channel: id,
                amount: v.voucher.amount,
                result,
            });
        }
        out
    }
}

fn describe(e: VoucherError) -> &'static str {
    match e {
        VoucherError::WrongChain => "wrong chain",
        VoucherError::WrongChannel => "wrong channel",
        VoucherError::BadSignature => "bad signature",
        VoucherError::NotIncreasing => "amount does not exceed the last voucher",
        VoucherError::StaleNonce => "nonce does not exceed the last voucher",
        VoucherError::ExceedsDeposit => "amount exceeds the channel deposit",
        VoucherError::BadAmount => "bad amount",
    }
}
