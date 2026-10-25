//! The consensus state machine (Jolteon-style 2-chain HotStuff). See `docs/CONSENSUS.md`.
//!
//! The replica performs no I/O and reads no clock. Inputs are `on_message`, `on_timeout(now)`
//! and `propose`; outputs are `Action`s. All maps are ordered so behaviour is deterministic.

use crate::storage::{SafetyState, SafetyStorage};
use crate::types::*;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// How the leader of a round is chosen. Both are pure functions of (round, validator set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaderPolicy {
    /// `index = round mod n`. Ignores stake; every validator leads equally often.
    RoundRobin,
    /// Smooth weighted round-robin over a fixed cycle in which each validator holds a number
    /// of slots proportional to its stake (rounded to 1/(32n) of the total, minimum one slot).
    /// Deterministic, interleaved, and the gap between two turns of a validator is bounded, so
    /// a faulty validator can never hold the leader seat for many rounds in a row.
    StakeWeighted,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub chain_id: Vec<u8>,
    /// Ticks to wait in a round before timing out (doubles per consecutive timeout round).
    pub base_timeout: Tick,
    /// Cap on the doubling exponent.
    pub max_backoff_shift: u32,
    pub max_payload_bytes: usize,
    pub leader_policy: LeaderPolicy,
    /// Bound on buffered blocks whose parent is unknown.
    pub max_pending_blocks: usize,
    /// Ticks before an unanswered block request is retried at another peer.
    pub sync_retry: Tick,
    /// Per-round bookkeeping older than this many rounds behind the commit point is dropped.
    pub retain_rounds: u64,
    /// Bound on payloads queued through `propose`.
    pub max_queued_payloads: usize,
    /// Epoch number (domain-separates signatures). 0 for the first epoch.
    pub epoch: u64,
    /// Height of the last block of this epoch; 0 = the epoch never ends. See `docs/CONSENSUS.md`.
    pub end_height: Height,
}

/// Where an epoch starts: the finalized chain it extends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// Height of the last block of the previous epoch.
    pub height: Height,
    /// A 32-byte id every validator of the new epoch derives identically from the finalized
    /// chain (for example the chain header hash of block `height`).
    pub id: Hash,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chain_id: b"btb-test".to_vec(),
            base_timeout: 20,
            max_backoff_shift: 3,
            max_payload_bytes: 1 << 20,
            leader_policy: LeaderPolicy::RoundRobin,
            max_pending_blocks: 256,
            sync_retry: 10,
            retain_rounds: 1_000,
            max_queued_payloads: 1_024,
            epoch: 0,
            end_height: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub votes_cast: u64,
    pub timeouts_sent: u64,
    pub proposals_made: u64,
    pub qcs_formed: u64,
    pub tcs_formed: u64,
    pub commits: u64,
    pub evidence_found: u64,
    pub rejected_messages: u64,
    /// Times a commit candidate did not extend the committed chain. Must stay 0 under < 1/3 faults.
    pub conflicting_commits: u64,
    /// Proposals not voted for because their execution attestation did not match ours.
    pub exec_rejected: u64,
    /// Times a vote (or proposal) had to wait until the embedder executed an ancestor.
    pub exec_deferred: u64,
}

const MAX_SYNC_BLOCKS: usize = 64;
const TIMEOUT_ROUND_WINDOW: u64 = 64;
const VERIFIED_CACHE_MAX: usize = 4096;

struct Requested {
    at: Tick,
    tries: u32,
    hint: ValidatorIndex,
}

#[derive(Default)]
struct TimeoutPool {
    entries: BTreeMap<ValidatorIndex, (Round, Sig)>,
    best_qc: Option<QuorumCert>,
}

enum Item {
    Prop(Proposal),
    Blk(Block),
}
impl Item {
    fn block(&self) -> &Block {
        match self {
            Item::Prop(p) => &p.block,
            Item::Blk(b) => b,
        }
    }
}

pub struct Replica {
    ctx: Context,
    schedule: Vec<ValidatorIndex>,
    key: SigningKey,
    me: ValidatorIndex,
    cfg: Config,
    storage: Option<Box<dyn SafetyStorage>>,
    now: Tick,

    round: Round,
    round_deadline: Tick,
    failures: u32,
    safety: SafetyState,
    last_tc: Option<TimeoutCert>,

    blocks: BTreeMap<Hash, Block>,
    qcs: BTreeMap<Hash, QuorumCert>,
    pending: BTreeMap<Hash, Item>,
    requested: BTreeMap<Hash, Requested>,
    last_served: BTreeMap<ValidatorIndex, (Tick, Hash)>,

    vote_pool: BTreeMap<(Round, Hash), BTreeMap<ValidatorIndex, Sig>>,
    votes_seen: BTreeMap<(Round, ValidatorIndex), (Hash, Sig)>,
    proposals_seen: BTreeMap<(Round, ValidatorIndex), (Hash, Sig)>,
    timeout_pool: BTreeMap<Round, TimeoutPool>,
    evidence_seen: BTreeSet<(u8, ValidatorIndex, Round)>,
    catchup_sent: BTreeMap<ValidatorIndex, Tick>,
    verified_qc: BTreeSet<Hash>,
    verified_tc: BTreeSet<Hash>,
    payloads: VecDeque<Vec<u8>>,
    outbox: Vec<Action>,
    stats: Stats,
    /// The epoch's last block is committed: no more voting, proposing or timeouts.
    ended: bool,
    /// Embedder hook: a proposal whose block fails this check is not voted for.
    payload_check: Option<PayloadCheck>,
    /// Do not propose an empty block on one's own initiative; wait for `propose`.
    hold_empty: bool,
    /// Execution attestation hooks (see `set_exec_hooks`).
    exec_hooks: Option<(ExecProvider, ExecCheck)>,
    /// For every linked block `P`: the height of the block whose execution result a child of
    /// `P` must attest (`exec_height`), a pure function of the ancestry.
    exec_base: BTreeMap<Hash, Height>,
    /// The proposal for the current round that waits for the embedder to execute commits.
    deferred: Option<(Proposal, Hash)>,
}

