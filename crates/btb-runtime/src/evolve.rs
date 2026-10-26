//! Schema evolution: compare the schema of the code a contract runs with the schema of the code
//! it is being upgraded to, and say what the upgrade needs.
//!
//! Both sides are read from the ELFs ([`Schema::from_elf`]), so every node, `btb dev` and
//! `btb upgrade` reach the same verdict. The rules, per table:
//!
//! | change | verdict |
//! | --- | --- |
//! | new table | compatible, no work |
//! | new fields **appended at the end**, all `Option<_>` | compatible, no work: old rows end early and the missing fields read as `None` (generated `Decode` and index extractors do this) |
//! | new `#[index]` / new `#[primary]` on a field that had none | compatible, **backfill** the entries of existing rows |
//! | removed `#[index]` (not the primary) | compatible, the entries of existing rows are deleted |
//! | a field removed, reordered, renamed or retyped; a non-`Option` field appended; the primary changed or removed; an index turned into the primary | **incompatible**: needs a migration |
//! | a table removed | always rejected (its rows would be orphaned) |
//!
//! An incompatible table needs `#[btb::table(version = N, of = "T")]` for the old row type, a
//! `#[btb::migration(from = old)]` function, and a `version` greater than the old one. The old
//! row type must match the stored schema field for field, so a migration written against the
//! wrong shape is refused before anything runs. A migration declared next to a compatible
//! change (with a version bump) also runs, to transform data while keeping the layout.

use crate::schema::{Schema, TableSchema};
use std::collections::BTreeSet;

/// What has to run for one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    /// Rewrite every row with the migration function (and re-index it).
    Migrate { from: u32, to: u32 },
    /// Add the entries of new indexes and delete the entries of removed ones.
    Backfill {
        added: Vec<String>,
        removed: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub table: String,
    pub kind: TaskKind,
}

/// The result of comparing two schemas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Row-rewriting work, ordered by table name.
    pub tasks: Vec<Task>,
    /// One human-readable line per change, including the ones that need no work.
    pub notes: Vec<String>,
}

impl Plan {
    /// Nothing to rewrite (the upgrade is just a code swap).
    pub fn is_trivial(&self) -> bool {
        self.tasks.is_empty()
    }
}

