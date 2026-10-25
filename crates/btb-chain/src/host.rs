//! The hosting-node registry: who may be paid the relay share of a fee.
//!
//! A hosting or edge node that forwards client-signed transactions registers its account once
//! (`Register`, with the URL clients reach it at). A transaction whose signed `relayer` is a
//! registered host pays that host 20% of its fee (`RELAY_SHARE_BPS`); otherwise the whole fee
//! goes to the validator pool (`docs/CHAIN.md`, "Fees"). The registry is one key per host:
//! `TAG_HOST || address` -> the URL bytes.
//!
//! There is no bond yet: anyone can register, including a sender who then names itself, so
//! the executor never pays a sender its own relay share (it falls back to the validator pool).

use crate::state::StateError;
use crate::tx::Address;
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};

pub const TAG_HOST: u8 = 0x60;
pub const HOST_GAS: u64 = 10_000;
pub const MAX_URL_BYTES: usize = 256;

/// State key of a registered host.
pub fn host_key(host: &Address) -> Vec<u8> {
    let mut k = vec![TAG_HOST];
    k.extend_from_slice(host);
    k
}

/// Whether `host` is a registered hosting node.
pub fn is_host(kv: &impl KvStore, host: &Address) -> Result<bool, KvError> {
    Ok(kv.get(&host_key(host))?.is_some())
}

/// The URL `host` registered, if any.
pub fn url(kv: &impl KvStore, host: &Address) -> Result<Option<Vec<u8>>, KvError> {
    kv.get(&host_key(host))
}

/// A registry transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostOp {
    /// Register (or update the URL of) the sender as a hosting node.
    Register { url: Vec<u8> },
    /// Remove the sender from the registry.
    Unregister,
}

/// Why a registry transaction was refused (the fee is still charged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    UrlTooLong,
    NotRegistered,
}

impl HostOp {
    pub fn gas(&self) -> u64 {
        HOST_GAS
    }

    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            HostOp::Register { url } => {
                0u8.encode_to(out);
                url.encode_to(out);
            }
            HostOp::Unregister => 1u8.encode_to(out),
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => HostOp::Register {
                url: Vec::decode_from(r)?,
            },
            1 => HostOp::Unregister,
            t => return Ok(Err(t)),
        }))
    }

    /// Checks that need no state.
    pub fn check_static(&self) -> Result<(), HostError> {
        match self {
            HostOp::Register { url } if url.len() > MAX_URL_BYTES => Err(HostError::UrlTooLong),
            _ => Ok(()),
        }
    }
}

pub(crate) enum Fail {
    Tx(HostError),
    State(StateError),
}

impl From<KvError> for Fail {
    fn from(e: KvError) -> Self {
        Fail::State(e.into())
    }
}

pub(crate) fn apply(kv: &mut impl KvStore, sender: &Address, op: &HostOp) -> Result<(), Fail> {
    op.check_static().map_err(Fail::Tx)?;
    match op {
        HostOp::Register { url } => kv.put(&host_key(sender), url)?,
        HostOp::Unregister => {
            if kv.get(&host_key(sender))?.is_none() {
                return Err(Fail::Tx(HostError::NotRegistered));
            }
            kv.delete(&host_key(sender))?;
        }
    }
    Ok(())
}

/// What a relay (a hosting or edge node) checks before it forwards a client-signed
/// transaction to validators: it decodes, the signature is valid, it is for this chain, and it
/// names `relayer` (this relay's own account). The client chose and signed the relayer, so the
/// relay can neither add nor change it; a transaction naming someone else would earn the relay
/// nothing and is refused.
pub fn check_relayed(
    bytes: &[u8],
    chain_id: u64,
    relayer: &Address,
) -> Result<crate::tx::Transaction, String> {
    let tx =
        crate::tx::Transaction::decode(bytes).map_err(|e| format!("not a transaction: {e:?}"))?;
    if !tx.verify_signature() {
        return Err("bad signature".into());
    }
    if tx.body.chain_id != chain_id {
        return Err(format!(
            "transaction is for chain {}, this node serves chain {chain_id}",
            tx.body.chain_id
        ));
    }
    match tx.body.relayer {
        Some(r) if &r == relayer => Ok(tx),
        Some(_) => Err("the transaction names a different relayer".into()),
        None => {
            Err("the transaction names no relayer: sign it with this node's relayer address".into())
        }
    }
}
