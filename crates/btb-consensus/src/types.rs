//! Wire and state types: validator set, certificates, blocks, messages, evidence.
//!
//! All signed byte strings are domain separated and bound to the chain's genesis id
//! (which commits to the chain id and the validator set), so a signature is never valid
//! for another message type, chain or validator set.

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;

pub type Hash = [u8; 32];
pub type Round = u64;
pub type Height = u64;
/// Logical time. One tick is whatever unit the embedder chooses; the crate never reads a clock.
pub type Tick = u64;
/// Index into the validator set.
pub type ValidatorIndex = u32;

/// Rounds above this are rejected outright (keeps `round + 1` arithmetic safe).
pub const MAX_ROUND: Round = 1 << 62;
/// Upper bound on validators (also bounds decoded vector lengths).
pub const MAX_VALIDATORS: usize = 10_000;

/// SHA-256 over the concatenation of `parts`.
pub fn sha256(parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// A 64-byte Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sig(pub [u8; 64]);

impl fmt::Debug for Sig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sig({:02x}{:02x}..)", self.0[0], self.0[1])
    }
}

/// Errors from constructing a validator set or replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    EmptyValidatorSet,
    TooManyValidators,
    ZeroStake,
    DuplicateKey,
    InvalidPublicKey,
    StakeOverflow,
    NotAValidator,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Validator {
    pub key: [u8; 32],
    pub stake: u64,
}

/// The validator set of one epoch. Order is significant: indices are positions.
#[derive(Clone, Debug)]
pub struct ValidatorSet {
    validators: Vec<Validator>,
    keys: Vec<VerifyingKey>,
    total: u128,
    digest: Hash,
}

impl ValidatorSet {
    pub fn new(validators: Vec<Validator>) -> Result<Self, Error> {
        if validators.is_empty() {
            return Err(Error::EmptyValidatorSet);
        }
        if validators.len() > MAX_VALIDATORS {
            return Err(Error::TooManyValidators);
        }
        let mut seen = BTreeSet::new();
        let mut keys = Vec::with_capacity(validators.len());
        let mut total: u128 = 0;
        let mut h = Sha256::new();
        h.update(b"BTB/valset/v1");
        for v in &validators {
            if v.stake == 0 {
                return Err(Error::ZeroStake);
            }
            if !seen.insert(v.key) {
                return Err(Error::DuplicateKey);
            }
            keys.push(VerifyingKey::from_bytes(&v.key).map_err(|_| Error::InvalidPublicKey)?);
            total = total
                .checked_add(v.stake as u128)
                .ok_or(Error::StakeOverflow)?;
            h.update(v.key);
            h.update(v.stake.to_le_bytes());
        }
        Ok(Self {
            validators,
            keys,
            total,
            digest: h.finalize().into(),
        })
    }

    pub fn len(&self) -> usize {
        self.validators.len()
    }
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }
    pub fn get(&self, i: ValidatorIndex) -> Option<&Validator> {
        self.validators.get(i as usize)
    }
    pub fn index_of(&self, key: &[u8; 32]) -> Option<ValidatorIndex> {
        self.validators
            .iter()
            .position(|v| &v.key == key)
            .map(|i| i as u32)
    }
    pub fn total_stake(&self) -> u128 {
        self.total
    }
    pub fn digest(&self) -> Hash {
        self.digest
    }
    /// Strictly more than 2/3 of total stake.
    pub fn is_quorum(&self, stake: u128) -> bool {
        stake * 3 > self.total * 2
    }
    /// Strictly more than 1/3 of total stake: contains at least one honest validator
    /// when the faulty stake is below 1/3.
    pub fn is_blocking(&self, stake: u128) -> bool {
        stake * 3 > self.total
    }
    /// Stake of validator `i` (0 if out of range).
    pub fn stake_of(&self, i: ValidatorIndex) -> u128 {
        self.validators
            .get(i as usize)
            .map_or(0, |v| v.stake as u128)
    }
}

/// Chain context: validator set plus the genesis id that domain-separates every signature.
#[derive(Clone, Debug)]
pub struct Context {
    pub set: ValidatorSet,
    pub genesis: Hash,
}

impl Context {
    pub fn new(chain_id: &[u8], set: ValidatorSet) -> Self {
        let genesis = sha256(&[
            b"BTB/genesis/v1",
            &(chain_id.len() as u64).to_le_bytes(),
            chain_id,
            &set.digest(),
        ]);
        Self { set, genesis }
    }

