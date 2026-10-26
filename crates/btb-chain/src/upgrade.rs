//! Contract upgrades and schema migrations (`TxKind::Upgrade`, tag 23). See `docs/CHAIN.md`.
//!
//! An upgrade replaces the code of a deployed contract at the same address. Who may do it is
//! the contract's **upgrade authority**: the deployer by default, another account if the deploy
//! manifest names one, nobody if it says `none` (immutable). The chain reads the old and the new
//! schema from the two ELFs (`btb_runtime::evolve::diff`):
//!
//! * a compatible change (new table, appended `Option` fields, new or removed indexes) is
//!   accepted; new and removed indexes are **backfilled**;
//! * an incompatible change is accepted only with a declared `#[btb::migration]` (and the old row
//!   type matching the stored schema), else the transaction fails and nothing changes.
//!
//! Row work (migration, backfill) is a **job**, run by the chain at the end of every block in
//! bounded batches (`ROWS_PER_BLOCK` rows across all jobs), in row id order. The upgrader prepays
//! `budget_gas * gas_price` into an escrow; every row costs gas (see the constants), which moves
//! from the escrow to the validator pool; the rest is refunded when the job ends.
//!
//! **Consistency.** The new code is live at once, and each table the job rewrites is **locked**
//! (`layout::TAG_TABLE_LOCK`): while the lock exists every table host call on it by that
//! contract faults (`TableLocked`), reads included, so no call ever sees a mix of old and new
//! rows. Other tables, contract storage and balances keep working. A job over a small table
//! finishes in the block of the upgrade, so nobody notices.
//!
//! **Failure.** A migration that traps or produces a malformed row, an exhausted budget, a
//! duplicate in a new unique index, or a deposit the contract cannot pay all abort the job:
//! migrated rows are restored from an undo log (their old bytes, kept until the job succeeds),
//! the old code and manifest are reinstalled, the tables are unlocked and the unused escrow is
//! refunded. Rolling back is free for the upgrader (the validators pay for it with the work
//! already paid). If the restored rows need a deposit the contract cannot pay, the rollback
//! waits and retries each block.

use crate::exec::Ctx;
use crate::state::{
    contract_key, cron_key, read_u64, register_manifest, ContractRecord, StateError,
};
use crate::tx::{Address, Manifest, UpgradePolicy};
use crate::{sha256, validator_pool};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::evolve::{self, TaskKind};
use btb_runtime::kv::{KvStore, WriteBatch};
use btb_runtime::schema::{migration_entry, Schema};
use btb_runtime::{layout, RuntimeHost, Tables};
use btb_vm::Contract;

/// Upgrade jobs: `78 00 A` is the active marker (empty value), `78 01 A` the job record.
pub const TAG_UPGRADE_JOB: u8 = 0x78;
/// `79 A len(table) table id:u64be`: the row as it was before the migration.
pub const TAG_UPGRADE_UNDO: u8 = 0x79;

/// Gas of the `Upgrade` transaction itself, before the per-byte code cost.
pub const UPGRADE_BASE_GAS: u64 = 100_000;
/// Gas per migrated row, before the migration function's own VM gas.
pub const MIGRATE_ROW_GAS: u64 = 1_000;
/// Gas per row visited by an index backfill.
pub const BACKFILL_ROW_GAS: u64 = 300;
/// Gas per byte of the old row plus the new row of a migration.
pub const ROW_BYTE_GAS: u64 = 1;
/// Gas per index entry written or deleted.
pub const INDEX_ENTRY_GAS: u64 = 100;
/// Rows rewritten (forward or back) per block, across all jobs.
pub const ROWS_PER_BLOCK: usize = 32;
/// Undo entries deleted per block after a successful job.
pub const CLEANUP_PER_BLOCK: usize = 256;

/// Where upgrade budgets are held until the job ends.
pub fn upgrade_pool() -> Address {
    sha256(&[b"btb.pool.v1", b"upgrade-escrow"])
}

/// Why an upgrade was refused (the gas used is still charged, nothing else changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeError {
    NoSuchContract,
    /// The contract was deployed (or later set) with no upgrade authority.
    Immutable,
    /// The sender is not the upgrade authority.
    NotAuthorized,
    /// The previous upgrade's migration is still running (or rolling back).
    InProgress,
    BadContract(String),
    BadManifest(String),
    /// The schema change needs a migration that was not declared (or declared wrongly).
    Incompatible(String),
}

