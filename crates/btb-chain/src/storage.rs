//! Storage nodes: erasure-coded large files whose pieces live off-chain, with the proofs,
//! payments and repairs on-chain. See `docs/STORAGE.md`.
//!
//! **Only hashes and assignments are on-chain, never file bytes.** A client erasure-codes a
//! file into `k` data + `m` parity pieces of equal length, commits to them with a two-level
//! SHA-256 Merkle tree ([`proof`]) and sends `Store { root, size, k, m, periods }`, paying
//! `n * periods * pay` base units into an escrow. The chain assigns the `n` pieces to registered
//! storage nodes (deterministically, spread over distinct nodes where possible), and every
//! `epoch_blocks` it challenges each holder for one random 1 KiB chunk of its piece. A node
//! that answers (`Prove`) in time earns `pay` from the escrow; a node that does not loses part
//! of its deposit (into the file's escrow, which pays the replacement) and the piece is
//! reassigned to another node, which rebuilds it off-chain from any `k` others and proves it.
//!
//! # State
//!
//! | Tag | Key | Value |
//! | --- | --- | --- |
//! | `0x40` node | `40 node` | endpoint, held bytes, piece count, registration height |
//! | `0x41` node deposit | `41 node` | `u64` LE (counted in the supply invariant) |
//! | `0x42` file | `42 file` | owner, size, k, m, piece length, periods, pay, live pieces |
//! | `0x43` escrow | `43 file` | `u64` LE remaining payment (counted in the supply invariant) |
//! | `0x44` piece | `44 file index` | holder, state, periods left, next due, open challenge |
//! | `0x45` schedule | `45 due:u64be file index` | next challenge round of a piece |
//! | `0x46` deadline | `46 deadline:u64be file index` | an open challenge's (or repair's) deadline |
//! | `0x47` holding | `47 node file index` | pieces a node holds |
//! | `0x48` ended file | `48 file` | size, created, periods of a file whose paid periods all ran (`file_info` reports it `expired`; deleted if the root is stored again) |
//!
//! # Randomness
//!
//! Assignment and challenge chunks use the **threshold beacon** of `docs/ORACLES.md` whenever the
//! block has one: the seed is `SHA256("btb.storage.seed.v1" || beacon seed)` of the block being
//! executed (state key `50 02`). The validators release the beacon of a height only after that
//! block is committed, so neither the transactions that place a file nor the block that closes
//! a challenge round can be steered by a proposer.
//!
//! **Fallback.** A chain without an oracle config, and the first `dkg_window` blocks of a chain
//! that generates its first beacon key on chain, have no beacon. There the seed is the **parent
//! block hash** for `Store` (the head hash while a block runs) and the **block's own hash** at
//! the end-of-block scheduler. That is *grindable by a proposer*, who can try different
//! transactions or timestamps to bias placement or challenged chunks; it cannot forge a proof
//! or steal funds. Once the beacon exists the fallback is never used again.

#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use crate::state::{read_u64, ChainState, StateError};
use crate::tx::{Address, Hash};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};
use btb_runtime::layout;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const TAG_NODE: u8 = 0x40;
pub const TAG_NODE_DEPOSIT: u8 = 0x41;
pub const TAG_FILE: u8 = 0x42;
pub const TAG_ESCROW: u8 = 0x43;
pub const TAG_PIECE: u8 = 0x44;
pub const TAG_SCHED: u8 = 0x45;
pub const TAG_DEADLINE: u8 = 0x46;
pub const TAG_HOLD: u8 = 0x47;
pub const TAG_ENDED: u8 = 0x48;

/// Most pieces (`k + m`) of one file.
pub const MAX_PIECES: usize = 128;
/// Largest file, bytes.
pub const MAX_FILE_BYTES: u64 = 1 << 40;
/// Most payment periods one `Store` can buy.
pub const MAX_PERIODS: u32 = 1 << 20;
pub const MAX_ENDPOINT: usize = 256;
pub const REGISTER_GAS: u64 = 20_000;
pub const STORE_GAS: u64 = 30_000;
/// A proof check hashes about 30 nodes.
pub const PROVE_GAS: u64 = 40_000;
/// Work cap per block of the scheduler (later entries wait for the next block).
const MAX_EVENTS_PER_BLOCK: usize = 256;
/// Periods after a missed challenge during which a node is a last choice for new pieces.
const COOLDOWN_PERIODS: u64 = 4;

/// Merkle commitments over file pieces: the part of the protocol both the chain and the
/// off-chain storage code must compute identically.
pub mod proof {
    use super::*;

    /// Bytes per challenged chunk. Every piece length is a multiple of this.
    pub const CHUNK: usize = 1024;

    fn h(parts: &[&[u8]]) -> Hash {
        let mut s = Sha256::new();
        for p in parts {
            s.update(p);
        }
        s.finalize().into()
    }

    pub fn chunk_leaf(chunk: &[u8]) -> Hash {
        h(&[&[0x00], chunk])
    }
    pub fn node(l: &Hash, r: &Hash) -> Hash {
        h(&[&[0x01], l, r])
    }
    /// Leaf of the file-level tree: a piece's root.
    pub fn piece_leaf(piece_root: &Hash) -> Hash {
        h(&[&[0x02], piece_root])
    }

    /// Piece length for a file of `size` bytes split over `k` data pieces: `ceil(size / k)`
    /// rounded up to a multiple of [`CHUNK`], at least one chunk.
    pub fn piece_len(size: u64, k: usize) -> u64 {
        let per = size.div_ceil(k.max(1) as u64);
        per.div_ceil(CHUNK as u64).max(1) * CHUNK as u64
    }

    /// Root of a Merkle tree over `leaves` (a lone node on an odd level is carried up).
    pub fn root(leaves: &[Hash]) -> Hash {
        let mut level = leaves.to_vec();
        if level.is_empty() {
            return h(&[b"btb.storage.empty"]);
        }
        while level.len() > 1 {
            level = level
                .chunks(2)
                .map(|c| {
                    if c.len() == 2 {
                        node(&c[0], &c[1])
                    } else {
                        c[0]
                    }
                })
                .collect();
        }
        level[0]
    }

    /// Sibling hashes from leaf `index` up to the root (levels without a sibling are skipped).
    pub fn path(leaves: &[Hash], index: usize) -> Vec<Hash> {
        let mut level = leaves.to_vec();
        let (mut i, mut out) = (index, Vec::new());
        while level.len() > 1 {
            if i ^ 1 < level.len() {
                out.push(level[i ^ 1]);
            }
            level = level
                .chunks(2)
                .map(|c| {
                    if c.len() == 2 {
                        node(&c[0], &c[1])
                    } else {
                        c[0]
                    }
                })
                .collect();
            i /= 2;
        }
        out
    }

