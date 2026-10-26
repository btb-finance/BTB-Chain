//! The driver: one thread that owns the consensus `Replica`, the chain state, the mempool and
//! the executor, and wires them together.
//!
//! Inputs (network frames, RPC calls, a timer tick) arrive on one channel and are handled one
//! at a time, so nothing in here needs locks and execution order is exactly commit order.
//!
//! Block flow. The leader asks the mempool for transactions, packs them into a `Batch` payload
//! and hands it to `Replica::propose`. Replicas check the payload before voting
//! (`wire::payload_ok`). When consensus commits a block, the driver turns its payload into a
//! `btb_chain::Block` on top of the current chain head and executes it. Every committed
//! consensus block yields exactly one chain block, empty ones included (crons run per block).
//!
//! Pacing. The replica proposes only when told to (`set_hold_empty_proposals`). The driver
//! proposes at once when there are transactions, or when an earlier non-empty block still needs
//! the next block's certificate to become final, and otherwise at most every `idle_block_ms`.
//!
//! Besides consensus the driver does three more jobs: it files `SubmitEvidence` transactions
//! for double signing it catches (`queue_evidence`), it keeps the block history
//! (`blockstore`), and it serves and uses block sync (`serve_sync`, `apply_sync`) so a node
//! that is arbitrarily far behind can catch up from its peers' history.

use crate::actions::ActionNode;
use crate::attest;
use crate::blockstore::{BlockStore, StoredBlock, StoredReceipt};
use crate::config::NodeParams;
use crate::cross::CrossNode;
use crate::explorer::tx_json;
use crate::hex;
use crate::net::NetHandle;
use crate::oracle::{OracleMsg, OracleNode};
use crate::reports::ReportNode;
use crate::storage::{ExecLog, ExecMeta, FileSafety};
use crate::sync::{self, Expect, SyncError};
use crate::wire::{
    consensus_frame, payload_ok, sync_request_frame, sync_response_frame, tx_frame, Batch, Inbound,
    SyncResponse, MAX_PAYLOAD,
};
use btb_chain::{
    account_address, Address, Block as ChainBlock, ChainConfig, ChainState, Executor, Mempool,
    MempoolConfig, OracleInput, Receipt, Status, Transaction, TxBody, TxKind,
};
use btb_consensus::{
    Action, Anchor, Block as ConsensusBlock, Config as ConsensusConfig, Context, Evidence, Hash,
    LeaderPolicy, MemoryStorage, Message, QuorumCert, Replica, SafetyStorage,
};
use btb_runtime::KvStore;
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod glue;

/// Heights of history an in-memory node keeps for the API and for serving block sync (a node
/// with a RocksDB data directory keeps all of it).
const KEEP_BLOCKS: usize = 4096;
/// No commit for this long makes a node ask its peers for blocks.
const SYNC_STALL: Duration = Duration::from_millis(1_000);
/// Give up on a peer that has not answered a block request within this time.
const SYNC_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Minimum spacing of block requests while stalled.
const SYNC_GAP: Duration = Duration::from_millis(300);
/// Consensus blocks of one epoch a response is built from (plus up to 2 proof blocks, and the
/// epoch's closing proof of up to 3).
const SYNC_RANGE: usize = 16;
/// Epoch-tail blocks remembered while waiting for the certificate that closes an epoch.
const MAX_TAILS: usize = 64;
/// Validators file the same evidence one after the other, this far apart.
const EVIDENCE_STAGGER: Duration = Duration::from_millis(150);
/// A reported offence that is still unslashed after this long is given up on by this node.
const EVIDENCE_GIVE_UP: Duration = Duration::from_secs(120);
/// A `SubmitEvidence` seen from another node holds back our own for this long.
const EVIDENCE_SEEN_HOLD: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn invalid_params(m: impl Into<String>) -> Self {
        RpcError {
            code: -32602,
            message: m.into(),
        }
    }
    pub fn app(m: impl Into<String>) -> Self {
        RpcError {
            code: -32000,
            message: m.into(),
        }
    }
}

pub type RpcResult = Result<Value, RpcError>;

/// Work for the driver thread that needs the chain state (see `Node::with_chain`). The state
/// is passed as `&mut dyn Any` because `Input` is not generic over the store; the closure
/// downcasts to the `ChainState<S>` it knows.
pub type StateJob = Box<dyn FnOnce(&mut dyn std::any::Any) + Send>;

/// Called on the driver thread after every executed block, with the chain state and the stored
/// block (header, consensus block, receipts, state root). A hosting node refreshes its
/// subscriptions and indexes here.
pub type BlockHook = Box<dyn FnMut(&mut dyn std::any::Any, &StoredBlock) + Send>;

pub enum Input {
    Net([u8; 32], Inbound),
    Rpc {
        method: String,
        params: Value,
        reply: tokio::sync::oneshot::Sender<RpcResult>,
    },
    /// Run a closure on the driver thread against the chain state.
    Job(StateJob),
    Shutdown,
}

#[derive(Default)]
struct SyncState {
    /// Which peer to ask next (index into the sorted peer list); moves on after a failure.
    cursor: usize,
    inflight: Option<(Instant, [u8; 32])>,
    last_req: Option<Instant>,
    requests: u64,
    blocks: u64,
    served: u64,
    rejected: u64,
    unsolicited: u64,
}

struct PendingEvidence {
    epoch: u64,
    ev: Evidence,
    due: Instant,
    give_up: Instant,
}

/// Ended consensus instances kept to help laggards finish their epoch.
const KEEP_ENDED_EPOCHS: usize = 8;

struct Driver<S: KvStore + 'static> {
    /// The running consensus instance of `epoch` (None if this node is not in the set).
    replica: Option<Replica>,
    epoch: u64,
    /// Validator keys of the current epoch, by consensus index.
    keys: Vec<[u8; 32]>,
    /// Finished epochs: their replicas (ended) and key lists.
    ended: BTreeMap<u64, (Replica, Vec<[u8; 32]>)>,
    signing: SigningKey,
    data_dir: Option<PathBuf>,
    base_timeout: u64,
    sync_retry: u64,
    epoch_tick_base: u64,
    state: ChainState<S>,
    executor: Executor,
    mempool: Mempool,
    net: NetHandle,
    exec_log: ExecLog,
    chain_id: u64,
    started: Instant,
    tick: Duration,
    idle_block: Duration,
    inflight_ttl: Duration,

    store: BlockStore,
    /// What this node executed recently, for the execution attestation hooks.
    exec_win: Rc<RefCell<attest::Window>>,
    sync: SyncState,
    /// Blocks just above an epoch's last height seen in proposals and sync answers, by epoch:
    /// candidates for the certificate that finalizes the epoch's last block.
    tails: Vec<(u64, ConsensusBlock)>,
    /// Last blocks of epochs whose closing proof is not stored yet: (epoch, block).
    proofs_wanted: Vec<(u64, ConsensusBlock)>,
    reporter: SigningKey,
    reporter_pending: Vec<Hash>,
    evidence_queue: Vec<PendingEvidence>,
    /// (epoch, offender index) pairs already queued: one report per offender and epoch.
    evidence_handled: HashSet<(u64, u32)>,
    /// Verified `SubmitEvidence` transactions seen from others, by (epoch, offender index).
    evidence_seen: HashMap<(u64, u32), Instant>,
    evidence_submitted: Vec<String>,
    submitted: HashMap<Hash, Instant>,
    latencies: VecDeque<u64>,
    /// (sender, nonce) of transactions seen in proposals that are not final yet.
    inflight: HashMap<(Address, u64), Instant>,
    max_nonempty_round: u64,
    proposed_for_round: u64,
    last_progress: Instant,
    last_commit_at: Option<Instant>,
    evidence: Vec<String>,
    fatal: Option<String>,
    hook: Option<BlockHook>,
    /// Stall time before asking for blocks, and the spacing of requests (`NodeParams::sync_poll_ms`).
    sync_stall: Duration,
    sync_gap: Duration,
    /// Beacon partials and price observations (`None`: the chain has no oracle config).
    oracle: Option<OracleNode>,
    /// Committed blocks waiting for their beacon before they can be executed (oracle chains only).
    pending_exec: VecDeque<btb_consensus::Block>,
    /// Blocks are being executed from a block sync response (not live consensus).
    applying_sync: bool,
    /// HTTPS actions: fetches in flight and the validators' votes (`crate::actions`).
    actions: ActionNode,
    /// Signed reports for other chains (`crate::reports`).
    reports: ReportNode,
    /// Cross-chain keys, signing and vault duties (`crate::cross`); chains with oracles only.
    cross: Option<CrossNode>,
    /// Next nonce for transactions signed by the consensus key (DKG and cross-chain messages).
    ckey_nonce: glue::CkeyNonce,
}

/// `(sender, nonce)` of every transaction in the given block payloads.
fn payload_nonces(payloads: &[&[u8]]) -> HashSet<(Address, u64)> {
    let mut out = HashSet::new();
    for p in payloads {
        for tx in Batch::decode(p).map(|b| b.txs).unwrap_or_default() {
            out.insert((tx.sender(), tx.body.nonce));
        }
    }
    out
}

