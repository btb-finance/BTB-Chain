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

use btb_chain::actions::ActionVote;
use btb_chain::exec::check_static;
use btb_chain::{ChainConfig, Transaction};
use btb_codec::{Decode, Encode, Reader};
use btb_consensus::{Block, Message, QuorumCert};
use btb_oracle::SignedObservation;
use std::collections::BTreeSet;

pub const KIND_CONSENSUS: u8 = 1;
pub const KIND_TX: u8 = 2;
/// Block sync request: `from_height:u64`.
pub const KIND_SYNC_REQ: u8 = 3;
/// Block sync response: see `SyncResponse`.
pub const KIND_SYNC_RESP: u8 = 4;
/// Oracle traffic (beacon partials and values, price observations): see `crate::oracle`.
pub const KIND_ORACLE: u8 = 5;

/// Most consensus blocks in one sync response (committed blocks plus the blocks that prove them).
pub const MAX_SYNC_CHAIN: usize = 24;

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
    /// Ask for committed blocks starting at this height.
    SyncReq(u64),
    SyncResp(Box<SyncResponse>),
    /// An oracle message (decoded by `oracle::OracleMsg::decode`).
    Oracle(Vec<u8>),
}

/// Answer to a block sync request: consecutive consensus blocks of one epoch, oldest first,
/// and optionally a quorum certificate for the last of them. Whether the leading blocks are
/// committed is proven by the blocks that follow them (see `sync::verify`), never by trust.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncResponse {
    pub epoch: u64,
    pub blocks: Vec<Block>,
    pub qc: Option<QuorumCert>,
}

impl SyncResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = self.epoch.to_le_bytes().to_vec();
        let blocks = Message::SyncResponse {
            blocks: self.blocks.clone(),
        }
        .encode();
        o.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
        o.extend_from_slice(&blocks);
        match &self.qc {
            Some(qc) => {
                o.push(1);
                o.extend_from_slice(
                    &Message::Catchup {
                        qc: qc.clone(),
                        tc: None,
                    }
                    .encode(),
                );
            }
            None => o.push(0),
        }
        o
    }

    /// Total decoder: `None` for anything truncated, oversize or with trailing bytes.
    pub fn decode(data: &[u8]) -> Option<SyncResponse> {
        let epoch = u64::from_le_bytes(data.get(..8)?.try_into().ok()?);
        let n = u32::from_le_bytes(data.get(8..12)?.try_into().ok()?) as usize;
        let rest = data.get(12..)?;
        let blocks = rest.get(..n)?;
        let Message::SyncResponse { blocks } = Message::decode(blocks)? else {
            return None;
        };
        if blocks.len() > MAX_SYNC_CHAIN {
            return None;
        }
        let tail = rest.get(n..)?;
        let qc = match *tail.first()? {
            0 if tail.len() == 1 => None,
            1 => match Message::decode(&tail[1..])? {
                Message::Catchup { qc, tc: None } => Some(qc),
                _ => return None,
            },
            _ => return None,
        };
        Some(SyncResponse { epoch, blocks, qc })
    }
}

pub fn sync_request_frame(from_height: u64) -> Vec<u8> {
    frame(KIND_SYNC_REQ, &from_height.to_le_bytes())
}

pub fn sync_response_frame(r: &SyncResponse) -> Vec<u8> {
    frame(KIND_SYNC_RESP, &r.encode())
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
        KIND_SYNC_REQ if data.len() == 9 => Some(Inbound::SyncReq(u64::from_le_bytes(
            data[1..9].try_into().ok()?,
        ))),
        KIND_SYNC_RESP => SyncResponse::decode(&data[1..]).map(|r| Inbound::SyncResp(Box::new(r))),
        KIND_ORACLE if data.len() <= 1 + crate::oracle::MAX_ORACLE_MSG => {
            Some(Inbound::Oracle(data[1..].to_vec()))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Proposer's wall clock, Unix milliseconds (clamped to the parent's on execution).
    pub timestamp: u64,
    pub txs: Vec<Transaction>,
    /// Validators' signed price observations the proposer collected (`btb_chain::oracle`).
    /// Encoded as an optional trailing list, so a batch without any is byte-identical to the
    /// old format.
    pub observations: Vec<SignedObservation>,
    /// Validators' signed answers to pending HTTPS actions (`btb_chain::actions`). A second
    /// optional trailing list (the observation list is then present, possibly empty).
    pub actions: Vec<ActionVote>,
}