    /// The root implied by `leaf` at `index` of a tree with `count` leaves and sibling `path`;
    /// `None` if the path has the wrong length or the index is out of range.
    pub fn root_from_path(leaf: Hash, index: usize, count: usize, path: &[Hash]) -> Option<Hash> {
        if index >= count {
            return None;
        }
        let (mut i, mut len, mut acc, mut used) = (index, count, leaf, 0usize);
        while len > 1 {
            if i ^ 1 < len {
                let sib = path.get(used)?;
                used += 1;
                acc = if i % 2 == 0 {
                    node(&acc, sib)
                } else {
                    node(sib, &acc)
                };
            }
            i /= 2;
            len = len.div_ceil(2);
        }
        (used == path.len()).then_some(acc)
    }

    /// Root of one piece (its length must be a multiple of [`CHUNK`]).
    pub fn piece_root(piece: &[u8]) -> Hash {
        let leaves: Vec<Hash> = piece.chunks(CHUNK).map(chunk_leaf).collect();
        root(&leaves)
    }

    /// The file id: the tree root bound to the file's size and encoding.
    pub fn file_id(tree_root: &Hash, size: u64, k: u8, m: u8) -> Hash {
        h(&[
            b"btb.storage.file.v1",
            tree_root,
            &size.to_le_bytes(),
            &[k, m],
        ])
    }

    /// File id from the roots of all `k + m` pieces.
    pub fn file_id_of_pieces(piece_roots: &[Hash], size: u64, k: u8, m: u8) -> Hash {
        let leaves: Vec<Hash> = piece_roots.iter().map(piece_leaf).collect();
        file_id(&root(&leaves), size, k, m)
    }

    /// A challenged chunk with the paths up to the file id.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ChunkProof {
        pub chunk: Vec<u8>,
        /// Siblings from the chunk to its piece root.
        pub chunk_path: Vec<Hash>,
        /// Siblings from the piece root to the file tree root.
        pub piece_path: Vec<Hash>,
    }

    impl Encode for ChunkProof {
        fn encode_to(&self, out: &mut Vec<u8>) {
            self.chunk.encode_to(out);
            self.chunk_path.encode_to(out);
            self.piece_path.encode_to(out);
        }
    }
    impl Decode for ChunkProof {
        fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
            Ok(ChunkProof {
                chunk: Vec::decode_from(r)?,
                chunk_path: Vec::decode_from(r)?,
                piece_path: Vec::decode_from(r)?,
            })
        }
    }

    /// Build the proof for chunk `chunk` of piece `piece`, given the file's piece roots.
    pub fn prove_chunk(
        piece: &[u8],
        chunk: usize,
        piece_roots: &[Hash],
        index: usize,
    ) -> Option<ChunkProof> {
        let leaves: Vec<Hash> = piece.chunks(CHUNK).map(chunk_leaf).collect();
        let c = piece.chunks(CHUNK).nth(chunk)?;
        let file_leaves: Vec<Hash> = piece_roots.iter().map(piece_leaf).collect();
        Some(ChunkProof {
            chunk: c.to_vec(),
            chunk_path: path(&leaves, chunk),
            piece_path: path(&file_leaves, index),
        })
    }

    /// Whether `p` shows that chunk `chunk` of piece `index` (of `n = k + m` pieces, each
    /// `piece_len` bytes) belongs to the file `id`.
    pub fn verify_chunk(
        id: &Hash,
        size: u64,
        k: u8,
        m: u8,
        piece_len: u64,
        index: usize,
        chunk: usize,
        p: &ChunkProof,
    ) -> bool {
        let n = k as usize + m as usize;
        let chunks = (piece_len / CHUNK as u64) as usize;
        if p.chunk.len() != CHUNK || index >= n || chunk >= chunks {
            return false;
        }
        let Some(piece_root) = root_from_path(chunk_leaf(&p.chunk), chunk, chunks, &p.chunk_path)
        else {
            return false;
        };
        let Some(tree) = root_from_path(piece_leaf(&piece_root), index, n, &p.piece_path) else {
            return false;
        };
        &file_id(&tree, size, k, m) == id
    }

    /// Which chunk of a piece is challenged: a function of the seed only.
    pub fn challenge_chunk(seed: &Hash, file: &Hash, index: u8, round: u32, chunks: u64) -> u64 {
        let d = h(&[
            b"btb.storage.chunk.v1",
            seed,
            file,
            &[index],
            &round.to_le_bytes(),
        ]);
        u64::from_be_bytes(d[..8].try_into().expect("8 bytes")) % chunks.max(1)
    }
}

pub use proof::ChunkProof;

/// Storage parameters, fixed at genesis (part of [`crate::ChainConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageConfig {
    /// Blocks between two challenges of the same piece (a payment period).
    pub epoch_blocks: u64,
    /// Blocks a holder has to answer a challenge. Less than `epoch_blocks`.
    pub challenge_blocks: u64,
    /// Blocks a replacement node has to rebuild a piece and prove it.
    pub repair_blocks: u64,
    /// Payment per KiB of piece per period, base units.
    pub price_per_kib_period: u64,
    /// Deposit a node must hold per KiB of pieces it stores.
    pub deposit_per_kib: u64,
    /// Deposit needed to register at all.
    pub min_deposit: u64,
    /// Share of a piece's collateral slashed per missed challenge, basis points.
    pub slash_bps: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            epoch_blocks: 600,
            challenge_blocks: 60,
            repair_blocks: 300,
            price_per_kib_period: 100_000,
            deposit_per_kib: 1_000_000,
            min_deposit: 10 * crate::UNITS_PER_BTB,
            slash_bps: 5_000,
        }
    }
}

impl StorageConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.epoch_blocks < 2 {
            return Err("storage epoch_blocks must be at least 2".into());
        }
        if self.challenge_blocks == 0 || self.challenge_blocks >= self.epoch_blocks {
            return Err("storage challenge_blocks must be 1..epoch_blocks".into());
        }
        if self.repair_blocks == 0 {
            return Err("storage repair_blocks must be at least 1".into());
        }
        if self.slash_bps > 10_000 {
            return Err("storage slash_bps above 10000".into());
        }
        Ok(())
    }

    pub(crate) fn fields(&self) -> [u64; 7] {
        [
            self.epoch_blocks,
            self.challenge_blocks,
            self.repair_blocks,
            self.price_per_kib_period,
            self.deposit_per_kib,
            self.min_deposit,
            self.slash_bps,
        ]
    }

    pub(crate) fn from_fields(f: [u64; 7]) -> Self {
        StorageConfig {
            epoch_blocks: f[0],
            challenge_blocks: f[1],
            repair_blocks: f[2],
            price_per_kib_period: f[3],
            deposit_per_kib: f[4],
            min_deposit: f[5],
            slash_bps: f[6],
        }
    }

    /// Payment per piece per period for pieces of `piece_len` bytes.
    pub fn pay_per_period(&self, piece_len: u64) -> Option<u64> {
        self.price_per_kib_period.checked_mul(piece_len / 1024)
    }

    /// Deposit a node needs to hold `held` bytes of pieces.
    pub fn required_deposit(&self, held: u64) -> Option<u64> {
        self.deposit_per_kib
            .checked_mul(held.div_ceil(1024))?
            .checked_add(self.min_deposit)
    }

    /// Escrow a `Store` of this shape must pay, or `None` on overflow.
    pub fn escrow_for(&self, size: u64, k: u8, m: u8, periods: u32) -> Option<u64> {
        let pl = proof::piece_len(size, k as usize);
        self.pay_per_period(pl)?
            .checked_mul(u64::from(k) + u64::from(m))?
            .checked_mul(u64::from(periods))
    }
}