    pub fn vote_bytes(&self, round: Round, block_id: &Hash) -> Vec<u8> {
        let mut m = b"BTB/vote/v1".to_vec();
        m.extend_from_slice(&self.genesis);
        m.extend_from_slice(&round.to_le_bytes());
        m.extend_from_slice(block_id);
        m
    }
    pub fn timeout_bytes(&self, round: Round, qc_round: Round) -> Vec<u8> {
        let mut m = b"BTB/timeout/v1".to_vec();
        m.extend_from_slice(&self.genesis);
        m.extend_from_slice(&round.to_le_bytes());
        m.extend_from_slice(&qc_round.to_le_bytes());
        m
    }
    pub fn proposal_bytes(&self, round: Round, block_id: &Hash) -> Vec<u8> {
        let mut m = b"BTB/proposal/v1".to_vec();
        m.extend_from_slice(&self.genesis);
        m.extend_from_slice(&round.to_le_bytes());
        m.extend_from_slice(block_id);
        m
    }

    pub fn verify_sig(&self, who: ValidatorIndex, msg: &[u8], sig: &Sig) -> bool {
        match self.set.keys.get(who as usize) {
            Some(vk) => vk
                .verify_strict(msg, &Signature::from_bytes(&sig.0))
                .is_ok(),
            None => false,
        }
    }

    pub fn genesis_qc(&self) -> QuorumCert {
        QuorumCert {
            round: 0,
            block_id: self.genesis,
            votes: Vec::new(),
        }
    }

    /// Full structural and cryptographic check of a quorum certificate.
    pub fn verify_qc(&self, qc: &QuorumCert) -> bool {
        if qc.round == 0 {
            return qc.block_id == self.genesis && qc.votes.is_empty();
        }
        if qc.round > MAX_ROUND {
            return false;
        }
        let msg = self.vote_bytes(qc.round, &qc.block_id);
        let mut stake = 0u128;
        let mut prev: Option<u32> = None;
        for (who, sig) in &qc.votes {
            if (*who as usize) >= self.set.len() || prev.is_some_and(|p| p >= *who) {
                return false;
            }
            prev = Some(*who);
            if !self.verify_sig(*who, &msg, sig) {
                return false;
            }
            stake += self.set.stake_of(*who);
        }
        self.set.is_quorum(stake)
    }

    /// Full check of a timeout certificate (including its embedded high QC).
    pub fn verify_tc(&self, tc: &TimeoutCert) -> bool {
        if tc.round == 0 || tc.round > MAX_ROUND || tc.high_qc.round >= tc.round {
            return false;
        }
        let mut stake = 0u128;
        let mut prev: Option<u32> = None;
        let mut max_qc = 0;
        for e in &tc.entries {
            if (e.voter as usize) >= self.set.len() || prev.is_some_and(|p| p >= e.voter) {
                return false;
            }
            prev = Some(e.voter);
            if !self.verify_sig(e.voter, &self.timeout_bytes(tc.round, e.qc_round), &e.sig) {
                return false;
            }
            max_qc = max_qc.max(e.qc_round);
            stake += self.set.stake_of(e.voter);
        }
        self.set.is_quorum(stake) && tc.high_qc.round == max_qc && self.verify_qc(&tc.high_qc)
    }
}

/// Certificate that a quorum voted for `block_id` in `round`.
/// Votes are listed individually (sorted by validator index); signature aggregation is future work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuorumCert {
    pub round: Round,
    pub block_id: Hash,
    pub votes: Vec<(ValidatorIndex, Sig)>,
}

impl QuorumCert {
    pub fn digest(&self) -> Hash {
        sha256(&[b"BTB/qc/v1", &crate::codec::encode_qc(self)])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcEntry {
    pub voter: ValidatorIndex,
    /// Round of the high QC this validator held when it timed out (signed).
    pub qc_round: Round,
    pub sig: Sig,
}

/// Certificate that a quorum gave up on `round`. Carries the highest QC among the timeouts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeoutCert {
    pub round: Round,
    pub entries: Vec<TcEntry>,
    pub high_qc: QuorumCert,
}

impl TimeoutCert {
    pub fn digest(&self) -> Hash {
        sha256(&[b"BTB/tc/v1", &crate::codec::encode_tc(self)])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub round: Round,
    pub height: Height,
    /// Must equal `justify.block_id`.
    pub parent: Hash,
    /// QC for the parent.
    pub justify: QuorumCert,
    pub proposer: ValidatorIndex,
    /// SHA-256 of `payload`.
    pub payload_hash: Hash,
    /// Opaque application bytes (execution is out of scope for this crate).
    pub payload: Vec<u8>,
}

impl Block {
    pub fn hash_payload(payload: &[u8]) -> Hash {
        sha256(&[b"BTB/payload/v1", payload])
    }