impl Batch {
    pub fn encode(&self) -> Vec<u8> {
        let raw: Vec<Vec<u8>> = self.txs.iter().map(Transaction::encode).collect();
        let mut out = (self.timestamp, raw).encode();
        if !self.observations.is_empty() || !self.actions.is_empty() {
            let obs: Vec<Vec<u8>> = self
                .observations
                .iter()
                .map(|o| o.encode().to_vec())
                .collect();
            obs.encode_to(&mut out);
        }
        if !self.actions.is_empty() {
            let votes: Vec<Vec<u8>> = self.actions.iter().map(Encode::encode).collect();
            votes.encode_to(&mut out);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Batch> {
        let mut r = Reader::new(bytes, MAX_PAYLOAD);
        let timestamp = u64::decode_from(&mut r).ok()?;
        let raw: Vec<Vec<u8>> = Vec::decode_from(&mut r).ok()?;
        let (observations, trailing) = if r.remaining() == 0 {
            (Vec::new(), false)
        } else {
            let obs: Vec<Vec<u8>> = Vec::decode_from(&mut r).ok()?;
            if obs.len() > btb_chain::oracle::MAX_OBS_PER_BLOCK {
                return None;
            }
            let obs = obs
                .iter()
                .map(|b| SignedObservation::decode(b))
                .collect::<Option<Vec<_>>>()?;
            (obs, true)
        };
        let actions = if trailing && r.remaining() > 0 {
            let votes: Vec<Vec<u8>> = Vec::decode_from(&mut r).ok()?;
            if votes.is_empty() || votes.len() > btb_chain::actions::MAX_VOTES_PER_BLOCK {
                return None;
            }
            votes
                .iter()
                .map(|b| ActionVote::decode(b).ok())
                .collect::<Option<Vec<_>>>()?
        } else {
            Vec::new()
        };
        // An empty observation list is only valid as the lead-in of a vote list.
        if r.remaining() != 0 || (trailing && observations.is_empty() && actions.is_empty()) {
            return None;
        }
        let txs = raw
            .iter()
            .map(|b| Transaction::decode(b).ok())
            .collect::<Option<Vec<_>>>()?;
        Some(Batch {
            timestamp,
            txs,
            observations,
            actions,
        })
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
                relayer: None,
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
            observations: vec![],
            actions: vec![],
        };
        assert_eq!(Batch::decode(&good.encode()), Some(good.clone()));
        assert!(payload_ok(&good.encode(), &cfg));
        // Wrong chain id, repeated tx, bad signature, garbage and trailing bytes are rejected.
        let wrong = Batch {
            timestamp: 5,
            txs: vec![tx(8, 0, &k)],
            observations: vec![],
            actions: vec![],
        };
        assert!(!payload_ok(&wrong.encode(), &cfg));
        let dup = Batch {
            timestamp: 5,
            txs: vec![tx(7, 0, &k), tx(7, 0, &k)],
            observations: vec![],
            actions: vec![],
        };
        assert!(!payload_ok(&dup.encode(), &cfg));
        let mut forged = tx(7, 0, &k);
        forged.signature[0] ^= 1;
        let bad = Batch {
            timestamp: 5,
            txs: vec![forged],
            observations: vec![],
            actions: vec![],
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
    fn batches_carry_action_votes() {
        use btb_chain::actions::ActionVote;
        let cfg = ChainConfig::new(7);
        let k = SigningKey::from_bytes(&[1; 32]);
        let v = |i: u8| ActionVote::sign(7, &k, [i; 32], 0, vec![i; 5]);
        // votes only, votes next to observations, and next to transactions
        for (txs, votes) in [(vec![], vec![v(1), v(2)]), (vec![tx(7, 0, &k)], vec![v(3)])] {
            let b = Batch {
                timestamp: 5,
                txs,
                observations: vec![],
                actions: votes,
            };
            assert_eq!(Batch::decode(&b.encode()), Some(b.clone()));
            assert!(payload_ok(&b.encode(), &cfg));
        }
        // a batch without votes encodes as before (no trailing section at all)
        let plain = Batch {
            timestamp: 5,
            txs: vec![],
            observations: vec![],
            actions: vec![],
        };
        assert_eq!(plain.encode().len(), 8 + 4);
        // an empty vote list, a vote list without the observation list, trailing junk and a
        // truncated vote are all refused
        let mut with = Batch {
            actions: vec![v(1)],
            ..plain.clone()
        }
        .encode();
        let full = with.clone();
        with.push(7);
        assert!(Batch::decode(&with).is_none());
        for cut in [1, 5, 40] {
            assert!(Batch::decode(&full[..full.len() - cut]).is_none());
        }
        let mut empty_lists = plain.encode();
        empty_lists.extend_from_slice(&0u32.to_le_bytes());
        assert!(
            Batch::decode(&empty_lists).is_none(),
            "empty observation list alone"
        );
        empty_lists.extend_from_slice(&0u32.to_le_bytes());
        assert!(Batch::decode(&empty_lists).is_none(), "empty vote list");
        // a result above the cap does not even decode
        let big = ActionVote::sign(
            7,
            &k,
            [9; 32],
            0,
            vec![0; btb_chain::actions::MAX_RESULT_BYTES + 1],
        );
        let b = Batch {
            actions: vec![big],
            ..plain
        };
        assert!(Batch::decode(&b.encode()).is_none());
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
        // Sync frames: wrong request length, junk and truncated responses.
        assert!(parse_frame(&[KIND_SYNC_REQ, 1, 2]).is_none());
        assert!(matches!(
            parse_frame(&sync_request_frame(5)),
            Some(Inbound::SyncReq(5))
        ));
        assert!(parse_frame(&[KIND_SYNC_RESP]).is_none());
        assert!(parse_frame(&[KIND_SYNC_RESP; 40]).is_none());
    }

    #[test]
    fn sync_response_roundtrip_and_truncation() {
        use btb_consensus::Sig;
        let qc = QuorumCert {
            round: 3,
            block_id: [4; 32],
            votes: vec![(0, Sig([1; 64])), (2, Sig([2; 64]))],
        };
        let block = Block {
            round: 3,
            height: 9,
            parent: [4; 32],
            justify: qc.clone(),
            proposer: 1,
            payload_hash: Block::hash_payload(&[7, 7]),
            payload: vec![7, 7],
            exec: vec![3; 72],
        };
        let r = SyncResponse {
            epoch: 2,
            blocks: vec![block.clone(), block],
            qc: Some(qc),
        };
        let bytes = r.encode();
        assert_eq!(SyncResponse::decode(&bytes), Some(r.clone()));
        // Every strict prefix is rejected, and so is a trailing byte.
        for cut in 0..bytes.len() {
            assert!(SyncResponse::decode(&bytes[..cut]).is_none(), "cut {cut}");
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(SyncResponse::decode(&extra).is_none());
        // Too many blocks.
        let big = SyncResponse {
            epoch: 0,
            blocks: vec![r.blocks[0].clone(); MAX_SYNC_CHAIN + 1],
            qc: None,
        };
        assert!(SyncResponse::decode(&big.encode()).is_none());
    }
}
