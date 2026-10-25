//! BTB testnet faucet.
//!
//! A small HTTP service holding one funded key. `POST /drip {"address": hex}` sends a `Transfer`
//! from that key through a `btb-node` JSON-RPC endpoint. Per-address and per-IP rate limits,
//! a maximum amount, nonce management with retry and an optional proof of work slow abuse.
//! See `docs/TESTNET.md`. The faucet key is a hot wallet: fund it with testnet coins only.

pub mod limits;
pub mod pow;
pub mod rpc;
pub mod server;

use btb_chain::{account_address, Address, SigningKey, Transaction, TxBody, TxKind};
use limits::{Denied, LimitConfig, Limits};
use rpc::{RpcClient, RpcFail};
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const UNITS_PER_BTB: u64 = btb_chain::UNITS_PER_BTB;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// Node RPC endpoints, `host:port`.
    pub rpc: Vec<String>,
    /// Sent when a request names no amount (base units).
    pub default_amount: u64,
    /// Largest amount one request may ask for (base units).
    pub max_amount: u64,
    pub limits: LimitConfig,
    /// Leading zero bits required of the proof of work; 0 turns it off.
    pub pow_bits: u32,
    /// Take the client IP from the last `X-Forwarded-For` entry (set only behind a proxy that
    /// appends the peer address, such as nginx `$proxy_add_x_forwarded_for`).
    pub trust_proxy: bool,
}

impl Config {
    pub fn new(rpc: Vec<String>) -> Self {
        Config {
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            rpc,
            default_amount: 100 * UNITS_PER_BTB,
            max_amount: 1_000 * UNITS_PER_BTB,
            limits: LimitConfig {
                address_cooldown: Duration::from_secs(24 * 3600),
                ip_max: 10,
                ip_window: Duration::from_secs(24 * 3600),
            },
            pow_bits: 0,
            trust_proxy: false,
        }
    }
}

/// A failed request: HTTP status, message, optional seconds to wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DripError {
    pub status: u16,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl DripError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        DripError {
            status,
            message: message.into(),
            retry_after: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dripped {
    pub tx_id: String,
    pub nonce: u64,
    pub amount: u64,
}

/// A request body for `/drip`.
pub struct DripRequest {
    pub address: String,
    /// Whole BTB (`None`: the default).
    pub amount_btb: Option<u64>,
    /// (challenge hex, nonce)
    pub pow: Option<(String, u64)>,
}

impl DripRequest {
    pub fn from_json(v: &Value) -> Result<Self, DripError> {
        let address = v
            .get("address")
            .and_then(Value::as_str)
            .ok_or_else(|| DripError::new(400, "missing `address`"))?
            .to_string();
        let num = |x: &Value| -> Option<u64> {
            x.as_u64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        };
        let amount_btb = match v.get("amount") {
            None | Some(Value::Null) => None,
            Some(a) => Some(
                num(a)
                    .ok_or_else(|| DripError::new(400, "`amount` must be a whole number of BTB"))?,
            ),
        };
        let pow = match v.get("pow") {
            None | Some(Value::Null) => None,
            Some(p) => {
                let c = p
                    .get("challenge")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DripError::new(400, "`pow.challenge` missing"))?;
                let n = p
                    .get("nonce")
                    .and_then(num)
                    .ok_or_else(|| DripError::new(400, "`pow.nonce` missing"))?;
                Some((c.to_string(), n))
            }
        };
        Ok(DripRequest {
            address,
            amount_btb,
            pow,
        })
    }
}

pub fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

pub fn parse_address(s: &str) -> Option<Address> {
    parse_hex(s)?.try_into().ok()
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Read a key file `{"secret": "<64 hex digits>"}` (the format `btb-node init-testnet` writes).
pub fn load_key(path: &std::path::Path) -> Result<SigningKey, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read key {}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("key json: {e}"))?;
    let secret = v
        .get("secret")
        .and_then(Value::as_str)
        .ok_or("key file needs a `secret` field")?;
    let b: [u8; 32] = parse_hex(secret)
        .and_then(|b| b.try_into().ok())
        .ok_or("secret must be 64 hex digits")?;
    Ok(SigningKey::from_bytes(&b))
}

/// Nonce bookkeeping, held across the whole "pick a nonce, sign, submit" step so concurrent
/// drips never choose the same one.
struct Sender {
    /// Next nonce to use: at least the chain's, ahead of it while transfers are pending.
    next: u64,
    /// Chain nonce seen last, and when it last moved (to notice a stuck pending transfer).
    chain_nonce: u64,
    moved_at: Instant,
}