/// How far a job is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Rewriting rows; the tables are locked.
    Forward,
    /// Done; deleting the undo log.
    Cleanup,
    Done,
    /// Failed: restoring rows from the undo log.
    RollingBack(String),
    /// Failed and restored: the old code runs again.
    RolledBack(String),
}

impl Phase {
    fn tag(&self) -> u8 {
        match self {
            Phase::Forward => 0,
            Phase::Cleanup => 1,
            Phase::Done => 2,
            Phase::RollingBack(_) => 3,
            Phase::RolledBack(_) => 4,
        }
    }
    /// Does the job still have work to do?
    pub fn active(&self) -> bool {
        matches!(
            self,
            Phase::Forward | Phase::Cleanup | Phase::RollingBack(_)
        )
    }
    pub fn name(&self) -> &'static str {
        match self {
            Phase::Forward => "migrating",
            Phase::Cleanup => "cleanup",
            Phase::Done => "done",
            Phase::RollingBack(_) => "rolling_back",
            Phase::RolledBack(_) => "rolled_back",
        }
    }
}

/// One table's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    pub table: String,
    /// `Some((from, to))` for a migration; `None` for an index backfill.
    pub migrate: Option<(u32, u32)>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// The next row id to process.
    pub cursor: u64,
    /// Rows processed so far.
    pub done: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub upgrader: Address,
    pub prev_code: Vec<u8>,
    pub prev_manifest: Manifest,
    pub started: u64,
    pub budget: u64,
    /// Gas charged so far (`escrow` base units = `charged * gas_price`).
    pub charged: u64,
    pub tasks: Vec<TaskState>,
    /// Task being processed (forward).
    pub cur: u32,
    /// Rollback position: task, and next row id for a backfill.
    pub rb_task: u32,
    pub rb_next: u64,
    pub phase: Phase,
}

impl Job {
    fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.upgrader.encode_to(&mut o);
        self.prev_code.encode_to(&mut o);
        self.prev_manifest.encode_to(&mut o);
        (self.started, self.budget, self.charged).encode_to(&mut o);
        (self.cur, self.rb_task, self.rb_next).encode_to(&mut o);
        self.phase.tag().encode_to(&mut o);
        match &self.phase {
            Phase::RollingBack(m) | Phase::RolledBack(m) => m.encode_to(&mut o),
            _ => String::new().encode_to(&mut o),
        }
        (self.tasks.len() as u32).encode_to(&mut o);
        for t in &self.tasks {
            t.table.encode_to(&mut o);
            let (m, a, b) = match t.migrate {
                Some((a, b)) => (1u8, a, b),
                None => (0, 0, 0),
            };
            (m, a, b).encode_to(&mut o);
            t.added.encode_to(&mut o);
            t.removed.encode_to(&mut o);
            (t.cursor, t.done).encode_to(&mut o);
        }
        o
    }

    fn decode(b: &[u8]) -> Result<Job, DecodeError> {
        let mut r = Reader::new(b, 1 << 24);
        let upgrader = <[u8; 32]>::decode_from(&mut r)?;
        let prev_code = Vec::decode_from(&mut r)?;
        let prev_manifest = Manifest::decode_from(&mut r)?;
        let (started, budget, charged) = <(u64, u64, u64)>::decode_from(&mut r)?;
        let (cur, rb_task, rb_next) = <(u32, u32, u64)>::decode_from(&mut r)?;
        let tag = u8::decode_from(&mut r)?;
        let reason = String::decode_from(&mut r)?;
        let phase = match tag {
            0 => Phase::Forward,
            1 => Phase::Cleanup,
            2 => Phase::Done,
            3 => Phase::RollingBack(reason),
            4 => Phase::RolledBack(reason),
            _ => return Err(DecodeError::InvalidOptionTag(tag)),
        };
        let n = u32::decode_from(&mut r)?;
        let mut tasks = Vec::new();
        for _ in 0..n.min(256) {
            let table = String::decode_from(&mut r)?;
            let (m, a, c) = <(u8, u32, u32)>::decode_from(&mut r)?;
            tasks.push(TaskState {
                table,
                migrate: (m == 1).then_some((a, c)),
                added: Vec::decode_from(&mut r)?,
                removed: Vec::decode_from(&mut r)?,
                cursor: u64::decode_from(&mut r)?,
                done: u64::decode_from(&mut r)?,
            });
        }
        Ok(Job {
            upgrader,
            prev_code,
            prev_manifest,
            started,
            budget,
            charged,
            tasks,
            cur,
            rb_task,
            rb_next,
            phase,
        })
    }
}

