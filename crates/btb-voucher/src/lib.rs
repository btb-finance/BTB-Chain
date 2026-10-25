//! Payment-channel vouchers (see `docs/TOKENOMICS.md`, "Payment channels for per-request fees").
//!
//! A voucher says: "on chain `chain_id`, in channel `channel_id`, I (the payer) have now paid
//! `amount` in total". `amount` is *cumulative*, so the highest voucher supersedes all earlier
//! ones and the payee only ever needs to keep the latest. `nonce` is a per-channel counter that
//! grows by at least one with every voucher; wallets and hosts use it (with `amount`) to
//! reject replays and reordering, the chain settles on the highest `amount`.
//!
//! Signed message: `SHA256("btb.voucher.v1" || chain_id:u64le || channel_id[32] || amount:u64le
//! || nonce:u64le)`, signed with Ed25519 (strict verification) by the payer's account key. The
//! chain id and the channel id (unique per channel) make a voucher useless anywhere else.
//!
//! Wire form of a signed voucher ([`SignedVoucher::SIZE`] = 120 bytes): `chain_id:u64le ||
//! channel_id[32] || amount:u64le || nonce:u64le || signature[64]`.
//!
//! `no_std`, no allocation. [`Spender`] is the payer-side counter, [`Ledger`] the payee-side
//! check; both refuse anything that is not strictly increasing.

#![no_std]

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

pub const DOMAIN: &[u8] = b"btb.voucher.v1";

pub type ChannelId = [u8; 32];

/// What the payer commits to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Voucher {
    pub chain_id: u64,
    pub channel_id: ChannelId,
    /// Total paid so far in this channel, base units.
    pub amount: u64,
    pub nonce: u64,
}

impl Voucher {
    /// The 32-byte hash that is signed.
    pub fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(DOMAIN);
        h.update(self.chain_id.to_le_bytes());
        h.update(self.channel_id);
        h.update(self.amount.to_le_bytes());
        h.update(self.nonce.to_le_bytes());
        h.finalize().into()
    }

    pub fn sign(self, key: &SigningKey) -> SignedVoucher {
        SignedVoucher {
            signature: key.sign(&self.digest()).to_bytes(),
            voucher: self,
        }
    }
}

/// A voucher and the payer's signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedVoucher {
    pub voucher: Voucher,
    pub signature: [u8; 64],
}

impl SignedVoucher {
    /// Encoded length: chain id 8 + channel id 32 + amount 8 + nonce 8 + signature 64.
    pub const SIZE: usize = 8 + 32 + 8 + 8 + 64;

    /// Whether `payer_key` (an Ed25519 public key) signed exactly this voucher. Strict: weak
    /// keys and non-canonical signatures fail.
    pub fn verify(&self, payer_key: &[u8; 32]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(payer_key) else {
            return false;
        };
        let sig = Signature::from_bytes(&self.signature);
        vk.verify_strict(&self.voucher.digest(), &sig).is_ok()
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let v = &self.voucher;
        let mut o = [0u8; Self::SIZE];
        o[0..8].copy_from_slice(&v.chain_id.to_le_bytes());
        o[8..40].copy_from_slice(&v.channel_id);
        o[40..48].copy_from_slice(&v.amount.to_le_bytes());
        o[48..56].copy_from_slice(&v.nonce.to_le_bytes());
        o[56..].copy_from_slice(&self.signature);
        o
    }

    /// `None` unless `b` is exactly [`Self::SIZE`] bytes.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != Self::SIZE {
            return None;
        }
        let u = |r: core::ops::Range<usize>| u64::from_le_bytes(b[r].try_into().unwrap());
        Some(SignedVoucher {
            voucher: Voucher {
                chain_id: u(0..8),
                channel_id: b[8..40].try_into().unwrap(),
                amount: u(40..48),
                nonce: u(48..56),
            },
            signature: b[56..].try_into().unwrap(),
        })
    }
}