/// A storage transaction (transaction tag 12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageOp {
    /// Become (or update) a storage node: lock `deposit` more and publish `endpoint`.
    Register { endpoint: Vec<u8>, deposit: u64 },
    /// Lock more deposit.
    AddDeposit { amount: u64 },
    /// Take deposit back, as long as the rest covers what the node still stores. Taking all of
    /// it while holding nothing removes the node.
    Withdraw { amount: u64 },
    /// Store a file: pay the escrow, get pieces assigned.
    Store {
        root: Hash,
        size: u64,
        k: u8,
        m: u8,
        periods: u32,
    },
    /// Answer a challenge (or finish a repair) for piece `index` of `file`.
    Prove {
        file: Hash,
        index: u8,
        proof: ChunkProof,
    },
}

impl StorageOp {
    pub fn gas(&self) -> u64 {
        match self {
            StorageOp::Store { .. } => STORE_GAS,
            StorageOp::Prove { .. } => PROVE_GAS,
            _ => REGISTER_GAS,
        }
    }

    /// Most this op can take from the sender's balance (besides the fee).
    pub fn debit(&self, cfg: &StorageConfig) -> u64 {
        match self {
            StorageOp::Register { deposit, .. } => *deposit,
            StorageOp::AddDeposit { amount } => *amount,
            StorageOp::Store {
                size,
                k,
                m,
                periods,
                ..
            } => cfg.escrow_for(*size, *k, *m, *periods).unwrap_or(u64::MAX),
            _ => 0,
        }
    }

    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            StorageOp::Register { endpoint, deposit } => {
                0u8.encode_to(out);
                endpoint.encode_to(out);
                deposit.encode_to(out);
            }
            StorageOp::AddDeposit { amount } => {
                1u8.encode_to(out);
                amount.encode_to(out);
            }
            StorageOp::Withdraw { amount } => {
                2u8.encode_to(out);
                amount.encode_to(out);
            }
            StorageOp::Store {
                root,
                size,
                k,
                m,
                periods,
            } => {
                3u8.encode_to(out);
                root.encode_to(out);
                size.encode_to(out);
                k.encode_to(out);
                m.encode_to(out);
                periods.encode_to(out);
            }
            StorageOp::Prove { file, index, proof } => {
                4u8.encode_to(out);
                file.encode_to(out);
                index.encode_to(out);
                proof.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => StorageOp::Register {
                endpoint: Vec::decode_from(r)?,
                deposit: u64::decode_from(r)?,
            },
            1 => StorageOp::AddDeposit {
                amount: u64::decode_from(r)?,
            },
            2 => StorageOp::Withdraw {
                amount: u64::decode_from(r)?,
            },
            3 => StorageOp::Store {
                root: <[u8; 32]>::decode_from(r)?,
                size: u64::decode_from(r)?,
                k: u8::decode_from(r)?,
                m: u8::decode_from(r)?,
                periods: u32::decode_from(r)?,
            },
            4 => StorageOp::Prove {
                file: <[u8; 32]>::decode_from(r)?,
                index: u8::decode_from(r)?,
                proof: ChunkProof::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    /// Checks that need no state: sizes and shapes.
    pub fn check_static(&self) -> Result<(), StorageError> {
        match self {
            StorageOp::Register { endpoint, .. } => {
                if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT {
                    return Err(StorageError::BadEndpoint);
                }
            }
            StorageOp::Store {
                size,
                k,
                m,
                periods,
                ..
            } => {
                let n = usize::from(*k) + usize::from(*m);
                if *k == 0 || *m == 0 || n > MAX_PIECES {
                    return Err(StorageError::BadShape);
                }
                if *size == 0 || *size > MAX_FILE_BYTES || *periods == 0 || *periods > MAX_PERIODS {
                    return Err(StorageError::BadShape);
                }
            }
            StorageOp::Prove { proof, .. } => {
                let bad = proof.chunk.len() != proof::CHUNK
                    || proof.chunk_path.len() > 40
                    || proof.piece_path.len() > 8;
                if bad {
                    return Err(StorageError::BadProof);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Why a storage transaction was refused (the fee is still charged, nothing changed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    BadEndpoint,
    /// `k`, `m`, size or periods out of range.
    BadShape,
    /// Deposit below `min_deposit`.
    DepositTooLow,
    NotRegistered,
    /// A withdrawal would leave less than the node's stored data requires (or more than it has).
    DepositLocked,
    AlreadyStored,
    /// The escrow does not fit in a `u64`.
    TooExpensive,
    /// Fewer eligible nodes than pieces cannot even be shared out.
    NotEnoughNodes,
    NoSuchFile,
    NoSuchPiece,
    /// The sender does not hold that piece.
    NotHolder,
    /// The piece has no open challenge (or its deadline passed).
    NoChallenge,
    BadProof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fail {
    Tx(StorageError),
    State(StateError),
}
impl From<StorageError> for Fail {
    fn from(e: StorageError) -> Self {
        Fail::Tx(e)
    }
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

// ------------------------------------------------------------------ records

/// A registered storage node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    pub address: Address,
    pub endpoint: Vec<u8>,
    pub deposit: u64,
    pub held_bytes: u64,
    pub pieces: u32,
    pub registered: u64,
}

struct NodeRec {
    endpoint: Vec<u8>,
    held: u64,
    pieces: u32,
    registered: u64,
    /// Height of the node's last missed challenge (0 = never).
    last_miss: u64,
}

impl NodeRec {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.endpoint.encode_to(&mut o);
        self.held.encode_to(&mut o);
        self.pieces.encode_to(&mut o);
        self.registered.encode_to(&mut o);
        self.last_miss.encode_to(&mut o);
        o
    }
    fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b, 1 << 20);
        Ok(NodeRec {
            endpoint: Vec::decode_from(&mut r)?,
            held: u64::decode_from(&mut r)?,
            pieces: u32::decode_from(&mut r)?,
            registered: u64::decode_from(&mut r)?,
            last_miss: u64::decode_from(&mut r)?,
        })
    }
}

/// What the chain knows about a stored file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub id: Hash,
    pub owner: Address,
    pub size: u64,
    pub k: u8,
    pub m: u8,
    pub piece_len: u64,
    pub periods: u32,
    /// Payment per piece per period.
    pub pay: u64,
    pub created: u64,
    /// Pieces not yet retired.
    pub live: u32,
    /// Escrow left.
    pub escrow: u64,
}

impl FileInfo {
    pub fn pieces(&self) -> usize {
        usize::from(self.k) + usize::from(self.m)
    }
    pub fn chunks(&self) -> u64 {
        self.piece_len / proof::CHUNK as u64
    }

    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.owner.encode_to(&mut o);
        self.size.encode_to(&mut o);
        self.k.encode_to(&mut o);
        self.m.encode_to(&mut o);
        self.piece_len.encode_to(&mut o);
        self.periods.encode_to(&mut o);
        self.pay.encode_to(&mut o);
        self.created.encode_to(&mut o);
        self.live.encode_to(&mut o);
        o
    }
    fn decode(id: Hash, escrow: u64, b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b, 1 << 20);
        Ok(FileInfo {
            id,
            owner: <[u8; 32]>::decode_from(&mut r)?,
            size: u64::decode_from(&mut r)?,
            k: u8::decode_from(&mut r)?,
            m: u8::decode_from(&mut r)?,
            piece_len: u64::decode_from(&mut r)?,
            periods: u32::decode_from(&mut r)?,
            pay: u64::decode_from(&mut r)?,
            created: u64::decode_from(&mut r)?,
            live: u32::decode_from(&mut r)?,
            escrow,
        })
    }
}

/// Where a piece stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceState {
    /// Held by `node`, which answers the periodic challenges.
    Holding,
    /// Reassigned after a miss: `node` must rebuild the piece and prove it.
    Repairing,
    /// Nobody could take it yet (not enough eligible nodes); retried until someone can.
    Vacant,
}

/// An open challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Challenge {
    pub chunk: u64,
    pub deadline: u64,
    /// Paid from the escrow when answered.
    pub pay: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceInfo {
    pub file: Hash,
    pub index: u8,
    pub node: Address,
    pub state: PieceState,
    pub periods_left: u32,
    pub next_due: u64,
    pub challenge: Option<Challenge>,
    pub proven: u32,
    pub missed: u32,
}

impl PieceInfo {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.node.encode_to(&mut o);
        (match self.state {
            PieceState::Holding => 0u8,
            PieceState::Repairing => 1,
            PieceState::Vacant => 2,
        })
        .encode_to(&mut o);
        self.periods_left.encode_to(&mut o);
        self.next_due.encode_to(&mut o);
        match &self.challenge {
            None => 0u8.encode_to(&mut o),
            Some(c) => {
                1u8.encode_to(&mut o);
                c.chunk.encode_to(&mut o);
                c.deadline.encode_to(&mut o);
                c.pay.encode_to(&mut o);
            }
        }
        self.proven.encode_to(&mut o);
        self.missed.encode_to(&mut o);
        o
    }
    fn decode(file: Hash, index: u8, b: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(b, 1 << 20);
        let node = <[u8; 32]>::decode_from(&mut r)?;
        let state = match u8::decode_from(&mut r)? {
            0 => PieceState::Holding,
            1 => PieceState::Repairing,
            _ => PieceState::Vacant,
        };
        let periods_left = u32::decode_from(&mut r)?;
        let next_due = u64::decode_from(&mut r)?;
        let challenge = if u8::decode_from(&mut r)? == 1 {
            Some(Challenge {
                chunk: u64::decode_from(&mut r)?,
                deadline: u64::decode_from(&mut r)?,
                pay: u64::decode_from(&mut r)?,
            })
        } else {
            None
        };
        Ok(PieceInfo {
            file,
            index,
            node,
            state,
            periods_left,
            next_due,
            challenge,
            proven: u32::decode_from(&mut r)?,
            missed: u32::decode_from(&mut r)?,
        })
    }
}

