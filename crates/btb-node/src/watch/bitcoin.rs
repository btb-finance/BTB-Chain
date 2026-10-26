//! The Bitcoin watcher: deposits to the vault's Taproot address, withdrawal transactions, and
//! their confirmation (`docs/CROSS_CHAIN.md`, "Chain watchers").
//!
//! Two interchangeable backends, the operator's choice: an **Esplora-compatible REST API**
//! (blockstream/electrs/mempool.space style) or a **bitcoind JSON-RPC** endpoint. Both are
//! parsed strictly: raw transactions and blocks are decoded with the `bitcoin` crate, the txid
//! is recomputed from the bytes (never taken from the answer), a bitcoind block must hash to the
//! hash that was asked for and have a valid merkle root, and anything malformed drops the
//! observation (no claim) instead of being guessed at.

use super::http::Http;
use ::bitcoin::consensus::{deserialize, serialize};
use ::bitcoin::hashes::Hash;
use ::bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use ::bitcoin::{
    absolute::LockTime, transaction::Version, Amount, Block, OutPoint, ScriptBuf, Sequence,
    Transaction, TxIn, TxOut, Txid, Witness,
};
use btb_chain::vault::{DepositClaim, Execution, Prepared, VaultUtxo, Withdrawal};
use btb_chain::vault_plan::DUST;
use btb_oracle::frost::{addr, Scheme};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Backend {
    /// Base URL of an Esplora REST API, e.g. `https://blockstream.info/api`.
    Esplora { url: String },
    /// bitcoind JSON-RPC; `auth` is a ready `Authorization` header value (`Basic ...`).
    Rpc { url: String, auth: Option<String> },
}

#[derive(Debug, Clone)]
pub struct BtcConfig {
    pub backend: Backend,
    /// Bech32 prefix of the network: `bc`, `tb` or `bcrt`.
    pub hrp: String,
    /// A deposit counts when its block has at least this many confirmations (including itself).
    pub confirmations: u32,
    /// Fee rate floor and default, sat/vB. Validators must agree on the fee a proposal uses, so
    /// estimates are rounded up to multiples of [`FEE_BUCKET`] and never below this.
    pub fee_rate: u64,
    /// If set, ask the backend for an estimate for this many blocks.
    pub fee_estimate_blocks: Option<u32>,
    /// bitcoind only: how many blocks (ending at the deepest confirmed one) are scanned.
    pub scan_blocks: u32,
    /// Esplora only: pages of 25 transactions read per poll.
    pub max_pages: u32,
    pub poll: Duration,
    pub timeout: Duration,
    pub allow_http: bool,
}

impl BtcConfig {
    pub fn new(backend: Backend) -> BtcConfig {
        BtcConfig {
            backend,
            hrp: "bc".into(),
            confirmations: 6,
            fee_rate: 10,
            fee_estimate_blocks: None,
            scan_blocks: 144,
            max_pages: 8,
            poll: Duration::from_secs(10),
            timeout: Duration::from_secs(10),
            allow_http: false,
        }
    }
}

/// Fee rates are rounded up to multiples of this (sat/vB) so that validators asking at slightly
/// different moments still build the same transaction.
pub const FEE_BUCKET: u64 = 5;
const MAX_FEE_RATE: u64 = 1_000;

struct Job {
    raw: Vec<u8>,
    txid: Txid,
    state: Execution,
}

#[derive(Default)]
struct Shared {
    vault: Option<[u8; 32]>,
    deposits: Vec<DepositClaim>,
    fee_rate: u64,
    jobs: BTreeMap<u64, Job>,
    last_error: Option<String>,
    polls: u64,
}

pub struct BitcoinWatcher {
    cfg: BtcConfig,
    http: Http,
    shared: Mutex<Shared>,
}

impl std::fmt::Debug for BitcoinWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BitcoinWatcher({:?})", self.cfg.backend)
    }
}

/// An output paying the vault, as seen in a block.
#[derive(Debug, Clone)]
struct Seen {
    txid: [u8; 32],
    vout: u32,
    value: u64,
    height: u64,
    block_hash: [u8; 32],
    memo: [u8; 32],
}

