//! The Solana watcher: SOL transfers to the vault (finalized commitment only), withdrawal
//! transactions, and the agreed recent blockhash (`docs/CROSS_CHAIN.md`, "Chain watchers").
//!
//! * **Deposits.** `getSignaturesForAddress` lists the vault's recent signatures; each is
//!   fetched with `getTransaction` (finalized, `json` encoding) and parsed strictly: the
//!   transaction must have succeeded, carry the signature that was asked for, have no address
//!   table lookups, contain top-level System `Transfer` instructions into the vault, exactly one
//!   Memo instruction with the recipient as 64 hex digits, and the vault's balance must have
//!   grown by at least the transferred amount.
//! * **Withdrawals.** One System `Transfer` from the vault. The recent blockhash is part of what
//!   the validators sign, so it has to be agreed before signing: every validator proposes the
//!   blockhash of an *anchor slot*, the last non-skipped finalized slot at or below
//!   `floor(finalized_slot / window) * window`. Validators whose finalized slots fall in the same
//!   window propose the same plan; the two-thirds vote on the plan is the agreement.

use super::http::{base64, Http};
use btb_chain::vault::{DepositClaim, Execution, Prepared, Withdrawal};
use btb_chain::vault_plan::solana_transfer_message;
use btb_oracle::frost::{addr, Scheme};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
const MEMO_V1: &str = "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo";
const MEMO_V2: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

#[derive(Debug, Clone)]
pub struct SolConfig {
    pub rpc_url: String,
    pub auth: Option<String>,
    /// Anchor slots are multiples of this (about 0.4 s each). Larger: validators agree more
    /// easily, the blockhash is older when the transaction lands (it is valid for ~150 slots
    /// after its own slot, and a finalized slot is already ~32 behind the tip).
    pub blockhash_window: u64,
    /// Signatures read per poll.
    pub signature_limit: u32,
    pub poll: Duration,
    pub timeout: Duration,
    pub allow_http: bool,
}

impl SolConfig {
    pub fn new(rpc_url: String) -> SolConfig {
        SolConfig {
            rpc_url,
            auth: None,
            blockhash_window: 16,
            signature_limit: 100,
            poll: Duration::from_secs(5),
            timeout: Duration::from_secs(10),
            allow_http: false,
        }
    }
}

struct Job {
    wire: Vec<u8>,
    signature: [u8; 64],
    state: Execution,
}

#[derive(Default)]
struct Shared {
    vault: Option<[u8; 32]>,
    deposits: Vec<DepositClaim>,
    /// Signatures already parsed (deposit or not), so that they are fetched once.
    done: BTreeSet<String>,
    found: BTreeMap<String, Vec<DepositClaim>>,
    slot_hash: BTreeMap<u64, [u8; 32]>,
    anchor: Option<(u64, [u8; 32])>,
    jobs: BTreeMap<u64, Job>,
    last_error: Option<String>,
    polls: u64,
}

pub struct SolanaWatcher {
    cfg: SolConfig,
    http: Http,
    shared: Mutex<Shared>,
}

impl std::fmt::Debug for SolanaWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SolanaWatcher({})", self.cfg.rpc_url)
    }
}

impl SolanaWatcher {
    pub fn new(cfg: SolConfig) -> Result<Arc<SolanaWatcher>, String> {
        if cfg.blockhash_window == 0 || cfg.signature_limit == 0 || cfg.signature_limit > 1000 {
            return Err(
                "solana: blockhash_window and signature_limit (1..=1000) must be positive".into(),
            );
        }
        let http = Http::new(cfg.timeout, 4 << 20, cfg.allow_http);
        http.check_url(&cfg.rpc_url)?;
        Ok(Arc::new(SolanaWatcher {
            cfg,
            http,
            shared: Mutex::new(Shared::default()),
        }))
    }

    fn sh(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().expect("solana watcher state")
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

    pub fn set_vault(&self, key: [u8; 32]) {
        self.sh().vault = Some(key);
    }

    pub fn deposits(&self) -> Vec<DepositClaim> {
        self.sh().deposits.clone()
    }

    fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        self.http
            .json_rpc(&self.cfg.rpc_url, self.cfg.auth.as_deref(), method, params)
    }

