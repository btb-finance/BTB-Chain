//! Chain watchers for the vault (`docs/CROSS_CHAIN.md`, "Chain watchers"): the real
//! implementation of [`btb_chain::vault::Observer`].
//!
//! Every validator runs its own watchers against **its own configured endpoints** and votes
//! (`CrossOp::Attest`, `CrossOp::Prepare`) only for what it saw itself. The chain mints at two
//! thirds of the stake agreeing on the identical claim, so a lying or lagging endpoint costs one
//! validator's vote, never a mint.
//!
//! The network I/O runs on one background thread per chain; the driver thread only reads the
//! snapshot that thread keeps (`Observer::deposits`, `prepare_with`) and queues work for it
//! (`broadcast`), so consensus never waits for an RPC.
//!
//! Node config (`"watch"` section of the node's JSON config):
//!
//! ```json
//! "watch": {
//!   "bitcoin": { "esplora": "https://esplora.example/api",     // or "rpc": {"url": ..., "user": ..., "password": ...}
//!                "network": "mainnet", "confirmations": 6, "fee_rate": 10,
//!                "fee_estimate_blocks": 3, "poll_ms": 10000 },
//!   "solana":  { "rpc": "https://solana-rpc.example", "blockhash_window": 16, "poll_ms": 5000 }
//! }
//! ```

pub mod bitcoin;
pub mod http;
pub mod solana;

use btb_chain::vault::{DepositClaim, Execution, Observer, Prepared, VaultUtxo, Withdrawal};
use btb_oracle::frost::Scheme;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use self::bitcoin::{Backend, BitcoinWatcher, BtcConfig};
pub use self::solana::{SolConfig, SolanaWatcher};

/// The validator's view of Bitcoin and Solana.
pub struct Watchers {
    pub bitcoin: Option<Arc<BitcoinWatcher>>,
    pub solana: Option<Arc<SolanaWatcher>>,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for Watchers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watchers")
            .field("bitcoin", &self.bitcoin)
            .field("solana", &self.solana)
            .finish()
    }
}

fn spawn_loop(stop: Arc<AtomicBool>, every: Duration, name: &str, f: impl Fn() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                f();
                let mut left = every;
                while !left.is_zero() && !stop.load(Ordering::Relaxed) {
                    let step = left.min(Duration::from_millis(50));
                    std::thread::sleep(step);
                    left -= step;
                }
            }
        });
}

impl Watchers {
    /// Start the poll threads of the configured chains.
    pub fn start(
        bitcoin: Option<BtcConfig>,
        solana: Option<SolConfig>,
    ) -> Result<Arc<Watchers>, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let bitcoin = bitcoin.map(BitcoinWatcher::new).transpose()?;
        let solana = solana.map(SolanaWatcher::new).transpose()?;
        if let Some(b) = &bitcoin {
            let (w, poll) = (b.clone(), b.poll_interval());
            spawn_loop(stop.clone(), poll, "btb-watch-btc", move || w.poll());
        }
        if let Some(s) = &solana {
            let (w, poll) = (s.clone(), s.poll_interval());
            spawn_loop(stop.clone(), poll, "btb-watch-sol", move || w.poll());
        }
        Ok(Arc::new(Watchers {
            bitcoin,
            solana,
            stop,
        }))
    }

    /// Parse the `"watch"` object of the node config and start the watchers.
    pub fn from_json(v: &Value) -> Result<Arc<Watchers>, String> {
        Watchers::start(
            v.get("bitcoin").map(btc_config).transpose()?,
            v.get("solana").map(sol_config).transpose()?,
        )
    }
}

impl Drop for Watchers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn ms(v: &Value, key: &str, default: Duration) -> Duration {
    v.get(key)
        .and_then(Value::as_u64)
        .map_or(default, Duration::from_millis)
}