/// The recipient named by a transaction: exactly one `OP_RETURN` output, of the form
/// `6a 20 <32 bytes>`. Anything else (none, several, other payloads) means no memo.
pub fn memo_of(tx: &Transaction) -> Option<[u8; 32]> {
    let mut found = None;
    for o in &tx.output {
        let s = o.script_pubkey.as_bytes();
        if s.first() == Some(&0x6a) {
            if found.is_some() || s.len() != 34 || s[1] != 0x20 {
                return None;
            }
            found = s[2..].try_into().ok();
        }
    }
    found
}

fn txid_bytes(t: &Txid) -> [u8; 32] {
    t.to_raw_hash().to_byte_array()
}

fn rev(h: [u8; 32]) -> [u8; 32] {
    let mut h = h;
    h.reverse();
    h
}

impl BitcoinWatcher {
    pub fn new(cfg: BtcConfig) -> Result<Arc<BitcoinWatcher>, String> {
        if cfg.confirmations == 0 {
            return Err("bitcoin: confirmations must be at least 1".into());
        }
        if !["bc", "tb", "bcrt"].contains(&cfg.hrp.as_str()) {
            return Err("bitcoin: network must be mainnet, testnet, signet or regtest".into());
        }
        let http = Http::new(cfg.timeout, 8 << 20, cfg.allow_http);
        let url = match &cfg.backend {
            Backend::Esplora { url } | Backend::Rpc { url, .. } => url,
        };
        http.check_url(url)?;
        let fee_rate = cfg.fee_rate.clamp(1, MAX_FEE_RATE);
        Ok(Arc::new(BitcoinWatcher {
            http,
            shared: Mutex::new(Shared {
                fee_rate,
                ..Default::default()
            }),
            cfg,
        }))
    }

    fn sh(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().expect("bitcoin watcher state")
    }

    pub fn poll_interval(&self) -> Duration {
        self.cfg.poll
    }

    pub fn last_error(&self) -> Option<String> {
        self.sh().last_error.clone()
    }

    pub fn polls(&self) -> u64 {
        self.sh().polls
    }

    /// Tell the watcher which vault key to watch.
    pub fn set_vault(&self, key: [u8; 32]) {
        self.sh().vault = Some(key);
    }

    pub fn deposits(&self) -> Vec<DepositClaim> {
        self.sh().deposits.clone()
    }

    // ------------------------------------------------------------ backend calls

    fn esplora(&self, path: &str) -> Result<Vec<u8>, String> {
        let Backend::Esplora { url } = &self.cfg.backend else {
            return Err("not esplora".into());
        };
        self.http
            .get(&format!("{}{path}", url.trim_end_matches('/')))
    }

    fn rpc(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let Backend::Rpc { url, auth } = &self.cfg.backend else {
            return Err("not rpc".into());
        };
        self.http.json_rpc(url, auth.as_deref(), method, params)
    }

    fn text(&self, path: &str) -> Result<String, String> {
        String::from_utf8(self.esplora(path)?).map_err(|_| "not utf-8".to_string())
    }

    pub fn tip(&self) -> Result<u64, String> {
        match &self.cfg.backend {
            Backend::Esplora { .. } => self
                .text("/blocks/tip/height")?
                .trim()
                .parse()
                .map_err(|_| "tip height is not a number".into()),
            Backend::Rpc { .. } => self
                .rpc("getblockcount", serde_json::json!([]))?
                .as_u64()
                .ok_or_else(|| "getblockcount: not a number".into()),
        }
    }

    fn hash_at(&self, h: u64) -> Result<[u8; 32], String> {
        let s = match &self.cfg.backend {
            Backend::Esplora { .. } => self.text(&format!("/block-height/{h}"))?,
            Backend::Rpc { .. } => self
                .rpc("getblockhash", serde_json::json!([h]))?
                .as_str()
                .ok_or("getblockhash: not a string")?
                .to_string(),
        };
        parse_hash(s.trim())
    }

    // ------------------------------------------------------------ deposits