/// Mempool transactions for the next block, skipping every `(sender, nonce)` in `taken`
/// (already in a pending block) and so continuing each sender at its first free nonce.
fn select_txs(
    mempool: &Mempool,
    state_nonce: impl Fn(&Address) -> u64,
    taken: &HashSet<(Address, u64)>,
    cfg: &ChainConfig,
) -> Vec<Transaction> {
    mempool.select(
        |a| {
            let mut n = state_nonce(a);
            while taken.contains(&(*a, n)) {
                n += 1;
            }
            n
        },
        cfg.max_block_txs as usize,
        cfg.block_gas_limit,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Run the driver loop on the current thread until `Shutdown`. `ready` gets the startup result.
pub fn run<S: KvStore + 'static>(
    params: NodeParams,
    state: ChainState<S>,
    rx: Receiver<Input>,
    net: NetHandle,
    ready: Sender<Result<(), String>>,
    hook: Option<BlockHook>,
) {
    match Driver::new(&params, state, net) {
        Ok(mut d) => {
            d.hook = hook;
            let _ = ready.send(Ok(()));
            d.main_loop(&rx);
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

impl<S: KvStore + 'static> Driver<S> {
    fn new(p: &NodeParams, state: ChainState<S>, net: NetHandle) -> Result<Self, String> {
        if state.config().chain_id != p.genesis.chain_id {
            return Err("chain state belongs to a different chain id than the genesis".into());
        }
        let t = p.timing;
        let tick_ms = t.tick_ms.max(1);
        let head_height = state.head().map_err(|e| e.to_string())?.height;
        let data = p.data_dir.clone();
        if let Some(d) = &data {
            std::fs::create_dir_all(d).map_err(|e| format!("create {}: {e}", d.display()))?;
        }
        let exec_log = ExecLog::open(data.as_deref(), [0; 32], head_height)?;
        let store = match &data {
            #[cfg(feature = "rocksdb")]
            Some(d) => BlockStore::disk(&d.join("blocks"))?,
            _ => BlockStore::memory(KEEP_BLOCKS as u64),
        };
        let chain_cfg = *state.config();
        let epoch = state.current_epoch().map_err(|e| e.to_string())?;
        let mut exec_win = attest::Window::load(
            &store,
            |h| {
                btb_runtime::merkle::root(state.store(), h)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("the chain state has no root for height {h}"))
            },
            exec_log.done().height,
        )?;
        if let Some(h) = p.test_exec_fault_from {
            exec_win.inject_fault(h);
        }
        let oracle = OracleNode::from_state(&state, p)?;
        let now = Instant::now();
        let mut d = Driver {
            replica: None,
            epoch,
            keys: Vec::new(),
            ended: BTreeMap::new(),
            signing: p.key.clone(),
            data_dir: data,
            base_timeout: (t.round_timeout_ms / tick_ms).max(2),
            sync_retry: (200 / tick_ms).max(2),
            epoch_tick_base: 0,
            state,
            executor: Executor::new(),
            mempool: Mempool::new(chain_cfg, MempoolConfig::default()),
            net,
            exec_log,
            chain_id: p.genesis.chain_id,
            started: now,
            tick: Duration::from_millis(tick_ms),
            idle_block: Duration::from_millis(t.idle_block_ms),
            inflight_ttl: Duration::from_millis(t.round_timeout_ms.saturating_mul(2)),
            store,
            exec_win: Rc::new(RefCell::new(exec_win)),
            sync: SyncState::default(),
            tails: Vec::new(),
            proofs_wanted: Vec::new(),
            reporter: p.reporter.clone().unwrap_or_else(|| p.key.clone()),
            reporter_pending: Vec::new(),
            evidence_queue: Vec::new(),
            evidence_handled: HashSet::new(),
            evidence_seen: HashMap::new(),
            evidence_submitted: Vec::new(),
            submitted: HashMap::new(),
            latencies: VecDeque::new(),
            inflight: HashMap::new(),
            max_nonempty_round: 0,
            proposed_for_round: 0,
            last_progress: now,
            last_commit_at: None,
            evidence: Vec::new(),
            fatal: None,
            hook: None,
            sync_stall: p.sync_poll_ms.map_or(SYNC_STALL, Duration::from_millis),
            sync_gap: p
                .sync_poll_ms
                .map_or(SYNC_GAP, |m| Duration::from_millis(m).min(SYNC_GAP)),
            oracle,
            pending_exec: VecDeque::new(),
            applying_sync: false,
            actions: ActionNode::new(p.genesis.chain_id, p.key.clone(), p.fetch_policy.clone()),
            reports: ReportNode::new(p.genesis.chain_id),
            cross: None,
            ckey_nonce: Default::default(),
        };
        if d.oracle.is_some() {
            d.cross = Some(CrossNode::new(
                p.genesis.chain_id,
                p.key.clone(),
                p.dkg_dir.as_ref().map(|d| d.join("cross")),
                p.key_passphrase.clone(),
                p.cross.clone(),
                p.observer.clone(),
            )?);
        }
        d.begin_epoch(epoch)?;
        d.oracle_after_block();
        Ok(d)
    }

    /// Start the consensus instance of `epoch` from the validator set the chain fixed for it.
    fn begin_epoch(&mut self, epoch: u64) -> Result<(), String> {
        let rec = self
            .state
            .epoch_record(epoch)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("chain has no validator set for epoch {epoch}"))?;
        let set = rec.consensus_set().map_err(|e| e.to_string())?;
        self.keys = rec.validators.iter().map(|v| v.consensus_key).collect();
        self.epoch = epoch;
        self.epoch_tick_base = self.ticks();
        self.max_nonempty_round = 0;
        self.proposed_for_round = 0;
        self.replica = None;
        let mut allowed = self.keys.clone();
        for (_, k) in self.ended.values() {
            allowed.extend(k.iter().copied());
        }
        allowed.sort();
        allowed.dedup();
        self.net.set_allowed(allowed);
        let me = self.signing.verifying_key().to_bytes();
        if !self.keys.contains(&me) {
            eprintln!("btb-node: not in the validator set of epoch {epoch}; observing only");
            return Ok(());
        }
        let anchor = (rec.start_height, rec.anchor);
        let cfg = ConsensusConfig {
            chain_id: self.state.consensus_chain_id(),
            base_timeout: self.base_timeout,
            max_payload_bytes: MAX_PAYLOAD,
            leader_policy: LeaderPolicy::StakeWeighted,
            sync_retry: self.sync_retry,
            epoch,
            end_height: (epoch + 1) * self.state.config().staking.epoch_len,
            ..ConsensusConfig::default()
        };
        let storage: Box<dyn SafetyStorage> = match &self.data_dir {
            Some(d) => {
                let fs = FileSafety::new(d, self.exec_log.done_handle(), epoch, anchor);
                fs.check()?;
                Box::new(fs)
            }
            None => Box::new(MemoryStorage::default()),
        };
        let mut replica = Replica::new_epoch(
            cfg,
            set,
            self.signing.clone(),
            Some(storage),
            Some(Anchor {
                height: anchor.0,
                id: anchor.1,
            }),
        )
        .map_err(|e| format!("replica for epoch {epoch}: {e}"))?;
        let chain_cfg = *self.state.config();
        replica.set_payload_check(Box::new(move |b| payload_ok(&b.payload, &chain_cfg)));
        let (provide, check) = attest::hooks(&self.exec_win);
        replica.set_exec_hooks(provide, check);
        replica.set_hold_empty_proposals(true);
        let acts = replica.start();
        self.replica = Some(replica);
        self.apply(acts, epoch);
        Ok(())
    }

    /// When the chain has moved past the running epoch (its last block was executed, by
    /// the replica's own commit or through block sync), start the instance of the chain's
    /// current epoch. A replica that finished its epoch stays as an ended instance to serve
    /// laggards; one that block sync overtook is dropped.
    fn check_epoch_switch(&mut self) {
        let Ok(head_epoch) = self.state.current_epoch() else {
            return;
        };
        if head_epoch <= self.epoch {
            return;
        }
        if let Some(r) = self.replica.take() {
            if r.epoch_ended() && head_epoch == self.epoch + 1 {
                let keys = std::mem::take(&mut self.keys);
                self.ended.insert(self.epoch, (r, keys));
                while self.ended.len() > KEEP_ENDED_EPOCHS {
                    self.ended.pop_first();
                }
            }
        }
        if let Err(e) = self.begin_epoch(head_epoch) {
            self.fatal = Some(e);
        }
    }

    fn ticks(&self) -> u64 {
        (self.started.elapsed().as_millis() / self.tick.as_millis().max(1)) as u64
    }

    fn main_loop(&mut self, rx: &Receiver<Input>) {
        let mut next_tick = Instant::now() + self.tick;
        loop {
            let wait = next_tick.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(Input::Shutdown) => return,
                Ok(i) => self.handle(i),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            for _ in 0..512 {
                match rx.try_recv() {
                    Ok(Input::Shutdown) => return,
                    Ok(i) => self.handle(i),
                    Err(_) => break,
                }
            }
            let now = Instant::now();
            if now >= next_tick {
                next_tick = (next_tick + self.tick).max(now);
                self.on_tick();
            }
            self.maybe_propose();
            if let Some(e) = self.fatal.take() {
                eprintln!("btb-node: FATAL: {e}; stopping the driver");
                return;
            }
        }
    }

    fn on_tick(&mut self) {
        let now_ticks = self.ticks().saturating_sub(self.epoch_tick_base);
        if let Some(r) = self.replica.as_mut() {
            let acts = r.on_timeout(now_ticks);
            let e = self.epoch;
            self.apply(acts, e);
        }
        self.check_epoch_switch();
        self.oracle_tick();
        self.action_tick();
        self.flush_evidence();
        if !self.proofs_wanted.is_empty() {
            self.resolve_proofs();
        }
        self.maybe_request_sync();
        let now = Instant::now();
        self.evidence_seen
            .retain(|_, t| now.duration_since(*t) < EVIDENCE_GIVE_UP);
        self.inflight
            .retain(|_, t| now.duration_since(*t) < self.inflight_ttl);
        if self.submitted.len() > 10_000 {
            self.submitted
                .retain(|_, t| now.duration_since(*t) < Duration::from_secs(120));
        }
    }

    fn handle(&mut self, i: Input) {
        let received = Instant::now();
        match i {
            Input::Net(from, Inbound::Consensus(epoch, msg)) => {
                let mut msg = *msg;
                if epoch == self.epoch || self.ended.contains_key(&epoch) {
                    self.note_tails(epoch, &msg);
                }
                let known = epoch == self.epoch || self.ended.contains_key(&epoch);
                let sender = if epoch == self.epoch {
                    self.keys.iter().position(|k| *k == from)
                } else {
                    self.ended
                        .get(&epoch)
                        .and_then(|(_, keys)| keys.iter().position(|k| *k == from))
                };
                if let (true, Some(idx), Message::SyncRequest { block_id, .. }) =
                    (known, sender, &msg)
                {
                    self.answer_block_request(epoch, idx as u32, *block_id);
                }
                if let (true, Some(idx), Message::Propose(p)) = (epoch == self.epoch, sender, &msg)
                {
                    self.remember_proposal(idx as u32, &p.block);
                }
                if epoch == self.epoch {
                    let Some(idx) = self.keys.iter().position(|k| *k == from) else {
                        return;
                    };
                    if let Message::SyncRequest { from, .. } = &mut msg {
                        *from = idx as u32;
                    }
                    if let Message::Propose(p) = &msg {
                        self.observe_payload(p.block.round, &p.block.payload);
                    }
                    if let Some(r) = self.replica.as_mut() {
                        let acts = r.on_message(msg);
                        self.apply(acts, epoch);
                    }
                    self.check_epoch_switch();
                } else if let Some((r, keys)) = self.ended.get_mut(&epoch) {
                    // An ended instance still serves laggards of that epoch.
                    let Some(idx) = keys.iter().position(|k| *k == from) else {
                        return;
                    };
                    if let Message::SyncRequest { from, .. } = &mut msg {
                        *from = idx as u32;
                    }
                    let acts = r.on_message(msg);
                    self.apply(acts, epoch);
                }
            }
            Input::Net(_, Inbound::Tx(bytes)) => {
                let _ = self.add_tx(&bytes, None);
            }
            Input::Net(from, Inbound::Oracle(b)) => self.on_oracle(from, &b),
            Input::Net(from, Inbound::SyncReq(h)) => self.serve_sync(from, h),
            Input::Net(from, Inbound::SyncResp(r)) => self.on_sync_response(from, *r),
            Input::Rpc {
                method,
                params,
                reply,
            } => {
                let r = self.rpc(&method, &params, received);
                let _ = reply.send(r);
            }
            Input::Job(job) => job(&mut self.state),
            Input::Shutdown => {}
        }
    }

    // -------------------------------------------------------------- consensus glue

    /// Remember which transactions a proposal carries and that a non-empty block is pending.
    fn observe_payload(&mut self, round: u64, payload: &[u8]) {
        self.last_progress = Instant::now();
        if payload.is_empty() {
            return;
        }
        self.max_nonempty_round = self.max_nonempty_round.max(round);
        if let Some(b) = Batch::decode(payload) {
            let now = Instant::now();
            for tx in &b.txs {
                self.inflight.insert((tx.sender(), tx.body.nonce), now);
                self.note_evidence_tx(tx);
            }
            if let Some(o) = self.oracle.as_mut() {
                // Observations already in a proposal need not be proposed again.
                o.prices.note_included(&b.observations);
            }
        }
    }

    /// Carry out what the replica of `epoch` asked for.
    fn apply(&mut self, actions: Vec<Action>, epoch: u64) {
        let current = epoch == self.epoch;
        let mut executed = false;
        for a in actions {
            match a {
                Action::Send { to, msg } => {
                    let key = if current {
                        self.keys.get(to as usize).copied()
                    } else {
                        self.ended
                            .get(&epoch)
                            .and_then(|(_, k)| k.get(to as usize).copied())
                    };
                    if let Some(k) = key {
                        self.net.send(k, consensus_frame(epoch, &msg));
                    }
                }
                Action::Broadcast(msg) => {
                    if let (true, Message::Propose(p)) = (current, &msg) {
                        self.observe_payload(p.block.round, &p.block.payload);
                        let me = self.index();
                        self.remember_proposal(me, &p.block);
                    }
                    self.note_tails(epoch, &msg);
                    self.net.broadcast(consensus_frame(epoch, &msg));
                }
                Action::Commit(block) => {
                    if current {
                        self.execute_commit(block);
                        executed = true;
                    }
                }
                Action::Evidence(e) => {
                    let what = match &e {
                        Evidence::DoubleVote { voter, round, .. } => {
                            format!("double vote by validator {voter} in round {round}")
                        }
                        Evidence::DoubleProposal {
                            proposer, round, ..
                        } => format!("double proposal by validator {proposer} in round {round}"),
                    };
                    if self.evidence.len() < 3 {
                        eprintln!("btb-node[{}]: evidence: {what}", self.index());
                    }
                    self.evidence.push(what);
                    self.queue_evidence(epoch, e);
                }
            }
        }
        if executed {
            self.resume_replica();
        }
    }

    /// The replica may have waited for execution (a vote on a block whose attestation names a
    /// height we had not executed yet, a proposal of our own that needs our execution result).
    /// Tell it that blocks were executed.
    fn resume_replica(&mut self) {
        if self.fatal.is_some() {
            return;
        }
        let epoch = self.epoch;
        if let Some(rep) = self.replica.as_mut() {
            let acts = rep.resume();
            if !acts.is_empty() {
                self.apply(acts, epoch);
            }
        }
    }

    fn maybe_propose(&mut self) {
        if self.fatal.is_some() {
            return;
        }
        let Some(rep) = self.replica.as_ref() else {
            return;
        };
        let me = self.index();
        let r = rep.round();
        let s = rep.safety_state();
        if rep.leader(r) != me
            || self.proposed_for_round >= r
            || s.last_proposed_round >= r
            || s.last_voted_round >= r
        {
            return;
        }
        let tip = rep.high_qc().block_id;
        let payload = self.select_payload(&tip);
        let tail = self.max_nonempty_round > s.committed_round;
        let idle = self.last_progress.elapsed() >= self.idle_block;
        if payload.is_empty() && !tail && !idle {
            return;
        }
        self.proposed_for_round = r;
        if let (Some(o), Some(b)) = (self.oracle.as_mut(), Batch::decode(&payload)) {
            // They are in our proposal now; a failed round makes validators observe again.
            o.prices.note_included(&b.observations);
        }
        let epoch = self.epoch;
        if let Some(rep) = self.replica.as_mut() {
            let acts = rep.propose(payload);
            self.apply(acts, epoch);
        }
        self.check_epoch_switch();
    }

    /// This node's validator index in the current epoch (u32::MAX if not a member).
    fn index(&self) -> u32 {
        let me = self.signing.verifying_key().to_bytes();
        self.keys
            .iter()
            .position(|k| *k == me)
            .map_or(u32::MAX, |i| i as u32)
    }

    /// Transactions for the next block (a child of consensus block `tip`), as an encoded
    /// `Batch` (empty bytes if none).
    ///
    /// Transactions in the uncommitted ancestors of `tip` can still commit, so they are never
    /// proposed again in this chain (the executor would skip the copy as `BadNonce`, and the
    /// copy's receipt would hide the real one). Transactions seen in other recent proposals
    /// are held back for `inflight_ttl`, but that is only a hint: a proposal can commit later.
    fn select_payload(&self, tip: &Hash) -> Vec<u8> {
        let mut taken: HashSet<(Address, u64)> = self.inflight.keys().copied().collect();
        if let Some(rep) = self.replica.as_ref() {
            taken.extend(payload_nonces(&rep.uncommitted_payloads(tip)));
        }
        let state = &self.state;
        let txs = select_txs(
            &self.mempool,
            |a| state.nonce(a).unwrap_or(0),
            &taken,
            state.config(),
        );
        let mut size = 64;
        let mut out = Vec::new();
        for tx in txs {
            let n = tx.encode().len() + 8;
            if size + n > MAX_PAYLOAD {
                break;
            }
            size += n;
            out.push(tx);
        }
        // Price observations the proposer collected ride along (and make a block worth
        // proposing even without transactions).
        let observations = self.oracle.as_ref().map_or_else(Vec::new, |o| {
            let head = self.exec_log.done().height;
            o.prices.for_proposal(head)
        });
        // Validators' answers to pending HTTPS actions ride along too, within the payload budget.
        let mut actions = Vec::new();
        for v in self.action_votes_for_proposal() {
            let n = btb_codec::Encode::encode(&v).len() + 8;
            if size + n > MAX_PAYLOAD {
                break;
            }
            size += n;
            actions.push(v);
        }
        if out.is_empty() && observations.is_empty() && actions.is_empty() {
            return Vec::new();
        }
        Batch {
            timestamp: now_ms(),
            txs: out,
            observations,
            actions,
        }
        .encode()
    }

    // -------------------------------------------------------------- execution

    /// A block was committed. Without oracles it is executed at once. With oracles it is queued
    /// and executed as soon as its beacon is known: the validators release their partial
    /// signatures for a height only now, after it is final, so the block's content cannot
    /// depend on the randomness it will see.
    fn execute_commit(&mut self, block: btb_consensus::Block) {
        if self.oracle.is_none() {
            return self.execute_now(block, None);
        }
        let last = self
            .pending_exec
            .back()
            .map_or(self.exec_log.done().height, |b| b.height);
        if block.height <= last {
            return; // re-delivered after a restart, or already queued
        }
        if block.height != last + 1 {
            self.fatal = Some(format!(
                "committed height {} does not follow height {last}",
                block.height
            ));
            return;
        }
        let height = block.height;
        self.pending_exec.push_back(block);
        let released = self
            .oracle
            .as_mut()
            .and_then(|o| o.beacons.on_commit(height));
        if let Some(m) = released {
            self.net.broadcast(m.frame());
        }
        self.drain_pending_exec();
    }

    /// Execute queued blocks whose beacon is available, in order. Returns whether any ran. A
    /// block of an epoch that has no beacon key yet (the first blocks of a chain that generates
    /// its key on chain) needs none and runs at once.
    fn drain_pending_exec(&mut self) -> bool {
        let mut ran = false;
        while self.fatal.is_none() {
            let Some(front) = self.pending_exec.front() else {
                break;
            };
            let h = front.height;
            let epoch = self.state.config().staking.epoch_of_block(h);
            let required =
                btb_chain::oracle::beacon_required(self.state.store(), epoch).unwrap_or(true);
            let beacon = if required {
                let found = self.oracle.as_ref().and_then(|o| o.beacons.get(h)).cloned();
                let Some(found) = found else {
                    if let Some(o) = self.oracle.as_mut() {
                        if o.waiting.map(|w| w.0) != Some(h) {
                            o.waiting = Some((h, Instant::now(), None));
                        }
                    }
                    break;
                };
                Some(found)
            } else {
                None
            };
            let block = self.pending_exec.pop_front().expect("front exists");
            self.execute_now(block, beacon);
            ran = true;
        }
        if self.pending_exec.is_empty() {
            if let Some(o) = self.oracle.as_mut() {
                o.waiting = None;
            }
        }
        ran
    }

    /// Execution that happens outside `apply` (a beacon arrived): the replica may have been
    /// waiting for it.
    fn after_oracle_execution(&mut self, ran: bool) {
        if ran {
            self.check_epoch_switch();
            self.resume_replica();
        }
    }

    fn execute_now(&mut self, block: btb_consensus::Block, beacon: Option<btb_oracle::Beacon>) {
        let done = self.exec_log.done();
        if block.height <= done.height {
            return; // re-delivered after a restart; already executed
        }
        if block.height != done.height + 1 {
            self.fatal = Some(format!(
                "committed height {} does not follow executed height {}",
                block.height, done.height
            ));
            return;
        }
        if let Err(e) = self.try_execute(block, beacon) {
            self.fatal = Some(e);
        }
    }

    fn try_execute(
        &mut self,
        block: btb_consensus::Block,
        beacon: Option<btb_oracle::Beacon>,
    ) -> Result<(), String> {
        // A committed block was certified by a quorum, and every honest voter checked its
        // attestation against its own execution. If ours differs, either this node computed
        // another state (a bug) or more than a third of the stake is faulty. Either way it
        // must not go on.
        self.exec_win
            .borrow()
            .check_executing(&block)
            .map_err(|e| {
                format!(
                    "state divergence at block {} (round {}): {e}",
                    block.height, block.round
                )
            })?;
        let head = self.state.head().map_err(|e| e.to_string())?;
        let root = self.state.root().map_err(|e| e.to_string())?;
        let (ts, txs, observations, action_votes) = match Batch::decode(&block.payload) {
            Some(b) => (
                b.timestamp.max(head.timestamp),
                b.txs,
                b.observations,
                b.actions,
            ),
            None => (head.timestamp, Vec::new(), Vec::new(), Vec::new()),
        };
        let meta = ExecMeta {
            height: block.height,
            round: block.round,
            id: block.id(),
        };
        self.exec_log.begin(meta, head.height)?;
        let chain_block = ChainBlock::new(head.height + 1, head.hash, ts, root, txs)
            .with_oracle(OracleInput {
                beacon: beacon.clone(),
                observations,
            })
            .with_actions(action_votes);
        let (chain_block, receipts, new_root) = match self
            .executor
            .execute_block(&mut self.state, &chain_block)
        {
            Ok((r, nr)) => (chain_block, r, nr),
            Err(e) => {
                // Every node fails identically (the state is untouched), so every node
                // falls back to an empty block and stays in step.
                eprintln!(
                    "btb-node[{}]: block rejected by the executor ({e:?}); executing it empty",
                    self.index()
                );
                let empty =
                    ChainBlock::new(head.height + 1, head.hash, head.timestamp, root, Vec::new())
                        .with_oracle(OracleInput {
                            beacon: beacon.clone(),
                            observations: Vec::new(),
                        });
                let (r, nr) = self
                    .executor
                    .execute_block(&mut self.state, &empty)
                    .map_err(|e| format!("empty block failed: {e:?}"))?;
                (empty, r, nr)
            }
        };
        // Record the attestation chain and the block before the executed height is settled: a
        // crash after the state commit then still finds this record on restart.
        let h = chain_block.header.height;
        let stored_receipts: Vec<StoredReceipt> = receipts.iter().map(stored_receipt).collect();
        let exec = self
            .exec_win
            .borrow_mut()
            .record(h, &new_root, &stored_receipts)?;
        let tx_ids: Vec<Hash> = chain_block.txs.iter().map(Transaction::id).collect();
        let stored = StoredBlock {
            header: chain_block.header,
            hash: chain_block.hash(),
            state_root_after: new_root,
            exec,
            consensus_round: block.round,
            tx_ids: tx_ids.clone(),
            receipts: stored_receipts,
            consensus: block,
        };
        self.store.put(&stored)?;
        self.exec_log.finish()?;
        if let Some(b) = &beacon {
            self.store.put_beacon(h, &b.encode())?;
        }
        if let Some(o) = self.oracle.as_mut() {
            o.beacons.prune(h);
            if let Some(b) = Batch::decode(&stored.consensus.payload) {
                o.prices.note_included(&b.observations);
            }
        }

        let now = Instant::now();
        self.last_progress = now;
        self.last_commit_at = Some(now);
        for id in &tx_ids {
            if let Some(t) = self.submitted.remove(id) {
                self.latencies
                    .push_back(now.duration_since(t).as_millis() as u64);
                if self.latencies.len() > 256 {
                    self.latencies.pop_front();
                }
            }
        }
        self.mempool.remove(&chain_block.txs);
        let state = &self.state;
        self.mempool.prune(|a| state.nonce(a).unwrap_or(0));
        self.inflight
            .retain(|(a, n), _| state.nonce(a).unwrap_or(0) <= *n);

        let epoch = self.state.config().staking.epoch_of_block(h);
        let is_epoch_end = h == self.state.config().staking.epoch_end(epoch);
        if let Some(hook) = self.hook.as_mut() {
            hook(&mut self.state, &stored);
        }
        if is_epoch_end {
            // The epoch's last block is final only together with the blocks that certify it;
            // keep that proof so block sync can serve this epoch to laggards.
            self.proofs_wanted.push((epoch, stored.consensus));
            self.resolve_proofs();
        }
        self.oracle_after_block();
        Ok(())
    }

    // -------------------------------------------------------------- oracles

    /// An oracle frame from `from`. Followers (non-validators) may only ask for beacons.
    fn on_oracle(&mut self, from: [u8; 32], bytes: &[u8]) {
        let Some(msg) = OracleMsg::decode(bytes) else {
            return;
        };
        if self.on_action_msg(from, &msg) {
            return;
        }
        let is_validator = self.keys.contains(&from);
        let Some(o) = self.oracle.as_mut() else {
            return;
        };
        let completed = match msg {
            OracleMsg::Want { height } => {
                let b = o.beacons.get(height).cloned().or_else(|| {
                    self.store
                        .beacon(height)
                        .and_then(|raw| btb_oracle::Beacon::decode(&raw))
                });
                if let Some(b) = b {
                    self.net.send(from, OracleMsg::Beacon(b).frame());
                }
                false
            }
            // A partial is authenticated against the key's holder list, which may include the
            // next epoch's validators before this node switched epochs.
            OracleMsg::Partial { height, index, sig } => {
                o.beacons.on_partial(&from, height, index, &sig).is_some()
            }
            _ if !is_validator => false,
            OracleMsg::Beacon(b) => o.beacons.on_beacon(b),
            OracleMsg::Price(ob) => {
                o.prices.on_observation(ob, &self.keys);
                false
            }
            // handled by `on_action_msg` above
            OracleMsg::Action(_)
            | OracleMsg::Cross(_)
            | OracleMsg::ReportPartial { .. }
            | OracleMsg::ReportSig { .. }
            | OracleMsg::ReportWant { .. } => false,
        };
        if completed {
            let ran = self.drain_pending_exec();
            self.after_oracle_execution(ran);
        }
    }

    /// After a block was executed: let the beacon key generation make progress (deal, complain,
    /// collect shares), install new epoch keys and release partials that were waiting for one.
    fn oracle_after_block(&mut self) {
        let me = self.index();
        let Some(o) = self.oracle.as_mut() else {
            return;
        };
        let Ok(head) = self.state.head() else {
            return;
        };
        let epoch = self.state.config().staking.epoch_of_block(head.height + 1);
        let ops = match o.dkg.step(self.state.store(), head.height + 1, epoch) {
            Ok(ops) => ops,
            Err(e) => {
                eprintln!("btb-node[{me}]: beacon key generation: {e}");
                Vec::new()
            }
        };
        if std::mem::take(&mut o.dkg.changed) {
            o.shares_changed = true;
        }
        o.sync_keys(&self.state, epoch);
        let released = o.beacons.flush();
        for m in released {
            self.net.broadcast(m.frame());
        }
        // While catching up on old blocks the windows are long past: send nothing.
        if !ops.is_empty() && !self.applying_sync {
            self.submit_dkg(ops);
        }
        self.cross_after_block();
    }

    /// Sign key generation operations with the consensus key and submit them.
    fn submit_dkg(&mut self, ops: Vec<btb_chain::DkgOp>) {
        let addr = account_address(&self.signing.verifying_key().to_bytes());
        let mempool = &self.mempool;
        if let Some(o) = self.oracle.as_mut() {
            o.dkg.pending.retain(|id| mempool.contains(id));
        }
        for op in ops {
            let nonce = self.next_ckey_nonce(&addr);
            let tx = Transaction::sign(
                TxBody {
                    relayer: None,
                    session: None,
                    chain_id: self.chain_id,
                    nonce,
                    kind: TxKind::Dkg(op),
                },
                &self.signing,
            );
            match self.add_tx(&tx.encode(), None) {
                Ok(id) => {
                    self.ckey_nonce.used(nonce);
                    if let Some(o) = self.oracle.as_mut() {
                        o.dkg.pending.push(id);
                    }
                }
                Err(e) => eprintln!(
                    "btb-node[{}]: could not submit a beacon key generation transaction: {e} \
                     (the consensus key's account must hold a little BTB for fees)",
                    self.index()
                ),
            }
        }
    }

    /// Periodic oracle work: observe prices, ask peers for a beacon that is late.
    fn oracle_tick(&mut self) {
        if self.oracle.is_none() {
            return;
        }
        let me_in_set = self.index() != u32::MAX;
        let head = self.exec_log.done().height;
        let now = Instant::now();
        let mut out = Vec::new();
        if let Some(o) = self.oracle.as_mut() {
            if me_in_set {
                for ob in o.prices.tick(now, head) {
                    out.push(OracleMsg::Price(ob).frame());
                }
            }
            if let Some((h, since, last)) = o.waiting {
                let due = now.duration_since(since) >= crate::oracle::WANT_AFTER
                    && last.is_none_or(|l| now.duration_since(l) >= crate::oracle::WANT_EVERY);
                if due {
                    o.waiting = Some((h, since, Some(now)));
                    out.push(OracleMsg::Want { height: h }.frame());
                }
            }
        }
        for f in out {
            self.net.broadcast(f);
        }
        // A beacon may have arrived through another path (the block store, a peer).
        let ran = self.drain_pending_exec();
        self.after_oracle_execution(ran);
    }

    // -------------------------------------------------------------- consensus block recovery

    /// Answer a consensus `SyncRequest` from the block store when it has the block. After a
    /// restart the replica holds no blocks, and if every validator restarted together nobody
    /// else would either.
    fn answer_block_request(&mut self, epoch: u64, to: u32, block_id: Hash) {
        let mut blocks = Vec::new();
        let mut cur = block_id;
        while blocks.len() < 64 {
            let Some(b) = self.store.block_by_id(&cur) else {
                break;
            };
            cur = b.parent;
            blocks.push(b);
        }
        if !blocks.is_empty() {
            self.apply(
                vec![Action::Send {
                    to,
                    msg: Message::SyncResponse { blocks },
                }],
                epoch,
            );
        }
    }

    /// Keep proposals from the round's leader so that the uncommitted tip survives a restart
    /// of all validators at once.
    fn remember_proposal(&mut self, from: u32, b: &ConsensusBlock) {
        let leader = self.replica.as_ref().map(|r| r.leader(b.round));
        if leader == Some(from) && b.proposer == from && b.height > self.exec_log.done().height {
            if let Err(e) = self.store.put_observed(b) {
                self.fatal = Some(e);
            }
        }
    }

    // -------------------------------------------------------------- epoch closing proofs

    /// Remember blocks just above the end of `epoch` (they only exist to certify its last block).
    fn note_tails(&mut self, epoch: u64, msg: &Message) {
        let end = self.state.config().staking.epoch_end(epoch);
        let mut add = |b: &ConsensusBlock| {
            if (b.height == end + 1 || b.height == end + 2)
                && !self
                    .tails
                    .iter()
                    .any(|(e, t)| *e == epoch && t.id() == b.id())
            {
                self.tails.push((epoch, b.clone()));
                if self.tails.len() > MAX_TAILS {
                    self.tails.remove(0);
                }
            }
        };
        match msg {
            Message::Propose(p) => add(&p.block),
            Message::SyncResponse { blocks } => blocks.iter().for_each(add),
            _ => {}
        }
    }

    /// The consensus context of `epoch` rebuilt from the chain state, with the epoch's anchor
    /// and start height.
    fn epoch_context(&self, epoch: u64) -> Option<(Context, Hash, u64)> {
        let rec = self.state.epoch_record(epoch).ok()??;
        let set = rec.consensus_set().ok()?;
        Some((
            Context::for_epoch(
                &self.state.consensus_chain_id(),
                epoch,
                set,
                Some(rec.anchor),
            ),
            rec.anchor,
            rec.start_height,
        ))
    }

    /// Store the closing proof of every epoch whose last block was executed, as far as the
    /// certificates are known (they arrive a moment after the block commits).
    fn resolve_proofs(&mut self) {
        for (epoch, last) in std::mem::take(&mut self.proofs_wanted) {
            if self.store.proof(epoch).is_some() {
                continue;
            }
            match self.build_proof(epoch, &last) {
                Some(p) => {
                    if let Err(e) = self.store.put_proof(&p) {
                        self.fatal = Some(e);
                    }
                }
                None => self.proofs_wanted.push((epoch, last)),
            }
        }
        while self.proofs_wanted.len() > 8 {
            self.proofs_wanted.remove(0);
        }
    }

    /// Blocks after `last` (the last block of `epoch`) and the certificate that, with `last`,
    /// satisfies the same check a syncing node runs.
    fn build_proof(&self, epoch: u64, last: &ConsensusBlock) -> Option<SyncResponse> {
        let (ctx, _, _) = self.epoch_context(epoch)?;
        let replica = if epoch == self.epoch {
            self.replica.as_ref()
        } else {
            self.ended.get(&epoch).map(|(r, _)| r)
        };
        let mut qcs: Vec<QuorumCert> = replica.map(|r| r.high_qc().clone()).into_iter().collect();
        qcs.extend(
            self.tails
                .iter()
                .filter(|(e, _)| *e == epoch)
                .map(|(_, t)| t.justify.clone()),
        );
        let last_id = last.id();
        for qc in qcs.into_iter().filter(|q| q.round > 0) {
            // Walk from the certified block back to `last` through remembered tail blocks.
            let mut path = Vec::new();
            let mut cur = qc.block_id;
            while cur != last_id && path.len() < 3 {
                let Some((_, t)) = self
                    .tails
                    .iter()
                    .find(|(e, t)| *e == epoch && t.id() == cur)
                else {
                    break;
                };
                path.push(t.clone());
                cur = t.parent;
            }
            if cur != last_id || path.is_empty() {
                continue;
            }
            path.reverse();
            let mut blocks = vec![last.clone()];
            blocks.extend(path.iter().cloned());
            let candidate = SyncResponse {
                epoch,
                blocks,
                qc: Some(qc.clone()),
            };
            let expect = Expect {
                ctx: &ctx,
                epoch,
                first_height: last.height,
                parent: last.parent,
                end_height: last.height,
            };
            if sync::verify(&candidate, &expect).is_ok() {
                return Some(SyncResponse {
                    epoch,
                    blocks: path,
                    qc: Some(qc),
                });
            }
        }
        None
    }

    // -------------------------------------------------------------- block sync

    /// Answer a peer's request for committed blocks starting at `from_height`. An empty answer
    /// means "nothing I can prove from there".
    fn serve_sync(&mut self, to: [u8; 32], from_height: u64) {
        self.sync.served += 1;
        let resp = self.build_sync_response(from_height).unwrap_or_default();
        self.net.send(to, sync_response_frame(&resp));
    }

    fn build_sync_response(&self, from: u64) -> Option<SyncResponse> {
        if from == 0 || from > self.store.latest() {
            return None;
        }
        let cfg = self.state.config().staking;
        let epoch = cfg.epoch_of_block(from);
        let end = cfg.epoch_end(epoch);
        let mut blocks = Vec::new();
        let mut h = from;
        while h <= end && blocks.len() < SYNC_RANGE {
            let Some(b) = self.store.get(h) else { break };
            blocks.push(b.consensus);
            h += 1;
        }
        if blocks.is_empty() {
            return None;
        }
        // Blocks after the range prove that the range is committed (2-chain rule).
        for _ in 0..2 {
            if h > end {
                break;
            }
            let Some(b) = self.store.get(h) else { break };
            blocks.push(b.consensus);
            h += 1;
        }
        let mut qc = None;
        if blocks.last().is_some_and(|b| b.height == end) {
            if let Some(p) = self.store.proof(epoch) {
                blocks.extend(p.blocks);
                qc = p.qc;
            }
        }
        Some(SyncResponse { epoch, blocks, qc })
    }

    /// Peers worth asking: validators of the current and the kept ended epochs, except us.
    fn sync_peers(&self) -> Vec<[u8; 32]> {
        let me = self.signing.verifying_key().to_bytes();
        let mut peers: Vec<[u8; 32]> = self.keys.clone();
        for (_, k) in self.ended.values() {
            peers.extend(k.iter().copied());
        }
        peers.retain(|k| *k != me);
        peers.sort();
        peers.dedup();
        peers
    }

    fn maybe_request_sync(&mut self) {
        if self.fatal.is_some() {
            return;
        }
        let since = self.last_commit_at.unwrap_or(self.started);
        if since.elapsed() < self.sync_stall {
            return;
        }
        let now = Instant::now();
        if let Some((t, _)) = self.sync.inflight {
            if now.duration_since(t) < SYNC_TIMEOUT {
                return;
            }
            self.sync.inflight = None;
            self.sync.cursor += 1;
        }
        if self
            .sync
            .last_req
            .is_some_and(|t| now.duration_since(t) < self.sync_gap)
        {
            return;
        }
        self.send_sync_request();
    }

    fn send_sync_request(&mut self) {
        let peers = self.sync_peers();
        if peers.is_empty() {
            return;
        }
        let peer = peers[self.sync.cursor % peers.len()];
        let now = Instant::now();
        self.sync.inflight = Some((now, peer));
        self.sync.last_req = Some(now);
        self.sync.requests += 1;
        let from = self.exec_log.done().height + 1;
        self.net.send(peer, sync_request_frame(from));
    }

    fn on_sync_response(&mut self, from: [u8; 32], r: SyncResponse) {
        // Only the peer we asked, once; anything else is ignored without looking at it.
        if !matches!(self.sync.inflight, Some((_, p)) if p == from) {
            self.sync.unsolicited += 1;
            return;
        }
        self.sync.inflight = None;
        match self.apply_sync(&r) {
            Ok(n) => {
                self.sync.blocks += n;
                // More may be waiting at the same peer.
                self.send_sync_request();
            }
            Err(SyncError::Empty | SyncError::NoProof) => self.sync.cursor += 1,
            Err(e) => {
                self.sync.rejected += 1;
                self.sync.cursor += 1;
                eprintln!(
                    "btb-node[{}]: rejected a block sync response: {e:?}",
                    self.index()
                );
            }
        }
    }

    /// Verify `r` against the validator set this node derived from its own executed chain, and
    /// execute the blocks it proves. Returns how many blocks were executed.
    fn apply_sync(&mut self, r: &SyncResponse) -> Result<u64, SyncError> {
        let epoch = self.epoch;
        let (ctx, anchor, start) = self.epoch_context(epoch).ok_or(SyncError::WrongEpoch)?;
        let done = self.exec_log.done();
        let end = self.state.config().staking.epoch_end(epoch);
        let expect = Expect {
            ctx: &ctx,
            epoch,
            first_height: done.height + 1,
            parent: if done.height <= start {
                anchor
            } else {
                done.id
            },
            end_height: end,
        };
        let n = sync::verify(r, &expect)?;
        self.applying_sync = true;
        for b in r.blocks.iter().take(n) {
            self.execute_commit(b.clone());
            if self.fatal.is_some() {
                // The driver stops; report it as a bad start rather than panicking.
                self.applying_sync = false;
                return Err(SyncError::BadStart);
            }
        }
        self.applying_sync = false;
        // The response may carry the proof that closes the epoch: keep it for others.
        let last = &r.blocks[n - 1];
        if last.height == end && r.blocks.len() > n && self.store.proof(epoch).is_none() {
            let candidate = SyncResponse {
                epoch,
                blocks: r.blocks[n - 1..].to_vec(),
                qc: r.qc.clone(),
            };
            let e = Expect {
                ctx: &ctx,
                epoch,
                first_height: end,
                parent: last.parent,
                end_height: end,
            };
            if sync::verify(&candidate, &e).is_ok() {
                let proof = SyncResponse {
                    epoch,
                    blocks: r.blocks[n..].to_vec(),
                    qc: r.qc.clone(),
                };
                if let Err(e) = self.store.put_proof(&proof) {
                    self.fatal = Some(e);
                }
            }
        }
        self.check_epoch_switch();
        self.resume_replica();
        Ok(n as u64)
    }

    // -------------------------------------------------------------- evidence

    /// A verified double-signing report: queue one `SubmitEvidence` per offender and epoch.
    fn queue_evidence(&mut self, epoch: u64, ev: Evidence) {
        let off = ev.offender();
        let me = self.index();
        let keys_len = if epoch == self.epoch {
            self.keys.len()
        } else {
            self.ended.get(&epoch).map_or(0, |(_, k)| k.len())
        };
        if me == u32::MAX || keys_len == 0 || !self.evidence_handled.insert((epoch, off)) {
            return;
        }
        // Validators report in turn, starting after the offender, so one transaction usually
        // does the job and the others see it and stand down.
        let n = keys_len as u32;
        let rank = (me + n - off % n) % n;
        if rank == 0 {
            return; // never report oneself
        }
        let now = Instant::now();
        self.evidence_queue.push(PendingEvidence {
            epoch,
            ev,
            due: now + EVIDENCE_STAGGER * (rank - 1),
            give_up: now + EVIDENCE_GIVE_UP,
        });
    }

    /// Note a `SubmitEvidence` transaction from someone else, if its evidence checks out.
    fn note_evidence_tx(&mut self, tx: &Transaction) {
        let TxKind::SubmitEvidence { epoch, evidence } = &tx.body.kind else {
            return;
        };
        let key = (*epoch, evidence.offender());
        if self.evidence_seen.contains_key(&key) {
            return;
        }
        if let Some((ctx, _, _)) = self.epoch_context(*epoch) {
            if evidence.verify(&ctx) {
                self.evidence_seen.insert(key, Instant::now());
            }
        }
    }

    fn flush_evidence(&mut self) {
        if self.evidence_queue.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut keep = Vec::new();
        for mut p in std::mem::take(&mut self.evidence_queue) {
            if p.due > now {
                keep.push(p);
                continue;
            }
            if now >= p.give_up {
                continue;
            }
            let off = p.ev.offender();
            let operator = self
                .state
                .epoch_record(p.epoch)
                .ok()
                .flatten()
                .and_then(|r| r.validators.get(off as usize).map(|v| v.operator));
            let Some(operator) = operator else { continue };
            if self
                .state
                .slashed_in_epoch(p.epoch, &operator)
                .unwrap_or(true)
            {
                continue; // already slashed (or unreadable): nothing to file
            }
            let seen = self
                .evidence_seen
                .get(&(p.epoch, off))
                .is_some_and(|t| now.duration_since(*t) < EVIDENCE_SEEN_HOLD);
            if seen {
                p.due = now + Duration::from_secs(1);
                keep.push(p);
                continue;
            }
            self.submit_evidence(p.epoch, p.ev);
        }
        self.evidence_queue = keep;
    }

    fn submit_evidence(&mut self, epoch: u64, ev: Evidence) {
        let addr = account_address(&self.reporter.verifying_key().to_bytes());
        let mempool = &self.mempool;
        self.reporter_pending.retain(|id| mempool.contains(id));
        let shared = addr == account_address(&self.signing.verifying_key().to_bytes());
        let nonce = if shared {
            // same account as the DKG and cross-chain messages: one allocator for all
            self.next_ckey_nonce(&addr)
        } else {
            self.state.nonce(&addr).unwrap_or(0) + self.reporter_pending.len() as u64
        };
        let off = ev.offender();
        let tx = Transaction::sign(
            TxBody {
                relayer: None,
                session: None,
                chain_id: self.chain_id,
                nonce,
                kind: TxKind::SubmitEvidence {
                    epoch,
                    evidence: ev,
                },
            },
            &self.reporter,
        );
        match self.add_tx(&tx.encode(), None) {
            Ok(id) => {
                if shared {
                    self.ckey_nonce.used(nonce);
                }
                self.reporter_pending.push(id);
                self.evidence_seen.insert((epoch, off), Instant::now());
                self.evidence_submitted.push(hex::encode(&id));
                eprintln!(
                    "btb-node[{}]: filed evidence against validator {off} (epoch {epoch}) in tx {}",
                    self.index(),
                    hex::encode(&id)
                );
            }
            Err(e) => eprintln!(
                "btb-node[{}]: could not file evidence against validator {off}: {e}",
                self.index()
            ),
        }
    }

    // -------------------------------------------------------------- transactions

    /// Admit a transaction; `local` is the RPC arrival time (None for gossip).
    fn add_tx(&mut self, bytes: &[u8], local: Option<Instant>) -> Result<Hash, String> {
        let tx = Transaction::decode(bytes).map_err(|e| format!("bad transaction: {e:?}"))?;
        let id = tx.id();
        if local.is_none() {
            self.note_evidence_tx(&tx);
        }
        if self.mempool.contains(&id) {
            return if local.is_some() {
                Ok(id)
            } else {
                Err("duplicate".into())
            };
        }
        if tx.body.session.is_some() {
            // The mempool is stateless; a session grant lives in state (`btb-chain::session`).
            self.state
                .check_session(&tx)
                .map_err(|e| format!("Invalid({e:?})"))?;
        }
        let sender = tx.sender();
        let nonce = self.state.nonce(&sender).map_err(|e| e.to_string())?;
        let balance = self.state.balance(&sender).map_err(|e| e.to_string())?;
        let frame = tx_frame(&tx);
        self.mempool
            .insert(tx, nonce, balance)
            .map_err(|e| format!("{e:?}"))?;
        if let Some(t) = local {
            self.submitted.insert(id, t);
        }
        self.net.broadcast(frame);
        Ok(id)
    }

    // -------------------------------------------------------------- RPC

    fn rpc(&mut self, method: &str, params: &Value, received: Instant) -> RpcResult {
        let arr: &[Value] = params.as_array().map_or(&[], Vec::as_slice);
        let arg = |i: usize| -> Result<&Value, RpcError> {
            arr.get(i)
                .ok_or_else(|| RpcError::invalid_params(format!("missing parameter {i}")))
        };
        let hex_arg = |i: usize| -> Result<Vec<u8>, RpcError> {
            hex::decode(arg(i)?.as_str().ok_or_else(|| {
                RpcError::invalid_params(format!("parameter {i} must be a hex string"))
            })?)
            .map_err(RpcError::invalid_params)
        };
        let h32 = |i: usize| -> Result<[u8; 32], RpcError> {
            hex_arg(i)?
                .try_into()
                .map_err(|_| RpcError::invalid_params(format!("parameter {i} must be 32 bytes")))
        };
        match method {
            "btb_sendTransaction" => {
                let bytes = hex_arg(0)?;
                let id = self.add_tx(&bytes, Some(received)).map_err(RpcError::app)?;
                Ok(json!({"id": hex::encode(&id)}))
            }
            "btb_getReceipt" => {
                let id = h32(0)?;
                Ok(match self.store.receipt(&id) {
                    Some((h, i, r)) => receipt_json(&r, h, i),
                    None => Value::Null,
                })
            }
            "btb_getBlock" => {
                let h = match arg(0)? {
                    Value::String(s) if s == "latest" => {
                        self.state
                            .head()
                            .map_err(|e| RpcError::app(e.to_string()))?
                            .height
                    }
                    v => v.as_u64().ok_or_else(|| {
                        RpcError::invalid_params("height must be a number or \"latest\"")
                    })?,
                };
                if self.store.bounded() && h != 0 && h < self.store.first() {
                    return Err(RpcError::app(format!(
                        "block {h} was pruned: this node keeps heights {}..={} in memory \
                         (run with RocksDB to keep all history)",
                        self.store.first(),
                        self.store.latest()
                    )));
                }
                match self.store.get(h) {
                    Some(b) => Ok(block_json(h, &b)),
                    None => Ok(Value::Null),
                }
            }
            "btb_getBalance" => {
                let a = h32(0)?;
                let bal = self
                    .state
                    .balance(&a)
                    .map_err(|e| RpcError::app(e.to_string()))?;
                let nonce = self
                    .state
                    .nonce(&a)
                    .map_err(|e| RpcError::app(e.to_string()))?;
                // Strings: u64 balances exceed what JSON numbers carry safely in many clients.
                Ok(json!({"balance": bal.to_string(), "nonce": nonce}))
            }
            "btb_query" => {
                let contract = h32(0)?;
                let function = arg(1)?
                    .as_str()
                    .ok_or_else(|| RpcError::invalid_params("function must be a string"))?
                    .to_string();
                let args = if arr.len() > 2 {
                    hex_arg(2)?
                } else {
                    Vec::new()
                };
                let caller = if arr.len() > 3 { h32(3)? } else { [0u8; 32] };
                let (out, gas) = self
                    .executor
                    .query(&mut self.state, &contract, &function, &args, &caller)
                    .map_err(|e| RpcError::app(format!("query failed: {e:?}")))?;
                Ok(json!({"output": hex::encode(&out), "gas_used": gas}))
            }
            "btb_upgradeStatus" => {
                // [contract]: the latest upgrade job of a contract (null if it never had one).
                let contract = h32(0)?;
                let st = self
                    .state
                    .upgrade_status(&contract)
                    .map_err(|e| RpcError::app(e.to_string()))?;
                Ok(match st {
                    None => Value::Null,
                    Some(st) => json!({
                        "phase": st.phase,
                        "error": st.error,
                        "upgrader": hex::encode(&st.upgrader),
                        "started": st.started,
                        "budget_gas": st.budget_gas,
                        "charged_gas": st.charged_gas,
                        "tables": st.tasks.iter().map(|t| json!({
                            "table": t.table,
                            "kind": t.kind,
                            "done": t.done,
                            "cursor": t.cursor,
                            "next_id": t.next_id,
                        })).collect::<Vec<_>>(),
                    }),
                })
            }
            "btb_getBlocks" => {
                // [before?, limit?]: newest first, heights strictly below `before`.
                let head = self
                    .state
                    .head()
                    .map_err(|e| RpcError::app(e.to_string()))?
                    .height;
                let before = match arr.first() {
                    None | Some(Value::Null) => head + 1,
                    Some(v) => v
                        .as_u64()
                        .ok_or_else(|| RpcError::invalid_params("before must be a number"))?,
                };
                let limit = match arr.get(1) {
                    None | Some(Value::Null) => 20,
                    Some(v) => v
                        .as_u64()
                        .ok_or_else(|| RpcError::invalid_params("limit must be a number"))?
                        .min(100),
                };
                let mut out = Vec::new();
                let mut h = before.min(head + 1);
                while h > 1 && (out.len() as u64) < limit {
                    h -= 1;
                    if let Some(b) = self.store.get(h) {
                        out.push(block_summary(&b));
                    } else if h < self.store.first() {
                        break;
                    }
                }
                Ok(json!({"head": head, "blocks": out}))
            }
            "btb_getTransaction" => {
                let id = h32(0)?;
                let Some((h, _, r)) = self.store.receipt(&id) else {
                    return Ok(Value::Null);
                };
                let Some(b) = self.store.get(h) else {
                    return Ok(Value::Null);
                };
                let index = b.tx_ids.iter().position(|t| *t == id);
                let tx = b.transactions().into_iter().find(|t| t.id() == id);
                let mut v = tx
                    .as_ref()
                    .map_or_else(|| json!({"id": hex::encode(&id)}), tx_json);
                v["height"] = json!(h);
                v["block_hash"] = json!(hex::encode(&b.hash));
                v["timestamp"] = json!(b.header.timestamp);
                v["index"] = json!(index);
                v["receipt"] = receipt_json(&r, h, index.unwrap_or(0));
                Ok(v)
            }
            "btb_getAccount" => {
                let a = h32(0)?;
                let e = |e: btb_runtime::KvError| RpcError::app(e.to_string());
                let validator = self
                    .state
                    .validator(&a)
                    .map_err(|e| RpcError::app(e.to_string()))?
                    .map(|v| {
                        json!({
                            "consensus_key": hex::encode(&v.consensus_key),
                            "jailed": v.jailed,
                            "bonded": v.bonded.to_string(),
                            "unbonding": v.unbonding.to_string(),
                        })
                    });
                Ok(json!({
                    "address": hex::encode(&a),
                    "balance": self.state.balance(&a).map_err(e)?.to_string(),
                    "nonce": self.state.nonce(&a).map_err(e)?,
                    "staked": self.state.staked(&a).map_err(e)?.to_string(),
                    "unbonding": self.state.unbonding(&a).map_err(e)?.to_string(),
                    "validator": validator,
                    "is_contract": self
                        .state
                        .contract(&a)
                        .map_err(|e| RpcError::app(e.to_string()))?
                        .is_some(),
                }))
            }
            "btb_getAccountTxs" => {
                // [address, limit?, before_height?]: newest first.
                let a = h32(0)?;
                let limit = match arr.get(1) {
                    None | Some(Value::Null) => 25,
                    Some(v) => v
                        .as_u64()
                        .ok_or_else(|| RpcError::invalid_params("limit must be a number"))?
                        .min(100),
                } as usize;
                let before = match arr.get(2) {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_u64()
                            .ok_or_else(|| RpcError::invalid_params("before must be a number"))?,
                    ),
                };
                let mut out = Vec::new();
                for (h, i) in self.store.account_txs(&a, before, limit) {
                    let Some(b) = self.store.get(h) else { continue };
                    let Some(tx) = b.transactions().into_iter().nth(i as usize) else {
                        continue;
                    };
                    let mut v = tx_json(&tx);
                    v["height"] = json!(h);
                    v["timestamp"] = json!(b.header.timestamp);
                    if let Some(r) = b.receipts.iter().find(|r| r.id == tx.id()) {
                        v["status"] = json!(r.status);
                        v["fee"] = json!(r.fee.to_string());
                        v["gas_used"] = json!(r.gas_used);
                    }
                    out.push(v);
                }
                Ok(json!({"txs": out}))
            }
            "btb_status" => self.status(),
            "btb_getEpoch" => {
                let e = match arr.first() {
                    Some(v) => v
                        .as_u64()
                        .ok_or_else(|| RpcError::invalid_params("epoch must be a number"))?,
                    None => self.epoch,
                };
                let rec = self
                    .state
                    .epoch_record(e)
                    .map_err(|e| RpcError::app(e.to_string()))?;
                Ok(match rec {
                    None => Value::Null,
                    Some(r) => json!({
                        "epoch": r.epoch,
                        "start_height": r.start_height,
                        "anchor": hex::encode(&r.anchor),
                        "end_height": (r.epoch + 1) * self.state.config().staking.epoch_len,
                        "validators": r.validators.iter().map(|v| json!({
                            "key": hex::encode(&v.consensus_key),
                            "operator": hex::encode(&v.operator),
                            "stake": v.stake.to_string(),
                            // Current state (a validator jailed after this epoch's set was fixed).
                            "jailed": self
                                .state
                                .validator(&v.operator)
                                .ok()
                                .flatten()
                                .map(|i| i.jailed),
                        })).collect::<Vec<_>>(),
                    }),
                })
            }
            "btb_actionRequests" => self.rpc_action_requests(),
            "btb_oracleReport" => self.rpc_report(arr),
            m if m.starts_with("btb_cross") || m == "btb_vault" => self.rpc_cross(m, arr),
            m if m.starts_with("btb_oracle") => {
                crate::oracle_rpc::handle(&self.state, &self.store, m, arr)
            }
            m if m.starts_with("btb_dex") => crate::dex_rpc::handle(&self.state, m, arr),
            m if m.starts_with("btb_storage") => crate::storage_rpc::handle(&self.state, m, arr),
            m if m.starts_with("btb_channel") => crate::channel_rpc::handle(&self.state, m, arr),
            _ => Err(RpcError {
                code: -32601,
                message: format!("method not found: {method}"),
            }),
        }
    }

    fn status(&self) -> RpcResult {
        let head = self
            .state
            .head()
            .map_err(|e| RpcError::app(e.to_string()))?;
        let root = self
            .state
            .root()
            .map_err(|e| RpcError::app(e.to_string()))?;
        let st = self
            .replica
            .as_ref()
            .map(|r| r.stats().clone())
            .unwrap_or_default();
        let lat: Vec<u64> = self.latencies.iter().copied().collect();
        let avg = if lat.is_empty() {
            Value::Null
        } else {
            json!(lat.iter().sum::<u64>() / lat.len() as u64)
        };
        let round = self.replica.as_ref().map_or(0, Replica::round);
        Ok(json!({
            "chain_id": self.chain_id,
            "validator_index": self.index(),
            "validators": self.keys.len(),
            "epoch": self.epoch,
            "height": head.height,
            "head_hash": hex::encode(&head.hash),
            "state_root": hex::encode(&root),
            "round": round,
            "leader": self.replica.as_ref().map(|r| r.leader(round)),
            "peers": self.net.connected(),
            "mempool": self.mempool.len(),
            "last_commit_latency_ms": self.latencies.back(),
            "avg_commit_latency_ms": avg,
            "commit_latencies_ms": lat,
            "last_commit_age_ms": self.last_commit_at.map(|t| t.elapsed().as_millis() as u64),
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "consensus": {
                "commits": st.commits,
                "votes_cast": st.votes_cast,
                "timeouts_sent": st.timeouts_sent,
                "proposals_made": st.proposals_made,
                "rejected_messages": st.rejected_messages,
                "evidence_found": st.evidence_found,
                "exec_rejected": st.exec_rejected,
                "exec_deferred": st.exec_deferred,
            },
            "oracle": self.oracle.as_ref().map(|o| json!({
                "holds_share": o.beacons.holds_share(),
                "blocks_waiting_for_beacon": self.pending_exec.len(),
                "rejected_partials": o.beacons.rejected,
                "dkg_deals_sent": o.dkg.deals_sent,
                "dkg_complaints_sent": o.dkg.complaints_sent,
                "pending_prices": o.prices.pending(),
            })),
            "cross": self.cross.as_ref().map(|c| json!({
                "deals_sent": c.stats.deals_sent,
                "complaints_sent": c.stats.complaints_sent,
                "signatures_made": c.stats.signatures_made,
                "nonces_used": c.stats.nonces_used,
                "attempts_abandoned": c.stats.attempts_abandoned,
                "evidence_sent": c.stats.evidence_sent,
                "attestations_sent": c.stats.attestations_sent,
                "proposals_sent": c.stats.proposals_sent,
                "in_flight": c.in_flight(),
                "last_error": c.last_error(),
            })),
            "evidence": self.evidence,
            "evidence_submitted": self.evidence_submitted,
            "history": {
                "first": self.store.first(),
                "latest": self.store.latest(),
                "pruned_in_memory": self.store.bounded(),
            },
            "sync": {
                "requests": self.sync.requests,
                "blocks_synced": self.sync.blocks,
                "served": self.sync.served,
                "rejected": self.sync.rejected,
                "unsolicited": self.sync.unsolicited,
            },
            "net": {
                "received": self.net.stats.received.load(std::sync::atomic::Ordering::Relaxed),
                "sent": self.net.stats.sent.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_malformed": self.net.stats.dropped_malformed.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_rate": self.net.stats.dropped_rate.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_banned": self.net.stats.dropped_banned.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_unknown_peer": self.net.stats.dropped_unknown_peer.load(std::sync::atomic::Ordering::Relaxed),
                "followers": self.net.stats.followers.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_follower": self.net.stats.dropped_follower.load(std::sync::atomic::Ordering::Relaxed),
            },
        }))
    }
}