// ------------------------------------------------------------------ keys and helpers

fn key1(tag: u8, a: &[u8]) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(a);
    k
}
fn piece_key(file: &Hash, index: u8) -> Vec<u8> {
    let mut k = key1(TAG_PIECE, file);
    k.push(index);
    k
}
fn timed_key(tag: u8, at: u64, file: &Hash, index: u8) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(&at.to_be_bytes());
    k.extend_from_slice(file);
    k.push(index);
    k
}
fn hold_key(node: &Address, file: &Hash, index: u8) -> Vec<u8> {
    let mut k = key1(TAG_HOLD, node);
    k.extend_from_slice(file);
    k.push(index);
    k
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
fn debit(kv: &mut impl KvStore, a: &Address, amount: u64) -> Result<(), StateError> {
    let b = balance(kv, a)?
        .checked_sub(amount)
        .ok_or_else(|| StateError("storage debit exceeds balance".into()))?;
    Ok(set_balance(kv, a, b)?)
}
fn put_u64(kv: &mut impl KvStore, key: &[u8], v: u64) -> Result<(), KvError> {
    if v == 0 {
        kv.delete(key)
    } else {
        kv.put(key, &v.to_le_bytes())
    }
}
fn deposit_of(kv: &impl KvStore, node: &Address) -> Result<u64, KvError> {
    read_u64(kv, &key1(TAG_NODE_DEPOSIT, node))
}
fn escrow_of(kv: &impl KvStore, file: &Hash) -> Result<u64, KvError> {
    read_u64(kv, &key1(TAG_ESCROW, file))
}
fn node_rec(kv: &impl KvStore, node: &Address) -> Result<Option<NodeRec>, StateError> {
    match kv.get(&key1(TAG_NODE, node))? {
        None => Ok(None),
        Some(b) => Ok(Some(NodeRec::decode(&b)?)),
    }
}
fn put_node(kv: &mut impl KvStore, node: &Address, r: &NodeRec) -> Result<(), KvError> {
    kv.put(&key1(TAG_NODE, node), &r.encode())
}
fn file_rec(kv: &impl KvStore, id: &Hash) -> Result<Option<FileInfo>, StateError> {
    match kv.get(&key1(TAG_FILE, id))? {
        None => Ok(None),
        Some(b) => Ok(Some(FileInfo::decode(*id, escrow_of(kv, id)?, &b)?)),
    }
}
fn put_file(kv: &mut impl KvStore, f: &FileInfo) -> Result<(), KvError> {
    kv.put(&key1(TAG_FILE, &f.id), &f.encode())
}
fn piece_rec(kv: &impl KvStore, file: &Hash, index: u8) -> Result<Option<PieceInfo>, StateError> {
    match kv.get(&piece_key(file, index))? {
        None => Ok(None),
        Some(b) => Ok(Some(PieceInfo::decode(*file, index, &b)?)),
    }
}
fn put_piece(kv: &mut impl KvStore, p: &PieceInfo) -> Result<(), KvError> {
    kv.put(&piece_key(&p.file, p.index), &p.encode())
}

/// Page through a key range.
fn scan_all(
    kv: &impl KvStore,
    start: Vec<u8>,
    end: Option<Vec<u8>>,
    limit: usize,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, KvError> {
    let mut out = Vec::new();
    let mut from = start;
    loop {
        let page = kv.scan(&from, end.as_deref(), false, 1024.min(limit - out.len()))?;
        let n = page.len();
        if let Some((k, _)) = page.last() {
            from = k.clone();
            from.push(0);
        }
        out.extend(page);
        if n < 1024 || out.len() >= limit {
            return Ok(out);
        }
    }
}

fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut e = prefix.to_vec();
    while let Some(last) = e.last_mut() {
        if *last == 0xff {
            e.pop();
        } else {
            *last += 1;
            break;
        }
    }
    e
}

fn pieces_of_file(kv: &impl KvStore, file: &Hash) -> Result<Vec<PieceInfo>, StateError> {
    let p = key1(TAG_PIECE, file);
    scan_all(kv, p.clone(), Some(prefix_end(&p)), MAX_PIECES)?
        .into_iter()
        .map(|(k, v)| Ok(PieceInfo::decode(*file, k[33], &v)?))
        .collect()
}

fn digest(parts: &[&[u8]]) -> Hash {
    let mut s = Sha256::new();
    for p in parts {
        s.update(p);
    }
    s.finalize().into()
}

/// The seed derived from the beacon of block `height`, if the block has one.
fn beacon_seed(kv: &impl KvStore, height: u64) -> Result<Option<Hash>, StateError> {
    let Some(rec) = kv.get(&layout::oracle_seed_key())? else {
        return Ok(None);
    };
    let at = rec.get(..8).and_then(|b| <[u8; 8]>::try_from(b).ok());
    Ok(match (at, rec.get(8..40)) {
        (Some(at), Some(seed)) if u64::from_le_bytes(at) == height && rec.len() == 40 => {
            Some(digest(&[b"btb.storage.seed.v1", seed]))
        }
        _ => None,
    })
}

/// The seed for block `height`: its beacon if it has one, else `fallback` (a block hash).
fn seed_for(kv: &impl KvStore, height: u64, fallback: Hash) -> Result<Hash, StateError> {
    Ok(beacon_seed(kv, height)?.unwrap_or(fallback))
}

/// The chain's fallback seed for assignment while a block runs: the head (parent) hash.
fn head_hash(kv: &impl KvStore) -> Result<Hash, StateError> {
    let b = kv
        .get(&crate::state::meta_head_key())?
        .ok_or_else(|| StateError("no head".into()))?;
    Ok(crate::state::Head::decode(&b)?.hash)
}

// ------------------------------------------------------------------ assignment

/// Pick the node for piece `index`: among eligible nodes (registered, deposit covering the
/// piece on top of what they hold, not in `exclude`), the one holding the fewest other pieces
/// of this file (`mine`, which includes pieces planned in the same call), then the lowest
/// `SHA256(seed || file || index || node)`. Deterministic for a given seed; spreads a file over
/// distinct nodes whenever enough exist. Nodes that missed a challenge within the last
/// `COOLDOWN_PERIODS` periods rank after every other eligible node, so a dead node is not
/// handed the pieces of another dead node over and over; they stay eligible as a last resort.
fn pick_node(
    kv: &impl KvStore,
    cfg: &StorageConfig,
    height: u64,
    seed: &Hash,
    file: &Hash,
    index: u8,
    piece_len: u64,
    exclude: &BTreeSet<Address>,
    mine: &BTreeMap<Address, u32>,
    planned: &BTreeMap<Address, u64>,
) -> Result<Option<Address>, StateError> {
    let mut best: Option<((bool, u32, Hash), Address)> = None;
    let start = vec![TAG_NODE];
    let end = vec![TAG_NODE + 1];
    for (k, v) in scan_all(kv, start, Some(end), usize::MAX)? {
        let node: Address = k[1..]
            .try_into()
            .map_err(|_| StateError("bad node key".into()))?;
        if exclude.contains(&node) {
            continue;
        }
        let rec = NodeRec::decode(&v)?;
        let held = rec
            .held
            .saturating_add(planned.get(&node).copied().unwrap_or(0))
            .saturating_add(piece_len);
        let Some(need) = cfg.required_deposit(held) else {
            continue;
        };
        if deposit_of(kv, &node)? < need {
            continue;
        }
        let cooling = rec.last_miss != 0
            && height
                < rec
                    .last_miss
                    .saturating_add(COOLDOWN_PERIODS * cfg.epoch_blocks);
        let score = (
            cooling,
            mine.get(&node).copied().unwrap_or(0),
            digest(&[b"btb.storage.assign.v1", seed, file, &[index], &node]),
        );
        if best.as_ref().is_none_or(|(s, _)| score < *s) {
            best = Some((score, node));
        }
    }
    Ok(best.map(|(_, n)| n))
}

// ------------------------------------------------------------------ transactions

/// Apply a storage transaction from `sender` at `height`. On success the output is empty
/// except for `Store`, which returns the assigned node of each piece (32 bytes each, in order).
pub(crate) fn apply(
    kv: &mut impl KvStore,
    cfg: &StorageConfig,
    sender: &Address,
    height: u64,
    op: &StorageOp,
) -> Result<Vec<u8>, Fail> {
    op.check_static()?;
    match op {
        StorageOp::Register { endpoint, deposit } => {
            let have = deposit_of(kv, sender)?;
            let total = have
                .checked_add(*deposit)
                .ok_or(StorageError::TooExpensive)?;
            let rec = node_rec(kv, sender)?;
            let held = rec.as_ref().map_or(0, |r| r.held);
            if total < cfg.required_deposit(held).unwrap_or(u64::MAX) {
                return Err(StorageError::DepositTooLow.into());
            }
            debit(kv, sender, *deposit)?;
            put_u64(kv, &key1(TAG_NODE_DEPOSIT, sender), total)?;
            let r = NodeRec {
                endpoint: endpoint.clone(),
                held,
                pieces: rec.as_ref().map_or(0, |r| r.pieces),
                registered: rec.as_ref().map_or(height, |r| r.registered),
                last_miss: rec.as_ref().map_or(0, |r| r.last_miss),
            };
            put_node(kv, sender, &r)?;
            Ok(Vec::new())
        }
        StorageOp::AddDeposit { amount } => {
            node_rec(kv, sender)?.ok_or(StorageError::NotRegistered)?;
            let total = deposit_of(kv, sender)?
                .checked_add(*amount)
                .ok_or(StorageError::TooExpensive)?;
            debit(kv, sender, *amount)?;
            put_u64(kv, &key1(TAG_NODE_DEPOSIT, sender), total)?;
            Ok(Vec::new())
        }
        StorageOp::Withdraw { amount } => {
            let rec = node_rec(kv, sender)?.ok_or(StorageError::NotRegistered)?;
            let have = deposit_of(kv, sender)?;
            let left = have
                .checked_sub(*amount)
                .ok_or(StorageError::DepositLocked)?;
            if rec.pieces == 0 && left == 0 {
                kv.delete(&key1(TAG_NODE, sender))?;
            } else if left < cfg.required_deposit(rec.held).unwrap_or(u64::MAX) {
                return Err(StorageError::DepositLocked.into());
            }
            put_u64(kv, &key1(TAG_NODE_DEPOSIT, sender), left)?;
            credit(kv, sender, *amount)?;
            Ok(Vec::new())
        }
        StorageOp::Store {
            root,
            size,
            k,
            m,
            periods,
        } => store(kv, cfg, sender, height, root, *size, *k, *m, *periods),
        StorageOp::Prove { file, index, proof } => {
            prove(kv, cfg, sender, height, file, *index, proof)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn store(
    kv: &mut impl KvStore,
    cfg: &StorageConfig,
    sender: &Address,
    height: u64,
    root: &Hash,
    size: u64,
    k: u8,
    m: u8,
    periods: u32,
) -> Result<Vec<u8>, Fail> {
    if kv.get(&key1(TAG_FILE, root))?.is_some() {
        return Err(StorageError::AlreadyStored.into());
    }
    let n = usize::from(k) + usize::from(m);
    let piece_len = proof::piece_len(size, usize::from(k));
    let escrow = cfg
        .escrow_for(size, k, m, periods)
        .ok_or(StorageError::TooExpensive)?;
    let pay = cfg
        .pay_per_period(piece_len)
        .ok_or(StorageError::TooExpensive)?;
    // Plan the whole assignment before writing anything.
    let seed = seed_for(kv, height, head_hash(kv)?)?;
    let mut mine: BTreeMap<Address, u32> = BTreeMap::new();
    let mut planned: BTreeMap<Address, u64> = BTreeMap::new();
    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        let node = pick_node(
            kv,
            cfg,
            height,
            &seed,
            root,
            i as u8,
            piece_len,
            &BTreeSet::new(),
            &mine,
            &planned,
        )?
        .ok_or(StorageError::NotEnoughNodes)?;
        *mine.entry(node).or_default() += 1;
        *planned.entry(node).or_default() += piece_len;
        nodes.push(node);
    }
    debit(kv, sender, escrow)?;
    kv.delete(&key1(TAG_ENDED, root))?;
    put_u64(kv, &key1(TAG_ESCROW, root), escrow)?;
    put_file(
        kv,
        &FileInfo {
            id: *root,
            owner: *sender,
            size,
            k,
            m,
            piece_len,
            periods,
            pay,
            created: height,
            live: n as u32,
            escrow,
        },
    )?;
    let half = (cfg.epoch_blocks / 2).max(1);
    for (i, node) in nodes.iter().enumerate() {
        let i = i as u8;
        let spread = u64::from_be_bytes(
            digest(&[b"btb.storage.spread.v1", root, &[i]])[..8]
                .try_into()
                .expect("8 bytes"),
        ) % half;
        let due = height + half + spread;
        add_hold(kv, node, root, i, piece_len)?;
        put_piece(
            kv,
            &PieceInfo {
                file: *root,
                index: i,
                node: *node,
                state: PieceState::Holding,
                periods_left: periods,
                next_due: due,
                challenge: None,
                proven: 0,
                missed: 0,
            },
        )?;
        kv.put(&timed_key(TAG_SCHED, due, root, i), &[0])?;
    }
    Ok(nodes.concat())
}

fn add_hold(
    kv: &mut impl KvStore,
    node: &Address,
    file: &Hash,
    index: u8,
    piece_len: u64,
) -> Result<(), StateError> {
    let mut r = node_rec(kv, node)?.ok_or_else(|| StateError("holder not registered".into()))?;
    r.held += piece_len;
    r.pieces += 1;
    put_node(kv, node, &r)?;
    kv.put(&hold_key(node, file, index), &[0])?;
    Ok(())
}

fn drop_hold(
    kv: &mut impl KvStore,
    node: &Address,
    file: &Hash,
    index: u8,
    piece_len: u64,
) -> Result<(), StateError> {
    if let Some(mut r) = node_rec(kv, node)? {
        r.held = r.held.saturating_sub(piece_len);
        r.pieces = r.pieces.saturating_sub(1);
        put_node(kv, node, &r)?;
    }
    kv.delete(&hold_key(node, file, index))?;
    Ok(())
}

fn prove(
    kv: &mut impl KvStore,
    _cfg: &StorageConfig,
    sender: &Address,
    height: u64,
    file: &Hash,
    index: u8,
    p: &ChunkProof,
) -> Result<Vec<u8>, Fail> {
    let f = file_rec(kv, file)?.ok_or(StorageError::NoSuchFile)?;
    let mut piece = piece_rec(kv, file, index)?.ok_or(StorageError::NoSuchPiece)?;
    if piece.state == PieceState::Vacant || &piece.node != sender {
        return Err(StorageError::NotHolder.into());
    }
    let ch = piece.challenge.ok_or(StorageError::NoChallenge)?;
    if height > ch.deadline {
        return Err(StorageError::NoChallenge.into());
    }
    if !proof::verify_chunk(
        file,
        f.size,
        f.k,
        f.m,
        f.piece_len,
        usize::from(index),
        ch.chunk as usize,
        p,
    ) {
        return Err(StorageError::BadProof.into());
    }
    let pay = ch.pay.min(f.escrow);
    kv.delete(&timed_key(TAG_DEADLINE, ch.deadline, file, index))?;
    put_u64(kv, &key1(TAG_ESCROW, file), f.escrow - pay)?;
    credit(kv, sender, pay)?;
    piece.challenge = None;
    piece.state = PieceState::Holding;
    piece.proven += 1;
    put_piece(kv, &piece)?;
    Ok(pay.to_le_bytes().to_vec())
}

// ------------------------------------------------------------------ scheduler

/// Run the challenge scheduler for the block at `height` (called by the executor after the
/// transactions and crons). `block_hash` seeds the challenge chunks and reassignments.
pub(crate) fn end_block(
    kv: &mut impl KvStore,
    cfg: &StorageConfig,
    height: u64,
    block_hash: Hash,
) -> Result<(), StateError> {
    let block_hash = seed_for(kv, height, block_hash)?;
    if kv
        .scan(&[TAG_FILE], Some(&[TAG_FILE + 1]), false, 1)?
        .is_empty()
    {
        return Ok(()); // no files: nothing is scheduled
    }
    let end = |tag: u8| {
        let mut e = vec![tag];
        e.extend_from_slice(&height.saturating_add(1).to_be_bytes());
        e
    };
    let due = kv.scan(
        &[TAG_DEADLINE],
        Some(&end(TAG_DEADLINE)),
        false,
        MAX_EVENTS_PER_BLOCK,
    )?;
    for (k, _) in due {
        let at = u64::from_be_bytes(k[1..9].try_into().expect("8"));
        let file: Hash = k[9..41].try_into().expect("32");
        deadline(kv, cfg, height, &block_hash, at, &file, k[41])?;
    }
    let due = kv.scan(
        &[TAG_SCHED],
        Some(&end(TAG_SCHED)),
        false,
        MAX_EVENTS_PER_BLOCK,
    )?;
    for (k, _) in due {
        let at = u64::from_be_bytes(k[1..9].try_into().expect("8"));
        let file: Hash = k[9..41].try_into().expect("32");
        round(kv, cfg, height, &block_hash, at, &file, k[41])?;
    }
    Ok(())
}

/// A piece's period starts: retire it if it ran its course, else open a challenge.
fn round(
    kv: &mut impl KvStore,
    cfg: &StorageConfig,
    height: u64,
    seed: &Hash,
    at: u64,
    file: &Hash,
    index: u8,
) -> Result<(), StateError> {
    kv.delete(&timed_key(TAG_SCHED, at, file, index))?;
    let (Some(f), Some(mut p)) = (file_rec(kv, file)?, piece_rec(kv, file, index)?) else {
        return Ok(());
    };
    if p.periods_left == 0 {
        return retire(kv, f, p);
    }
    p.periods_left -= 1;
    p.next_due = at + cfg.epoch_blocks;
    kv.put(&timed_key(TAG_SCHED, p.next_due, file, index), &[0])?;
    if p.state == PieceState::Holding && p.challenge.is_none() {
        let round_no = f.periods - p.periods_left;
        let chunk = proof::challenge_chunk(seed, file, index, round_no, f.chunks());
        let deadline = height + cfg.challenge_blocks;
        p.challenge = Some(Challenge {
            chunk,
            deadline,
            pay: f.pay,
        });
        kv.put(&timed_key(TAG_DEADLINE, deadline, file, index), &[0])?;
    }
    // A piece being repaired (or vacant) skips this period's payment: it stays in the escrow.
    put_piece(kv, &p)?;
    Ok(())
}

/// A piece ran its course: free the holder's capacity; the last piece ends the file and refunds
/// whatever is left of the escrow to the owner.
fn retire(kv: &mut impl KvStore, mut f: FileInfo, p: PieceInfo) -> Result<(), StateError> {
    if let Some(c) = p.challenge {
        kv.delete(&timed_key(TAG_DEADLINE, c.deadline, &p.file, p.index))?;
    }
    if p.state != PieceState::Vacant {
        drop_hold(kv, &p.node, &p.file, p.index, f.piece_len)?;
    }
    kv.delete(&piece_key(&p.file, p.index))?;
    f.live = f.live.saturating_sub(1);
    if f.live == 0 {
        let left = escrow_of(kv, &f.id)?;
        put_u64(kv, &key1(TAG_ESCROW, &f.id), 0)?;
        credit(kv, &f.owner, left)?;
        kv.delete(&key1(TAG_FILE, &f.id))?;
        // Remember that it ended, so contracts can tell "expired" from "never stored".
        let mut t = Vec::new();
        f.size.encode_to(&mut t);
        f.created.encode_to(&mut t);
        f.periods.encode_to(&mut t);
        kv.put(&key1(TAG_ENDED, &f.id), &t)?;
    } else {
        put_file(kv, &f)?;
    }
    Ok(())
}

/// A challenge or repair deadline passed (or a vacant piece is due for another assignment try).
fn deadline(
    kv: &mut impl KvStore,
    cfg: &StorageConfig,
    height: u64,
    seed: &Hash,
    at: u64,
    file: &Hash,
    index: u8,
) -> Result<(), StateError> {
    kv.delete(&timed_key(TAG_DEADLINE, at, file, index))?;
    let (Some(f), Some(mut p)) = (file_rec(kv, file)?, piece_rec(kv, file, index)?) else {
        return Ok(());
    };
    let mut failed: BTreeSet<Address> = BTreeSet::new();
    match p.state {
        PieceState::Vacant => {}
        _ => {
            match p.challenge {
                Some(c) if c.deadline == at => {}
                _ => return Ok(()), // stale entry: the challenge was answered
            }
            // Missed: slash into the escrow, release the holder, mark for repair.
            let collateral = cfg.deposit_per_kib.saturating_mul(f.piece_len / 1024);
            let cut = (u128::from(collateral) * u128::from(cfg.slash_bps) / 10_000) as u64;
            let dep = deposit_of(kv, &p.node)?;
            let cut = cut.min(dep);
            put_u64(kv, &key1(TAG_NODE_DEPOSIT, &p.node), dep - cut)?;
            let esc = escrow_of(kv, file)?;
            put_u64(kv, &key1(TAG_ESCROW, file), esc + cut)?;
            drop_hold(kv, &p.node, file, index, f.piece_len)?;
            if let Some(mut r) = node_rec(kv, &p.node)? {
                r.last_miss = height;
                put_node(kv, &p.node, &r)?;
            }
            failed.insert(p.node);
            p.missed += 1;
            p.challenge = None;
        }
    }
    // Reassign: avoid the failed node; prefer nodes without another piece of this file.
    let others = pieces_of_file(kv, file)?;
    let mut mine: BTreeMap<Address, u32> = BTreeMap::new();
    for o in &others {
        if o.index != index && o.state != PieceState::Vacant {
            *mine.entry(o.node).or_default() += 1;
        }
    }
    let seed = digest(&[seed, &p.missed.to_le_bytes()]);
    match pick_node(
        kv,
        cfg,
        height,
        &seed,
        file,
        index,
        f.piece_len,
        &failed,
        &mine,
        &BTreeMap::new(),
    )? {
        Some(node) => {
            add_hold(kv, &node, file, index, f.piece_len)?;
            let d = height + cfg.repair_blocks;
            let chunk = proof::challenge_chunk(&seed, file, index, p.missed, f.chunks());
            p.node = node;
            p.state = PieceState::Repairing;
            p.challenge = Some(Challenge {
                chunk,
                deadline: d,
                pay: f.pay,
            });
            kv.put(&timed_key(TAG_DEADLINE, d, file, index), &[0])?;
        }
        None => {
            p.state = PieceState::Vacant;
            p.challenge = None;
            kv.put(
                &timed_key(TAG_DEADLINE, height + cfg.repair_blocks, file, index),
                &[0],
            )?;
        }
    }
    put_piece(kv, &p)?;
    Ok(())
}

// ------------------------------------------------------------------ queries

/// Every storage node with its deposit.
pub fn nodes(kv: &impl KvStore) -> Result<Vec<NodeInfo>, StateError> {
    let mut out = Vec::new();
    for (k, v) in scan_all(kv, vec![TAG_NODE], Some(vec![TAG_NODE + 1]), usize::MAX)? {
        let address: Address = k[1..]
            .try_into()
            .map_err(|_| StateError("bad node key".into()))?;
        let r = NodeRec::decode(&v)?;
        out.push(NodeInfo {
            address,
            endpoint: r.endpoint,
            deposit: deposit_of(kv, &address)?,
            held_bytes: r.held,
            pieces: r.pieces,
            registered: r.registered,
        });
    }
    Ok(out)
}

impl<S: KvStore> ChainState<S> {
    /// Storage nodes, in address order.
    pub fn storage_nodes(&self) -> Result<Vec<NodeInfo>, StateError> {
        nodes(self.store())
    }

    pub fn storage_node(&self, node: &Address) -> Result<Option<NodeInfo>, StateError> {
        Ok(nodes(self.store())?
            .into_iter()
            .find(|n| &n.address == node))
    }

    /// A stored file and its escrow, if it is still live.
    pub fn storage_file(&self, id: &Hash) -> Result<Option<FileInfo>, StateError> {
        file_rec(self.store(), id)
    }

    /// The pieces of a file, by index.
    pub fn storage_pieces(&self, id: &Hash) -> Result<Vec<PieceInfo>, StateError> {
        pieces_of_file(self.store(), id)
    }

    /// Pieces assigned to `node` (what it has to keep and prove).
    pub fn storage_duties(&self, node: &Address) -> Result<Vec<PieceInfo>, StateError> {
        let kv = self.store();
        let p = key1(TAG_HOLD, node);
        let mut out = Vec::new();
        for (k, _) in scan_all(kv, p.clone(), Some(prefix_end(&p)), 100_000)? {
            let file: Hash = k[33..65]
                .try_into()
                .map_err(|_| StateError("bad hold".into()))?;
            if let Some(pi) = piece_rec(kv, &file, k[65])? {
                out.push(pi);
            }
        }
        Ok(out)
    }

    /// Files with their pieces, newest first is not tracked: this lists up to `limit` live files
    /// in id order.
    pub fn storage_files(&self, limit: usize) -> Result<Vec<FileInfo>, StateError> {
        let kv = self.store();
        let mut out = Vec::new();
        for (k, _) in scan_all(kv, vec![TAG_FILE], Some(vec![TAG_FILE + 1]), limit)? {
            let id: Hash = k[1..]
                .try_into()
                .map_err(|_| StateError("bad file key".into()))?;
            if let Some(f) = file_rec(kv, &id)? {
                out.push(f);
            }
        }
        Ok(out)
    }

    /// The storage parameters of this chain.
    pub fn storage_config(&self) -> StorageConfig {
        self.config().storage
    }
}

// ------------------------------------------------------------------ files from contracts

/// Adapter so the generic storage code can run on the `&mut dyn KvStore` of a running contract.
struct DynKv<'a>(&'a mut dyn KvStore);

impl KvStore for DynKv<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        self.0.get(key)
    }
    fn scan(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        reverse: bool,
        limit: usize,
    ) -> Result<Vec<btb_runtime::kv::Entry>, KvError> {
        self.0.scan(start, end, reverse, limit)
    }
    fn commit(&mut self, batch: btb_runtime::kv::WriteBatch) -> Result<(), KvError> {
        self.0.commit(batch)
    }
}