    /// Final deposits to `vault` according to this backend.
    fn scan(&self, vault: &[u8; 32]) -> Result<Vec<DepositClaim>, String> {
        let tip = self.tip()?;
        let n = u64::from(self.cfg.confirmations);
        let script = ScriptBuf::from_bytes(addr::taproot_script_pubkey(vault));
        let seen = match &self.cfg.backend {
            Backend::Esplora { .. } => self.scan_esplora(vault, &script, tip, n)?,
            Backend::Rpc { .. } => self.scan_rpc(&script, tip, n)?,
        };
        Ok(seen
            .into_iter()
            .map(|s| DepositClaim {
                asset: Scheme::Taproot,
                txid: s.txid,
                index: s.vout,
                amount: s.value,
                recipient: s.memo,
                height: s.height,
                block_hash: s.block_hash,
            })
            .collect())
    }

    fn scan_esplora(
        &self,
        vault: &[u8; 32],
        script: &ScriptBuf,
        tip: u64,
        n: u64,
    ) -> Result<Vec<Seen>, String> {
        let address = btb_chain::vault::deposit_address(Scheme::Taproot, vault, &self.cfg.hrp);
        let mut listed = Vec::new();
        let mut last: Option<String> = None;
        for _ in 0..self.cfg.max_pages {
            let path = match &last {
                None => format!("/address/{address}/txs"),
                Some(t) => format!("/address/{address}/txs/chain/{t}"),
            };
            let v: serde_json::Value =
                serde_json::from_slice(&self.esplora(&path)?).map_err(|e| format!("txs: {e}"))?;
            let arr = v.as_array().ok_or("txs: not an array")?;
            if arr.is_empty() {
                break;
            }
            for t in arr {
                let txid = t
                    .get("txid")
                    .and_then(|x| x.as_str())
                    .ok_or("txs: entry without txid")?
                    .to_string();
                let st = t.get("status").ok_or("txs: entry without status")?;
                if st.get("confirmed").and_then(|x| x.as_bool()) != Some(true) {
                    continue;
                }
                let height = st
                    .get("block_height")
                    .and_then(|x| x.as_u64())
                    .ok_or("txs: confirmed without height")?;
                let bh = parse_hash(
                    st.get("block_hash")
                        .and_then(|x| x.as_str())
                        .ok_or("txs: confirmed without block hash")?,
                )?;
                listed.push((txid.clone(), height, bh));
                last = Some(txid);
            }
            if arr.len() < 25 {
                break;
            }
        }
        let mut out = Vec::new();
        let mut hash_cache: BTreeMap<u64, [u8; 32]> = BTreeMap::new();
        for (txid, height, bh) in listed {
            if height + n > tip + 1 {
                continue; // not deep enough yet
            }
            // the raw transaction decides what was paid; its txid must be the one named
            let raw = crate::hex::decode(self.text(&format!("/tx/{txid}/hex"))?.trim())?;
            let tx: Transaction = match deserialize(&raw) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if tx.compute_txid().to_string() != txid {
                return Err(format!("tx {txid}: bytes hash to another txid"));
            }
            // the block must still be the one at that height
            let cur = match hash_cache.get(&height) {
                Some(h) => *h,
                None => {
                    let h = self.hash_at(height)?;
                    hash_cache.insert(height, h);
                    h
                }
            };
            if cur != bh {
                continue;
            }
            collect(&tx, script, height, bh, &mut out);
        }
        Ok(out)
    }

    fn scan_rpc(&self, script: &ScriptBuf, tip: u64, n: u64) -> Result<Vec<Seen>, String> {
        if tip + 1 < n {
            return Ok(Vec::new());
        }
        let hi = tip + 1 - n; // the deepest height that still has n confirmations... or deeper
        let lo = hi.saturating_sub(u64::from(self.cfg.scan_blocks).saturating_sub(1));
        let mut out = Vec::new();
        let mut hi_hash = [0u8; 32];
        for h in lo..=hi {
            let hash = self.hash_at(h)?;
            hi_hash = hash;
            let hex = self.rpc(
                "getblock",
                serde_json::json!([crate::hex::encode(&hash), 0]),
            )?;
            let raw = crate::hex::decode(hex.as_str().ok_or("getblock: not a string")?)?;
            let block: Block = deserialize(&raw).map_err(|e| format!("block {h}: {e}"))?;
            if rev(block.block_hash().to_byte_array()) != hash || !block.check_merkle_root() {
                return Err(format!("block {h}: header or merkle root does not match"));
            }
            for tx in &block.txdata {
                collect(tx, script, h, hash, &mut out);
            }
        }
        // a reorg while we were reading: drop the whole observation
        if self.hash_at(hi)? != hi_hash {
            return Err("chain reorganised during the scan".into());
        }
        Ok(out)
    }