/// See `Replica::set_payload_check`.
pub type PayloadCheck = Box<dyn Fn(&Block) -> bool>;

/// Outcome of an execution attestation check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecVerdict {
    /// The attestation matches the embedder's own execution.
    Accept,
    /// It does not (or is malformed): never vote for this block.
    Reject,
    /// The embedder has not executed the attested height yet; ask again after `resume`.
    Defer,
}

/// Produces the attestation for a block this replica is about to propose: the execution
/// result at the given height, or `None` while that height is not executed yet.
pub type ExecProvider = Box<dyn Fn(Height) -> Option<Vec<u8>>>;

/// Checks `block.exec` against the embedder's execution at the given height.
pub type ExecCheck = Box<dyn Fn(&Block, Height) -> ExecVerdict>;

impl Replica {
    /// Create a replica. `key` must belong to a validator in `set`. If `storage` holds
    /// saved safety state it is restored (crash recovery); blocks are re-fetched by sync.
    pub fn new(
        config: Config,
        set: ValidatorSet,
        key: SigningKey,
        storage: Option<Box<dyn SafetyStorage>>,
    ) -> Result<Self, Error> {
        Self::new_epoch(config, set, key, storage, None)
    }

    /// Create the replica of one epoch. `anchor` is the finalized chain the epoch extends
    /// (`None` for the first epoch, which starts at genesis). The epoch is the instance's whole
    /// world: a fresh round counter, its own set and signature domain, and it stops when the
    /// block at `config.end_height` is committed (`epoch_ended`). `storage` must be this
    /// epoch's own.
    pub fn new_epoch(
        config: Config,
        set: ValidatorSet,
        key: SigningKey,
        storage: Option<Box<dyn SafetyStorage>>,
        anchor: Option<Anchor>,
    ) -> Result<Self, Error> {
        let me = set
            .index_of(&key.verifying_key().to_bytes())
            .ok_or(Error::NotAValidator)?;
        let anchor_height = anchor.map_or(0, |a| a.height);
        if config.end_height != 0 && config.end_height <= anchor_height {
            return Err(Error::InvalidEpoch);
        }
        let ctx = Context::for_epoch(&config.chain_id, config.epoch, set, anchor.map(|a| a.id));
        let genesis_qc = ctx.genesis_qc();
        let safety = storage
            .as_ref()
            .and_then(|s| s.load())
            .unwrap_or_else(|| SafetyState {
                last_voted_round: 0,
                locked_round: 0,
                last_proposed_round: 0,
                high_qc: genesis_qc.clone(),
                committed_height: anchor_height,
                committed_round: 0,
                committed_id: ctx.root,
            });
        let genesis_block = Block {
            round: 0,
            height: anchor_height,
            parent: [0; 32],
            justify: genesis_qc.clone(),
            proposer: 0,
            payload_hash: Block::hash_payload(&[]),
            payload: Vec::new(),
            exec: Vec::new(),
        };
        let mut blocks = BTreeMap::new();
        blocks.insert(ctx.root, genesis_block);
        let mut exec_base = BTreeMap::new();
        exec_base.insert(ctx.root, anchor_height);
        let round = (safety.high_qc.round + 1)
            .max(safety.last_voted_round)
            .max(safety.last_proposed_round)
            .max(1);
        let mut qcs = BTreeMap::new();
        qcs.insert(ctx.root, genesis_qc);
        let schedule = match config.leader_policy {
            LeaderPolicy::RoundRobin => (0..ctx.set.len() as u32).collect(),
            LeaderPolicy::StakeWeighted => weighted_schedule(&ctx.set),
        };
        let mut r = Self {
            ctx,
            schedule,
            key,
            me,
            round,
            round_deadline: 0,
            failures: 0,
            safety,
            last_tc: None,
            blocks,
            qcs,
            pending: BTreeMap::new(),
            requested: BTreeMap::new(),
            last_served: BTreeMap::new(),
            vote_pool: BTreeMap::new(),
            votes_seen: BTreeMap::new(),
            proposals_seen: BTreeMap::new(),
            timeout_pool: BTreeMap::new(),
            evidence_seen: BTreeSet::new(),
            catchup_sent: BTreeMap::new(),
            verified_qc: BTreeSet::new(),
            verified_tc: BTreeSet::new(),
            payloads: VecDeque::new(),
            outbox: Vec::new(),
            stats: Stats::default(),
            ended: false,
            payload_check: None,
            hold_empty: false,
            exec_hooks: None,
            exec_base,
            deferred: None,
            cfg: config,
            storage,
            now: 0,
        };
        r.round_deadline = r.cfg.base_timeout;
        r.ended = r.cfg.end_height != 0 && r.safety.committed_height >= r.cfg.end_height;
        Ok(r)
    }

    // ---------------------------------------------------------------- accessors

    pub fn index(&self) -> ValidatorIndex {
        self.me
    }
    pub fn round(&self) -> Round {
        self.round
    }
    pub fn committed_height(&self) -> Height {
        self.safety.committed_height
    }
    pub fn high_qc(&self) -> &QuorumCert {
        &self.safety.high_qc
    }
    pub fn safety_state(&self) -> &SafetyState {
        &self.safety
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
    pub fn context(&self) -> &Context {
        &self.ctx
    }
    pub fn genesis_id(&self) -> Hash {
        self.ctx.genesis
    }
    /// The epoch this replica serves.
    pub fn epoch(&self) -> u64 {
        self.cfg.epoch
    }
    /// Whether the last block of the epoch (`Config::end_height`) is committed. From then on
    /// the replica only answers sync requests and timeouts of laggards; the embedder starts
    /// the next epoch with `Replica::new_epoch`.
    pub fn epoch_ended(&self) -> bool {
        self.ended
    }
    pub fn pending_blocks(&self) -> usize {
        self.pending.len()
    }

    /// The payloads of the uncommitted blocks that a child of `tip` would have as ancestors,
    /// newest first. They can still commit, so an embedder must not propose their contents
    /// again in a child of `tip`. Stops at the committed height or at a block this replica
    /// does not hold.
    pub fn uncommitted_payloads(&self, tip: &Hash) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut cur = *tip;
        while let Some(b) = self.blocks.get(&cur) {
            if b.height <= self.safety.committed_height {
                break;
            }
            out.push(b.payload.as_slice());
            cur = b.parent;
        }
        out
    }

