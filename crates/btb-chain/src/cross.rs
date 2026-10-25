//! The cross-chain transaction (tag 21): key generation for the external-chain keys, signature
//! submission and evidence, deposit attestation and withdrawals (`docs/CROSS_CHAIN.md`).
//!
//! | Sub | Operation | Sent by | Module |
//! | --- | --- | --- | --- |
//! | 0 | `Deal` | validator (its consensus key) | `crate::xdkg` |
//! | 1 | `Complain` | validator | `crate::xdkg` |
//! | 2 | `Signature` | anyone (the result is self-authenticating) | `crate::xsign` |
//! | 3 | `Evidence` | anyone | `crate::xsign` |
//! | 4 | `Attest` | validator | `crate::vault` |
//! | 5 | `Prepare` | validator | `crate::vault` |
//! | 6 | `Withdraw` | any account holding the wrapped token | `crate::vault` |
//!
//! `check_static` and `gas` need no state; `apply` runs against the overlay. A refused operation
//! charges its fee and changes nothing ("validate first, write last").

use crate::staking::StakingConfig;
use crate::tx::Address;
use crate::vault::{self, DepositClaim, VaultError, VaultFail};
use crate::xdkg::{self, XFail, XdkgError};
use crate::xsign::{self, EvidenceOp, Proof, XsignError, XsignFail};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_oracle::dkg_crypto::{CT_BYTES, PROOF_BYTES};
use btb_oracle::frost::Scheme;
use btb_runtime::kv::KvStore;

/// Largest single commitment or share field accepted in a transaction.
const MAX_FIELD: usize = 1 << 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossOp {
    Deal {
        scheme: Scheme,
        target: u64,
        eph: [u8; 32],
        commitments: Vec<Vec<u8>>,
        ciphertexts: Vec<[u8; CT_BYTES]>,
    },
    Complain {
        scheme: Scheme,
        target: u64,
        dealer: u32,
        shared: [u8; 32],
        proof: [u8; PROOF_BYTES],
    },
    Signature {
        request: u64,
        signature: Vec<u8>,
    },
    Evidence(EvidenceOp),
    Attest {
        claims: Vec<DepositClaim>,
    },
    Prepare {
        withdrawal: u64,
        digest: [u8; 32],
        sighashes: Vec<[u8; 32]>,
    },
    Withdraw {
        asset: Scheme,
        amount: u64,
        destination: Vec<u8>,
    },
}

/// Why a cross-chain transaction was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossError {
    Dkg(XdkgError),
    Sign(XsignError),
    Vault(VaultError),
}

pub(crate) enum Fail {
    Tx(CrossError),
    State(crate::state::StateError),
}

impl From<XFail> for Fail {
    fn from(e: XFail) -> Self {
        match e {
            XFail::Tx(e) => Fail::Tx(CrossError::Dkg(e)),
            XFail::State(e) => Fail::State(e),
        }
    }
}
impl From<XsignFail> for Fail {
    fn from(e: XsignFail) -> Self {
        match e {
            XsignFail::Tx(e) => Fail::Tx(CrossError::Sign(e)),
            XsignFail::State(e) => Fail::State(e),
        }
    }
}
impl From<VaultFail> for Fail {
    fn from(e: VaultFail) -> Self {
        match e {
            VaultFail::Tx(e) => Fail::Tx(CrossError::Vault(e)),
            VaultFail::State(e) => Fail::State(e),
        }
    }
}

