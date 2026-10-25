//! Wire frames between nodes and the block payload format.
//!
//! A frame is one kind byte followed by the body. Kind 1 is a consensus `Message`, kind 2 a
//! gossiped transaction (its wire encoding). Everything is size-bounded before it is parsed.
//!
//! The consensus payload of a block is a `Batch`: `timestamp:u64 || [tx wire bytes]`, or the
//! empty byte string (an empty block). The chain block (`btb_chain::Block`) is *derived* from the
//! batch when the consensus block commits, because the chain header carries the state root
//! of the previous block and, with a pipelined 2-chain commit, that block is not executed yet
//! when the next one is proposed. See docs/NODE.md.

use btb_chain::exec::check_static;
use btb_chain::{ChainConfig, Transaction};
use btb_codec::{Decode, Encode, Reader};
use btb_consensus::Message;
use std::collections::BTreeSet;

pub const KIND_CONSENSUS: u8 = 1;
pub const KIND_TX: u8 = 2;

/// Largest frame accepted from a peer. A sync response carries up to 64 blocks of at most
/// `MAX_PAYLOAD` bytes each.
pub const MAX_FRAME: usize = 20 << 20;
/// Largest block payload (also the consensus `max_payload_bytes`). Transactions bigger than
/// this are never included in a block.
pub const MAX_PAYLOAD: usize = 256 << 10;

pub fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 1);
    v.push(kind);
    v.extend_from_slice(body);
    v
}

/// A consensus message tagged with the epoch of the consensus instance it belongs to.
pub fn consensus_frame(epoch: u64, m: &Message) -> Vec<u8> {
    let mut body = epoch.to_le_bytes().to_vec();
    body.extend_from_slice(&m.encode());
    frame(KIND_CONSENSUS, &body)
}

pub fn tx_frame(tx: &Transaction) -> Vec<u8> {
    frame(KIND_TX, &tx.encode())
}

/// A parsed frame.
#[derive(Debug, Clone)]
pub enum Inbound {
    Consensus(u64, Box<Message>),
    Tx(Vec<u8>),
}

/// `None` for anything malformed (unknown kind, oversize, undecodable message).
pub fn parse_frame(data: &[u8]) -> Option<Inbound> {
    if data.is_empty() || data.len() > MAX_FRAME {
        return None;
    }
    match data[0] {
        KIND_CONSENSUS if data.len() >= 9 => {
            let epoch = u64::from_le_bytes(data[1..9].try_into().ok()?);
            Message::decode(&data[9..]).map(|m| Inbound::Consensus(epoch, Box::new(m)))
        }
        KIND_TX if data.len() - 1 <= btb_chain::tx::MAX_CODE_BYTES + 4096 => {
            Some(Inbound::Tx(data[1..].to_vec()))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Proposer's wall clock, Unix milliseconds (clamped to the parent's on execution).
    pub timestamp: u64,
    pub txs: Vec<Transaction>,
}

impl Batch {
    pub fn encode(&self) -> Vec<u8> {
        let raw: Vec<Vec<u8>> = self.txs.iter().map(Transaction::encode).collect();
        (self.timestamp, raw).encode()
    }

    pub fn decode(bytes: &[u8]) -> Option<Batch> {
        let mut r = Reader::new(bytes, MAX_PAYLOAD);
        let timestamp = u64::decode_from(&mut r).ok()?;
        let raw: Vec<Vec<u8>> = Vec::decode_from(&mut r).ok()?;
        if r.remaining() != 0 {
            return None;
        }
        let txs = raw
            .iter()
            .map(|b| Transaction::decode(b).ok())
            .collect::<Option<Vec<_>>>()?;
        Some(Batch { timestamp, txs })
    }
}

/// The application check every replica runs before voting for a block: the payload is empty or
/// a well-formed batch whose transactions pass the stateless checks (chain id, kind,
/// signature, limits), with no repeated transaction and within the block limits.
/// State-dependent validity (nonce, balance) is left to execution, which skips such
/// transactions deterministically.
pub fn payload_ok(payload: &[u8], cfg: &ChainConfig) -> bool {
    if payload.is_empty() {
        return true;
    }
    if payload.len() > MAX_PAYLOAD {
        return false;
    }
    let Some(b) = Batch::decode(payload) else {
        return false;
    };
    if b.txs.len() > cfg.max_block_txs as usize {
        return false;
    }
    let gas = b
        .txs
        .iter()
        .fold(0u64, |a, t| a.saturating_add(t.gas_limit()));
    if gas > cfg.block_gas_limit {
        return false;
    }
    let mut seen = BTreeSet::new();
    b.txs
        .iter()
        .all(|t| seen.insert(t.id()) && check_static(t, cfg).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_chain::{SigningKey, TxBody, TxKind};

    fn tx(chain_id: u64, nonce: u64, key: &SigningKey) -> Transaction {
        Transaction::sign(
            TxBody {
                chain_id,
                nonce,
                kind: TxKind::Transfer {
                    to: [9; 32],
                    amount: 1,
                },
            },
            key,
        )
    }

    #[test]
    fn payload_checks() {
        let cfg = ChainConfig::new(7);
        let k = SigningKey::from_bytes(&[1; 32]);
        assert!(payload_ok(&[], &cfg));
        let good = Batch {
            timestamp: 5,
            txs: vec![tx(7, 0, &k), tx(7, 1, &k)],
        };
        assert_eq!(Batch::decode(&good.encode()), Some(good.clone()));
        assert!(payload_ok(&good.encode(), &cfg));
        // Wrong chain id, repeated tx, bad signature, garbage and trailing bytes are rejected.
        let wrong = Batch {
            timestamp: 5,
            txs: vec![tx(8, 0, &k)],
        };
        assert!(!payload_ok(&wrong.encode(), &cfg));
        let dup = Batch {
            timestamp: 5,
            txs: vec![tx(7, 0, &k), tx(7, 0, &k)],
        };
        assert!(!payload_ok(&dup.encode(), &cfg));
        let mut forged = tx(7, 0, &k);
        forged.signature[0] ^= 1;
        let bad = Batch {
            timestamp: 5,
            txs: vec![forged],
        };
        assert!(!payload_ok(&bad.encode(), &cfg));
        assert!(!payload_ok(&[1, 2, 3], &cfg));
        let mut trailing = good.encode();
        trailing.push(0);
        assert!(!payload_ok(&trailing, &cfg));
        let mut small = cfg;
        small.max_block_txs = 1;
        assert!(!payload_ok(&good.encode(), &small));
    }

    #[test]
    fn frames_reject_malformed_input() {
        assert!(parse_frame(&[]).is_none());
        assert!(parse_frame(&[9, 1, 2]).is_none());
        assert!(parse_frame(&[KIND_CONSENSUS, 1, 2, 3]).is_none());
        let mut junk = vec![KIND_CONSENSUS];
        junk.extend_from_slice(&0u64.to_le_bytes());
        junk.extend_from_slice(&[0xff; 64]);
        assert!(parse_frame(&junk).is_none());
        assert!(parse_frame(&vec![KIND_TX; MAX_FRAME + 1]).is_none());
    }
}
