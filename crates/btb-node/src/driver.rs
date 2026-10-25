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

use crate::config::NodeParams;
use crate::hex;
use crate::net::NetHandle;
use crate::storage::{ExecLog, ExecMeta, FileSafety};
use crate::wire::{consensus_frame, payload_ok, tx_frame, Batch, Inbound, MAX_PAYLOAD};
use btb_chain::{
    Address, Block as ChainBlock, ChainState, Executor, Mempool, MempoolConfig, Receipt, Status,
    Transaction,
};
use btb_consensus::{
    Action, Anchor, Config as ConsensusConfig, Evidence, Hash, LeaderPolicy, MemoryStorage,
    Message, Replica, SafetyStorage,
};
use btb_runtime::KvStore;
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many recent blocks (and their receipts) the node keeps for the API.
const KEEP_BLOCKS: usize = 4096;

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

pub enum Input {
    Net([u8; 32], Inbound),
    Rpc {
        method: String,
        params: Value,
        reply: tokio::sync::oneshot::Sender<RpcResult>,
    },
    Shutdown,
}

struct StoredBlock {
    header: btb_chain::Header,
    hash: Hash,
    state_root_after: Hash,
    consensus_round: u64,
    tx_ids: Vec<Hash>,
    receipts: Vec<Receipt>,
}

/// Ended consensus instances kept to help laggards finish their epoch.
const KEEP_ENDED_EPOCHS: usize = 8;

struct Driver<S: KvStore> {
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