fn names(v: &[String]) -> String {
    v.iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why `old` cannot simply become `new` (`None`: it can, possibly after appended options).
fn incompatibility(old: &TableSchema, new: &TableSchema) -> Option<String> {
    if new.fields.len() < old.fields.len() {
        let gone: Vec<String> = old.fields[new.fields.len()..]
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        return Some(format!("removed field(s) {}", names(&gone)));
    }
    for (i, (on, ot)) in old.fields.iter().enumerate() {
        let (nn, nt) = &new.fields[i];
        if on != nn {
            return Some(if new.fields.iter().any(|(n, _)| n == on) {
                format!("field `{on}` moved (position {i} is now `{nn}`)")
            } else {
                format!("field `{on}` removed or renamed (position {i} is now `{nn}`)")
            });
        }
        if ot != nt {
            return Some(format!("field `{on}` changed type from {ot} to {nt}"));
        }
    }
    for (n, t) in &new.fields[old.fields.len()..] {
        if !t.is_opt() {
            return Some(format!(
                "appended field `{n}: {t}` is not an Option (old rows have no value for it)"
            ));
        }
    }
    let added: Vec<&String> = new
        .indexes
        .iter()
        .filter(|i| !old.indexes.contains(i))
        .collect();
    match (&old.primary, &new.primary) {
        (a, b) if a == b => None,
        (None, Some(p)) if added.contains(&p) => None,
        (Some(p), None) => Some(format!("primary key `{p}` removed")),
        (Some(a), Some(b)) => Some(format!("primary key changed from `{a}` to `{b}`")),
        (None, Some(p)) => Some(format!(
            "index `{p}` turned into the primary key (uniqueness of existing rows is unchecked)"
        )),
        (None, None) => None,
    }
}

/// Compare the schema of the running code with the schema of the new code.
///
/// `Err` is the reason the upgrade must be refused, naming the table and the change.
pub fn diff(old: &Schema, new: &Schema) -> Result<Plan, String> {
    let mut plan = Plan::default();
    for o in &old.tables {
        if new.table(&o.name).is_none() {
            return Err(format!(
                "table `{}` is no longer declared; tables cannot be removed (their rows would be orphaned)",
                o.name
            ));
        }
    }
    for n in &new.tables {
        let Some(o) = old.table(&n.name) else {
            plan.notes.push(format!("new table `{}`", n.name));
            continue;
        };
        if n.version < o.version {
            return Err(format!(
                "table `{}`: version goes down from {} to {}",
                n.name, o.version, n.version
            ));
        }
        let broken = incompatibility(o, n);
        let wants_migration = n.version > o.version && new.has_migration(&n.name, o.version);
        if broken.is_some() || wants_migration {
            if n.version == o.version {
                return Err(format!(
                    "table `{}`: {}; raise `version` above {} and add `#[btb::migration(from = {})]`",
                    n.name,
                    broken.unwrap_or_default(),
                    o.version,
                    o.version
                ));
            }
            let why = broken
                .clone()
                .unwrap_or_else(|| "data transformation".to_string());
            if !new.has_migration(&n.name, o.version) {
                return Err(format!(
                    "table `{}`: {why}; version {} -> {} needs `#[btb::migration(from = {})] fn(old: OldRow) -> {}`",
                    n.name, o.version, n.version, o.version, n.name
                ));
            }
            let Some(prev) = new.previous_shape(&n.name, o.version) else {
                return Err(format!(
                    "table `{}`: migration from {} has no old row type; declare it with `#[btb::table(version = {}, of = \"{}\")]`",
                    n.name, o.version, o.version, n.name
                ));
            };
            if prev.fields != o.fields {
                return Err(format!(
                    "table `{}`: the old row type declared for version {} does not match the stored schema",
                    n.name, o.version
                ));
            }
            plan.notes.push(format!(
                "migrate table `{}` from v{} to v{} ({why})",
                n.name, o.version, n.version
            ));
            plan.tasks.push(Task {
                table: n.name.clone(),
                kind: TaskKind::Migrate {
                    from: o.version,
                    to: n.version,
                },
            });
            continue;
        }
        let old_ix: BTreeSet<&String> = o.indexes.iter().collect();
        let new_ix: BTreeSet<&String> = n.indexes.iter().collect();
        let added: Vec<String> = n
            .indexes
            .iter()
            .filter(|i| !old_ix.contains(i))
            .cloned()
            .collect();
        let removed: Vec<String> = o
            .indexes
            .iter()
            .filter(|i| !new_ix.contains(i))
            .cloned()
            .collect();
        if n.fields.len() > o.fields.len() {
            let extra: Vec<String> = n.fields[o.fields.len()..]
                .iter()
                .map(|(n, _)| n.clone())
                .collect();
            plan.notes.push(format!(
                "table `{}`: appended optional field(s) {} (old rows read them as None)",
                n.name,
                names(&extra)
            ));
        }
        if !added.is_empty() {
            plan.notes.push(format!(
                "table `{}`: new index {} (backfilled for existing rows)",
                n.name,
                names(&added)
            ));
        }
        if !removed.is_empty() {
            plan.notes.push(format!(
                "table `{}`: removed index {} (entries deleted)",
                n.name,
                names(&removed)
            ));
        }
        if !added.is_empty() || !removed.is_empty() {
            plan.tasks.push(Task {
                table: n.name.clone(),
                kind: TaskKind::Backfill { added, removed },
            });
        }
    }
    plan.tasks.sort_by(|a, b| a.table.cmp(&b.table));
    Ok(plan)
}