/// `file_info` and `file_pin` of a contract, on the chain's storage state.
pub struct ChainFiles {
    pub cfg: StorageConfig,
    /// `btb dev`: there are no storage nodes. A pin records the file (and takes the escrow from
    /// the contract) without assigning pieces, and a file reads as expired once the height
    /// passes its expiry, since no scheduler retires it.
    pub nodeless: bool,
}

/// Record a file as stored without storage nodes (`btb dev`). With `charge` the escrow of a
/// `Store` is taken from `owner`'s balance and kept in the file's escrow; without it the file is
/// free (an upload to the dev server). `Ok(false)`: the root is already stored.
pub fn record_nodeless(
    kv: &mut dyn KvStore,
    cfg: &StorageConfig,
    owner: &Address,
    height: u64,
    op: &StorageOp,
    charge: bool,
) -> Result<bool, KvError> {
    let StorageOp::Store {
        root,
        size,
        k,
        m,
        periods,
    } = op
    else {
        return Ok(false);
    };
    let mut kv = DynKv(kv);
    if kv.get(&key1(TAG_FILE, root))?.is_some() {
        return Ok(false);
    }
    let escrow = if charge {
        cfg.escrow_for(*size, *k, *m, *periods).unwrap_or(0)
    } else {
        0
    };
    if escrow > 0 {
        debit(&mut kv, owner, escrow).map_err(|e| KvError(e.0))?;
        put_u64(&mut kv, &key1(TAG_ESCROW, root), escrow)?;
    }
    kv.delete(&key1(TAG_ENDED, root))?;
    let piece_len = proof::piece_len(*size, usize::from(*k));
    put_file(
        &mut kv,
        &FileInfo {
            id: *root,
            owner: *owner,
            size: *size,
            k: *k,
            m: *m,
            piece_len,
            periods: *periods,
            pay: cfg.pay_per_period(piece_len).unwrap_or(0),
            created: height,
            live: u32::from(*k) + u32::from(*m),
            escrow,
        },
    )?;
    Ok(true)
}