    /// Leader of `round` (pure function of the round and the validator set).
    pub fn leader(&self, round: Round) -> ValidatorIndex {
        self.schedule[(round % self.schedule.len() as u64) as usize]
    }

    // ---------------------------------------------------------------- embedder hooks

    /// Install an application check run on every proposal's block just before voting (after
    /// the protocol checks pass). A block it rejects is dropped without a vote, so the round
    /// times out if the leader is faulty. Honest replicas must all use the same rule, or they
    /// may fail to form quorums (safety is unaffected: not voting is always safe).
    pub fn set_payload_check(&mut self, check: PayloadCheck) {
        self.payload_check = Some(check);
    }

    /// Install the execution attestation hooks. `provider(h)` returns the bytes a proposal
    /// carries in `Block::exec` (the execution result at height `h`, or `None` while `h` is not
    /// executed yet), `check(block, h)` judges a received block's `exec` against the
    /// embedder's own execution. `h` is always `Replica::exec_height` of the block's parent: a
    /// block that has `QC(P)` as its justify attests the execution of the highest block that
    /// `QC(P)` (together with the chain below it) commits. Every honest replica of a
    /// validator set must use equivalent hooks. `Defer` and `None` mean "executed later": the
    /// embedder executes the `Commit` actions it was handed, then calls `resume`.
    pub fn set_exec_hooks(&mut self, provider: ExecProvider, check: ExecCheck) {
        self.exec_hooks = Some((provider, check));
    }

    /// The height whose execution result a child of `parent` attests: the newest block the
    /// ancestry of `parent` commits by the 2-chain rule (`P.parent` when `P` and its parent
    /// are in consecutive rounds, else the same for `P`'s parent), or the epoch's anchor.
    /// `None` if `parent` is not a linked block.
    pub fn exec_height(&self, parent: &Hash) -> Option<Height> {
        self.exec_base.get(parent).copied()
    }

    /// Continue what waited for the embedder's execution (a vote on a deferred proposal, a
    /// proposal whose attestation could not be produced yet). Call it after executing the
    /// blocks of `Action::Commit` (and after block sync executed some).
    pub fn resume(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        if let Some((p, id)) = self.deferred.take() {
            self.try_vote(&p, id, &mut out);
        }
        self.pump(&mut out);
        out.append(&mut self.outbox);
        out
    }

    /// When set, a leader with no queued payload does not propose by itself; the embedder
    /// calls `propose` (an empty payload is allowed) when it wants a block. Use it to pace
    /// blocks instead of producing empty ones as fast as the network allows. The embedder must
    /// then keep proposing often enough that rounds do not time out.
    pub fn set_hold_empty_proposals(&mut self, hold: bool) {
        self.hold_empty = hold;
    }

    // ---------------------------------------------------------------- inputs

    /// Kick off a freshly created (or restarted) replica; the leader of the first round proposes.
    pub fn start(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        self.pump(&mut out);
        out.append(&mut self.outbox);
        out
    }

