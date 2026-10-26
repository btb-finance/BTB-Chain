//! Session keys: a scoped, expiring, budgeted delegation from an account to another key.
//!
//! An account signs `SessionOp::Grant` once (transaction tag 22). From then on a transaction
//! signed by the **session key** acts as the account: `caller()` in the contract is the account,
//! the nonce and the fee are the account's. The transaction names the account in its signed
//! `session` field (`tx.rs`, tag bit `0x40`); the chain finds the grant under
//! `(account, public_key)` and checks it. This is the Convex-style "log in once, then the app
//! signs for you" flow without handing the app the account key.
//!
//! A grant is limited three ways:
//!
//! * **Scope.** A list of `(contract, functions)`; `functions: None` allows every mutation of
//!   that contract, `Some(names)` only those. Only `Call` transactions can be signed by a
//!   session key. Transfers, staking, deploys, DEX, storage, channels, hosting-node and every
//!   other kind are refused, and so is `Session` itself: a session can neither grant nor revoke
//!   sessions. (Transfers are therefore never allowed in this version; a future grant flag
//!   would have to opt in explicitly.) The scope covers the *entry* contract; that contract
//!   may call others as it can for any caller.
//! * **Expiry.** Valid while `height < expires_height`.
//! * **Fee budget.** `spend_limit` base units: the sum of fees the session's transactions may
//!   be charged over the grant's life. A transaction is valid only if `spent + gas_limit *
//!   gas_price <= spend_limit` (the reservation, like the balance check), and the fee actually
//!   charged is added to `spent`.
//!
//! **Invalid, not reverted.** A session transaction that breaks any of this is *invalid*: it is
//! rejected at node admission (`check_admission`), skipped in a block, costs nothing and does
//! not use the account's nonce (like a bad signature). A transaction that passed and then fails
//! in the VM is an ordinary revert: the fee is charged to the account and to the budget.
//!
//! Replay: the nonce is the **account's** nonce, shared by the account's own key and all its
//! sessions, so a session transaction cannot be replayed and cannot race the owner's own
//! transactions into a nonce collision silently (the later one is simply `BadNonce`).
//!
//! Revoking deletes the grant (`Revoke(session_key)`, signed by the account itself). Up to
//! [`MAX_SESSIONS`] grants per account; a new grant purges that account's expired ones.
//!
//! | tag | key | value |
//! | --- | --- | --- |
//! | `0x72` | `72 account session_key` | `Grant` encoding `\|\| spent:u64 LE` |

use crate::state::{ChainConfig, StateError};
use crate::tx::{Address, Transaction, TxKind, MAX_NAME_BYTES};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};

pub const TAG_SESSION: u8 = 0x72;
pub const SESSION_GAS: u64 = 10_000;
/// Live grants one account may hold.
pub const MAX_SESSIONS: usize = 16;
/// Contracts in one grant, and function names per contract.
pub const MAX_SCOPES: usize = 16;
pub const MAX_FUNCTIONS: usize = 32;

/// State key of the grant of `account` to `session`.
pub fn session_key_of(account: &Address, session: &[u8; 32]) -> Vec<u8> {
    let mut k = vec![TAG_SESSION];
    k.extend_from_slice(account);
    k.extend_from_slice(session);
    k
}

/// One contract a session may call, optionally restricted to named mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub contract: Address,
    /// `None`: every mutation of the contract.
    pub functions: Option<Vec<String>>,
}

impl Scope {
    fn allows(&self, contract: &Address, function: &str) -> bool {
        &self.contract == contract
            && self
                .functions
                .as_ref()
                .is_none_or(|f| f.iter().any(|n| n == function))
    }
}

impl Encode for Scope {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.contract.encode_to(out);
        self.functions.encode_to(out);
    }
}

impl Decode for Scope {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Scope {
            contract: Address::decode_from(r)?,
            functions: Option::decode_from(r)?,
        })
    }
}

/// What the account signs once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The session's Ed25519 public key.
    pub session_key: [u8; 32],
    /// The first height at which the grant no longer works.
    pub expires_height: u64,
    pub allowed: Vec<Scope>,
    /// Fee budget in base units.
    pub spend_limit: u64,
}

impl Grant {
    /// Whether this grant lets a call of `function` on `contract` through.
    pub fn allows(&self, contract: &Address, function: &str) -> bool {
        self.allowed.iter().any(|s| s.allows(contract, function))
    }
}

impl Encode for Grant {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.session_key.encode_to(out);
        self.expires_height.encode_to(out);
        self.allowed.encode_to(out);
        self.spend_limit.encode_to(out);
    }
}

impl Decode for Grant {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Grant {
            session_key: <[u8; 32]>::decode_from(r)?,
            expires_height: u64::decode_from(r)?,
            allowed: Vec::decode_from(r)?,
            spend_limit: u64::decode_from(r)?,
        })
    }
}