/// Why a voucher or a payment was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoucherError {
    WrongChain,
    WrongChannel,
    BadSignature,
    /// The amount is not above the highest one seen (a replay or an older voucher).
    NotIncreasing,
    /// The nonce is not above the last one seen.
    StaleNonce,
    /// The amount is above what the channel holds.
    ExceedsDeposit,
    /// The payment is zero, or the running total would overflow.
    BadAmount,
}

/// Payer side: tracks what has been spent in one channel and signs the next voucher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spender {
    chain_id: u64,
    channel_id: ChannelId,
    deposit: u64,
    spent: u64,
    nonce: u64,
}

impl Spender {
    pub fn new(chain_id: u64, channel_id: ChannelId, deposit: u64) -> Self {
        Spender {
            chain_id,
            channel_id,
            deposit,
            spent: 0,
            nonce: 0,
        }
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }
    pub fn remaining(&self) -> u64 {
        self.deposit - self.spent
    }
    /// Record a top-up of the channel.
    pub fn add_deposit(&mut self, amount: u64) -> Result<(), VoucherError> {
        self.deposit = self
            .deposit
            .checked_add(amount)
            .ok_or(VoucherError::BadAmount)?;
        Ok(())
    }

    /// Pay `price` more: returns the voucher for the new running total. Nothing changes on error,
    /// and the spender never signs more than the deposit.
    pub fn pay(&mut self, price: u64, key: &SigningKey) -> Result<SignedVoucher, VoucherError> {
        if price == 0 {
            return Err(VoucherError::BadAmount);
        }
        let amount = self
            .spent
            .checked_add(price)
            .ok_or(VoucherError::BadAmount)?;
        if amount > self.deposit {
            return Err(VoucherError::ExceedsDeposit);
        }
        let nonce = self.nonce.checked_add(1).ok_or(VoucherError::BadAmount)?;
        let v = Voucher {
            chain_id: self.chain_id,
            channel_id: self.channel_id,
            amount,
            nonce,
        }
        .sign(key);
        self.spent = amount;
        self.nonce = nonce;
        Ok(v)
    }
}

/// Payee (host) side: accepts vouchers for one channel and keeps the highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ledger {
    chain_id: u64,
    channel_id: ChannelId,
    payer_key: [u8; 32],
    deposit: u64,
    best: Option<SignedVoucher>,
}

impl Ledger {
    pub fn new(chain_id: u64, channel_id: ChannelId, payer_key: [u8; 32], deposit: u64) -> Self {
        Ledger {
            chain_id,
            channel_id,
            payer_key,
            deposit,
            best: None,
        }
    }

    /// Total the payer has committed to so far.
    pub fn earned(&self) -> u64 {
        self.best.map_or(0, |b| b.voucher.amount)
    }
    /// The voucher to submit on chain to claim [`Self::earned`].
    pub fn best(&self) -> Option<&SignedVoucher> {
        self.best.as_ref()
    }
    pub fn add_deposit(&mut self, amount: u64) -> Result<(), VoucherError> {
        self.deposit = self
            .deposit
            .checked_add(amount)
            .ok_or(VoucherError::BadAmount)?;
        Ok(())
    }

    /// Check a voucher and, if it is new, keep it. Returns the newly earned amount. Refuses a
    /// voucher for another chain or channel, a bad signature, an amount or nonce that does not
    /// strictly grow (so replays and reordered vouchers change nothing) and one above the deposit.
    pub fn accept(&mut self, sv: &SignedVoucher) -> Result<u64, VoucherError> {
        let v = &sv.voucher;
        if v.chain_id != self.chain_id {
            return Err(VoucherError::WrongChain);
        }
        if v.channel_id != self.channel_id {
            return Err(VoucherError::WrongChannel);
        }
        if !sv.verify(&self.payer_key) {
            return Err(VoucherError::BadSignature);
        }
        if let Some(b) = &self.best {
            if v.amount <= b.voucher.amount {
                return Err(VoucherError::NotIncreasing);
            }
            if v.nonce <= b.voucher.nonce {
                return Err(VoucherError::StaleNonce);
            }
        }
        if v.amount > self.deposit {
            return Err(VoucherError::ExceedsDeposit);
        }
        let delta = v.amount - self.earned();
        self.best = Some(*sv);
        Ok(delta)
    }
}
