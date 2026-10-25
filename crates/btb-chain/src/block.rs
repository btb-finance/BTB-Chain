//! Blocks.
//!
//! A header carries `state_root`, the Merkle root of the state *before* this block (the root
//! the previous block produced), so a proposer can build a block without executing it, and
//! every block commits to the exact state it extends. The root after a block is returned by
//! `Executor::execute_block` and goes into the next header.
//!
//! `timestamp` (Unix milliseconds) is chosen by the proposer. Execution checks only that it is
//! not earlier than the parent's; contracts never see it, and crons run by block height.

use crate::sha256;
use crate::tx::{Hash, Transaction, TxError};
use btb_codec::{Decode, DecodeError, Encode, Reader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub height: u64,
    pub parent_hash: Hash,
    pub timestamp: u64,
    /// State root before this block.
    pub state_root: Hash,
    /// `SHA256("btb.txroot.v1" || id of each tx in order)`.
    pub tx_root: Hash,
}

impl Header {
    /// Block hash: `SHA256("btb.block.v1" || height || parent || timestamp || state_root ||
    /// tx_root)`, integers big-endian.
    pub fn hash(&self) -> Hash {
        sha256(&[
            b"btb.block.v1",
            &self.height.to_be_bytes(),
            &self.parent_hash,
            &self.timestamp.to_be_bytes(),
            &self.state_root,
            &self.tx_root,
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub header: Header,
    pub txs: Vec<Transaction>,
}

pub fn tx_root(txs: &[Transaction]) -> Hash {
    let ids: Vec<Hash> = txs.iter().map(Transaction::id).collect();
    let mut parts: Vec<&[u8]> = vec![b"btb.txroot.v1"];
    parts.extend(ids.iter().map(|i| &i[..]));
    sha256(&parts)
}

impl Block {
    /// Build a block, computing `tx_root`.
    pub fn new(
        height: u64,
        parent_hash: Hash,
        timestamp: u64,
        state_root: Hash,
        txs: Vec<Transaction>,
    ) -> Self {
        Block {
            header: Header {
                height,
                parent_hash,
                timestamp,
                state_root,
                tx_root: tx_root(&txs),
            },
            txs,
        }
    }

    pub fn hash(&self) -> Hash {
        self.header.hash()
    }

    /// `height || parent || timestamp || state_root || tx_root || count:u32 || txs`, each tx as
    /// length-prefixed wire bytes.
    pub fn encode(&self) -> Vec<u8> {
        let h = &self.header;
        let mut o = Vec::new();
        h.height.encode_to(&mut o);
        h.parent_hash.encode_to(&mut o);
        h.timestamp.encode_to(&mut o);
        h.state_root.encode_to(&mut o);
        h.tx_root.encode_to(&mut o);
        let txs: Vec<Vec<u8>> = self.txs.iter().map(Transaction::encode).collect();
        txs.encode_to(&mut o);
        o
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, TxError> {
        let mut r = Reader::new(bytes, 1 << 24);
        let header = Header {
            height: u64::decode_from(&mut r)?,
            parent_hash: <[u8; 32]>::decode_from(&mut r)?,
            timestamp: u64::decode_from(&mut r)?,
            state_root: <[u8; 32]>::decode_from(&mut r)?,
            tx_root: <[u8; 32]>::decode_from(&mut r)?,
        };
        let raw: Vec<Vec<u8>> = Vec::decode_from(&mut r)?;
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes.into());
        }
        let txs = raw
            .iter()
            .map(|b| Transaction::decode(b))
            .collect::<Result<_, _>>()?;
        Ok(Block { header, txs })
    }
}