/// A stored grant and how much of its budget is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub grant: Grant,
    pub spent: u64,
}

fn decode_stored(bytes: &[u8]) -> Result<Stored, StateError> {
    let mut r = Reader::new(bytes, bytes.len().max(1));
    let bad = |_| StateError("bad stored session".into());
    let grant = Grant::decode_from(&mut r).map_err(bad)?;
    let spent = u64::decode_from(&mut r).map_err(bad)?;
    Ok(Stored { grant, spent })
}

/// The stored grant of `account` to `session`, if any (expired ones included).
pub fn get(
    kv: &impl KvStore,
    account: &Address,
    session: &[u8; 32],
) -> Result<Option<Stored>, StateError> {
    kv.get(&session_key_of(account, session))?
        .map(|v| decode_stored(&v))
        .transpose()
}

/// All stored grants of `account`.
pub fn list(kv: &impl KvStore, account: &Address) -> Result<Vec<Stored>, StateError> {
    let mut start = vec![TAG_SESSION];
    start.extend_from_slice(account);
    let mut end = start.clone();
    end.push(0xff);
    let mut out = Vec::new();
    for (_, v) in kv.scan(&start, Some(&end), false, 1 << 10)? {
        out.push(decode_stored(&v)?);
    }
    Ok(out)
}

/// A session operation, signed by the account itself (never by a session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOp {
    Grant(Grant),
    Revoke([u8; 32]),
}

/// Why a session operation was refused (the fee is still charged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// Empty or oversized scope, unknown shape, bad key or an expiry in the past.
    BadGrant,
    TooManySessions,
    NoSuchSession,
}

impl SessionOp {
    pub fn gas(&self) -> u64 {
        SESSION_GAS
    }

    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            SessionOp::Grant(g) => {
                0u8.encode_to(out);
                g.encode_to(out);
            }
            SessionOp::Revoke(k) => {
                1u8.encode_to(out);
                k.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => SessionOp::Grant(Grant::decode_from(r)?),
            1 => SessionOp::Revoke(<[u8; 32]>::decode_from(r)?),
            t => return Ok(Err(t)),
        }))
    }

    /// Checks that need no state.
    pub fn check_static(&self) -> Result<(), SessionError> {
        let SessionOp::Grant(g) = self else {
            return Ok(());
        };
        let key_ok = ed25519_dalek::VerifyingKey::from_bytes(&g.session_key).is_ok();
        let scopes_ok = !g.allowed.is_empty()
            && g.allowed.len() <= MAX_SCOPES
            && g.allowed.iter().all(|s| {
                s.functions.as_ref().is_none_or(|f| {
                    !f.is_empty()
                        && f.len() <= MAX_FUNCTIONS
                        && f.iter().all(|n| !n.is_empty() && n.len() <= MAX_NAME_BYTES)
                })
            });
        if key_ok && scopes_ok {
            Ok(())
        } else {
            Err(SessionError::BadGrant)
        }
    }
}

pub(crate) enum Fail {
    Tx(SessionError),
    State(StateError),
}

impl From<KvError> for Fail {
    fn from(e: KvError) -> Self {
        Fail::State(e.into())
    }
}

impl From<StateError> for Fail {
    fn from(e: StateError) -> Self {
        Fail::State(e)
    }
}

pub(crate) fn apply(
    kv: &mut impl KvStore,
    height: u64,
    account: &Address,
    op: &SessionOp,
) -> Result<(), Fail> {
    op.check_static().map_err(Fail::Tx)?;
    match op {
        SessionOp::Grant(g) => {
            if g.expires_height <= height {
                return Err(Fail::Tx(SessionError::BadGrant));
            }
            let mut live = 0;
            for s in list(kv, account)? {
                if s.grant.expires_height <= height {
                    kv.delete(&session_key_of(account, &s.grant.session_key))?;
                } else if s.grant.session_key != g.session_key {
                    live += 1;
                }
            }
            if live >= MAX_SESSIONS {
                return Err(Fail::Tx(SessionError::TooManySessions));
            }
            // A new grant for a known key replaces the old one, budget included.
            put(
                kv,
                account,
                &Stored {
                    grant: g.clone(),
                    spent: 0,
                },
            )?;
        }
        SessionOp::Revoke(k) => {
            let key = session_key_of(account, k);
            if kv.get(&key)?.is_none() {
                return Err(Fail::Tx(SessionError::NoSuchSession));
            }
            kv.delete(&key)?;
        }
    }
    Ok(())
}

fn put(kv: &mut impl KvStore, account: &Address, s: &Stored) -> Result<(), KvError> {
    let mut v = s.grant.encode();
    s.spent.encode_to(&mut v);
    kv.put(&session_key_of(account, &s.grant.session_key), &v)
}