    // ------------------------------------------------------------ fees

    fn estimate_fee(&self) -> Result<Option<u64>, String> {
        let Some(blocks) = self.cfg.fee_estimate_blocks else {
            return Ok(None);
        };
        let rate = match &self.cfg.backend {
            Backend::Esplora { .. } => {
                let v: serde_json::Value = serde_json::from_slice(&self.esplora("/fee-estimates")?)
                    .map_err(|e| format!("fee-estimates: {e}"))?;
                let o = v.as_object().ok_or("fee-estimates: not an object")?;
                let mut best: Option<(u32, f64)> = None;
                for (k, x) in o {
                    let (Ok(k), Some(x)) = (k.parse::<u32>(), x.as_f64()) else {
                        return Err("fee-estimates: malformed entry".into());
                    };
                    // the estimate for the closest target not above ours
                    if k <= blocks && best.is_none_or(|(b, _)| k > b) {
                        best = Some((k, x));
                    }
                }
                best.map(|(_, x)| x)
            }
            Backend::Rpc { .. } => self
                .rpc("estimatesmartfee", serde_json::json!([blocks]))?
                .get("feerate")
                .and_then(|x| x.as_f64())
                .map(|btc_per_kvb| btc_per_kvb * 1e5),
        };
        Ok(rate.filter(|r| r.is_finite() && *r > 0.0).map(|r| {
            let r = r.ceil() as u64;
            r.div_ceil(FEE_BUCKET) * FEE_BUCKET
        }))
    }

    // ------------------------------------------------------------ the poll

    /// One round: deposits, fee rate, and the state of broadcast withdrawals.
    pub fn poll(&self) {
        let vault = self.sh().vault;
        let mut err = None;
        if let Some(v) = vault {
            match self.scan(&v) {
                Ok(d) => self.sh().deposits = d,
                Err(e) => err = Some(format!("deposits: {e}")),
            }
        }
        match self.estimate_fee() {
            Ok(Some(r)) => {
                self.sh().fee_rate = r.clamp(self.cfg.fee_rate.max(1), MAX_FEE_RATE);
            }
            Ok(None) => {}
            Err(e) => err = Some(format!("fee estimate: {e}")),
        }
        if let Err(e) = self.drive_jobs() {
            err = Some(format!("broadcast: {e}"));
        }
        let mut s = self.sh();
        s.polls += 1;
        if err.is_some() {
            s.last_error = err;
        }
    }

    fn drive_jobs(&self) -> Result<(), String> {
        let todo: Vec<(u64, Vec<u8>, Txid, Execution)> = self
            .sh()
            .jobs
            .iter()
            .filter(|(_, j)| j.state != Execution::Confirmed && j.state != Execution::Failed)
            .map(|(id, j)| (*id, j.raw.clone(), j.txid, j.state))
            .collect();
        for (id, raw, txid, state) in todo {
            let mut new = state;
            if state == Execution::Unknown {
                match self.send(&raw) {
                    Ok(()) => new = Execution::Pending,
                    Err(e) if is_known_error(&e) => new = Execution::Pending,
                    Err(e) => return Err(e),
                }
            }
            if new == Execution::Pending {
                new = self.status(&txid)?;
            }
            if let Some(j) = self.sh().jobs.get_mut(&id) {
                j.state = new;
            }
        }
        Ok(())
    }