    pub fn id(&self) -> Hash {
        sha256(&[
            b"BTB/block/v1",
            &self.round.to_le_bytes(),
            &self.height.to_le_bytes(),
            &self.parent,
            &self.justify.digest(),
            &self.proposer.to_le_bytes(),
            &self.payload_hash,
        ])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub block: Block,
    /// Present when the previous round ended in a timeout (`justify.round + 1 != round`).
    pub tc: Option<TimeoutCert>,
    /// Proposer signature over (round, block id).
    pub sig: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vote {
    pub round: Round,
    pub block_id: Hash,
    pub voter: ValidatorIndex,
    pub sig: Sig,
    /// The proposer's signature over (round, block id), copied from the proposal voted for.
    /// Not covered by `sig`; it lets everyone turn conflicting votes into proposer evidence.
    pub proposal_sig: Sig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeout {
    pub round: Round,
    pub high_qc: QuorumCert,
    pub voter: ValidatorIndex,
    /// Signature over (round, high_qc.round).
    pub sig: Sig,
    /// The leader-signed proposal header the sender saw in `round`, if any. Lets everyone
    /// notice a leader that sent different proposals to different validators.
    pub seen_proposal: Option<(Hash, Sig)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Propose(Proposal),
    Vote(Vote),
    Timeout(Timeout),
    /// "Here is the newest certificate I know": answer to a stale timeout.
    Catchup {
        qc: QuorumCert,
        tc: Option<TimeoutCert>,
    },
    /// Unauthenticated request for a block and its ancestors. `from` is where to answer.
    SyncRequest {
        from: ValidatorIndex,
        block_id: Hash,
    },
    /// Hash-chained blocks, newest first.
    SyncResponse {
        blocks: Vec<Block>,
    },
    Evidence(Evidence),
}

/// Self-contained, independently verifiable proof of misbehaviour (input for slashing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// Two votes by one validator in one round for different blocks.
    DoubleVote {
        voter: ValidatorIndex,
        round: Round,
        a: (Hash, Sig),
        b: (Hash, Sig),
    },
    /// Two proposals signed by one validator for one round with different block ids.
    DoubleProposal {
        proposer: ValidatorIndex,
        round: Round,
        a: (Hash, Sig),
        b: (Hash, Sig),
    },
}

impl Evidence {
    pub fn offender(&self) -> ValidatorIndex {
        match self {
            Evidence::DoubleVote { voter, .. } => *voter,
            Evidence::DoubleProposal { proposer, .. } => *proposer,
        }
    }
    pub fn round(&self) -> Round {
        match self {
            Evidence::DoubleVote { round, .. } | Evidence::DoubleProposal { round, .. } => *round,
        }
    }
    /// Deduplication key: (kind, offender, round).
    pub fn key(&self) -> (u8, ValidatorIndex, Round) {
        let kind = match self {
            Evidence::DoubleVote { .. } => 0,
            Evidence::DoubleProposal { .. } => 1,
        };
        (kind, self.offender(), self.round())
    }
    /// Checks both signatures against the offender's key and that the two ids differ.
    pub fn verify(&self, ctx: &Context) -> bool {
        match self {
            Evidence::DoubleVote { voter, round, a, b } => {
                a.0 != b.0
                    && ctx.verify_sig(*voter, &ctx.vote_bytes(*round, &a.0), &a.1)
                    && ctx.verify_sig(*voter, &ctx.vote_bytes(*round, &b.0), &b.1)
            }
            Evidence::DoubleProposal {
                proposer,
                round,
                a,
                b,
            } => {
                a.0 != b.0
                    && ctx.verify_sig(*proposer, &ctx.proposal_bytes(*round, &a.0), &a.1)
                    && ctx.verify_sig(*proposer, &ctx.proposal_bytes(*round, &b.0), &b.1)
            }
        }
    }
}

/// What a replica asks its embedder to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send to one validator.
    Send { to: ValidatorIndex, msg: Message },
    /// Send to every other validator.
    Broadcast(Message),
    /// A block became final. Emitted exactly once per height, in increasing height order.
    Commit(Block),
    /// Newly observed (and verified) misbehaviour. Also broadcast to peers when first found.
    Evidence(Evidence),
}