impl ChainFiles {
    /// Block height by which a file stored at `created` for `periods` periods has paid out: every
    /// piece's last round is at most `periods * epoch_blocks` after the store.
    fn expiry(&self, created: u64, periods: u32) -> u64 {
        created.saturating_add(u64::from(periods).saturating_mul(self.cfg.epoch_blocks))
    }
}

impl btb_runtime::FileService for ChainFiles {
    fn info(
        &self,
        kv: &dyn KvStore,
        root: &[u8; 32],
        height: u64,
    ) -> Result<btb_vm::host::FileRecord, KvError> {
        use btb_vm::host::{abi, FileRecord};
        let bad = |e: DecodeError| KvError(format!("corrupt file record: {e:?}"));
        if let Some(b) = kv.get(&key1(TAG_FILE, root))? {
            let f = FileInfo::decode(*root, 0, &b).map_err(bad)?;
            let expiry = self.expiry(f.created, f.periods);
            return Ok(FileRecord {
                size: f.size,
                status: if self.nodeless && height >= expiry {
                    abi::FILE_EXPIRED
                } else {
                    abi::FILE_STORED
                },
                expiry,
            });
        }
        if let Some(b) = kv.get(&key1(TAG_ENDED, root))? {
            let mut r = Reader::new(&b, 1 << 10);
            let size = u64::decode_from(&mut r).map_err(bad)?;
            let created = u64::decode_from(&mut r).map_err(bad)?;
            let periods = u32::decode_from(&mut r).map_err(bad)?;
            return Ok(FileRecord {
                size,
                status: abi::FILE_EXPIRED,
                expiry: self.expiry(created, periods),
            });
        }
        Ok(FileRecord::default())
    }