    fn send(&self, raw: &[u8]) -> Result<(), String> {
        let hex = crate::hex::encode(raw);
        match &self.cfg.backend {
            Backend::Esplora { url } => {
                let (s, b) = self.http.request(
                    "POST",
                    &format!("{}/tx", url.trim_end_matches('/')),
                    &[("content-type", "text/plain".into())],
                    Some(hex.as_bytes()),
                )?;
                if (200..300).contains(&s) {
                    Ok(())
                } else {
                    Err(format!(
                        "broadcast refused ({s}): {}",
                        String::from_utf8_lossy(&b)
                    ))
                }
            }
            Backend::Rpc { .. } => self
                .rpc("sendrawtransaction", serde_json::json!([hex]))
                .map(|_| ()),
        }
    }

    /// Where a broadcast transaction stands.
    fn status(&self, txid: &Txid) -> Result<Execution, String> {
        let n = u64::from(self.cfg.confirmations);
        match &self.cfg.backend {
            Backend::Esplora { .. } => {
                let (s, _) = self.status_raw_esplora(txid)?;
                match s {
                    None => Ok(Execution::Unknown),
                    Some(None) => Ok(Execution::Pending),
                    Some(Some(h)) => {
                        let tip = self.tip()?;
                        Ok(if tip + 1 >= h + n {
                            Execution::Confirmed
                        } else {
                            Execution::Pending
                        })
                    }
                }
            }
            Backend::Rpc { url, auth } => {
                let r = self.http.json_rpc(
                    url,
                    auth.as_deref(),
                    "getrawtransaction",
                    serde_json::json!([txid.to_string(), true]),
                );
                match r {
                    Err(e) if e.contains("-5") => Ok(Execution::Unknown),
                    Err(e) => Err(e),
                    Ok(v) => Ok(match v.get("confirmations").and_then(|c| c.as_u64()) {
                        Some(c) if c >= n => Execution::Confirmed,
                        _ => Execution::Pending,
                    }),
                }
            }
        }
    }

    /// `(Some(None))`: in the mempool; `Some(Some(height))`: confirmed; `None`: unknown.
    #[allow(clippy::type_complexity)]
    fn status_raw_esplora(&self, txid: &Txid) -> Result<(Option<Option<u64>>, ()), String> {
        let Backend::Esplora { url } = &self.cfg.backend else {
            return Err("not esplora".into());
        };
        let (s, b) = self.http.request(
            "GET",
            &format!("{}/tx/{txid}/status", url.trim_end_matches('/')),
            &[],
            None,
        )?;
        if s == 404 {
            return Ok((None, ()));
        }
        if !(200..300).contains(&s) {
            return Err(format!("tx status: http {s}"));
        }
        let v: serde_json::Value =
            serde_json::from_slice(&b).map_err(|e| format!("status: {e}"))?;
        match v.get("confirmed").and_then(|c| c.as_bool()) {
            Some(true) => Ok((
                Some(Some(
                    v.get("block_height")
                        .and_then(|h| h.as_u64())
                        .ok_or("status: no height")?,
                )),
                (),
            )),
            Some(false) => Ok((Some(None), ())),
            None => Err("status: malformed".into()),
        }
    }

    // ------------------------------------------------------------ withdrawals

    pub fn fee_rate(&self) -> u64 {
        self.sh().fee_rate
    }

    /// Build the unsigned transaction for a withdrawal from the vault's UTXOs.
    pub fn prepare(
        &self,
        w: &Withdrawal,
        vault: &[u8; 32],
        utxos: &[VaultUtxo],
    ) -> Option<Prepared> {
        build(w, vault, utxos, self.fee_rate())
    }

    /// Assemble the signed transaction from the plan and the signatures and queue it for
    /// broadcast (the poll thread sends it). Idempotent.
    pub fn broadcast(&self, w: &Withdrawal, signatures: &[Vec<u8>]) {
        if self.sh().jobs.contains_key(&w.id) {
            return;
        }
        let Some((raw, txid)) = assemble(&w.plan, signatures) else {
            self.sh().last_error = Some(format!("withdrawal {}: cannot assemble", w.id));
            return;
        };
        self.sh().jobs.insert(
            w.id,
            Job {
                raw,
                txid,
                state: Execution::Unknown,
            },
        );
    }

    pub fn execution(&self, id: u64) -> Execution {
        self.sh()
            .jobs
            .get(&id)
            .map_or(Execution::Unknown, |j| j.state)
    }

