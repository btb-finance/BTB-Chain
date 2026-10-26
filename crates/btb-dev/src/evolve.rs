//! Schema evolution on `btb dev` reload: the same diff the chain runs on an upgrade
//! (`btb_runtime::evolve`), applied at once and atomically to the dev store.
//!
//! New indexes are backfilled for rows written by earlier builds, removed ones are deleted,
//! and `#[btb::migration]` functions rewrite the rows of an incompatible table. Everything is
//! computed into one write batch before anything is committed, so a failing migration (or an
//! incompatible change without one) rejects the reload and the old build keeps running on the
//! untouched data. There is no gas, no budget and no table lock here.

use btb_runtime::evolve::{diff, TaskKind};
use btb_runtime::kv::WriteBatch;
use btb_runtime::schema::{migration_entry, Schema};
use btb_runtime::{layout, KvStore, RuntimeHost, Tables};
use btb_vm::Contract;

const GAS: u64 = 50_000_000;
const MEM: usize = 1 << 20;
const PAGE: usize = 256;

fn registry(addr: &[u8; 32], schema: &Schema) -> Result<Tables, String> {
    let mut t = Tables::new();
    for (name, defs) in schema.index_defs()? {
        t.register(*addr, name.as_bytes(), defs)
            .map_err(|e| e.to_string())?;
    }
    Ok(t)
}

/// What a reload to `new` (code `contract`) needs, as a batch to commit and a report to print.
/// `Err` is the reason the reload must be refused.
pub(crate) fn plan<S: KvStore>(
    store: &S,
    addr: &[u8; 32],
    old: &Schema,
    new: &Schema,
    contract: &Contract,
    height: u64,
) -> Result<(WriteBatch, Vec<String>), String> {
    let plan = diff(old, new)?;
    let mut notes = plan.notes.clone();
    if plan.tasks.is_empty() {
        return Ok((WriteBatch::new(), notes));
    }
    let (old_t, new_t) = (registry(addr, old)?, registry(addr, new)?);
    let mut host = RuntimeHost::new(store, &new_t, *addr, [0u8; 32], height);
    for task in &plan.tasks {
        let table = task.table.as_bytes();
        let mut next = 0u64;
        let mut rows = 0u64;
        loop {
            let prefix = layout::row_prefix(addr, table);
            let end = layout::prefix_successor(&prefix);
            let mut from = prefix;
            from.extend_from_slice(&next.to_be_bytes());
            let page = host
                .overlay_mut()
                .scan(&from, end.as_deref(), false, PAGE)
                .map_err(|e| e.to_string())?;
            if page.is_empty() {
                break;
            }
            for (key, row) in page {
                let id = layout::id_from_key(&key).ok_or("bad row key")?;
                let fail = |m: String| format!("table {}, row {id}: {m}", task.table);
                let out = match &task.kind {
                    TaskKind::Migrate { from, .. } => {
                        let o = contract
                            .call_transform(&migration_entry(&task.table, *from), &row, GAS, MEM)
                            .map_err(|e| fail(format!("migration failed: {e:?}")))?;
                        new.validate_row(&task.table, &o.output)
                            .map_err(|m| fail(format!("migration produced a bad row: {m}")))?;
                        o.output
                    }
                    TaskKind::Backfill { .. } => row.clone(),
                };
                new_t
                    .rewrite(&old_t, host.overlay_mut(), addr, table, id, &row, &out)
                    .map_err(|e| fail(e.to_string()))?;
                next = id + 1;
                rows += 1;
            }
        }
        notes.push(format!("table `{}`: {rows} row(s) rewritten", task.table));
    }
    let batch = host
        .finish()
        .map_err(|f| format!("storage deposit: {f:?}"))?;
    Ok((batch, notes))
}