impl CrossOp {
    pub fn gas(&self) -> u64 {
        match self {
            CrossOp::Deal {
                commitments,
                ciphertexts,
                ..
            } => {
                xdkg::DEAL_BASE_GAS
                    + xdkg::DEAL_PER_ITEM_GAS
                        * (commitments.len() + ciphertexts.len()).min(2 * xdkg::MAX_PARTIES) as u64
            }
            CrossOp::Complain { .. } => xdkg::COMPLAIN_GAS,
            CrossOp::Signature { .. } => xsign::SIGNATURE_GAS,
            CrossOp::Evidence(_) => xsign::EVIDENCE_GAS,
            CrossOp::Attest { claims } => {
                vault::ATTEST_BASE_GAS
                    + vault::ATTEST_PER_CLAIM_GAS * claims.len().min(vault::MAX_CLAIMS) as u64
            }
            CrossOp::Prepare { .. } => vault::PREPARE_GAS,
            CrossOp::Withdraw { .. } => vault::WITHDRAW_GAS,
        }
    }

    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            CrossOp::Deal {
                scheme,
                target,
                eph,
                commitments,
                ciphertexts,
            } => {
                0u8.encode_to(out);
                scheme.byte().encode_to(out);
                target.encode_to(out);
                eph.encode_to(out);
                commitments.encode_to(out);
                ciphertexts.encode_to(out);
            }
            CrossOp::Complain {
                scheme,
                target,
                dealer,
                shared,
                proof,
            } => {
                1u8.encode_to(out);
                scheme.byte().encode_to(out);
                target.encode_to(out);
                dealer.encode_to(out);
                shared.encode_to(out);
                proof.encode_to(out);
            }
            CrossOp::Signature { request, signature } => {
                2u8.encode_to(out);
                request.encode_to(out);
                signature.encode_to(out);
            }
            CrossOp::Evidence(e) => {
                3u8.encode_to(out);
                e.request.encode_to(out);
                e.attempt.encode_to(out);
                e.origin.encode_to(out);
                e.holder.encode_to(out);
                e.proof.encode_to(out);
            }
            CrossOp::Attest { claims } => {
                4u8.encode_to(out);
                (claims.len() as u32).encode_to(out);
                for c in claims {
                    c.encode_to(out);
                }
            }
            CrossOp::Prepare {
                withdrawal,
                digest,
                sighashes,
            } => {
                5u8.encode_to(out);
                withdrawal.encode_to(out);
                digest.encode_to(out);
                sighashes.encode_to(out);
            }
            CrossOp::Withdraw {
                asset,
                amount,
                destination,
            } => {
                6u8.encode_to(out);
                asset.byte().encode_to(out);
                amount.encode_to(out);
                destination.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        fn scheme(r: &mut Reader<'_>) -> Result<Scheme, DecodeError> {
            Scheme::from_byte(u8::decode_from(r)?).ok_or(DecodeError::TrailingBytes)
        }
        Ok(Ok(match u8::decode_from(r)? {
            0 => CrossOp::Deal {
                scheme: scheme(r)?,
                target: u64::decode_from(r)?,
                eph: <[u8; 32]>::decode_from(r)?,
                commitments: Vec::decode_from(r)?,
                ciphertexts: Vec::decode_from(r)?,
            },
            1 => CrossOp::Complain {
                scheme: scheme(r)?,
                target: u64::decode_from(r)?,
                dealer: u32::decode_from(r)?,
                shared: <[u8; 32]>::decode_from(r)?,
                proof: <[u8; PROOF_BYTES]>::decode_from(r)?,
            },
            2 => CrossOp::Signature {
                request: u64::decode_from(r)?,
                signature: Vec::decode_from(r)?,
            },
            3 => CrossOp::Evidence(EvidenceOp {
                request: u64::decode_from(r)?,
                attempt: u32::decode_from(r)?,
                origin: u64::decode_from(r)?,
                holder: u32::decode_from(r)?,
                proof: Proof::decode_from(r)?,
            }),
            4 => {
                let n = u32::decode_from(r)? as usize;
                if n > vault::MAX_CLAIMS {
                    return Err(DecodeError::TrailingBytes);
                }
                let mut claims = Vec::with_capacity(n);
                for _ in 0..n {
                    claims.push(DepositClaim::decode_from(r)?);
                }
                CrossOp::Attest { claims }
            }
            5 => CrossOp::Prepare {
                withdrawal: u64::decode_from(r)?,
                digest: <[u8; 32]>::decode_from(r)?,
                sighashes: Vec::decode_from(r)?,
            },
            6 => CrossOp::Withdraw {
                asset: scheme(r)?,
                amount: u64::decode_from(r)?,
                destination: Vec::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    /// Checks that need no state.
    pub fn check_static(&self) -> Result<(), CrossError> {
        let too_big = || CrossError::Dkg(XdkgError::BadShape);
        match self {
            CrossOp::Deal {
                commitments,
                ciphertexts,
                ..
            } => {
                if commitments.is_empty()
                    || commitments.len() > xdkg::MAX_PARTIES
                    || ciphertexts.is_empty()
                    || ciphertexts.len() > xdkg::MAX_PARTIES
                    || commitments.iter().any(|c| c.len() > 64)
                {
                    return Err(too_big());
                }
            }
            CrossOp::Signature { signature, .. } => {
                if signature.len() > 128 {
                    return Err(CrossError::Sign(XsignError::BadSignature));
                }
            }
            CrossOp::Evidence(e) => {
                if let Proof::BadShare { package, share, .. } = &e.proof {
                    if package.len() > MAX_FIELD * 4 || share.len() > MAX_FIELD {
                        return Err(CrossError::Sign(XsignError::BadEvidence));
                    }
                }
            }
            CrossOp::Attest { claims } => {
                if claims.is_empty() || claims.len() > vault::MAX_CLAIMS {
                    return Err(CrossError::Vault(VaultError::BadShape));
                }
            }
            CrossOp::Prepare { sighashes, .. } => {
                if sighashes.is_empty() || sighashes.len() > vault::MAX_SIGHASHES {
                    return Err(CrossError::Vault(VaultError::BadShape));
                }
            }
            CrossOp::Withdraw { destination, .. } => {
                if destination.len() > vault::MAX_DESTINATION {
                    return Err(CrossError::Vault(VaultError::BadDestination));
                }
            }
            CrossOp::Complain { .. } => {}
        }
        Ok(())
    }
}

/// Run a cross-chain transaction. `sender` is the account address, `sender_key` the transaction's
/// public key (a consensus key for the validator operations).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(
    kv: &mut impl KvStore,
    cfg: &StakingConfig,
    chain_id: u64,
    height: u64,
    sender: &Address,
    sender_key: &[u8; 32],
    op: &CrossOp,
) -> Result<Vec<u8>, Fail> {
    op.check_static().map_err(Fail::Tx)?;
    let epoch = cfg.epoch_of_block(height);
    match op {
        CrossOp::Deal {
            scheme,
            target,
            eph,
            commitments,
            ciphertexts,
        } => Ok(xdkg::apply_deal(
            kv,
            height,
            sender_key,
            *scheme,
            *target,
            eph,
            commitments,
            ciphertexts,
        )?),
        CrossOp::Complain {
            scheme,
            target,
            dealer,
            shared,
            proof,
        } => Ok(xdkg::apply_complain(
            kv, chain_id, height, sender_key, *scheme, *target, *dealer, shared, proof,
        )?),
        CrossOp::Signature { request, signature } => Ok(xsign::apply_signature(
            kv, epoch, height, *request, signature,
        )?),
        CrossOp::Evidence(e) => Ok(xsign::apply_evidence(kv, chain_id, epoch, e)?),
        CrossOp::Attest { claims } => Ok(vault::apply_attest(kv, epoch, sender_key, claims)?),
        CrossOp::Prepare {
            withdrawal,
            digest,
            sighashes,
        } => Ok(vault::apply_prepare(
            kv,
            epoch,
            height,
            sender_key,
            *withdrawal,
            digest,
            sighashes,
        )?),
        CrossOp::Withdraw {
            asset,
            amount,
            destination,
        } => Ok(vault::apply_withdraw(
            kv,
            epoch,
            height,
            sender,
            *asset,
            *amount,
            destination,
        )?),
    }
}

/// Start of a block (after the beacon DKG's own hook).
pub(crate) fn begin_block(
    kv: &mut impl KvStore,
    height: u64,
) -> Result<(), crate::state::StateError> {
    xdkg::begin_block(kv, height)
}

/// End of a block (after the beacon DKG's own hook).
pub(crate) fn end_block(
    kv: &mut impl KvStore,
    height: u64,
) -> Result<(), crate::state::StateError> {
    xdkg::end_block(kv, height)?;
    xsign::end_block(kv, height)
}