fn job_key(c: &Address) -> Vec<u8> {
    let mut k = vec![TAG_UPGRADE_JOB, 1];
    k.extend_from_slice(c);
    k
}
fn active_key(c: &Address) -> Vec<u8> {
    let mut k = vec![TAG_UPGRADE_JOB, 0];
    k.extend_from_slice(c);
    k
}
fn undo_prefix(c: &Address, table: &str) -> Vec<u8> {
    let mut k = vec![TAG_UPGRADE_UNDO];
    k.extend_from_slice(c);
    k.extend_from_slice(&(table.len() as u32).to_be_bytes());
    k.extend_from_slice(table.as_bytes());
    k
}
fn undo_key(c: &Address, table: &str, id: u64) -> Vec<u8> {
    let mut k = undo_prefix(c, table);
    k.extend_from_slice(&id.to_be_bytes());
    k
}

fn load_job(kv: &impl KvStore, c: &Address) -> Result<Option<Job>, StateError> {
    match kv.get(&job_key(c))? {
        None => Ok(None),
        Some(b) => Ok(Some(Job::decode(&b).map_err(StateError::from)?)),
    }
}

fn save_job(kv: &mut impl KvStore, c: &Address, job: &Job) -> Result<(), StateError> {
    kv.put(&job_key(c), &job.encode())?;
    if job.phase.active() {
        kv.put(&active_key(c), &[])?;
    } else {
        kv.delete(&active_key(c))?;
    }
    Ok(())
}

/// What a client can ask about an upgrade ([`ChainState::upgrade_status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeStatus {
    pub phase: &'static str,
    /// Why it failed (rolling back or rolled back).
    pub error: Option<String>,
    pub upgrader: Address,
    pub started: u64,
    /// Gas prepaid, and gas charged so far.
    pub budget_gas: u64,
    pub charged_gas: u64,
    pub tasks: Vec<TaskStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStatus {
    pub table: String,
    /// `migrate v1->v2` or `backfill`.
    pub kind: String,
    /// Rows processed; `next_id` is an upper bound on the rows to process (ids are never
    /// reused), so `done`/`next_id` is a lower bound of the progress.
    pub done: u64,
    pub cursor: u64,
    pub next_id: u64,
}

impl<S: KvStore> crate::state::ChainState<S> {
    /// The latest upgrade job of a contract (running or finished), if it ever had one.
    pub fn upgrade_status(&self, contract: &Address) -> Result<Option<UpgradeStatus>, StateError> {
        let Some(job) = load_job(&self.kv, contract)? else {
            return Ok(None);
        };
        let mut tasks = Vec::new();
        for t in &job.tasks {
            tasks.push(TaskStatus {
                table: t.table.clone(),
                kind: match t.migrate {
                    Some((a, b)) => format!("migrate v{a}->v{b}"),
                    None => "backfill".into(),
                },
                done: t.done,
                cursor: t.cursor,
                next_id: read_u64(&self.kv, &layout::counter_key(contract, t.table.as_bytes()))?,
            });
        }
        Ok(Some(UpgradeStatus {
            phase: job.phase.name(),
            error: match &job.phase {
                Phase::RollingBack(m) | Phase::RolledBack(m) => Some(m.clone()),
                _ => None,
            },
            upgrader: job.upgrader,
            started: job.started,
            budget_gas: job.budget,
            charged_gas: job.charged,
            tasks,
        }))
    }
}

/// The account allowed to upgrade a contract (`None`: immutable).
pub fn authority(rec: &ContractRecord) -> Option<Address> {
    match rec.manifest.upgrade_policy() {
        Ok(None) | Ok(Some(UpgradePolicy::Deployer)) | Err(_) => Some(rec.deployer),
        Ok(Some(UpgradePolicy::Immutable)) => None,
        Ok(Some(UpgradePolicy::Authority(a))) => Some(a),
    }
}