/// How long pending transfers may sit without the chain nonce moving before the faucet
/// starts over from the chain's nonce.
const STUCK_AFTER: Duration = Duration::from_secs(60);
const SEND_ATTEMPTS: usize = 8;

pub struct Faucet {
    pub cfg: Config,
    key: SigningKey,
    addr: Address,
    rpc: RpcClient,
    chain_id: Mutex<Option<u64>>,
    limits: Mutex<Limits>,
    pow: Mutex<pow::Issuer>,
    sender: Mutex<Sender>,
}

impl Faucet {
    pub fn new(cfg: Config, key: SigningKey) -> Self {
        let addr = account_address(&key.verifying_key().to_bytes());
        let rpc = RpcClient::new(cfg.rpc.clone());
        Faucet {
            cfg,
            key,
            addr,
            rpc,
            chain_id: Mutex::new(None),
            limits: Mutex::new(Limits::default()),
            pow: Mutex::new(pow::Issuer::default()),
            sender: Mutex::new(Sender {
                next: 0,
                chain_nonce: 0,
                moved_at: Instant::now(),
            }),
        }
    }

    pub fn address(&self) -> Address {
        self.addr
    }

    fn unreachable(e: RpcFail) -> DripError {
        match e {
            RpcFail::Transport(m) => DripError::new(503, format!("node unreachable: {m}")),
            RpcFail::App { message, .. } => DripError::new(502, format!("node error: {message}")),
        }
    }

    fn chain_id(&self) -> Result<u64, DripError> {
        let mut g = self.chain_id.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = *g {
            return Ok(c);
        }
        let st = self
            .rpc
            .call("btb_status", json!([]))
            .map_err(Self::unreachable)?;
        let c = st
            .get("chain_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| DripError::new(502, "node status has no chain_id"))?;
        *g = Some(c);
        Ok(c)
    }

    /// (balance, nonce) of the faucet account on the chain.
    fn account(&self) -> Result<(u64, u64), DripError> {
        let v = self
            .rpc
            .call("btb_getBalance", json!([to_hex(&self.addr)]))
            .map_err(Self::unreachable)?;
        let bal = v
            .get("balance")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| DripError::new(502, "bad balance answer"))?;
        let nonce = v
            .get("nonce")
            .and_then(Value::as_u64)
            .ok_or_else(|| DripError::new(502, "bad balance answer"))?;
        Ok((bal, nonce))
    }

    /// Facts for `GET /info`.
    pub fn info(&self) -> Value {
        let (balance, online) = match self.account() {
            Ok((b, _)) => (Some(b.to_string()), true),
            Err(_) => (None, false),
        };
        json!({
            "address": to_hex(&self.addr),
            "balance": balance,
            "node_reachable": online,
            "chain_id": self.chain_id().ok(),
            "default_amount_btb": self.cfg.default_amount / UNITS_PER_BTB,
            "max_amount_btb": self.cfg.max_amount / UNITS_PER_BTB,
            "address_cooldown_secs": self.cfg.limits.address_cooldown.as_secs(),
            "ip_max": self.cfg.limits.ip_max,
            "ip_window_secs": self.cfg.limits.ip_window.as_secs(),
            "pow_bits": self.cfg.pow_bits,
        })
    }

    /// A proof-of-work challenge for `GET /challenge`.
    pub fn challenge(&self) -> Result<Value, DripError> {
        let c = self
            .pow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .issue(Instant::now())
            .ok_or_else(|| DripError::new(503, "too many open challenges"))?;
        Ok(json!({"challenge": to_hex(&c), "bits": self.cfg.pow_bits}))
    }

    /// Receipt (or `{"status":"pending"}`) of a drip transaction.
    pub fn tx_status(&self, id: &str) -> Result<Value, DripError> {
        if parse_address(id).is_none() {
            return Err(DripError::new(400, "bad transaction id"));
        }
        let r = self
            .rpc
            .call("btb_getReceipt", json!([id]))
            .map_err(Self::unreachable)?;
        Ok(if r.is_null() {
            json!({"status": "pending"})
        } else {
            r
        })
    }