    /// The txid (display order) of a queued withdrawal.
    pub fn txid_of(&self, id: u64) -> Option<String> {
        self.sh().jobs.get(&id).map(|j| j.txid.to_string())
    }
}

fn is_known_error(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    e.contains("already") || e.contains("txn-already-known") || e.contains("-27")
}

fn parse_hash(s: &str) -> Result<[u8; 32], String> {
    crate::hex::decode32(s)
}

fn collect(
    tx: &Transaction,
    script: &ScriptBuf,
    height: u64,
    block_hash: [u8; 32],
    out: &mut Vec<Seen>,
) {
    let Some(memo) = memo_of(tx) else {
        return; // no (or an ambiguous) recipient: never claimed
    };
    let txid = txid_bytes(&tx.compute_txid());
    for (i, o) in tx.output.iter().enumerate() {
        if &o.script_pubkey == script && o.value.to_sat() > 0 {
            out.push(Seen {
                txid,
                vout: i as u32,
                value: o.value.to_sat(),
                height,
                block_hash,
                memo,
            });
        }
    }
}

/// Weight units of the withdrawal transaction, key-path spends only.
fn vsize(n_in: usize, out0_script: usize, change: bool) -> u64 {
    let mut w = 42 + n_in as u64 * (41 * 4 + 66) + (8 + 1 + out0_script as u64) * 4;
    if change {
        w += (8 + 1 + 34) * 4;
    }
    w.div_ceil(4)
}

/// The deterministic Bitcoin withdrawal: coin selection from chain state, output 0 pays the
/// destination `amount - fee`, output 1 returns change to the vault.
pub fn build(
    w: &Withdrawal,
    vault: &[u8; 32],
    utxos: &[VaultUtxo],
    fee_rate: u64,
) -> Option<Prepared> {
    let coins = btb_chain::vault::select_coins(utxos, w.amount)?;
    let sum: u64 = coins.iter().map(|c| c.amount).sum();
    let change = sum - w.amount;
    let has_change = change >= DUST;
    let fee = vsize(coins.len(), w.destination.len(), has_change).checked_mul(fee_rate)?;
    let pay = w.amount.checked_sub(fee).filter(|p| *p >= DUST)?;
    let vault_script = ScriptBuf::from_bytes(addr::taproot_script_pubkey(vault));
    let mut output = vec![TxOut {
        value: Amount::from_sat(pay),
        script_pubkey: ScriptBuf::from_bytes(w.destination.clone()),
    }];
    if has_change {
        output.push(TxOut {
            value: Amount::from_sat(change),
            script_pubkey: vault_script.clone(),
        });
    }
    let tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: coins
            .iter()
            .map(|c| TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array(c.txid), c.vout),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            })
            .collect(),
        output,
    };
    let prevouts: Vec<TxOut> = coins
        .iter()
        .map(|c| TxOut {
            value: Amount::from_sat(c.amount),
            script_pubkey: vault_script.clone(),
        })
        .collect();
    let mut cache = SighashCache::new(&tx);
    let mut sighashes = Vec::new();
    for i in 0..coins.len() {
        let h = cache
            .taproot_key_spend_signature_hash(i, &Prevouts::All(&prevouts), TapSighashType::Default)
            .ok()?;
        sighashes.push(h.to_byte_array());
    }
    Some(Prepared {
        digest: txid_bytes(&tx.compute_txid()),
        sighashes,
        plan: serialize(&tx),
    })
}

/// Attach the 64-byte BIP-340 signatures to the planned transaction.
pub fn assemble(plan: &[u8], signatures: &[Vec<u8>]) -> Option<(Vec<u8>, Txid)> {
    let mut tx: Transaction = deserialize(plan).ok()?;
    if tx.input.len() != signatures.len() || serialize(&tx) != plan {
        return None;
    }
    for (i, s) in signatures.iter().enumerate() {
        if s.len() != 64 {
            return None;
        }
        tx.input[i].witness = Witness::from_slice(&[s.as_slice()]);
    }
    Some((serialize(&tx), tx.compute_txid()))
}