fn move_funds(
    kv: &mut impl KvStore,
    from: &Address,
    to: &Address,
    amount: u64,
) -> Result<(), StateError> {
    if amount == 0 {
        return Ok(());
    }
    let f = read_u64(kv, &layout::balance_key(from))?;
    let f = f
        .checked_sub(amount)
        .ok_or_else(|| StateError("upgrade escrow underflow".into()))?;
    kv.put(&layout::balance_key(from), &f.to_le_bytes())?;
    let t = read_u64(kv, &layout::balance_key(to))?
        .checked_add(amount)
        .ok_or_else(|| StateError("balance overflow".into()))?;
    kv.put(&layout::balance_key(to), &t.to_le_bytes())?;
    Ok(())
}

fn registry(contract: &Address, schema: &Schema) -> Result<Tables, StateError> {
    let mut t = Tables::new();
    for (name, defs) in schema.index_defs().map_err(StateError)? {
        t.register(*contract, name.as_bytes(), defs)
            .map_err(|e| StateError(e.to_string()))?;
    }
    Ok(t)
}

impl<S: KvStore> Ctx<'_, S> {
    /// Handle `TxKind::Upgrade`. The outer `Err` is a state fault; the inner one a refusal that
    /// changed nothing.
    pub(crate) fn exec_upgrade(
        &mut self,
        sender: &Address,
        contract: &Address,
        code: &[u8],
        manifest: &Manifest,
        budget_gas: u64,
    ) -> Result<Result<Vec<u8>, UpgradeError>, StateError> {
        use UpgradeError as E;
        let Some(rec) = self.contract_record(contract)? else {
            return Ok(Err(E::NoSuchContract));
        };
        match authority(&rec) {
            None => return Ok(Err(E::Immutable)),
            Some(a) if a != *sender => return Ok(Err(E::NotAuthorized)),
            Some(_) => {}
        }
        if self.overlay.get(&active_key(contract))?.is_some() {
            return Ok(Err(E::InProgress));
        }
        let c = match Contract::from_elf(code) {
            Ok(c) => c,
            Err(e) => return Ok(Err(E::BadContract(format!("{e:?}")))),
        };
        if let Err(m) = manifest.validate() {
            return Ok(Err(E::BadManifest(m)));
        }
        for (f, _) in &manifest.crons {
            let ok = c
                .entry_points()
                .iter()
                .any(|e| e.kind == btb_vm::FunctionKind::Cron && &e.name == f);
            if !ok {
                return Ok(Err(E::BadManifest(format!("no cron entry point {f:?}"))));
            }
        }
        let old_schema = Schema::from_elf(&rec.code)
            .map_err(|m| StateError(format!("stored schema invalid: {m}")))?;
        let new_schema = match Schema::from_elf(code) {
            Ok(s) => s,
            Err(m) => return Ok(Err(E::BadContract(m))),
        };
        let plan = match evolve::diff(&old_schema, &new_schema) {
            Ok(p) => p,
            Err(m) => return Ok(Err(E::Incompatible(m))),
        };
        let mut tables = self.tables.clone();
        if let Err(m) = register_manifest(&mut tables, *contract, code, manifest) {
            return Ok(Err(E::BadManifest(m)));
        }
        // Nothing below can fail the transaction.
        let mut new_manifest = manifest.clone();
        if manifest.upgrade_policy().ok().flatten().is_none() {
            if let Ok(Some(old)) = rec.manifest.upgrade_policy() {
                new_manifest = new_manifest.with_upgrade_policy(old);
            }
        }
        self.tables = tables;
        let new_rec = ContractRecord {
            code: code.to_vec(),
            manifest: new_manifest,
            deployer: rec.deployer,
            height: rec.height,
        };
        self.overlay
            .put(&contract_key(contract), &new_rec.encode())?;
        for (f, every) in &manifest.crons {
            if !rec.manifest.crons.iter().any(|(g, _)| g == f) {
                if let Some(due) = self.height.checked_add(*every) {
                    self.overlay
                        .put(&cron_key(due, contract, f), &every.to_le_bytes())?;
                }
            }
        }
        let n = plan.tasks.len() as u32;
        if n > 0 {
            let cost = budget_gas.saturating_mul(self.cfg.gas_price);
            move_funds(&mut self.overlay, sender, &upgrade_pool(), cost)?;
            let tasks = plan
                .tasks
                .iter()
                .map(|t| {
                    let (migrate, added, removed) = match &t.kind {
                        TaskKind::Migrate { from, to } => (Some((*from, *to)), vec![], vec![]),
                        TaskKind::Backfill { added, removed } => {
                            (None, added.clone(), removed.clone())
                        }
                    };
                    TaskState {
                        table: t.table.clone(),
                        migrate,
                        added,
                        removed,
                        cursor: 0,
                        done: 0,
                    }
                })
                .collect::<Vec<_>>();
            for t in &tasks {
                self.overlay
                    .put(&layout::table_lock_key(contract, t.table.as_bytes()), &[1])?;
            }
            let job = Job {
                upgrader: *sender,
                prev_code: rec.code,
                prev_manifest: rec.manifest,
                started: self.height,
                budget: budget_gas,
                charged: 0,
                tasks,
                cur: 0,
                rb_task: 0,
                rb_next: 0,
                phase: Phase::Forward,
            };
            save_job(&mut self.overlay, contract, &job)?;
        }
        Ok(Ok(n.to_le_bytes().to_vec()))
    }

    /// End of block: advance the upgrade jobs by at most `ROWS_PER_BLOCK` rows.
    pub(crate) fn run_upgrades(&mut self) -> Result<(), StateError> {
        let end = [TAG_UPGRADE_JOB, 1];
        let active = self
            .overlay
            .scan(&[TAG_UPGRADE_JOB, 0], Some(&end), false, 64)?;
        let mut rows_left = ROWS_PER_BLOCK;
        for (k, _) in active {
            if rows_left == 0 {
                break;
            }
            let contract: Address = k
                .get(2..34)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| StateError("bad upgrade key".into()))?;
            let Some(job) = load_job(&self.overlay, &contract)? else {
                self.overlay.delete(&k)?;
                continue;
            };
            rows_left -= self.step_job(&contract, job, rows_left)?;
        }
        Ok(())
    }

    /// Advance one job. Returns the rows it used.
    fn step_job(&mut self, c: &Address, mut job: Job, rows: usize) -> Result<usize, StateError> {
        let rec = self
            .contract_record(c)?
            .ok_or_else(|| StateError("upgrade job of a missing contract".into()))?;
        let new_schema = Schema::from_elf(&rec.code).map_err(StateError)?;
        let old_schema = Schema::from_elf(&job.prev_code).map_err(StateError)?;
        let new_tables = registry(c, &new_schema)?;
        let old_tables = registry(c, &old_schema)?;
        match job.phase.clone() {
            Phase::Forward => {
                let before = job.clone();
                match self.forward(
                    c,
                    &mut job,
                    &rec,
                    &new_schema,
                    &new_tables,
                    &old_tables,
                    rows,
                )? {
                    Ok(used) => Ok(used),
                    Err(reason) => {
                        // Discard this batch; earlier batches are covered by the undo log.
                        job = before;
                        job.phase = Phase::RollingBack(reason);
                        job.rb_task = 0;
                        job.rb_next = 0;
                        save_job(&mut self.overlay, c, &job)?;
                        Ok(1)
                    }
                }
            }
            Phase::RollingBack(reason) => {
                self.rollback(c, job, &rec, reason, &new_tables, &old_tables, rows)
            }
            Phase::Cleanup => {
                let mut n = 0;
                for t in &job.tasks {
                    if t.migrate.is_none() {
                        continue;
                    }
                    let p = undo_prefix(c, &t.table);
                    let e = layout::prefix_successor(&p);
                    for (k, _) in
                        self.overlay
                            .scan(&p, e.as_deref(), false, CLEANUP_PER_BLOCK - n)?
                    {
                        self.overlay.delete(&k)?;
                        n += 1;
                    }
                }
                if n < CLEANUP_PER_BLOCK {
                    job.phase = Phase::Done;
                }
                save_job(&mut self.overlay, c, &job)?;
                Ok(1)
            }
            Phase::Done | Phase::RolledBack(_) => {
                self.overlay.delete(&active_key(c))?;
                Ok(0)
            }
        }
    }

    /// Unlock the job's tables, refund what is left of the budget.
    fn finish_job(&mut self, c: &Address, job: &Job) -> Result<(), StateError> {
        for t in &job.tasks {
            self.overlay
                .delete(&layout::table_lock_key(c, t.table.as_bytes()))?;
        }
        let spent = job.charged.saturating_mul(self.cfg.gas_price);
        let total = job.budget.saturating_mul(self.cfg.gas_price);
        move_funds(
            &mut self.overlay,
            &upgrade_pool(),
            &job.upgrader,
            total.saturating_sub(spent),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn forward(
        &mut self,
        c: &Address,
        job: &mut Job,
        rec: &ContractRecord,
        new_schema: &Schema,
        new_tables: &Tables,
        old_tables: &Tables,
        mut rows: usize,
    ) -> Result<Result<usize, String>, StateError> {
        let contract = crate::exec::load_contract(&mut *self.cache, &rec.code)?;
        let price = self.cfg.gas_price;
        let mut used_rows = 0;
        let mut batch: Option<WriteBatch> = None;
        let mut charge = 0u64;
        {
            let mut host =
                RuntimeHost::new(&self.overlay, &self.tables, *c, [0u8; 32], self.height);
            while rows > 0 && (job.cur as usize) < job.tasks.len() {
                let ti = job.cur as usize;
                let (table, migrate, cursor) = {
                    let t = &job.tasks[ti];
                    (t.table.clone(), t.migrate, t.cursor)
                };
                let prefix = layout::row_prefix(c, table.as_bytes());
                let end = layout::prefix_successor(&prefix);
                let mut from = prefix;
                from.extend_from_slice(&cursor.to_be_bytes());
                let found = self.overlay.scan(&from, end.as_deref(), false, rows)?;
                if found.is_empty() {
                    job.cur += 1;
                    continue;
                }
                for (key, row) in found {
                    let id = layout::id_from_key(&key)
                        .ok_or_else(|| StateError("bad row key".into()))?;
                    let fail = |m: String| -> Result<Result<usize, String>, StateError> {
                        Ok(Err(format!("table {table}, row {id}: {m}")))
                    };
                    let mut gas;
                    let new_row = match migrate {
                        Some((from_v, _)) => {
                            let entry = migration_entry(&table, from_v);
                            let out = match contract.call_transform(
                                &entry,
                                &row,
                                self.cfg.cron_gas_limit,
                                self.cfg.vm_memory as usize,
                            ) {
                                Ok(o) => o,
                                Err(btb_vm::CallError::Trap {
                                    trap: btb_vm::decode::Trap::Abort(m),
                                    ..
                                }) => return fail(format!("migration aborted: {m}")),
                                Err(e) => return fail(format!("migration failed: {e:?}")),
                            };
                            if let Err(m) = new_schema.validate_row(&table, &out.output) {
                                return fail(format!("migration produced a bad row: {m}"));
                            }
                            gas = MIGRATE_ROW_GAS
                                + out.gas_used
                                + ROW_BYTE_GAS * (row.len() + out.output.len()) as u64;
                            out.output
                        }
                        None => {
                            gas = BACKFILL_ROW_GAS;
                            row.clone()
                        }
                    };
                    let w = match new_tables.rewrite(
                        old_tables,
                        host.overlay_mut(),
                        c,
                        table.as_bytes(),
                        id,
                        &row,
                        &new_row,
                    ) {
                        Ok(w) => w,
                        Err(e) => return fail(e.to_string()),
                    };
                    gas += INDEX_ENTRY_GAS * u64::from(w.deleted + w.put);
                    if job.charged + gas > job.budget {
                        return fail(format!(
                            "upgrade budget of {} gas exhausted (row needs {gas} more)",
                            job.budget
                        ));
                    }
                    if migrate.is_some() {
                        host.overlay_mut()
                            .put(&undo_key(c, &table, id), &row)
                            .map_err(StateError::from)?;
                    }
                    job.charged += gas;
                    charge += gas;
                    let t = &mut job.tasks[ti];
                    t.cursor = id + 1;
                    t.done += 1;
                    used_rows += 1;
                    rows -= 1;
                }
            }
            if used_rows > 0 {
                match host.finish() {
                    Ok(b) => batch = Some(b),
                    Err(f) => return Ok(Err(format!("storage deposit: {f:?}"))),
                }
            }
        }
        if let Some(b) = batch {
            self.overlay.commit(b)?;
        }
        move_funds(
            &mut self.overlay,
            &upgrade_pool(),
            &validator_pool(),
            charge.saturating_mul(price),
        )?;
        if job.cur as usize >= job.tasks.len() {
            job.phase = if job.tasks.iter().any(|t| t.migrate.is_some()) {
                Phase::Cleanup
            } else {
                Phase::Done
            };
            self.finish_job(c, job)?;
        }
        save_job(&mut self.overlay, c, job)?;
        Ok(Ok(used_rows.max(1)))
    }

    #[allow(clippy::too_many_arguments)]
    fn rollback(
        &mut self,
        c: &Address,
        mut job: Job,
        rec: &ContractRecord,
        reason: String,
        new_tables: &Tables,
        old_tables: &Tables,
        mut rows: usize,
    ) -> Result<usize, StateError> {
        let used_start = rows;
        let batch;
        {
            let mut host =
                RuntimeHost::new(&self.overlay, &self.tables, *c, [0u8; 32], self.height);
            while rows > 0 && (job.rb_task as usize) < job.tasks.len() {
                let t = job.tasks[job.rb_task as usize].clone();
                let table = t.table.as_bytes();
                let asked = rows;
                let mut moved = 0usize;
                let exhausted;
                if t.migrate.is_some() {
                    let p = undo_prefix(c, &t.table);
                    let e = layout::prefix_successor(&p);
                    for (uk, old_row) in self.overlay.scan(&p, e.as_deref(), false, asked)? {
                        let id = layout::id_from_key(&uk)
                            .ok_or_else(|| StateError("bad undo key".into()))?;
                        if let Some(cur) = self.overlay.get(&layout::row_key(c, table, id))? {
                            old_tables
                                .rewrite(
                                    new_tables,
                                    host.overlay_mut(),
                                    c,
                                    table,
                                    id,
                                    &cur,
                                    &old_row,
                                )
                                .map_err(|e| StateError(format!("rollback: {e}")))?;
                        }
                        host.overlay_mut().delete(&uk).map_err(StateError::from)?;
                        moved += 1;
                    }
                    exhausted = moved < asked;
                } else {
                    let mut from = layout::row_prefix(c, table);
                    let end = layout::prefix_successor(&from);
                    from.extend_from_slice(&job.rb_next.to_be_bytes());
                    let found = self.overlay.scan(&from, end.as_deref(), false, asked)?;
                    let mut ex = found.len() < asked;
                    for (k, row) in found {
                        let id = layout::id_from_key(&k)
                            .ok_or_else(|| StateError("bad row key".into()))?;
                        if id >= t.cursor {
                            ex = true;
                            break;
                        }
                        old_tables
                            .rewrite(new_tables, host.overlay_mut(), c, table, id, &row, &row)
                            .map_err(|e| StateError(format!("rollback: {e}")))?;
                        job.rb_next = id + 1;
                        moved += 1;
                    }
                    exhausted = ex;
                }
                rows -= moved;
                if exhausted {
                    job.rb_task += 1;
                    job.rb_next = 0;
                }
            }
            match host.finish() {
                Ok(b) => batch = b,
                // The contract cannot pay the deposit of the restored rows: retry next block.
                Err(_) => return Ok(1),
            }
        }
        self.overlay.commit(batch)?;
        if job.rb_task as usize >= job.tasks.len() {
            let restored = ContractRecord {
                code: job.prev_code.clone(),
                manifest: job.prev_manifest.clone(),
                deployer: rec.deployer,
                height: rec.height,
            };
            self.overlay.put(&contract_key(c), &restored.encode())?;
            let mut tables = self.tables.clone();
            register_manifest(&mut tables, *c, &restored.code, &restored.manifest)
                .map_err(StateError)?;
            self.tables = tables;
            self.finish_job(c, &job)?;
            job.phase = Phase::RolledBack(reason);
        }
        save_job(&mut self.overlay, c, &job)?;
        Ok((used_start - rows).max(1))
    }
}