fn btc_config(v: &Value) -> Result<BtcConfig, String> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let backend = match (v.get("esplora"), v.get("rpc")) {
        (Some(e), None) => Backend::Esplora {
            url: e
                .as_str()
                .ok_or("watch.bitcoin.esplora must be a URL")?
                .to_string(),
        },
        (None, Some(r)) => {
            let url = r
                .get("url")
                .and_then(Value::as_str)
                .ok_or("watch.bitcoin.rpc.url missing")?;
            let auth = match (
                r.get("user").and_then(Value::as_str),
                r.get("password").and_then(Value::as_str),
            ) {
                (Some(u), Some(p)) => Some(format!(
                    "Basic {}",
                    http::base64(format!("{u}:{p}").as_bytes())
                )),
                (None, None) => None,
                _ => return Err("watch.bitcoin.rpc: give both user and password".into()),
            };
            Backend::Rpc {
                url: url.to_string(),
                auth,
            }
        }
        _ => return Err("watch.bitcoin: set exactly one of `esplora` and `rpc`".into()),
    };
    let mut c = BtcConfig::new(backend);
    c.hrp = match s("network").as_deref().unwrap_or("mainnet") {
        "mainnet" => "bc",
        "testnet" | "signet" => "tb",
        "regtest" => "bcrt",
        o => return Err(format!("watch.bitcoin.network: unknown network {o}")),
    }
    .into();
    let u = |k: &str| v.get(k).and_then(Value::as_u64);
    if let Some(n) = u("confirmations") {
        c.confirmations = u32::try_from(n).map_err(|_| "watch.bitcoin.confirmations too large")?;
    }
    c.fee_rate = u("fee_rate").unwrap_or(c.fee_rate);
    c.fee_estimate_blocks = u("fee_estimate_blocks").map(|n| n as u32);
    c.scan_blocks = u("scan_blocks").map_or(c.scan_blocks, |n| n as u32);
    c.max_pages = u("max_pages").map_or(c.max_pages, |n| n as u32);
    c.poll = ms(v, "poll_ms", c.poll);
    c.timeout = ms(v, "timeout_ms", c.timeout);
    c.allow_http = v
        .get("allow_http")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(c)
}

fn sol_config(v: &Value) -> Result<SolConfig, String> {
    let url = v
        .get("rpc")
        .and_then(Value::as_str)
        .ok_or("watch.solana.rpc missing")?;
    let mut c = SolConfig::new(url.to_string());
    let u = |k: &str| v.get(k).and_then(Value::as_u64);
    c.blockhash_window = u("blockhash_window").unwrap_or(c.blockhash_window);
    c.signature_limit = u("signature_limit").map_or(c.signature_limit, |n| n as u32);
    c.poll = ms(v, "poll_ms", c.poll);
    c.timeout = ms(v, "timeout_ms", c.timeout);
    c.allow_http = v
        .get("allow_http")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(a) = v.get("auth_header").and_then(Value::as_str) {
        c.auth = Some(a.to_string());
    }
    Ok(c)
}

impl Observer for Watchers {
    fn deposits(&self, scheme: Scheme, vault_key: &[u8; 32]) -> Vec<DepositClaim> {
        match scheme {
            Scheme::Taproot => self.bitcoin.as_ref().map(|b| {
                b.set_vault(*vault_key);
                b.deposits()
            }),
            Scheme::Ed25519 => self.solana.as_ref().map(|s| {
                s.set_vault(*vault_key);
                s.deposits()
            }),
        }
        .unwrap_or_default()
    }

    fn prepare(&self, _w: &Withdrawal, _vault_key: &[u8; 32]) -> Option<Prepared> {
        None
    }

    fn prepare_with(
        &self,
        w: &Withdrawal,
        vault_key: &[u8; 32],
        utxos: &[VaultUtxo],
    ) -> Option<Prepared> {
        match w.asset {
            Scheme::Taproot => self.bitcoin.as_ref()?.prepare(w, vault_key, utxos),
            Scheme::Ed25519 => self.solana.as_ref()?.prepare(w, vault_key),
        }
    }

    fn broadcast(&self, w: &Withdrawal, _vault_key: &[u8; 32], signatures: &[Vec<u8>]) {
        match w.asset {
            Scheme::Taproot => {
                if let Some(b) = &self.bitcoin {
                    b.broadcast(w, signatures);
                }
            }
            Scheme::Ed25519 => {
                if let Some(s) = &self.solana {
                    s.broadcast(w, signatures);
                }
            }
        }
    }

    fn execution(&self, w: &Withdrawal) -> Execution {
        match w.asset {
            Scheme::Taproot => self
                .bitcoin
                .as_ref()
                .map_or(Execution::Confirmed, |b| b.execution(w.id)),
            Scheme::Ed25519 => self
                .solana
                .as_ref()
                .map_or(Execution::Confirmed, |s| s.execution(w.id)),
        }
    }
}
