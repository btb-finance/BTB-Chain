//! Hover text and completion lists. Purely textual so it works on code that does not parse.

/// A completion candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: &'static str,
    pub detail: &'static str,
    /// LSP `CompletionItemKind`: 3 function, 14 keyword (used for macros), 22 struct.
    pub kind: u32,
}

/// `(name, rules)` for each `#[btb::*]` attribute.
pub(crate) const MACROS: [(&str, &str); 5] = [
    ("query", "**`#[btb::query]`**: read-only function. Runs without consensus on hosting/edge nodes and can be subscribed to.\n\n- Must not write state: `storage_set`/`storage_remove`, table `insert`/`update`/`delete` and token `transfer`/`mint`/`burn`/`create` trap with `WriteInQuery` (flagged as an error `btb::query-write`).\n- No randomness, no floats, no `std`.\n- Gas: every instruction and host call is metered; reads cost per byte moved (see `crates/btb-vm/README.md`). Keep scans bounded with a page limit.\n- Name must match `[a-z0-9_]{1,64}`; exported symbol is `btb_query_<name>`."),
    ("mutation", "**`#[btb::mutation]`**: changes state in a transaction and goes through consensus.\n\n- May write storage, tables and tokens; an `abort` rolls the whole call back.\n- Randomness only from the chain beacon, only in mutations/crons marked `randomness`.\n- No floats, no `std`, no system time or threads (determinism).\n- Gas: charged per instruction and per host call; writes cost per byte (see `crates/btb-vm/README.md`).\n- Name must match `[a-z0-9_]{1,64}`; exported symbol is `btb_mutation_<name>`."),
    ("cron", "**`#[btb::cron(every = \"1h\")]`**: a mutation run on a schedule by validators.\n\n- Same rules as `mutation`: may write state, deterministic, metered.\n- Not callable by users; the optional `every` is validated but the scheduler arrives in Phase 2.\n- Name must match `[a-z0-9_]{1,64}`; exported symbol is `btb_cron_<name>`."),
    ("action", "**`#[btb::action]`**: calls the outside world over HTTPS; validators must agree on the result.\n\n- Runs read-only as the first phase of an `Action` transaction and returns the `HttpRequest` every validator makes (the host must be in the contract's allowlist).\n- `#[btb::transform(\"name\")]` strips what differs between validators' responses (pure, no state); the signed result is the encoding of its return value.\n- The agreed result arrives in the mutation `<name>_callback(id, args, status, data)`; start it with `action::require_callback()`.\n- Name must match `[a-z0-9_]{1,64}`; exported symbol is `btb_action_<name>`."),
    ("table", "**`#[btb::table]`**: declares a table row type. Generates `insert`, `get`, `update`, `delete`, `scan` and index queries.\n\n- Reads are allowed in queries; `insert`/`update`/`delete` only in mutations and crons.\n- Fields use the canonical encoding (docs/ENCODING.md); the first 32 bytes can be an indexed owner address.\n- Gas: row bytes are charged on every read and write."),
];

const HOST: [(&str, &str); 22] = [
    ("storage_get", "Read a key (query-safe). Gas per byte read."),
    (
        "storage_set",
        "WRITE: set a key. Mutations/crons only. Gas per byte written.",
    ),
    (
        "storage_remove",
        "WRITE: remove a key. Mutations/crons only.",
    ),
    ("caller", "Address of the caller."),
    ("contract_address", "Address of this contract."),
    ("block_height", "Current block height (deterministic)."),
    ("token_balance", "Native BTB balance of an address."),
    (
        "token_transfer",
        "WRITE: transfer native BTB from this contract.",
    ),
    (
        "return_data",
        "Return encoded result and stop. Gas per byte.",
    ),
    ("abort", "Abort and roll back with a message. Gas per byte."),
    ("table_insert", "WRITE: insert a row, returns its id."),
    ("table_get", "Read a row by id (query-safe)."),
    ("table_update", "WRITE: replace a row by id."),
    ("table_delete", "WRITE: delete a row by id."),
    ("table_scan", "List row ids from a start id (query-safe)."),
    (
        "table_index_scan",
        "Page through a secondary index (query-safe).",
    ),
    ("token_create_raw", "WRITE: create a user token."),
    ("token_transfer_of_raw", "WRITE: transfer a user token."),
    (
        "token_balance_of_raw",
        "Read a user token balance (query-safe).",
    ),
    (
        "token_mint_raw",
        "WRITE: mint a user token (mint authority only).",
    ),
    (
        "token_burn_raw",
        "WRITE: burn a user token (mint authority only).",
    ),
    (
        "token_holders_raw",
        "Page through token holders (query-safe).",
    ),
];

const MACRO_ITEMS: [(&str, &str); 5] = [
    ("query", "read-only function, subscribable"),
    ("mutation", "state-changing function in a transaction"),
    ("cron", "scheduled mutation"),
    ("action", "outside HTTPS call agreed by validators"),
    ("table", "table row type"),
];

/// Completion candidates for the text on the line before the cursor.
///
/// After `btb::` only the macro names; after `host::` only host calls; otherwise both.
pub fn completions(line_prefix: &str) -> Vec<CompletionItem> {
    let p = line_prefix.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
    let macros = || {
        MACRO_ITEMS.iter().map(|&(label, detail)| CompletionItem {
            label,
            detail,
            kind: 14,
        })
    };
    let host = || {
        HOST.iter().map(|&(label, detail)| CompletionItem {
            label,
            detail,
            kind: 3,
        })
    };
    if p.ends_with("btb::") {
        macros().collect()
    } else if p.ends_with("host::") {
        host().collect()
    } else {
        macros().chain(host()).collect()
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Hover text (markdown) for a `btb::<kind>` attribute at `(line, col)`, if any.
pub fn hover(src: &str, line: usize, col: usize) -> Option<String> {
    let text: Vec<char> = src.lines().nth(line)?.chars().collect();
    if col > text.len() {
        return None;
    }
    let mut s = col.min(text.len());
    while s > 0 && is_ident(text[s - 1]) {
        s -= 1;
    }
    let mut e = col.min(text.len());
    while e < text.len() && is_ident(text[e]) {
        e += 1;
    }
    if s == e {
        return None;
    }
    let word: String = text[s..e].iter().collect();
    let before: String = text[..s].iter().collect();
    let after: String = text[e..].iter().collect();
    let kind = if word == "btb" && after.starts_with("::") {
        let rest: String = after[2..].chars().take_while(|&c| is_ident(c)).collect();
        rest
    } else if before.trim_end().ends_with("btb::") || before.trim_end().ends_with("#[") {
        word
    } else {
        return None;
    };
    MACROS
        .iter()
        .find(|(n, _)| *n == kind)
        .map(|(_, d)| d.to_string())
}
