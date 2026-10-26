//! The hosting-node registry: who may be paid the relay share of a fee.
//!
//! A hosting or edge node that forwards client-signed transactions registers its account once
//! (`Register`, with the URL clients reach it at). A transaction whose signed `relayer` is a
//! registered host pays that host 20% of its fee (`RELAY_SHARE_BPS`); otherwise the whole fee
//! goes to the validator pool (`docs/CHAIN.md`, "Fees"). The registry is one key per host:
//! `TAG_HOST || address` -> the URL bytes.
//!
//! Registering locks a bond (`ChainConfig::host_bond`) taken from the host's balance into
//! `0x61` (`TAG_HOST_BOND`, counted by the supply invariant). `Unregister` removes the host
//! from the registry at once (it stops earning) and moves the bond into `0x62` (unbonding)
//! until `host_unbonding_blocks` later, when `begin_block` returns it to the balance. The
//! executor never pays a sender its own relay share (it falls back to the validator pool). A
//! sender who bonds a second account can still rebate itself the relay share; that is a
//! priced fee discount, not a hole (`docs/CHAIN.md`, "Relayers and the hosting-node registry").
//!
//! | tag | key | value |
//! | --- | --- | --- |
//! | `0x60` | `60 host` | URL bytes |
//! | `0x61` | `61 host` | bond, `u64` LE |
//! | `0x62` | `62 mature:u64 BE host` | unbonding amount, `u64` LE |

use crate::state::{read_u64, ChainConfig, StateError};
use crate::tx::Address;
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};
use btb_runtime::layout;

pub const TAG_HOST: u8 = 0x60;
pub const TAG_HOST_BOND: u8 = 0x61;
pub const TAG_HOST_UNBOND: u8 = 0x62;
pub const HOST_GAS: u64 = 10_000;
pub const MAX_URL_BYTES: usize = 256;

/// State key of a registered host.
pub fn host_key(host: &Address) -> Vec<u8> {
    let mut k = vec![TAG_HOST];
    k.extend_from_slice(host);
    k
}

fn bond_key(host: &Address) -> Vec<u8> {
    let mut k = vec![TAG_HOST_BOND];
    k.extend_from_slice(host);
    k
}

fn unbond_key(mature: u64, host: &Address) -> Vec<u8> {
    let mut k = vec![TAG_HOST_UNBOND];
    k.extend_from_slice(&mature.to_be_bytes());
    k.extend_from_slice(host);
    k
}

/// The bond currently locked for `host` (0 if not registered).
pub fn bond(kv: &impl KvStore, host: &Address) -> Result<u64, KvError> {
    read_u64(kv, &bond_key(host))
}

/// The host's unbonding bond: `(maturity height, amount)` entries.
pub fn unbonding(kv: &impl KvStore, host: &Address) -> Result<Vec<(u64, u64)>, KvError> {
    let mut out = Vec::new();
    for (k, v) in kv.scan(
        &[TAG_HOST_UNBOND],
        Some(&[TAG_HOST_UNBOND + 1]),
        false,
        1 << 20,
    )? {
        if k.len() == 41 && &k[9..] == host {
            let mature = <[u8; 8]>::try_from(&k[1..9]).map_or(0, u64::from_be_bytes);
            out.push((mature, <[u8; 8]>::try_from(v).map_or(0, u64::from_le_bytes)));
        }
    }
    Ok(out)
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
    /// The balance cannot cover the registration bond.
    InsufficientBond,
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

pub(crate) fn apply(
    kv: &mut impl KvStore,
    cfg: &ChainConfig,
    height: u64,
    sender: &Address,
    op: &HostOp,
) -> Result<(), Fail> {
    op.check_static().map_err(Fail::Tx)?;
    match op {
        HostOp::Register { url } => {
            // Re-registering only updates the URL; the bond is taken once.
            if kv.get(&host_key(sender))?.is_none() {
                let bal = read_u64(kv, &layout::balance_key(sender))?;
                let Some(rest) = bal.checked_sub(cfg.host_bond) else {
                    return Err(Fail::Tx(HostError::InsufficientBond));
                };
                kv.put(&layout::balance_key(sender), &rest.to_le_bytes())?;
                kv.put(&bond_key(sender), &cfg.host_bond.to_le_bytes())?;
            }
            kv.put(&host_key(sender), url)?;
        }
        HostOp::Unregister => {
            if kv.get(&host_key(sender))?.is_none() {
                return Err(Fail::Tx(HostError::NotRegistered));
            }
            kv.delete(&host_key(sender))?;
            let amount = read_u64(kv, &bond_key(sender))?;
            kv.delete(&bond_key(sender))?;
            let key = unbond_key(height.saturating_add(cfg.host_unbonding_blocks), sender);
            let total = read_u64(kv, &key)?
                .checked_add(amount)
                .ok_or_else(|| Fail::State(StateError("unbonding overflow".into())))?;
            kv.put(&key, &total.to_le_bytes())?;
        }
    }
    Ok(())
}

/// Start of every block: return host bonds whose unbonding has matured to their balances.
pub(crate) fn begin_block(kv: &mut impl KvStore, height: u64) -> Result<(), StateError> {
    let mut end = vec![TAG_HOST_UNBOND];
    end.extend_from_slice(&height.saturating_add(1).to_be_bytes());
    loop {
        let due = kv.scan(&[TAG_HOST_UNBOND], Some(&end), false, 256)?;
        if due.is_empty() {
            return Ok(());
        }
        for (k, v) in due {
            let host: Address = k[9..]
                .try_into()
                .map_err(|_| StateError("bad host unbonding key".into()))?;
            let amount = <[u8; 8]>::try_from(v).map_or(0, u64::from_le_bytes);
            let bal = read_u64(kv, &layout::balance_key(&host))?
                .checked_add(amount)
                .ok_or_else(|| StateError("balance overflow".into()))?;
            kv.put(&layout::balance_key(&host), &bal.to_le_bytes())?;
            kv.delete(&k)?;
        }
    }
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