fn stored_receipt(r: &Receipt) -> StoredReceipt {
    let (status, detail) = match &r.status {
        Status::Success => ("success", String::new()),
        Status::Reverted(x) => ("reverted", format!("{x:?}")),
        Status::Invalid(x) => ("invalid", format!("{x:?}")),
        Status::Paused => ("paused", String::new()),
    };
    StoredReceipt {
        id: r.id,
        status: status.into(),
        detail,
        gas_used: r.gas_used,
        fee: r.fee,
        output: r.output.clone(),
    }
}

fn receipt_json(r: &StoredReceipt, height: u64, index: usize) -> Value {
    json!({
        "id": hex::encode(&r.id),
        "height": height,
        "index": index,
        "status": r.status,
        "detail": r.detail,
        "gas_used": r.gas_used,
        "fee": r.fee.to_string(),
        "output": hex::encode(&r.output),
    })
}

fn block_summary(b: &StoredBlock) -> Value {
    json!({
        "height": b.header.height,
        "hash": hex::encode(&b.hash),
        "timestamp": b.header.timestamp,
        "tx_count": b.tx_ids.len(),
        "proposer": b.consensus.proposer,
        "gas_used": b.receipts.iter().map(|r| r.gas_used).sum::<u64>(),
        "fees": b.receipts.iter().map(|r| r.fee).sum::<u64>().to_string(),
    })
}