    // ------------------------------------------------------------ deposits

    fn block_hash(&self, slot: u64) -> Result<Option<[u8; 32]>, String> {
        if let Some(h) = self.sh().slot_hash.get(&slot) {
            return Ok(Some(*h));
        }
        let r = self.rpc(
            "getBlock",
            json!([slot, {"commitment": "finalized", "transactionDetails": "none",
                          "rewards": false, "maxSupportedTransactionVersion": 0}]),
        );
        let v = match r {
            Ok(v) => v,
            // -32007 / -32009: the slot was skipped or is missing in long-term storage
            Err(e) if e.contains("-32007") || e.contains("-32009") => return Ok(None),
            Err(e) => return Err(e),
        };
        if v.is_null() {
            return Ok(None);
        }
        let s = v
            .get("blockhash")
            .and_then(|b| b.as_str())
            .ok_or("getBlock: no blockhash")?;
        let h: [u8; 32] = addr::base58_decode(s)
            .and_then(|b| b.try_into().ok())
            .ok_or("getBlock: bad blockhash")?;
        let mut sh = self.sh();
        if sh.slot_hash.len() > 4096 {
            sh.slot_hash.clear();
        }
        sh.slot_hash.insert(slot, h);
        Ok(Some(h))
    }

    fn scan(&self, vault: &[u8; 32]) -> Result<(), String> {
        let address = addr::solana_address(vault);
        let list = self.rpc(
            "getSignaturesForAddress",
            json!([address, {"commitment": "finalized", "limit": self.cfg.signature_limit}]),
        )?;
        let list = list
            .as_array()
            .ok_or("getSignaturesForAddress: not an array")?;
        let mut bad: Option<String> = None;
        for e in list {
            let sig = e
                .get("signature")
                .and_then(|s| s.as_str())
                .ok_or("entry without signature")?;
            let sig_bytes: [u8; 64] = addr::base58_decode(sig)
                .and_then(|b| b.try_into().ok())
                .ok_or("bad signature encoding")?;
            if self.sh().done.contains(sig) {
                continue;
            }
            if !e.get("err").is_none_or(Value::is_null) {
                self.sh().done.insert(sig.to_string());
                continue; // failed transactions move no money
            }
            let slot = e
                .get("slot")
                .and_then(|s| s.as_u64())
                .ok_or("entry without slot")?;
            let tx = self.rpc(
                "getTransaction",
                json!([sig, {"commitment": "finalized", "encoding": "json",
                             "maxSupportedTransactionVersion": 0}]),
            )?;
            if tx.is_null() {
                continue; // not available yet: try again next poll
            }
            if tx.get("slot").and_then(|s| s.as_u64()) != Some(slot) {
                bad = Some("getTransaction: slot differs from the listing".to_string());
                continue;
            }
            let Some(hash) = self.block_hash(slot)? else {
                continue;
            };
            // one bad answer costs that transaction (retried next poll), not the others
            let claims = match parse_deposits(&tx, sig, &sig_bytes, vault, slot, hash) {
                Ok(c) => c,
                Err(e) => {
                    bad = Some(e);
                    continue;
                }
            };
            let mut sh = self.sh();
            sh.done.insert(sig.to_string());
            if !claims.is_empty() {
                sh.found.insert(sig.to_string(), claims);
            }
            sh.deposits = sh.found.values().flatten().cloned().collect();
        }
        match bad {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    // ------------------------------------------------------------ the agreed blockhash

    fn refresh_anchor(&self) -> Result<(), String> {
        let slot = self
            .rpc("getSlot", json!([{"commitment": "finalized"}]))?
            .as_u64()
            .ok_or("getSlot: not a number")?;
        let w = self.cfg.blockhash_window;
        let mut s = slot / w * w;
        for _ in 0..16 {
            if let Some(h) = self.block_hash(s)? {
                self.sh().anchor = Some((s, h));
                return Ok(());
            }
            if s == 0 {
                break;
            }
            s -= 1;
        }
        Err("no block found near the anchor slot".into())
    }

    // ------------------------------------------------------------ the poll

    pub fn poll(&self) {
        let vault = self.sh().vault;
        let mut err = None;
        if let Some(v) = vault {
            if let Err(e) = self.scan(&v) {
                err = Some(format!("deposits: {e}"));
            }
        }
        if let Err(e) = self.refresh_anchor() {
            err = Some(format!("blockhash: {e}"));
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
        let todo: Vec<(u64, Vec<u8>, [u8; 64], Execution)> = self
            .sh()
            .jobs
            .iter()
            .filter(|(_, j)| !matches!(j.state, Execution::Confirmed | Execution::Failed))
            .map(|(id, j)| (*id, j.wire.clone(), j.signature, j.state))
            .collect();
        for (id, wire, sig, state) in todo {
            let sig58 = addr::base58_encode(&sig);
            let mut new = self.status(&sig58)?;
            if new == Execution::Unknown && state == Execution::Unknown {
                self.rpc(
                    "sendTransaction",
                    json!([base64(&wire), {"encoding": "base64", "skipPreflight": false,
                                           "preflightCommitment": "confirmed"}]),
                )?;
                new = Execution::Pending;
            } else if new == Execution::Unknown {
                // sent before but the cluster does not know it (dropped): try again
                self.rpc(
                    "sendTransaction",
                    json!([base64(&wire), {"encoding": "base64", "skipPreflight": true}]),
                )?;
                new = Execution::Pending;
            }
            if let Some(j) = self.sh().jobs.get_mut(&id) {
                j.state = new;
            }
        }
        Ok(())
    }

    fn status(&self, sig58: &str) -> Result<Execution, String> {
        let r = self.rpc(
            "getSignatureStatuses",
            json!([[sig58], {"searchTransactionHistory": true}]),
        )?;
        let v = r
            .get("value")
            .and_then(|v| v.as_array())
            .ok_or("getSignatureStatuses: no value")?;
        match v.first() {
            None | Some(Value::Null) => Ok(Execution::Unknown),
            Some(s) => {
                if !s.get("err").is_none_or(Value::is_null) {
                    return Ok(Execution::Failed);
                }
                Ok(match s.get("confirmationStatus").and_then(|c| c.as_str()) {
                    Some("finalized") => Execution::Confirmed,
                    _ => Execution::Pending,
                })
            }
        }
    }

    // ------------------------------------------------------------ withdrawals

    pub fn prepare(&self, w: &Withdrawal, vault: &[u8; 32]) -> Option<Prepared> {
        let to: [u8; 32] = w.destination.as_slice().try_into().ok()?;
        let (_, bh) = self.sh().anchor?;
        let msg = solana_transfer_message(vault, &to, w.amount, &bh);
        let digest: [u8; 32] = Sha256::digest(&msg).into();
        Some(Prepared {
            digest,
            sighashes: vec![digest],
            plan: msg,
        })
    }

    pub fn broadcast(&self, w: &Withdrawal, signatures: &[Vec<u8>]) {
        if self.sh().jobs.contains_key(&w.id) {
            return;
        }
        let Some(sig) = signatures
            .first()
            .and_then(|s| <[u8; 64]>::try_from(s.as_slice()).ok())
        else {
            return;
        };
        let mut wire = vec![1u8];
        wire.extend_from_slice(&sig);
        wire.extend_from_slice(&w.plan);
        self.sh().jobs.insert(
            w.id,
            Job {
                wire,
                signature: sig,
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

    pub fn signature_of(&self, id: u64) -> Option<String> {
        self.sh()
            .jobs
            .get(&id)
            .map(|j| addr::base58_encode(&j.signature))
    }
}

/// Strict parse of a `getTransaction` answer (`encoding: json`) into deposit claims.
pub fn parse_deposits(
    tx: &Value,
    sig: &str,
    sig_bytes: &[u8; 64],
    vault: &[u8; 32],
    slot: u64,
    block_hash: [u8; 32],
) -> Result<Vec<DepositClaim>, String> {
    let bad = |m: &str| format!("getTransaction {sig}: {m}");
    let meta = tx.get("meta").ok_or_else(|| bad("no meta"))?;
    if !meta.get("err").is_none_or(Value::is_null) {
        return Ok(Vec::new());
    }
    let t = tx.get("transaction").ok_or_else(|| bad("no transaction"))?;
    if t.get("signatures")
        .and_then(|s| s.get(0))
        .and_then(|s| s.as_str())
        != Some(sig)
    {
        return Err(bad("first signature is not the one asked for"));
    }
    let m = t.get("message").ok_or_else(|| bad("no message"))?;
    if m.get("addressTableLookups")
        .and_then(|a| a.as_array())
        .is_some_and(|a| !a.is_empty())
    {
        return Ok(Vec::new()); // account indices would need the loaded addresses: not supported
    }
    let keys: Vec<&str> = m
        .get("accountKeys")
        .and_then(|k| k.as_array())
        .ok_or_else(|| bad("no accountKeys"))?
        .iter()
        .map(|k| k.as_str().ok_or_else(|| bad("account key is not a string")))
        .collect::<Result<_, _>>()?;
    let vault58 = addr::solana_address(vault);
    let Some(vi) = keys.iter().position(|k| *k == vault58) else {
        return Ok(Vec::new());
    };
    let ixs = m
        .get("instructions")
        .and_then(|i| i.as_array())
        .ok_or_else(|| bad("no instructions"))?;
    let mut memos = Vec::new();
    let mut transfers = Vec::new();
    for (n, ix) in ixs.iter().enumerate() {
        let pi = ix
            .get("programIdIndex")
            .and_then(|p| p.as_u64())
            .ok_or_else(|| bad("instruction without programIdIndex"))? as usize;
        let program = *keys
            .get(pi)
            .ok_or_else(|| bad("program index out of range"))?;
        let data = addr::base58_decode(
            ix.get("data")
                .and_then(|d| d.as_str())
                .ok_or_else(|| bad("instruction without data"))?,
        )
        .ok_or_else(|| bad("instruction data is not base58"))?;
        let accounts: Vec<usize> = ix
            .get("accounts")
            .and_then(|a| a.as_array())
            .ok_or_else(|| bad("instruction without accounts"))?
            .iter()
            .map(|a| {
                a.as_u64()
                    .map(|x| x as usize)
                    .ok_or_else(|| bad("bad account index"))
            })
            .collect::<Result<_, _>>()?;
        if program == MEMO_V1 || program == MEMO_V2 {
            memos.push(data);
        } else if program == SYSTEM_PROGRAM
            && data.len() == 12
            && data[..4] == 2u32.to_le_bytes()
            && accounts.len() == 2
            && accounts[1] == vi
        {
            let lamports = u64::from_le_bytes(data[4..12].try_into().expect("8"));
            transfers.push((n as u32, lamports));
        }
    }
    if transfers.is_empty() || memos.len() != 1 {
        return Ok(Vec::new()); // no deposit, or no unambiguous recipient: never claimed
    }
    let Ok(text) = std::str::from_utf8(&memos[0]) else {
        return Ok(Vec::new());
    };
    let Some(recipient) = crate::hex::decode32(text.trim())
        .ok()
        .filter(|_| text.trim().len() == 64)
    else {
        return Ok(Vec::new());
    };
    // the balance really grew by (at least) what the instructions say
    let bal = |k: &str| -> Result<u64, String> {
        meta.get(k)
            .and_then(|b| b.as_array())
            .and_then(|b| b.get(vi))
            .and_then(|b| b.as_u64())
            .ok_or_else(|| bad("missing balances"))
    };
    let grew = bal("postBalances")?.saturating_sub(bal("preBalances")?);
    let total = transfers
        .iter()
        .try_fold(0u64, |a, (_, l)| a.checked_add(*l))
        .ok_or_else(|| bad("overflow"))?;
    if grew < total {
        return Err(bad("vault balance grew by less than the transfers"));
    }
    let txid: [u8; 32] = sig_bytes[..32].try_into().expect("32");
    Ok(transfers
        .into_iter()
        .filter(|(_, l)| *l > 0)
        .map(|(index, amount)| DepositClaim {
            asset: Scheme::Ed25519,
            txid,
            index,
            amount,
            recipient,
            height: slot,
            block_hash,
        })
        .collect())
}
