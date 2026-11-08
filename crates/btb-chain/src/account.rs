//! Accounts with hidden, rotating keys (proposal 0002, phase A).
//!
//! An **account** has a permanent id that does not depend on any key:
//! `SHA256("btb.account.v2" || creator || nonce)`. It is controlled by up to [`MAX_SLOTS`] key
//! **slots**. State stores only each slot's **commitment**,
//! `SHA256("btb.key.v1" || scheme || public_key)`, never the public key itself.
//!
//! A transaction acts as an account when its body carries [`AccountAuth`] (`tx.rs`, tag bit
//! `0x20`): the account id, the slot index and, for a rotating slot, the commitment of the next
//! key. The transaction's `public_key` must hash to the slot's commitment and sign the body as
//! usual. For a **rotating** slot the same transaction replaces the slot's commitment with
//! `next`, so the public key it revealed never controls the account again. That holds even if
//! the transaction reverts (the key was revealed either way); an *invalid* transaction changes
//! nothing.
//!
//! Slots have a role: **admin** slots can do everything, other slots can spend but cannot
//! change the account's slots or manage its name (`names.rs`). The account's nonce, balance and
//! `caller()` are the account id's, like any address.
//!
//! Not in this phase: several signatures for one transaction (threshold), passkeys (P-256),
//! recovery and post-quantum schemes. The scheme byte in the commitment leaves room for them.
//!
//! | tag | key | value |
//! | --- | --- | --- |
//! | `0x73` | `73 account` | [`AccountRecord`] encoding |

use crate::state::StateError;
use crate::tx::{Address, Hash, Transaction, TxKind};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};

pub const TAG_ACCOUNT: u8 = 0x73;
pub const ACCOUNT_GAS: u64 = 10_000;
/// Key slots one account may have.
pub const MAX_SLOTS: usize = 8;
/// The only signature scheme of this phase.
pub const SCHEME_ED25519: u8 = 0;

const KEY_DOMAIN: &[u8] = b"btb.key.v1";
const ACCOUNT_V2_DOMAIN: &[u8] = b"btb.account.v2";

/// The commitment stored for an Ed25519 public key.
pub fn key_commitment(public_key: &[u8; 32]) -> Hash {
    crate::sha256(&[KEY_DOMAIN, &[SCHEME_ED25519], public_key])
}

/// The id of the account created by `creator` with its transaction nonce `nonce`.
pub fn account_id(creator: &Address, nonce: u64) -> Address {
    crate::sha256(&[ACCOUNT_V2_DOMAIN, creator, &nonce.to_be_bytes()])
}

pub fn account_key(id: &Address) -> Vec<u8> {
    let mut k = vec![TAG_ACCOUNT];
    k.extend_from_slice(id);
    k
}

/// One key slot. Only the commitment is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub commitment: Hash,
    /// May change the slots and manage the account's name.
    pub admin: bool,
    /// One-time key: every use must name the next key's commitment.
    pub rotate: bool,
}

impl Encode for Slot {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.commitment.encode_to(out);
        self.admin.encode_to(out);
        self.rotate.encode_to(out);
    }
}

impl Decode for Slot {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Slot {
            commitment: Hash::decode_from(r)?,
            admin: bool::decode_from(r)?,
            rotate: bool::decode_from(r)?,
        })
    }
}

/// What the chain stores for an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    pub slots: Vec<Slot>,
}

impl Encode for AccountRecord {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.slots.encode_to(out);
    }
}

impl Decode for AccountRecord {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(AccountRecord {
            slots: Vec::decode_from(r)?,
        })
    }
}

/// The part of a transaction body that says it acts as an account (signed with the body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountAuth {
    pub account: Address,
    pub slot: u8,
    /// Required for a rotating slot (the next key's commitment), absent otherwise.
    pub next: Option<Hash>,
}

impl Encode for AccountAuth {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.account.encode_to(out);
        self.slot.encode_to(out);
        self.next.encode_to(out);
    }
}

impl Decode for AccountAuth {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(AccountAuth {
            account: Address::decode_from(r)?,
            slot: u8::decode_from(r)?,
            next: Option::decode_from(r)?,
        })
    }
}