fn block_json(h: u64, b: &StoredBlock) -> Value {
    json!({
        "height": h,
        "hash": hex::encode(&b.hash),
        "parent_hash": hex::encode(&b.header.parent_hash),
        "timestamp": b.header.timestamp,
        "state_root_before": hex::encode(&b.header.state_root),
        "state_root_after": hex::encode(&b.state_root_after),
        "exec_digest": hex::encode(&b.exec.digest),
        "receipts_root": hex::encode(&b.exec.receipts_root),
        "attests_height": crate::attest::Claim::decode(&b.consensus.exec).map(|c| c.height),
        "tx_root": hex::encode(&b.header.tx_root),
        "consensus_round": b.consensus_round,
        "proposer": b.consensus.proposer,
        "gas_used": b.receipts.iter().map(|r| r.gas_used).sum::<u64>(),
        "fees": b.receipts.iter().map(|r| r.fee).sum::<u64>().to_string(),
        "transactions": b.tx_ids.iter().map(|i| hex::encode(i)).collect::<Vec<_>>(),
        "receipts": b.receipts.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use btb_chain::MempoolConfig;

    const CHAIN_ID: u64 = 7;

    fn transfers(key: &SigningKey, nonces: std::ops::Range<u64>) -> Vec<Transaction> {
        nonces
            .map(|nonce| {
                Transaction::sign(
                    TxBody {
                        relayer: None,
                        session: None,
                        chain_id: CHAIN_ID,
                        nonce,
                        kind: TxKind::Transfer {
                            to: [9; 32],
                            amount: 1,
                        },
                    },
                    key,
                )
            })
            .collect()
    }

    /// Regression: a transaction in an uncommitted block (for example one that gathered a QC
    /// but whose commit is still one round away) used to be proposed again once the
    /// `inflight` hint had expired after a few round timeouts. Both copies committed, the
    /// second failed with `BadNonce` and its receipt replaced the real one.
    #[test]
    fn transactions_of_uncommitted_ancestors_are_not_reproposed() {
        let key = SigningKey::from_bytes(&[5; 32]);
        let sender = account_address(&key.verifying_key().to_bytes());
        let cfg = ChainConfig::new(CHAIN_ID);
        let mut pool = Mempool::new(cfg, MempoolConfig::default());
        let txs = transfers(&key, 0..4);
        for t in &txs {
            pool.insert(t.clone(), 0, u64::MAX).unwrap();
        }
        // An ancestor block carries nonces 0 and 1; the state is still at nonce 0 and the
        // inflight hint has long expired (empty).
        let ancestor = Batch {
            timestamp: 1,
            txs: txs[..2].to_vec(),
            observations: Vec::new(),
            actions: Vec::new(),
        }
        .encode();
        let taken = payload_nonces(&[ancestor.as_slice()]);
        let picked = select_txs(&pool, |_| 0, &taken, &cfg);
        let nonces: Vec<u64> = picked.iter().map(|t| t.body.nonce).collect();
        assert_eq!(nonces, vec![2, 3]);
        assert!(picked.iter().all(|t| t.sender() == sender));
        // Without ancestors the same pool proposes from nonce 0 (the bug's behaviour).
        let none = select_txs(&pool, |_| 0, &HashSet::new(), &cfg);
        assert_eq!(none.len(), 4);
        // Already committed (state nonce 2) but not yet pruned from the pool: never selected.
        let committed = select_txs(&pool, |_| 2, &HashSet::new(), &cfg);
        let nonces: Vec<u64> = committed.iter().map(|t| t.body.nonce).collect();
        assert_eq!(nonces, vec![2, 3]);
        // An undecodable ancestor payload (an empty block) takes nothing.
        assert!(payload_nonces(&[&[][..]]).is_empty());
    }
}