    fn pin(
        &self,
        kv: &mut dyn KvStore,
        contract: &Address,
        pin: &btb_vm::host::FilePin,
        height: u64,
    ) -> Result<u64, KvError> {
        use btb_vm::host::abi;
        let op = StorageOp::Store {
            root: pin.root,
            size: pin.size,
            k: pin.k,
            m: pin.m,
            periods: pin.periods,
        };
        if op.check_static().is_err() {
            return Ok(abi::FILE_PIN_INVALID);
        }
        let mut kv = DynKv(kv);
        if kv.get(&key1(TAG_FILE, &pin.root))?.is_some() {
            return Ok(abi::FILE_PIN_EXISTS);
        }
        let Some(escrow) = self.cfg.escrow_for(pin.size, pin.k, pin.m, pin.periods) else {
            return Ok(abi::FILE_PIN_INVALID);
        };
        if balance(&kv, contract)? < escrow {
            return Ok(abi::FILE_PIN_INSUFFICIENT);
        }
        if self.nodeless {
            return Ok(
                if record_nodeless(kv.0, &self.cfg, contract, height, &op, true)? {
                    abi::FILE_PIN_OK
                } else {
                    abi::FILE_PIN_EXISTS
                },
            );
        }
        match apply(&mut kv, &self.cfg, contract, height, &op) {
            Ok(_) => Ok(abi::FILE_PIN_OK),
            Err(Fail::Tx(StorageError::AlreadyStored)) => Ok(abi::FILE_PIN_EXISTS),
            Err(Fail::Tx(StorageError::NotEnoughNodes)) => Ok(abi::FILE_PIN_NO_NODES),
            Err(Fail::Tx(_)) => Ok(abi::FILE_PIN_INVALID),
            Err(Fail::State(e)) => Err(KvError(e.0)),
        }
    }
}