/// Account operations (transaction tag 24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountOp {
    /// Create a new account with these slots and move `fund` from the sender into it. The
    /// output is the new account id.
    Create { slots: Vec<Slot>, fund: u64 },
    /// Replace the sender account's slots (needs an admin slot).
    SetSlots { slots: Vec<Slot> },
}

/// Why an account operation was refused (the fee is still charged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    /// No slots, too many, no admin slot, or a commitment used twice.
    BadSlots,
    /// `SetSlots` sent by an address that is not an account.
    NotAnAccount,
    /// The new account id is already taken (cannot happen with distinct nonces).
    Exists,
}

impl AccountOp {
    pub fn gas(&self) -> u64 {
        ACCOUNT_GAS
    }

    /// BTB the operation takes from the sender besides the fee.
    pub fn debit(&self) -> u64 {
        match self {
            AccountOp::Create { fund, .. } => *fund,
            AccountOp::SetSlots { .. } => 0,
        }
    }

    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            AccountOp::Create { slots, fund } => {
                0u8.encode_to(out);
                slots.encode_to(out);
                fund.encode_to(out);
            }
            AccountOp::SetSlots { slots } => {
                1u8.encode_to(out);
                slots.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => AccountOp::Create {
                slots: Vec::decode_from(r)?,
                fund: u64::decode_from(r)?,
            },
            1 => AccountOp::SetSlots {
                slots: Vec::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    pub fn slots_len(&self) -> usize {
        let (AccountOp::Create { slots, .. } | AccountOp::SetSlots { slots }) = self;
        slots.len()
    }

    /// Checks that need no state.
    pub fn check_static(&self) -> Result<(), AccountError> {
        let (AccountOp::Create { slots, .. } | AccountOp::SetSlots { slots }) = self;
        check_slots(slots)
    }
}

fn check_slots(slots: &[Slot]) -> Result<(), AccountError> {
    if slots.is_empty() || slots.len() > MAX_SLOTS || !slots.iter().any(|s| s.admin) {
        return Err(AccountError::BadSlots);
    }
    for (i, a) in slots.iter().enumerate() {
        if slots[i + 1..].iter().any(|b| b.commitment == a.commitment) {
            return Err(AccountError::BadSlots);
        }
    }
    Ok(())
}

pub fn get(kv: &impl KvStore, id: &Address) -> Result<Option<AccountRecord>, StateError> {
    match kv.get(&account_key(id))? {
        None => Ok(None),
        Some(b) => {
            let mut r = Reader::new(&b, b.len().max(1));
            AccountRecord::decode_from(&mut r)
                .map(Some)
                .map_err(|_| StateError("bad stored account".into()))
        }
    }
}

fn put(kv: &mut impl KvStore, id: &Address, rec: &AccountRecord) -> Result<(), KvError> {
    kv.put(&account_key(id), &rec.encode())
}

pub(crate) enum Fail {
    Tx(AccountError),
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

/// Apply an account operation of `sender` (whose nonce is `nonce`). Moving `fund` is done by
/// the executor after this succeeds. Returns the output (the new id for `Create`).
pub(crate) fn apply(
    kv: &mut impl KvStore,
    sender: &Address,
    nonce: u64,
    op: &AccountOp,
) -> Result<Vec<u8>, Fail> {
    op.check_static().map_err(Fail::Tx)?;
    match op {
        AccountOp::Create { slots, .. } => {
            let id = account_id(sender, nonce);
            if kv.get(&account_key(&id))?.is_some() {
                return Err(Fail::Tx(AccountError::Exists));
            }
            put(
                kv,
                &id,
                &AccountRecord {
                    slots: slots.clone(),
                },
            )?;
            Ok(id.to_vec())
        }
        AccountOp::SetSlots { slots } => {
            if get(kv, sender)?.is_none() {
                return Err(Fail::Tx(AccountError::NotAnAccount));
            }
            put(
                kv,
                sender,
                &AccountRecord {
                    slots: slots.clone(),
                },
            )?;
            Ok(Vec::new())
        }
    }
}

/// Why a transaction acting as an account is invalid (skipped, no fee, nothing changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountInvalid {
    /// Session keys and account slots cannot be combined in one transaction.
    WithSession,
    /// Consensus-key operations (DKG, cross-chain, evidence) are signed by a validator key.
    KindNotAllowed,
    NoSuchAccount,
    NoSuchSlot,
    /// The transaction's public key does not hash to the slot's commitment.
    WrongKey,
    /// A rotating slot without `next`, a `next` equal to a commitment of the account, or a
    /// non-rotating slot with `next`.
    BadRotation,
    /// Only admin slots may change slots or manage names.
    NotAdmin,
}

/// Stateless part.
pub fn check_static(tx: &Transaction) -> Result<(), AccountInvalid> {
    if tx.body.account.is_none() {
        return Ok(());
    }
    if tx.body.session.is_some() {
        return Err(AccountInvalid::WithSession);
    }
    if matches!(
        tx.body.kind,
        TxKind::Dkg(_) | TxKind::Cross(_) | TxKind::SubmitEvidence { .. }
    ) {
        return Err(AccountInvalid::KindNotAllowed);
    }
    Ok(())
}

/// A checked account authorization: what to rotate after the transaction ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Authorized {
    pub account: Address,
    pub slot: u8,
    pub next: Option<Hash>,
}

/// `authorize` failing.
#[derive(Debug)]
pub enum AuthError {
    Invalid(AccountInvalid),
    State(StateError),
}

impl From<AccountInvalid> for AuthError {
    fn from(i: AccountInvalid) -> Self {
        AuthError::Invalid(i)
    }
}

impl From<StateError> for AuthError {
    fn from(e: StateError) -> Self {
        AuthError::State(e)
    }
}

/// The stateful checks for a transaction acting as an account. `Ok(None)` without one.
pub fn authorize(kv: &impl KvStore, tx: &Transaction) -> Result<Option<Authorized>, AuthError> {
    let Some(auth) = tx.body.account else {
        return Ok(None);
    };
    check_static(tx)?;
    let rec = get(kv, &auth.account)?.ok_or(AccountInvalid::NoSuchAccount)?;
    let slot = *rec
        .slots
        .get(usize::from(auth.slot))
        .ok_or(AccountInvalid::NoSuchSlot)?;
    if key_commitment(&tx.public_key) != slot.commitment {
        return Err(AccountInvalid::WrongKey.into());
    }
    match (slot.rotate, auth.next) {
        (true, Some(n)) if !rec.slots.iter().any(|s| s.commitment == n) => {}
        (false, None) => {}
        _ => return Err(AccountInvalid::BadRotation.into()),
    }
    let needs_admin = matches!(tx.body.kind, TxKind::Name(_))
        || matches!(tx.body.kind, TxKind::Account(AccountOp::SetSlots { .. }));
    if needs_admin && !slot.admin {
        return Err(AccountInvalid::NotAdmin.into());
    }
    Ok(Some(Authorized {
        account: auth.account,
        slot: auth.slot,
        next: auth.next,
    }))
}

/// After the transaction ran (success or revert): retire a rotating slot's revealed key.
/// A `SetSlots` in the same transaction already replaced every slot; then nothing rotates.
pub(crate) fn rotate(
    kv: &mut impl KvStore,
    a: &Authorized,
    revealed: &Hash,
) -> Result<(), StateError> {
    let Some(next) = a.next else {
        return Ok(());
    };
    let Some(mut rec) = get(kv, &a.account)? else {
        return Ok(());
    };
    if let Some(s) = rec.slots.get_mut(usize::from(a.slot)) {
        if s.commitment == *revealed {
            s.commitment = next;
            put(kv, &a.account, &rec)?;
        }
    }
    Ok(())
}

/// Node admission (the mempool is stateless).
pub fn check_admission(kv: &impl KvStore, tx: &Transaction) -> Result<(), crate::exec::Invalid> {
    match authorize(kv, tx) {
        Ok(_) => Ok(()),
        Err(AuthError::Invalid(i)) => Err(crate::exec::Invalid::Account(i)),
        Err(AuthError::State(_)) => {
            Err(crate::exec::Invalid::Account(AccountInvalid::NoSuchAccount))
        }
    }
}