/// Why a transaction signed by a session key is invalid (skipped, no fee).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInvalid {
    /// Session keys may only sign contract calls.
    KindNotAllowed,
    NoSuchSession,
    Expired {
        expires_height: u64,
    },
    ContractNotAllowed,
    FunctionNotAllowed,
    BudgetExceeded {
        needed: u64,
        remaining: u64,
    },
}

/// Stateless part: a session transaction must be a `Call`.
pub fn check_kind(tx: &Transaction) -> Result<(), SessionInvalid> {
    if tx.body.session.is_some() && !matches!(tx.body.kind, TxKind::Call { .. }) {
        return Err(SessionInvalid::KindNotAllowed);
    }
    Ok(())
}

/// The stateful checks for a session transaction at `height`: the grant exists, has not
/// expired, covers the call and has budget left for the call's gas reservation. `Ok(None)` for
/// a transaction without a session.
pub fn authorize(
    kv: &impl KvStore,
    tx: &Transaction,
    height: u64,
    cfg: &ChainConfig,
) -> Result<Option<Stored>, AuthError> {
    let Some(account) = tx.body.session else {
        return Ok(None);
    };
    check_kind(tx)?;
    let stored = get(kv, &account, &tx.public_key)?.ok_or(SessionInvalid::NoSuchSession)?;
    if height >= stored.grant.expires_height {
        return Err(SessionInvalid::Expired {
            expires_height: stored.grant.expires_height,
        }
        .into());
    }
    let TxKind::Call {
        contract,
        function,
        gas_limit,
        ..
    } = &tx.body.kind
    else {
        return Err(SessionInvalid::KindNotAllowed.into());
    };
    if !stored.grant.allowed.iter().any(|s| &s.contract == contract) {
        return Err(SessionInvalid::ContractNotAllowed.into());
    }
    if !stored.grant.allows(contract, function) {
        return Err(SessionInvalid::FunctionNotAllowed.into());
    }
    let needed = gas_limit.saturating_mul(cfg.gas_price);
    let remaining = stored.grant.spend_limit.saturating_sub(stored.spent);
    if needed > remaining {
        return Err(SessionInvalid::BudgetExceeded { needed, remaining }.into());
    }
    Ok(Some(stored))
}

/// `authorize` failing: the transaction is invalid, or the state could not be read.
#[derive(Debug)]
pub enum AuthError {
    Invalid(SessionInvalid),
    State(StateError),
}

impl From<SessionInvalid> for AuthError {
    fn from(i: SessionInvalid) -> Self {
        AuthError::Invalid(i)
    }
}

impl From<StateError> for AuthError {
    fn from(e: StateError) -> Self {
        AuthError::State(e)
    }
}

/// Add the fee a session transaction was charged to its grant's `spent`.
pub(crate) fn add_spent(
    kv: &mut impl KvStore,
    account: &Address,
    session: &[u8; 32],
    fee: u64,
) -> Result<(), StateError> {
    let mut s = get(kv, account, session)?
        .ok_or_else(|| StateError("session vanished while its transaction ran".into()))?;
    s.spent = s
        .spent
        .checked_add(fee)
        .ok_or_else(|| StateError("session spend overflow".into()))?;
    Ok(put(kv, account, &s)?)
}

/// Node admission: the check a node runs before it puts a session transaction in its mempool
/// (the mempool itself is stateless). `height` is the height of the next block.
pub fn check_admission(
    kv: &impl KvStore,
    tx: &Transaction,
    height: u64,
    cfg: &ChainConfig,
) -> Result<(), crate::exec::Invalid> {
    match authorize(kv, tx, height, cfg) {
        Ok(_) => Ok(()),
        Err(AuthError::Invalid(i)) => Err(crate::exec::Invalid::Session(i)),
        // An unreadable state cannot vouch for the grant.
        Err(AuthError::State(_)) => {
            Err(crate::exec::Invalid::Session(SessionInvalid::NoSuchSession))
        }
    }
}

/// For single-node tools without a chain (`btb-dev --auth signed`): apply a session operation
/// of `account` at `height` to `kv` with the chain's exact rules.
pub fn apply_to(
    kv: &mut impl KvStore,
    height: u64,
    account: &Address,
    op: &SessionOp,
) -> Result<(), String> {
    match apply(kv, height, account, op) {
        Ok(()) => Ok(()),
        Err(Fail::Tx(e)) => Err(format!("{e:?}")),
        Err(Fail::State(e)) => Err(e.to_string()),
    }
}

/// Charge `fee` to the grant `account` gave `session` (see `apply_to`).
pub fn charge_to(
    kv: &mut impl KvStore,
    account: &Address,
    session: &[u8; 32],
    fee: u64,
) -> Result<(), String> {
    add_spent(kv, account, session, fee).map_err(|e| e.to_string())
}