    /// Handle one drip request from `ip`.
    pub fn drip(&self, req: &DripRequest, ip: IpAddr) -> Result<Dripped, DripError> {
        let to = parse_address(&req.address).ok_or_else(|| {
            DripError::new(
                400,
                "bad address: expected 64 hex digits (an account address)",
            )
        })?;
        if to == self.addr {
            return Err(DripError::new(400, "cannot send to the faucet itself"));
        }
        let amount = match req.amount_btb {
            None => self.cfg.default_amount,
            Some(0) => return Err(DripError::new(400, "amount must be at least 1 BTB")),
            Some(b) => b
                .checked_mul(UNITS_PER_BTB)
                .filter(|a| *a <= self.cfg.max_amount)
                .ok_or_else(|| {
                    DripError::new(
                        400,
                        format!(
                            "amount too large: at most {} BTB per request",
                            self.cfg.max_amount / UNITS_PER_BTB
                        ),
                    )
                })?,
        };
        let now = Instant::now();
        if self.cfg.pow_bits > 0 {
            let ok = req
                .pow
                .as_ref()
                .and_then(|(c, n)| {
                    let c: [u8; 16] = parse_hex(c)?.try_into().ok()?;
                    Some(self.pow.lock().unwrap_or_else(|e| e.into_inner()).verify(
                        &c,
                        &to,
                        *n,
                        self.cfg.pow_bits,
                        now,
                    ))
                })
                .unwrap_or(false);
            if !ok {
                return Err(DripError::new(
                    400,
                    "proof of work missing, wrong or already used: GET /challenge first",
                ));
            }
        }
        self.limits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reserve(&self.cfg.limits, to, ip, now)
            .map_err(|d| match d {
                Denied::Address(s) => DripError {
                    status: 429,
                    message: "this address already received coins recently".into(),
                    retry_after: Some(s),
                },
                Denied::Ip(s) => DripError {
                    status: 429,
                    message: "too many requests from your IP address".into(),
                    retry_after: Some(s),
                },
                Denied::Busy => DripError::new(503, "faucet is busy, try again later"),
            })?;
        match self.send_transfer(to, amount) {
            Ok(d) => Ok(d),
            Err(e) => {
                // Nothing was sent: do not make the user wait out the cooldown.
                self.limits
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .release(to, ip);
                Err(e)
            }
        }
    }

    /// Sign and submit one transfer with a fresh nonce, retrying on nonce conflicts.
    fn send_transfer(&self, to: Address, amount: u64) -> Result<Dripped, DripError> {
        let chain_id = self.chain_id()?;
        let mut st = self.sender.lock().unwrap_or_else(|e| e.into_inner());
        let (_, chain_nonce) = self.account()?;
        let now = Instant::now();
        if chain_nonce != st.chain_nonce {
            st.chain_nonce = chain_nonce;
            st.moved_at = now;
        }
        if st.next < chain_nonce {
            st.next = chain_nonce;
        } else if st.next > chain_nonce && now.duration_since(st.moved_at) > STUCK_AFTER {
            // Pending transfers never landed (dropped by the node, or a gap): start over.
            st.next = chain_nonce;
            st.moved_at = now;
        }
        for _ in 0..SEND_ATTEMPTS {
            let nonce = st.next;
            let tx = Transaction::sign(
                TxBody {
                    relayer: None,
                    chain_id,
                    nonce,
                    kind: TxKind::Transfer { to, amount },
                },
                &self.key,
            );
            match self
                .rpc
                .call("btb_sendTransaction", json!([to_hex(&tx.encode())]))
            {
                Ok(v) => {
                    st.next = nonce + 1;
                    let id = v
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| to_hex(&tx.id()));
                    return Ok(Dripped {
                        tx_id: id,
                        nonce,
                        amount,
                    });
                }
                Err(RpcFail::App { message, .. }) => {
                    if let Some(next) = message
                        .split_once("NonceTooLow { next: ")
                        .and_then(|(_, r)| r.split(|c: char| !c.is_ascii_digit()).next())
                        .and_then(|d| d.parse::<u64>().ok())
                    {
                        // The chain moved on (or someone else used our key): follow it.
                        st.next = next.max(nonce + 1);
                    } else if message.contains("NonceTaken") || message.contains("Duplicate") {
                        st.next = nonce + 1;
                    } else if message.contains("InsufficientBalance") {
                        return Err(DripError::new(503, "faucet is out of funds"));
                    } else if message.contains("SenderFull") || message.contains("PoolFull") {
                        return Err(DripError::new(503, "faucet is busy, try again shortly"));
                    } else {
                        return Err(DripError::new(
                            502,
                            format!("node rejected the transfer: {message}"),
                        ));
                    }
                }
                Err(e) => return Err(Self::unreachable(e)),
            }
        }
        Err(DripError::new(
            503,
            "could not find a free nonce, try again",
        ))
    }
}
