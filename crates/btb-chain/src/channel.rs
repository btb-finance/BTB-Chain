//! Unidirectional payment channels. See `docs/TOKENOMICS.md` and `docs/CHAIN.md`.
//!
//! A payer locks BTB for one payee. Off chain, the payer signs vouchers with a growing
//! cumulative amount (`btb-voucher`); the payee serves requests against them with no
//! consensus. On chain the channel settles once, at the highest voucher: the payee gets that
//! amount, the payer gets the rest. Nothing here creates BTB: a deposit moves from the payer's
//! balance into the channel (`0x31`, counted by the supply invariant) and back out to the two
//! parties on close.
//!
//! | Tag | Key | Value |
//! | --- | --- | --- |
//! | `0x30` channel | `30 id` | `(payer, payer_key, payee, expiry_height, closing)` |
//! | `0x31` locked | `31 id` | locked deposit, `u64` LE |
//!
//! `id = SHA256("btb.channel.v1" || payer || open_tx_nonce:u64le)`: unique per channel, so a
//! voucher can never be replayed into another channel, and a closed channel's vouchers are
//! dead (the channel is deleted; a later channel has another id).
//!
//! Transactions (`ChannelOp`): `Open`, `TopUp` (payer), `Close` (payee, with a voucher, any
//! time), `StartClose` (payer, once `expiry_height` has passed, optionally offering the
//! highest voucher the payer knows), `Finalize` (after the challenge period). During the
//! challenge period the payee can still `Close` with a higher voucher; the higher amount wins.

use crate::state::{read_u64, StateError};
use crate::tx::{Address, Hash};
use crate::{sha256, CHANNEL_CHALLENGE_BLOCKS};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};
use btb_runtime::layout;
use btb_voucher::SignedVoucher;

pub const TAG_CHANNEL: u8 = 0x30;
pub const TAG_LOCKED: u8 = 0x31;

/// Longest channel lifetime accepted at open, in blocks.
pub const MAX_EXPIRY_BLOCKS: u64 = 1 << 32;

pub type ChannelId = Hash;

/// A channel transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelOp {
    /// Lock `deposit` from the sender for `payee`; payer may time out after `expiry_blocks`.
    Open {
        payee: Address,
        deposit: u64,
        expiry_blocks: u64,
    },
    /// Payer adds `amount` to the deposit.
    TopUp { channel: ChannelId, amount: u64 },
    /// Payee claims the voucher's cumulative amount; the rest returns to the payer.
    Close { voucher: SignedVoucher },
    /// Payer starts the timeout close after expiry, optionally recording a voucher.
    StartClose {
        channel: ChannelId,
        voucher: Option<SignedVoucher>,
    },
    /// Settle a timeout close once the challenge period is over.
    Finalize { channel: ChannelId },
}

fn enc_voucher(v: &SignedVoucher, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_bytes());
}

fn dec_voucher(r: &mut Reader<'_>) -> Result<SignedVoucher, DecodeError> {
    let mut b = [0u8; SignedVoucher::SIZE];
    for x in b.iter_mut() {
        *x = u8::decode_from(r)?;
    }
    SignedVoucher::from_bytes(&b).ok_or(DecodeError::TrailingBytes)
}