    blocks: BTreeMap<u64, StoredBlock>,
    receipt_index: HashMap<Hash, (u64, usize)>,
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
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Run the driver loop on the current thread until `Shutdown`. `ready` gets the startup result.
pub fn run<S: KvStore>(
    params: NodeParams,
    state: ChainState<S>,
    rx: Receiver<Input>,
    net: NetHandle,
    ready: Sender<Result<(), String>>,
) {
    match Driver::new(&params, state, net) {
        Ok(mut d) => {
            let _ = ready.send(Ok(()));
            d.main_loop(&rx);
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

impl<S: KvStore> Driver<S> {
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
        let chain_cfg = *state.config();
        let epoch = state.current_epoch().map_err(|e| e.to_string())?;
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
            blocks: BTreeMap::new(),
            receipt_index: HashMap::new(),
            submitted: HashMap::new(),
            latencies: VecDeque::new(),
            inflight: HashMap::new(),
            max_nonempty_round: 0,
            proposed_for_round: 0,
            last_progress: now,
            last_commit_at: None,
            evidence: Vec::new(),
            fatal: None,
        };
        d.begin_epoch(epoch)?;
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
        replica.set_hold_empty_proposals(true);
        let acts = replica.start();
        self.replica = Some(replica);
        self.apply(acts, epoch);
        Ok(())
    }

    /// After the last block of the epoch is executed, move to the next epoch's instance.
    fn check_epoch_switch(&mut self) {
        loop {
            let ended = self.replica.as_ref().is_some_and(Replica::epoch_ended);
            let head_epoch = self.state.current_epoch().unwrap_or(self.epoch);
            if !(ended && head_epoch > self.epoch) {
                return;
            }
            if let Some(r) = self.replica.take() {
                let keys = std::mem::take(&mut self.keys);
                self.ended.insert(self.epoch, (r, keys));
                while self.ended.len() > KEEP_ENDED_EPOCHS {
                    self.ended.pop_first();
                }
            }
            if let Err(e) = self.begin_epoch(self.epoch + 1) {
                self.fatal = Some(e);
                return;
            }
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
        let now = Instant::now();
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
            Input::Rpc {
                method,
                params,
                reply,
            } => {
                let r = self.rpc(&method, &params, received);
                let _ = reply.send(r);
            }
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
            }
        }
    }

    /// Carry out what the replica of `epoch` asked for.
    fn apply(&mut self, actions: Vec<Action>, epoch: u64) {
        let current = epoch == self.epoch;
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
                    }
                    self.net.broadcast(consensus_frame(epoch, &msg));
                }
                Action::Commit(block) => {
                    if current {
                        self.execute_commit(block)
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
                }
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
        let payload = self.select_payload();
        let tail = self.max_nonempty_round > s.committed_round;
        let idle = self.last_progress.elapsed() >= self.idle_block;
        if payload.is_empty() && !tail && !idle {
            return;
        }
        self.proposed_for_round = r;
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

    /// Transactions for the next block, as an encoded `Batch` (empty bytes if none).
    fn select_payload(&self) -> Vec<u8> {
        let cfg = *self.state.config();
        let inflight = &self.inflight;
        let state = &self.state;
        let txs = self.mempool.select(
            |a| {
                let mut n = state.nonce(a).unwrap_or(0);
                while inflight.contains_key(&(*a, n)) {
                    n += 1;
                }
                n
            },
            cfg.max_block_txs as usize,
            cfg.block_gas_limit,
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
        if out.is_empty() {
            return Vec::new();
        }
        Batch {
            timestamp: now_ms(),
            txs: out,
        }
        .encode()
    }

    // -------------------------------------------------------------- execution

    fn execute_commit(&mut self, block: btb_consensus::Block) {
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
        if let Err(e) = self.try_execute(block) {
            self.fatal = Some(e);
        }
    }

    fn try_execute(&mut self, block: btb_consensus::Block) -> Result<(), String> {
        let head = self.state.head().map_err(|e| e.to_string())?;
        let root = self.state.root().map_err(|e| e.to_string())?;
        let (ts, txs) = match Batch::decode(&block.payload) {
            Some(b) => (b.timestamp.max(head.timestamp), b.txs),
            None => (head.timestamp, Vec::new()),
        };
        let meta = ExecMeta {
            height: block.height,
            round: block.round,
            id: block.id(),
        };
        self.exec_log.begin(meta, head.height)?;
        let chain_block = ChainBlock::new(head.height + 1, head.hash, ts, root, txs);
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
                    ChainBlock::new(head.height + 1, head.hash, head.timestamp, root, Vec::new());
                let (r, nr) = self
                    .executor
                    .execute_block(&mut self.state, &empty)
                    .map_err(|e| format!("empty block failed: {e:?}"))?;
                (empty, r, nr)
            }
        };
        self.exec_log.finish()?;

        let now = Instant::now();
        self.last_progress = now;
        self.last_commit_at = Some(now);
        let tx_ids: Vec<Hash> = chain_block.txs.iter().map(Transaction::id).collect();
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

        let h = chain_block.header.height;
        for (i, r) in receipts.iter().enumerate() {
            self.receipt_index.insert(r.id, (h, i));
        }
        self.blocks.insert(
            h,
            StoredBlock {
                header: chain_block.header,
                hash: chain_block.hash(),
                state_root_after: new_root,
                consensus_round: block.round,
                tx_ids,
                receipts,
            },
        );
        while self.blocks.len() > KEEP_BLOCKS {
            if let Some((_, old)) = self.blocks.pop_first() {
                for r in &old.receipts {
                    self.receipt_index.remove(&r.id);
                }
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------- transactions

    /// Admit a transaction; `local` is the RPC arrival time (None for gossip).
    fn add_tx(&mut self, bytes: &[u8], local: Option<Instant>) -> Result<Hash, String> {
        let tx = Transaction::decode(bytes).map_err(|e| format!("bad transaction: {e:?}"))?;
        let id = tx.id();
        if self.mempool.contains(&id) {
            return if local.is_some() {
                Ok(id)
            } else {
                Err("duplicate".into())
            };
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
                match self.receipt_index.get(&id) {
                    Some((h, i)) => {
                        let b = &self.blocks[h];
                        Ok(receipt_json(&b.receipts[*i], *h, *i))
                    }
                    None => Ok(Value::Null),
                }
            }
            "btb_getBlock" => {
                let h = match arg(0)? {
                    Value::String(s) if s == "latest" => {
                        self.blocks.last_key_value().map(|(h, _)| *h).unwrap_or(0)
                    }
                    v => v.as_u64().ok_or_else(|| {
                        RpcError::invalid_params("height must be a number or \"latest\"")
                    })?,
                };
                match self.blocks.get(&h) {
                    Some(b) => Ok(block_json(h, b)),
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
                        })).collect::<Vec<_>>(),
                    }),
                })
            }
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
            },
            "evidence": self.evidence,
            "net": {
                "received": self.net.stats.received.load(std::sync::atomic::Ordering::Relaxed),
                "sent": self.net.stats.sent.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_malformed": self.net.stats.dropped_malformed.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_rate": self.net.stats.dropped_rate.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_banned": self.net.stats.dropped_banned.load(std::sync::atomic::Ordering::Relaxed),
                "dropped_unknown_peer": self.net.stats.dropped_unknown_peer.load(std::sync::atomic::Ordering::Relaxed),
            },
        }))
    }
}

fn receipt_json(r: &Receipt, height: u64, index: usize) -> Value {
    let (status, detail) = match &r.status {
        Status::Success => ("success", String::new()),
        Status::Reverted(x) => ("reverted", format!("{x:?}")),
        Status::Invalid(x) => ("invalid", format!("{x:?}")),
        Status::Paused => ("paused", String::new()),
    };
    json!({
        "id": hex::encode(&r.id),
        "height": height,
        "index": index,
        "status": status,
        "detail": detail,
        "gas_used": r.gas_used,
        "fee": r.fee.to_string(),
        "output": hex::encode(&r.output),
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
        "tx_root": hex::encode(&b.header.tx_root),
        "consensus_round": b.consensus_round,
        "transactions": b.tx_ids.iter().map(|i| hex::encode(i)).collect::<Vec<_>>(),
        "receipts": b.receipts.len(),
    })
}