    /// Queue a payload for a future proposal; if this replica is the current leader and has
    /// not proposed yet this round, it proposes immediately. Oversized payloads are dropped.
    pub fn propose(&mut self, payload: Vec<u8>) -> Vec<Action> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        if payload.len() <= self.cfg.max_payload_bytes
            && self.payloads.len() < self.cfg.max_queued_payloads
        {
            self.payloads.push_back(payload);
        }
        self.pump(&mut out);
        out.append(&mut self.outbox);
        out
    }

    /// Advance logical time to `now` and fire the pacemaker if the round timer expired.
    /// Embedders call this on every tick (or at least at the next deadline).
    pub fn on_timeout(&mut self, now: Tick) -> Vec<Action> {
        let mut out = Vec::new();
        self.now = self.now.max(now);
        if self.ended {
            return out;
        }
        if self.now >= self.round_deadline {
            // Timeout (or retransmission of it) for the current round.
            let r = self.round;
            self.send_timeout(r, &mut out);
            // Retransmit at a constant period while stuck; backoff only applies to new rounds.
            self.round_deadline = self.now.saturating_add(self.cfg.base_timeout);
        }
        self.retry_requests(&mut out);
        self.pump(&mut out);
        out.append(&mut self.outbox);
        out
    }

    /// Process one message from the network. Never panics; invalid input is dropped.
    pub fn on_message(&mut self, msg: Message) -> Vec<Action> {
        let mut out = Vec::new();
        if self.ended {
            self.serve_ended(msg, &mut out);
            return out;
        }
        match msg {
            Message::Propose(p) => self.on_proposal(p, false, &mut out),
            Message::Vote(v) => self.on_vote(v, false, &mut out),
            Message::Timeout(t) => self.on_timeout_msg(t, false, &mut out),
            Message::Catchup { qc, tc } => {
                let ok = self.process_qc(&qc, &mut out)
                    && tc.as_ref().is_none_or(|t| self.process_tc(t, &mut out));
                if !ok {
                    self.stats.rejected_messages += 1;
                }
            }
            Message::SyncRequest { from, block_id } => {
                self.on_sync_request(from, block_id, &mut out)
            }
            Message::SyncResponse { blocks } => self.on_sync_response(blocks, &mut out),
            Message::Evidence(e) => {
                if e.verify(&self.ctx) {
                    self.note_evidence(e, false, &mut out);
                } else {
                    self.stats.rejected_messages += 1;
                }
            }
        }
        self.pump(&mut out);
        out.append(&mut self.outbox);
        out
    }

    // ---------------------------------------------------------------- epoch end

    /// After the epoch's last block is committed: help laggards finish the epoch (answer block
    /// requests, and answer a verified timeout with the newest certificates) and nothing else.
    fn serve_ended(&mut self, msg: Message, out: &mut Vec<Action>) {
        match msg {
            Message::SyncRequest { from, block_id } => self.on_sync_request(from, block_id, out),
            Message::Timeout(t) => {
                let ok = (t.voter as usize) < self.ctx.set.len()
                    && t.voter != self.me
                    && t.round != 0
                    && t.round <= MAX_ROUND
                    && self.ctx.verify_sig(
                        t.voter,
                        &self.ctx.timeout_bytes(t.round, t.high_qc.round),
                        &t.sig,
                    );
                let gap = (self.cfg.base_timeout / 2).max(1);
                let due = self
                    .catchup_sent
                    .get(&t.voter)
                    .is_none_or(|last| self.now >= last.saturating_add(gap));
                if ok && due {
                    self.catchup_sent.insert(t.voter, self.now);
                    out.push(Action::Send {
                        to: t.voter,
                        msg: Message::Catchup {
                            qc: self.safety.high_qc.clone(),
                            tc: self.last_tc.clone(),
                        },
                    });
                } else if !ok {
                    self.reject();
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- helpers

    fn timeout_duration(&self) -> Tick {
        self.cfg
            .base_timeout
            .saturating_mul(1u64 << self.failures.min(self.cfg.max_backoff_shift).min(40))
    }

    fn persist(&mut self) {
        if let Some(s) = self.storage.as_mut() {
            s.save(&self.safety);
        }
    }

    fn sign(&self, msg: &[u8]) -> Sig {
        Sig(self.key.sign(msg).to_bytes())
    }

    fn reject(&mut self) {
        self.stats.rejected_messages += 1;
    }

    fn enter_round(&mut self, r: Round, by_qc: bool) {
        if r <= self.round || r > MAX_ROUND {
            return;
        }
        self.round = r;
        if self.hold_empty {
            // The embedder queues a payload for the round it is leading; one that could not
            // be proposed in time (it waited for execution) must not resurface later, when
            // the transactions in it may have been included elsewhere.
            self.payloads.clear();
        }
        self.failures = if by_qc { 0 } else { self.failures + 1 };
        self.round_deadline = self.now.saturating_add(self.timeout_duration());
    }

    fn note_evidence(&mut self, e: Evidence, local: bool, out: &mut Vec<Action>) {
        if !self.evidence_seen.insert(e.key()) {
            return;
        }
        self.stats.evidence_found += 1;
        if local {
            out.push(Action::Broadcast(Message::Evidence(e.clone())));
        }
        out.push(Action::Evidence(e));
    }

    /// `sig` must already be verified.
    fn record_vote(
        &mut self,
        round: Round,
        voter: ValidatorIndex,
        id: Hash,
        sig: Sig,
        out: &mut Vec<Action>,
    ) {
        match self.votes_seen.get(&(round, voter)) {
            None => {
                self.votes_seen.insert((round, voter), (id, sig));
            }
            Some((old, _)) if *old == id => {}
            Some(old) => {
                let e = Evidence::DoubleVote {
                    voter,
                    round,
                    a: *old,
                    b: (id, sig),
                };
                self.note_evidence(e, true, out);
            }
        }
    }

    /// `sig` must already be verified.
    fn record_proposal(
        &mut self,
        round: Round,
        proposer: ValidatorIndex,
        id: Hash,
        sig: Sig,
        out: &mut Vec<Action>,
    ) {
        match self.proposals_seen.get(&(round, proposer)) {
            None => {
                self.proposals_seen.insert((round, proposer), (id, sig));
            }
            Some((old, _)) if *old == id => {}
            Some(old) => {
                let e = Evidence::DoubleProposal {
                    proposer,
                    round,
                    a: *old,
                    b: (id, sig),
                };
                self.note_evidence(e, true, out);
            }
        }
    }

    // ---------------------------------------------------------------- certificates

    /// Verify (cached) and absorb a QC: update high QC, lock, round, and try to commit.
    fn process_qc(&mut self, qc: &QuorumCert, out: &mut Vec<Action>) -> bool {
        if qc.round == 0 {
            return self.ctx.verify_qc(qc);
        }
        let digest = qc.digest();
        if !self.verified_qc.contains(&digest) {
            if !self.verify_qc_cached(qc) {
                return false;
            }
            if self.verified_qc.len() >= VERIFIED_CACHE_MAX {
                self.verified_qc.clear();
            }
            self.verified_qc.insert(digest);
            for (who, sig) in &qc.votes {
                self.record_vote(qc.round, *who, qc.block_id, *sig, out);
            }
        }
        if let Some(b) = self.blocks.get(&qc.block_id) {
            if b.round != qc.round {
                return false; // a "certified" block whose round differs from the certified round
            }
        }
        let mut changed = false;
        if qc.round > self.safety.high_qc.round {
            self.safety.high_qc = qc.clone();
            changed = true;
        }
        // Lock on the highest QC seen (2-chain rule: the 1-chain head).
        if qc.round > self.safety.locked_round {
            self.safety.locked_round = qc.round;
            changed = true;
        }
        if changed {
            self.persist();
        }
        self.qcs.entry(qc.block_id).or_insert_with(|| qc.clone());
        self.enter_round(qc.round + 1, true);
        if self.blocks.contains_key(&qc.block_id) {
            self.try_commit(qc.block_id, out);
        } else {
            self.request_block(qc.block_id, self.leader(qc.round));
        }
        true
    }

    /// `Context::verify_qc`, skipping signatures this replica already verified as standalone
    /// votes (a QC is mostly made of votes we received ourselves).
    fn verify_qc_cached(&self, qc: &QuorumCert) -> bool {
        let msg = self.ctx.vote_bytes(qc.round, &qc.block_id);
        let mut stake = 0u128;
        let mut prev: Option<u32> = None;
        if qc.round == 0 || qc.round > MAX_ROUND {
            return false;
        }
        for (who, sig) in &qc.votes {
            if (*who as usize) >= self.ctx.set.len() || prev.is_some_and(|p| p >= *who) {
                return false;
            }
            prev = Some(*who);
            let cached = self.votes_seen.get(&(qc.round, *who)) == Some(&(qc.block_id, *sig));
            if !cached && !self.ctx.verify_sig(*who, &msg, sig) {
                return false;
            }
            stake += self.ctx.set.stake_of(*who);
        }
        self.ctx.set.is_quorum(stake)
    }

    fn process_tc(&mut self, tc: &TimeoutCert, out: &mut Vec<Action>) -> bool {
        let digest = tc.digest();
        if !self.verified_tc.contains(&digest) {
            if !self.ctx.verify_tc(tc) {
                return false;
            }
            if self.verified_tc.len() >= VERIFIED_CACHE_MAX {
                self.verified_tc.clear();
            }
            self.verified_tc.insert(digest);
        }
        if !self.process_qc(&tc.high_qc, out) {
            return false;
        }
        if self.last_tc.as_ref().is_none_or(|t| tc.round > t.round) {
            self.last_tc = Some(tc.clone());
        }
        self.enter_round(tc.round + 1, false);
        true
    }

    // ---------------------------------------------------------------- proposals

    /// `local`: produced by this replica itself, so its signature need not be re-verified.
    fn on_proposal(&mut self, p: Proposal, local: bool, out: &mut Vec<Action>) {
        let b = &p.block;
        if b.round == 0
            || b.round > MAX_ROUND
            || b.payload.len() > self.cfg.max_payload_bytes
            || b.payload_hash != Block::hash_payload(&b.payload)
            || b.exec.len() > MAX_EXEC_BYTES
            || b.parent != b.justify.block_id
            || b.justify.round >= b.round
            || (b.proposer as usize) >= self.ctx.set.len()
            || b.height == 0
            // Blocks past the epoch's last height only exist to certify it; they carry nothing.
            || (self.cfg.end_height != 0 && b.height > self.cfg.end_height && !b.payload.is_empty())
        {
            return self.reject();
        }
        let id = b.id();
        if !local
            && !self
                .ctx
                .verify_sig(b.proposer, &self.ctx.proposal_bytes(b.round, &id), &p.sig)
        {
            return self.reject();
        }
        if self.leader(b.round) != b.proposer {
            return self.reject();
        }
        let (round, proposer, sig) = (b.round, b.proposer, p.sig);
        self.record_proposal(round, proposer, id, sig, out);
        if round + self.cfg.retain_rounds < self.round {
            return;
        }
        let justify = p.block.justify.clone();
        if !self.process_qc(&justify, out) {
            return self.reject();
        }
        if let Some(tc) = p.tc.clone() {
            if !self.process_tc(&tc, out) {
                return self.reject();
            }
        }
        if self.blocks.contains_key(&id) {
            // Already linked (e.g. via sync). Still allow a vote now that the certificates
            // may have moved us into this round.
            self.try_vote(&p, id, out);
            return;
        }
        self.link(Item::Prop(p), id, out);
    }

    /// Insert `item` if its parent is known, else buffer it and ask for the parent.
    fn link(&mut self, item: Item, id: Hash, out: &mut Vec<Action>) {
        let parent = item.block().parent;
        if !self.blocks.contains_key(&parent) {
            if self.pending.len() < self.cfg.max_pending_blocks {
                let hint = item.block().proposer;
                self.pending.insert(id, item);
                self.request_block(parent, hint);
            }
            return;
        }
        let mut work = vec![(id, item)];
        while let Some((id, item)) = work.pop() {
            if !self.insert_linked(&item, id, out) {
                continue;
            }
            self.requested.remove(&id);
            if self.qcs.contains_key(&id) {
                self.try_commit(id, out);
            }
            if let Item::Prop(p) = &item {
                self.try_vote(p, id, out);
            }
            let children: Vec<Hash> = self
                .pending
                .iter()
                .filter(|(_, it)| it.block().parent == id)
                .map(|(k, _)| *k)
                .collect();
            for c in children {
                if let Some(it) = self.pending.remove(&c) {
                    work.push((c, it));
                }
            }
        }
    }

    /// Final structural checks against the (known) parent, then store the block.
    fn insert_linked(&mut self, item: &Item, id: Hash, out: &mut Vec<Action>) -> bool {
        let b = item.block();
        let Some(parent) = self.blocks.get(&b.parent) else {
            return false;
        };
        let (ph, pr) = (parent.height, parent.round);
        let parent_base = self.exec_base.get(&b.parent).copied().unwrap_or(ph);
        if b.height != ph + 1 || pr != b.justify.round || b.round <= pr {
            self.reject();
            return false;
        }
        if matches!(item, Item::Blk(_)) && !self.process_qc(&b.justify.clone(), out) {
            self.reject();
            return false;
        }
        // What a child of this block attests: the parent is committed by the 2-chain rule when
        // this block is in the next round, else whatever the parent's own ancestry committed.
        let base = if b.round == pr + 1 { ph } else { parent_base };
        self.exec_base.insert(id, base);
        self.blocks.insert(id, b.clone());
        true
    }

    /// The voting rule. See docs/CONSENSUS.md.
    fn try_vote(&mut self, p: &Proposal, id: Hash, out: &mut Vec<Action>) {
        let b = &p.block;
        if self.ended || b.round != self.round || b.round <= self.safety.last_voted_round {
            return;
        }
        // Lock: never vote for a block that does not extend a QC at least as high as our lock.
        if b.justify.round < self.safety.locked_round {
            return;
        }
        let sequential = b.justify.round + 1 == b.round
            || p.tc
                .as_ref()
                .is_some_and(|tc| tc.round + 1 == b.round && b.justify.round >= tc.high_qc.round);
        if !sequential {
            return;
        }
        // Application check on the payload (embedder hook): never vote for what it rejects.
        if let Some(check) = &self.payload_check {
            if !check(b) {
                return self.reject();
            }
        }
        // Execution attestation: vote only for a block whose claimed execution result is ours.
        if let Some((_, check)) = &self.exec_hooks {
            let Some(h) = self.exec_base.get(&b.parent).copied() else {
                return self.reject();
            };
            match check(b, h) {
                ExecVerdict::Accept => {}
                ExecVerdict::Reject => {
                    self.stats.exec_rejected += 1;
                    return self.reject();
                }
                ExecVerdict::Defer => {
                    self.stats.exec_deferred += 1;
                    self.deferred = Some((p.clone(), id));
                    return;
                }
            }
        }
        // Write-ahead: persist before the vote can leave this function.
        self.safety.last_voted_round = b.round;
        self.persist();
        self.stats.votes_cast += 1;
        let vote = Vote {
            round: b.round,
            block_id: id,
            voter: self.me,
            sig: self.sign(&self.ctx.vote_bytes(b.round, &id)),
            proposal_sig: p.sig,
        };
        // Votes go to every validator (not only the next leader): any replica can then form
        // the QC, so a faulty next leader cannot veto this block, and equivocation by voters
        // is visible to everyone.
        out.push(Action::Broadcast(Message::Vote(vote.clone())));
        self.on_vote(vote, true, out);
    }

    // ---------------------------------------------------------------- votes

    fn on_vote(&mut self, v: Vote, local: bool, out: &mut Vec<Action>) {
        if (v.voter as usize) >= self.ctx.set.len()
            || v.round == 0
            || v.round > MAX_ROUND
            || v.round.saturating_add(1) < self.round
            || v.round > self.round.saturating_add(2)
        {
            return self.reject();
        }
        if !local
            && !self
                .ctx
                .verify_sig(v.voter, &self.ctx.vote_bytes(v.round, &v.block_id), &v.sig)
        {
            return self.reject();
        }
        // The leader's signature on the voted block (if valid) lets everyone catch a leader
        // that showed different blocks to different voters. It is only a hint: the vote
        // itself counts regardless.
        let leader = self.leader(v.round);
        let known = self
            .proposals_seen
            .get(&(v.round, leader))
            .is_some_and(|(id, _)| *id == v.block_id);
        if !known
            && self.ctx.verify_sig(
                leader,
                &self.ctx.proposal_bytes(v.round, &v.block_id),
                &v.proposal_sig,
            )
        {
            self.record_proposal(v.round, leader, v.block_id, v.proposal_sig, out);
        }
        self.record_vote(v.round, v.voter, v.block_id, v.sig, out);
        // Only a voter's first vote in a round counts toward a QC (and bounds the pool).
        let canonical = self
            .votes_seen
            .get(&(v.round, v.voter))
            .is_some_and(|(id, _)| *id == v.block_id);
        if !canonical || self.safety.high_qc.round >= v.round {
            return;
        }
        let pool = self.vote_pool.entry((v.round, v.block_id)).or_default();
        pool.insert(v.voter, v.sig);
        let stake: u128 = pool.keys().map(|i| self.ctx.set.stake_of(*i)).sum();
        if self.ctx.set.is_quorum(stake) {
            let votes: Vec<(u32, Sig)> = pool.iter().map(|(i, s)| (*i, *s)).collect();
            let qc = QuorumCert {
                round: v.round,
                block_id: v.block_id,
                votes,
            };
            self.stats.qcs_formed += 1;
            // Every constituent vote was verified on receipt.
            self.verified_qc.insert(qc.digest());
            self.process_qc(&qc, out);
        }
    }

    // ---------------------------------------------------------------- timeouts

    fn send_timeout(&mut self, r: Round, out: &mut Vec<Action>) {
        // After timing out in a round we never vote in it.
        if self.safety.last_voted_round < r {
            self.safety.last_voted_round = r;
            self.persist();
        }
        let hq = self.safety.high_qc.clone();
        let seen = self.proposals_seen.get(&(r, self.leader(r))).copied();
        let t = Timeout {
            round: r,
            sig: self.sign(&self.ctx.timeout_bytes(r, hq.round)),
            high_qc: hq,
            voter: self.me,
            seen_proposal: seen,
        };
        self.stats.timeouts_sent += 1;
        out.push(Action::Broadcast(Message::Timeout(t.clone())));
        self.on_timeout_msg(t, true, out);
    }

    fn on_timeout_msg(&mut self, t: Timeout, local: bool, out: &mut Vec<Action>) {
        if (t.voter as usize) >= self.ctx.set.len() || t.round == 0 || t.round > MAX_ROUND {
            return self.reject();
        }
        if !local
            && !self.ctx.verify_sig(
                t.voter,
                &self.ctx.timeout_bytes(t.round, t.high_qc.round),
                &t.sig,
            )
        {
            return self.reject();
        }
        if !self.process_qc(&t.high_qc, out) {
            return self.reject();
        }
        if let Some((id, sig)) = t.seen_proposal {
            let leader = self.leader(t.round);
            if self
                .ctx
                .verify_sig(leader, &self.ctx.proposal_bytes(t.round, &id), &sig)
            {
                self.record_proposal(t.round, leader, id, sig, out);
            }
        }
        if t.round < self.round {
            // The sender is behind: hand it the newest certificates (rate limited per peer, so a lost reply is retried).
            let min_gap = (self.cfg.base_timeout / 2).max(1);
            let due = self
                .catchup_sent
                .get(&t.voter)
                .is_none_or(|last| self.now >= last.saturating_add(min_gap));
            if t.voter != self.me && due {
                self.catchup_sent.insert(t.voter, self.now);
                out.push(Action::Send {
                    to: t.voter,
                    msg: Message::Catchup {
                        qc: self.safety.high_qc.clone(),
                        tc: self.last_tc.clone(),
                    },
                });
            }
            return;
        }
        if t.round > self.round + TIMEOUT_ROUND_WINDOW {
            return;
        }
        let pool = self.timeout_pool.entry(t.round).or_default();
        pool.entries.insert(t.voter, (t.high_qc.round, t.sig));
        if pool
            .best_qc
            .as_ref()
            .is_none_or(|q| t.high_qc.round > q.round)
        {
            pool.best_qc = Some(t.high_qc.clone());
        }
        let stake: u128 = pool.entries.keys().map(|i| self.ctx.set.stake_of(*i)).sum();
        let round = t.round;
        // Amplification: > 1/3 of stake timed out, so at least one honest validator did.
        if self.ctx.set.is_blocking(stake)
            && self.safety.last_voted_round < round
            && round >= self.round
        {
            if round > self.round {
                self.enter_round(round, false);
            }
            self.send_timeout(round, out);
            return; // send_timeout re-enters this function and re-checks the quorum
        }
        if self.ctx.set.is_quorum(stake) {
            let Some(pool) = self.timeout_pool.get(&round) else {
                return;
            };
            let Some(high_qc) = pool.best_qc.clone() else {
                return;
            };
            let entries: Vec<TcEntry> = pool
                .entries
                .iter()
                .map(|(v, (qr, s))| TcEntry {
                    voter: *v,
                    qc_round: *qr,
                    sig: *s,
                })
                .collect();
            let tc = TimeoutCert {
                round,
                entries,
                high_qc,
            };
            if self.last_tc.as_ref().is_none_or(|l| l.round < round) {
                self.stats.tcs_formed += 1;
                self.process_tc(&tc, out);
            }
        }
    }

    // ---------------------------------------------------------------- commit

    fn try_commit(&mut self, certified: Hash, out: &mut Vec<Action>) {
        let Some(b1) = self.blocks.get(&certified) else {
            return;
        };
        let Some(b0) = self.blocks.get(&b1.parent) else {
            return;
        };
        // 2-chain rule: a QC for b1 whose parent b0 is from the immediately preceding round.
        if b1.round != b0.round + 1 {
            return;
        }
        let target = b1.parent;
        let mut chain = Vec::new();
        let mut cur = target;
        loop {
            let Some(b) = self.blocks.get(&cur) else {
                return;
            };
            if b.height <= self.safety.committed_height {
                if b.height == self.safety.committed_height && cur != self.safety.committed_id {
                    self.stats.conflicting_commits += 1;
                    return;
                }
                break;
            }
            chain.push((cur, b.clone()));
            cur = b.parent;
        }
        let end = self.cfg.end_height;
        for (id, b) in chain.into_iter().rev() {
            if end != 0 && b.height > end {
                break; // epoch tail: certified only to finalize the last block of the epoch
            }
            self.safety.committed_height = b.height;
            self.safety.committed_round = b.round;
            self.safety.committed_id = id;
            self.stats.commits += 1;
            out.push(Action::Commit(b));
        }
        if end != 0 && self.safety.committed_height >= end {
            self.ended = true;
        }
        self.persist();
        self.prune();
    }

    fn prune(&mut self) {
        let cutoff = self
            .safety
            .committed_round
            .saturating_sub(self.cfg.retain_rounds);
        self.votes_seen = self.votes_seen.split_off(&(cutoff, 0));
        self.proposals_seen = self.proposals_seen.split_off(&(cutoff, 0));
        self.vote_pool.retain(|(r, _), _| *r >= cutoff);
        self.timeout_pool.retain(|r, _| *r >= cutoff);
        self.qcs.retain(|_, q| q.round >= cutoff);
    }

    // ---------------------------------------------------------------- proposing

    fn pump(&mut self, out: &mut Vec<Action>) {
        let r = self.round;
        if self.ended
            || self.leader(r) != self.me
            || self.safety.last_proposed_round >= r
            || self.safety.last_voted_round >= r
        {
            return;
        }
        let justify = self.safety.high_qc.clone();
        let Some(parent) = self.blocks.get(&justify.block_id) else {
            self.request_block(justify.block_id, self.me);
            return;
        };
        let height = parent.height + 1;
        let tc = if justify.round + 1 == r {
            None
        } else {
            match &self.last_tc {
                Some(tc) if tc.round + 1 == r && justify.round >= tc.high_qc.round => {
                    Some(tc.clone())
                }
                _ => return,
            }
        };
        // The attestation must exist before a payload is taken from the queue.
        let exec = match &self.exec_hooks {
            None => Vec::new(),
            Some((provider, _)) => {
                let Some(h) = self.exec_base.get(&justify.block_id).copied() else {
                    return;
                };
                match provider(h) {
                    Some(e) if e.len() <= MAX_EXEC_BYTES => e,
                    Some(_) => return,
                    None => {
                        self.stats.exec_deferred += 1;
                        return;
                    }
                }
            }
        };
        let end = self.cfg.end_height;
        let payload = if end != 0 && height > end {
            Vec::new()
        } else {
            match self.payloads.pop_front() {
                Some(p) => p,
                // The embedder paces blocks: wait for `propose` instead of sending an empty one.
                None if self.hold_empty => return,
                None => Vec::new(),
            }
        };
        let block = Block {
            round: r,
            height,
            parent: justify.block_id,
            justify,
            proposer: self.me,
            payload_hash: Block::hash_payload(&payload),
            payload,
            exec,
        };
        let id = block.id();
        let sig = self.sign(&self.ctx.proposal_bytes(r, &id));
        self.safety.last_proposed_round = r;
        self.persist();
        self.stats.proposals_made += 1;
        let p = Proposal { block, tc, sig };
        out.push(Action::Broadcast(Message::Propose(p.clone())));
        self.on_proposal(p, true, out);
    }

    /// Test/simulation helper: build a correctly signed proposal for the current round even
    /// when not the leader or already proposed (used to model Byzantine leaders).
    #[cfg(test)]
    pub(crate) fn forge_proposal(&self, payload: Vec<u8>) -> Option<Proposal> {
        let justify = self.safety.high_qc.clone();
        let justify_id = justify.block_id;
        let parent = self.blocks.get(&justify.block_id)?;
        let tc = if justify.round + 1 == self.round {
            None
        } else {
            self.last_tc.clone()
        };
        let block = Block {
            round: self.round,
            height: parent.height + 1,
            parent: justify.block_id,
            justify,
            proposer: self.me,
            payload_hash: Block::hash_payload(&payload),
            payload,
            exec: self.exec_hooks.as_ref().map_or_else(Vec::new, |(p, _)| {
                self.exec_base
                    .get(&justify_id)
                    .and_then(|h| p(*h))
                    .unwrap_or_default()
            }),
        };
        let id = block.id();
        Some(Proposal {
            sig: self.sign(&self.ctx.proposal_bytes(self.round, &id)),
            block,
            tc,
        })
    }

    #[cfg(test)]
    pub(crate) fn queued_payloads(&self) -> usize {
        self.payloads.len()
    }

    #[cfg(test)]
    pub(crate) fn sign_for_test(&self, msg: &[u8]) -> Sig {
        self.sign(msg)
    }

    // ---------------------------------------------------------------- sync

    fn request_block(&mut self, id: Hash, hint: ValidatorIndex) {
        if self.blocks.contains_key(&id) || self.requested.contains_key(&id) {
            return;
        }
        let hint = if hint == self.me || (hint as usize) >= self.ctx.set.len() {
            (self.me + 1) % self.ctx.set.len() as u32
        } else {
            hint
        };
        if hint == self.me {
            return; // single-validator set: nobody to ask
        }
        self.requested.insert(
            id,
            Requested {
                at: self.now,
                tries: 0,
                hint,
            },
        );
        self.outbox.push(Action::Send {
            to: hint,
            msg: Message::SyncRequest {
                from: self.me,
                block_id: id,
            },
        });
    }

    fn retry_requests(&mut self, out: &mut Vec<Action>) {
        let n = self.ctx.set.len() as u32;
        let (now, me, retry) = (self.now, self.me, self.cfg.sync_retry);
        for (id, rq) in self.requested.iter_mut() {
            if now >= rq.at.saturating_add(retry) {
                rq.at = now;
                rq.tries += 1;
                let mut to = (rq.hint + rq.tries) % n;
                if to == me {
                    to = (to + 1) % n;
                }
                if to != me {
                    out.push(Action::Send {
                        to,
                        msg: Message::SyncRequest {
                            from: me,
                            block_id: *id,
                        },
                    });
                }
            }
        }
    }

    fn on_sync_request(&mut self, from: ValidatorIndex, block_id: Hash, out: &mut Vec<Action>) {
        if (from as usize) >= self.ctx.set.len() || from == self.me {
            return self.reject();
        }
        if self.last_served.get(&from) == Some(&(self.now, block_id)) {
            return;
        }
        self.last_served.insert(from, (self.now, block_id));
        let mut blocks = Vec::new();
        let mut cur = block_id;
        while blocks.len() < MAX_SYNC_BLOCKS && cur != self.ctx.root {
            let Some(b) = self.blocks.get(&cur) else {
                break;
            };
            blocks.push(b.clone());
            cur = b.parent;
        }
        if !blocks.is_empty() {
            out.push(Action::Send {
                to: from,
                msg: Message::SyncResponse { blocks },
            });
        }
    }

    fn on_sync_response(&mut self, blocks: Vec<Block>, out: &mut Vec<Action>) {
        if blocks.is_empty() || blocks.len() > MAX_SYNC_BLOCKS {
            return self.reject();
        }
        // Must be a hash chain, newest first, whose head we asked for.
        let head = blocks[0].id();
        if !self.requested.contains_key(&head) && !self.qcs.contains_key(&head) {
            return self.reject();
        }
        let mut ids = Vec::with_capacity(blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            let id = b.id();
            if i > 0 && id != blocks[i - 1].parent {
                return self.reject();
            }
            if b.payload.len() > self.cfg.max_payload_bytes
                || b.payload_hash != Block::hash_payload(&b.payload)
                || b.exec.len() > MAX_EXEC_BYTES
                || b.parent != b.justify.block_id
                || b.justify.round >= b.round
            {
                return self.reject();
            }
            ids.push(id);
        }
        for (b, id) in blocks.into_iter().zip(ids).rev() {
            if !self.blocks.contains_key(&id) {
                self.link(Item::Blk(b), id, out);
            }
        }
    }
}

/// Smooth weighted round-robin (the "nginx" algorithm) over one full cycle.
/// Slots per validator: `max(1, round(stake * 32n / total))`.
fn weighted_schedule(set: &ValidatorSet) -> Vec<ValidatorIndex> {
    let n = set.len();
    let total = set.total_stake();
    let scale = 32 * n as u128;
    let slots: Vec<i128> = (0..n as u32)
        .map(|i| ((set.stake_of(i) * scale * 2 + total) / (2 * total)).max(1) as i128)
        .collect();
    let cycle: i128 = slots.iter().sum();
    let mut credit = vec![0i128; n];
    let mut out = Vec::with_capacity(cycle as usize);
    for _ in 0..cycle {
        let mut best = 0;
        for i in 0..n {
            credit[i] += slots[i];
            if credit[i] > credit[best] {
                best = i;
            }
        }
        credit[best] -= cycle;
        out.push(best as u32);
    }
    out
}