impl ChannelOp {
    /// Wire form: `sub:u8 || fields` (the transaction tag is written by `tx.rs`).
    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            ChannelOp::Open {
                payee,
                deposit,
                expiry_blocks,
            } => {
                0u8.encode_to(out);
                payee.encode_to(out);
                deposit.encode_to(out);
                expiry_blocks.encode_to(out);
            }
            ChannelOp::TopUp { channel, amount } => {
                1u8.encode_to(out);
                channel.encode_to(out);
                amount.encode_to(out);
            }
            ChannelOp::Close { voucher } => {
                2u8.encode_to(out);
                enc_voucher(voucher, out);
            }
            ChannelOp::StartClose { channel, voucher } => {
                3u8.encode_to(out);
                channel.encode_to(out);
                match voucher {
                    None => 0u8.encode_to(out),
                    Some(v) => {
                        1u8.encode_to(out);
                        enc_voucher(v, out);
                    }
                }
            }
            ChannelOp::Finalize { channel } => {
                4u8.encode_to(out);
                channel.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => ChannelOp::Open {
                payee: <[u8; 32]>::decode_from(r)?,
                deposit: u64::decode_from(r)?,
                expiry_blocks: u64::decode_from(r)?,
            },
            1 => ChannelOp::TopUp {
                channel: <[u8; 32]>::decode_from(r)?,
                amount: u64::decode_from(r)?,
            },
            2 => ChannelOp::Close {
                voucher: dec_voucher(r)?,
            },
            3 => ChannelOp::StartClose {
                channel: <[u8; 32]>::decode_from(r)?,
                voucher: match u8::decode_from(r)? {
                    0 => None,
                    1 => Some(dec_voucher(r)?),
                    _ => return Err(DecodeError::TrailingBytes),
                },
            },
            4 => ChannelOp::Finalize {
                channel: <[u8; 32]>::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    /// Gas charged before anything runs. Voucher checks cost a signature verification.
    pub fn gas(&self) -> u64 {
        match self {
            ChannelOp::Close { .. } | ChannelOp::StartClose { .. } => crate::CHANNEL_VOUCHER_GAS,
            _ => crate::CHANNEL_GAS,
        }
    }

    /// What the op takes from the sender's balance on top of the fee.
    pub fn debit(&self) -> u64 {
        match self {
            ChannelOp::Open { deposit, .. } => *deposit,
            ChannelOp::TopUp { amount, .. } => *amount,
            _ => 0,
        }
    }
}

/// Why a channel transaction was refused (a revert: nonce used, fee charged, nothing else).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    NoSuchChannel,
    NotPayer,
    NotPayee,
    ZeroAmount,
    /// Payer and payee are the same account.
    SelfChannel,
    BadExpiry,
    InsufficientBalance,
    DepositOverflow,
    /// The voucher is for another channel.
    WrongChannel,
    /// The voucher is for another chain.
    WrongChain,
    BadSignature,
    /// The voucher claims more than the channel holds.
    ExceedsDeposit,
    /// The payer may only time out once the channel's expiry height has passed.
    NotExpired,
    /// A timeout close is already running.
    AlreadyClosing,
    /// Finalize before the challenge period is over.
    ChallengeOpen,
    /// The challenge period is over; the payee can no longer claim, only `Finalize` is left.
    ChallengeOver,
    /// The channel is closing; no top-up.
    Closing,
    /// Finalize on a channel with no timeout close started.
    NotClosing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fail {
    Tx(ChannelError),
    State(StateError),
}
impl From<ChannelError> for Fail {
    fn from(e: ChannelError) -> Self {
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

/// A timeout close in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closing {
    /// First height at which `Finalize` works; the payee can claim before it.
    pub deadline: u64,
    /// The amount the payer offered when it started the close (0 if none).
    pub offered: u64,
}

/// A stored channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub payer: Address,
    /// The payer's Ed25519 key (from the opening transaction): vouchers verify against it.
    pub payer_key: [u8; 32],
    pub payee: Address,
    pub deposit: u64,
    /// The payer may start a timeout close from this height on.
    pub expiry_height: u64,
    pub closing: Option<Closing>,
}

fn rec_key(id: &ChannelId) -> Vec<u8> {
    let mut k = vec![TAG_CHANNEL];
    k.extend_from_slice(id);
    k
}
fn locked_key(id: &ChannelId) -> Vec<u8> {
    let mut k = vec![TAG_LOCKED];
    k.extend_from_slice(id);
    k
}

pub fn channel_id(payer: &Address, nonce: u64) -> ChannelId {
    sha256(&[b"btb.channel.v1", payer, &nonce.to_le_bytes()])
}

fn encode_rec(c: &Channel) -> Vec<u8> {
    let mut b = Vec::new();
    c.payer.encode_to(&mut b);
    c.payer_key.encode_to(&mut b);
    c.payee.encode_to(&mut b);
    c.expiry_height.encode_to(&mut b);
    match c.closing {
        None => 0u8.encode_to(&mut b),
        Some(cl) => {
            1u8.encode_to(&mut b);
            cl.deadline.encode_to(&mut b);
            cl.offered.encode_to(&mut b);
        }
    }
    b
}

fn decode_rec(b: &[u8], deposit: u64) -> Result<Channel, DecodeError> {
    let mut r = Reader::new(b, 1024);
    let payer = <[u8; 32]>::decode_from(&mut r)?;
    let payer_key = <[u8; 32]>::decode_from(&mut r)?;
    let payee = <[u8; 32]>::decode_from(&mut r)?;
    let expiry_height = u64::decode_from(&mut r)?;
    let closing = match u8::decode_from(&mut r)? {
        0 => None,
        _ => Some(Closing {
            deadline: u64::decode_from(&mut r)?,
            offered: u64::decode_from(&mut r)?,
        }),
    };
    Ok(Channel {
        payer,
        payer_key,
        payee,
        deposit,
        expiry_height,
        closing,
    })
}

/// Read a channel.
pub(crate) fn get(kv: &impl KvStore, id: &ChannelId) -> Result<Option<Channel>, StateError> {
    let Some(b) = kv.get(&rec_key(id))? else {
        return Ok(None);
    };
    Ok(Some(decode_rec(&b, read_u64(kv, &locked_key(id))?)?))
}

fn load(kv: &impl KvStore, id: &ChannelId) -> Result<Channel, Fail> {
    get(kv, id)?.ok_or_else(|| ChannelError::NoSuchChannel.into())
}

fn balance(kv: &impl KvStore, a: &Address) -> Result<u64, KvError> {
    read_u64(kv, &layout::balance_key(a))
}
fn set_balance(kv: &mut impl KvStore, a: &Address, v: u64) -> Result<(), KvError> {
    kv.put(&layout::balance_key(a), &v.to_le_bytes())
}
fn credit(kv: &mut impl KvStore, a: &Address, amount: u64) -> Result<(), StateError> {
    if amount == 0 {
        return Ok(());
    }
    let b = balance(kv, a)?
        .checked_add(amount)
        .ok_or_else(|| StateError("balance overflow".into()))?;
    Ok(set_balance(kv, a, b)?)
}

/// Check a voucher against the channel and return its amount.
fn check_voucher(
    chain_id: u64,
    id: &ChannelId,
    ch: &Channel,
    v: &SignedVoucher,
) -> Result<u64, Fail> {
    if v.voucher.chain_id != chain_id {
        return Err(ChannelError::WrongChain.into());
    }
    if v.voucher.channel_id != *id {
        return Err(ChannelError::WrongChannel.into());
    }
    if !v.verify(&ch.payer_key) {
        return Err(ChannelError::BadSignature.into());
    }
    if v.voucher.amount > ch.deposit {
        return Err(ChannelError::ExceedsDeposit.into());
    }
    Ok(v.voucher.amount)
}

/// Pay `claim` to the payee, the rest to the payer, and delete the channel.
fn settle(kv: &mut impl KvStore, id: &ChannelId, ch: &Channel, claim: u64) -> Result<(), Fail> {
    credit(kv, &ch.payee, claim)?;
    credit(kv, &ch.payer, ch.deposit - claim)?;
    kv.delete(&rec_key(id))?;
    kv.delete(&locked_key(id))?;
    Ok(())
}

/// Run one channel op for `sender` at block `height`. Everything is checked before anything is
/// written, so a refusal changes nothing. Returns the receipt output.
pub(crate) fn apply(
    kv: &mut impl KvStore,
    chain_id: u64,
    sender: &Address,
    sender_key: &[u8; 32],
    tx_nonce: u64,
    height: u64,
    op: &ChannelOp,
) -> Result<Vec<u8>, Fail> {
    match op {
        ChannelOp::Open {
            payee,
            deposit,
            expiry_blocks,
        } => {
            if *deposit == 0 {
                return Err(ChannelError::ZeroAmount.into());
            }
            if payee == sender {
                return Err(ChannelError::SelfChannel.into());
            }
            if !(1..=MAX_EXPIRY_BLOCKS).contains(expiry_blocks) {
                return Err(ChannelError::BadExpiry.into());
            }
            let bal = balance(kv, sender)?;
            if bal < *deposit {
                return Err(ChannelError::InsufficientBalance.into());
            }
            let id = channel_id(sender, tx_nonce);
            let ch = Channel {
                payer: *sender,
                payer_key: *sender_key,
                payee: *payee,
                deposit: *deposit,
                expiry_height: height.saturating_add(*expiry_blocks),
                closing: None,
            };
            set_balance(kv, sender, bal - deposit)?;
            kv.put(&rec_key(&id), &encode_rec(&ch))?;
            kv.put(&locked_key(&id), &deposit.to_le_bytes())?;
            Ok(id.to_vec())
        }
        ChannelOp::TopUp { channel, amount } => {
            let ch = load(kv, channel)?;
            if ch.payer != *sender {
                return Err(ChannelError::NotPayer.into());
            }
            if *amount == 0 {
                return Err(ChannelError::ZeroAmount.into());
            }
            if ch.closing.is_some() {
                return Err(ChannelError::Closing.into());
            }
            let bal = balance(kv, sender)?;
            if bal < *amount {
                return Err(ChannelError::InsufficientBalance.into());
            }
            let new = ch
                .deposit
                .checked_add(*amount)
                .ok_or(ChannelError::DepositOverflow)?;
            set_balance(kv, sender, bal - amount)?;
            kv.put(&locked_key(channel), &new.to_le_bytes())?;
            Ok(new.to_le_bytes().to_vec())
        }
        ChannelOp::Close { voucher } => {
            let id = voucher.voucher.channel_id;
            let ch = load(kv, &id)?;
            if ch.payee != *sender {
                return Err(ChannelError::NotPayee.into());
            }
            if matches!(ch.closing, Some(c) if height >= c.deadline) {
                return Err(ChannelError::ChallengeOver.into());
            }
            let amount = check_voucher(chain_id, &id, &ch, voucher)?;
            let offered = ch.closing.map_or(0, |c| c.offered);
            let claim = amount.max(offered);
            settle(kv, &id, &ch, claim)?;
            Ok(claim.to_le_bytes().to_vec())
        }
        ChannelOp::StartClose { channel, voucher } => {
            let mut ch = load(kv, channel)?;
            if ch.payer != *sender {
                return Err(ChannelError::NotPayer.into());
            }
            if height < ch.expiry_height {
                return Err(ChannelError::NotExpired.into());
            }
            if ch.closing.is_some() {
                return Err(ChannelError::AlreadyClosing.into());
            }
            let offered = match voucher {
                Some(v) => check_voucher(chain_id, channel, &ch, v)?,
                None => 0,
            };
            let deadline = height.saturating_add(CHANNEL_CHALLENGE_BLOCKS);
            ch.closing = Some(Closing { deadline, offered });
            kv.put(&rec_key(channel), &encode_rec(&ch))?;
            Ok(deadline.to_le_bytes().to_vec())
        }
        ChannelOp::Finalize { channel } => {
            let ch = load(kv, channel)?;
            if ch.payer != *sender && ch.payee != *sender {
                return Err(ChannelError::NotPayer.into());
            }
            let Some(c) = ch.closing else {
                return Err(ChannelError::NotClosing.into());
            };
            if height < c.deadline {
                return Err(ChannelError::ChallengeOpen.into());
            }
            settle(kv, channel, &ch, c.offered)?;
            Ok(c.offered.to_le_bytes().to_vec())
        }
    }
}

impl<S: KvStore> crate::state::ChainState<S> {
    /// A channel by id (`None` once it is settled).
    pub fn channel(&self, id: &ChannelId) -> Result<Option<Channel>, StateError> {
        get(&self.kv, id)
    }

    /// Total BTB currently locked in channels.
    pub fn channels_locked(&self) -> Result<u128, StateError> {
        Ok(crate::state::sum_tag(&self.kv, TAG_LOCKED)?)
    }
}
