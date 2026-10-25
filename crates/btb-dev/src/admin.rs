//! Read-only admin endpoints and the dashboard page (DEV ONLY, see docs/DEV_SERVER.md).
//!
//! All handlers only read the store. Responses are JSON built by hand (the crate has no JSON
//! dependency). Row bytes are always returned as hex; `decoded` is present when a field-type
//! hint exists for the table (`--fields Table:addr,bool,string`) and the row matches it.

use super::*;
use btb_codec::{Address, Decode, Reader};
use btb_runtime::kv::BatchOp;
use btb_runtime::{by_id, by_index_eq, scan_table, Cursor, Order, QueryOptions};

pub const DASHBOARD_HTML: &str = include_str!("../web/dashboard.html");

type Reply = Result<String, (&'static str, String)>;

const TYPES: [&str; 14] = [
    "addr", "bool", "string", "bytes", "u8", "u16", "u32", "u64", "u128", "i8", "i16", "i32",
    "i64", "i128",
];

fn fault(e: impl std::fmt::Display) -> (&'static str, String) {
    ("500 Internal Server Error", e.to_string())
}

fn bad(m: impl Into<String>) -> (&'static str, String) {
    ("400 Bad Request", m.into())
}

fn hexs(b: &[u8]) -> String {
    demo::hex(b)
}

fn js(s: &str) -> String {
    demo::json_str(s)
}

fn u64_le(v: Option<Vec<u8>>) -> u64 {
    v.and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}

/// Field-type hints, `Table:addr,bool,string` (types: addr bool string bytes u8 u16 u32 u64
/// u128 i8 i16 i32 i64 i128).
pub fn parse_fields(s: &str) -> Result<(String, Vec<String>), String> {
    let (t, f) = s
        .split_once(':')
        .ok_or_else(|| format!("bad fields {s:?}, expected Table:addr,bool,string"))?;
    let f: Vec<String> = f.split(',').map(|x| x.trim().to_string()).collect();
    if t.is_empty() || f.iter().any(|x| !TYPES.contains(&x.as_str())) {
        return Err(format!("bad fields {s:?}, types are {}", TYPES.join(" ")));
    }
    Ok((t.to_string(), f))
}

fn decode_one(ty: &str, r: &mut Reader<'_>) -> Result<String, String> {
    macro_rules! num {
        ($t:ty, $plain:expr) => {{
            let v = <$t>::decode_from(r).map_err(|e| format!("{e:?}"))?;
            if $plain {
                v.to_string()
            } else {
                js(&v.to_string())
            }
        }};
    }
    Ok(match ty {
        "addr" => js(&hexs(
            &Address::decode_from(r).map_err(|e| format!("{e:?}"))?.0,
        )),
        "bool" => bool::decode_from(r)
            .map_err(|e| format!("{e:?}"))?
            .to_string(),
        "string" => js(&String::decode_from(r).map_err(|e| format!("{e:?}"))?),
        "bytes" => js(&hexs(
            &Vec::<u8>::decode_from(r).map_err(|e| format!("{e:?}"))?,
        )),
        "u8" => num!(u8, true),
        "u16" => num!(u16, true),
        "u32" => num!(u32, true),
        "i8" => num!(i8, true),
        "i16" => num!(i16, true),
        "i32" => num!(i32, true),
        "u64" => num!(u64, false),
        "i64" => num!(i64, false),
        "u128" => num!(u128, false),
        "i128" => num!(i128, false),
        t => return Err(format!("unknown type {t:?}")),
    })
}

/// Decode a row with the hint; a JSON array of values, or why it does not match.
fn decode_row(types: &[String], data: &[u8]) -> Result<String, String> {
    let mut r = Reader::new(data, btb_codec::DEFAULT_MAX_LEN);
    let mut out = Vec::new();
    for t in types {
        out.push(decode_one(t, &mut r)?);
    }
    if r.remaining() != 0 {
        return Err("trailing bytes".into());
    }
    Ok(format!("[{}]", out.join(",")))
}

fn row_json(types: Option<&Vec<String>>, id: u64, data: &[u8]) -> String {
    let dec = match types {
        None => String::new(),
        Some(t) => match decode_row(t, data) {
            Ok(d) => format!(",\"decoded\":{d}"),
            Err(e) => format!(",\"decode_error\":{}", js(&e)),
        },
    };
    format!(
        "{{\"id\":{id},\"hex\":{},\"bytes\":{}{dec}}}",
        js(&hexs(data)),
        data.len()
    )
}

fn limit_of(req: &Request) -> usize {
    req.query
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(25)
        .clamp(1, 500)
}

fn cursor_of(req: &Request) -> Result<Option<Cursor>, (&'static str, String)> {
    match req.query.get("cursor").filter(|c| !c.is_empty()) {
        None => Ok(None),
        Some(c) => demo::unhex(c)
            .map(|b| Some(Cursor(b)))
            .ok_or_else(|| bad("cursor must be hex")),
    }
}

fn opts<'a>(req: &Request) -> Result<QueryOptions<'a>, (&'static str, String)> {
    Ok(QueryOptions {
        order: if req.query.get("order").is_some_and(|o| o == "desc") {
            Order::Desc
        } else {
            Order::Asc
        },
        limit: limit_of(req),
        cursor: cursor_of(req)?,
        filter: None,
    })
}

fn cursor_json(c: &Option<Cursor>) -> String {
    c.as_ref().map_or("null".into(), |c| js(&hexs(&c.0)))
}

type Scan = Vec<(Vec<u8>, Vec<u8>)>;

fn scan_prefix<S: KvStore>(
    store: &S,
    prefix: &[u8],
    cap: usize,
) -> Result<Scan, (&'static str, String)> {
    store
        .scan(
            prefix,
            layout::prefix_successor(prefix).as_deref(),
            false,
            cap,
        )
        .map_err(fault)
}

/// Every table of the contract with its row count (a table with a row-id counter but no rows
/// is listed with 0).
fn discover<S: KvStore>(store: &S) -> Result<BTreeMap<String, u64>, (&'static str, String)> {
    let mut out = BTreeMap::new();
    // Name from a key `tag A len(name) name ...`.
    let name_of = |k: &[u8]| -> Option<Vec<u8>> {
        let n = u32::from_be_bytes(k.get(33..37)?.try_into().ok()?) as usize;
        k.get(37..37 + n).map(<[u8]>::to_vec)
    };
    let mut c_prefix = vec![layout::TAG_COUNTER];
    c_prefix.extend_from_slice(&CONTRACT);
    for (k, _) in scan_prefix(store, &c_prefix, 10_000)? {
        if let Some(n) = name_of(&k) {
            out.insert(String::from_utf8_lossy(&n).into_owned(), 0);
        }
    }
    let mut r_prefix = vec![layout::TAG_ROW];
    r_prefix.extend_from_slice(&CONTRACT);
    let end = layout::prefix_successor(&r_prefix);
    let mut start = r_prefix;
    loop {
        let first = store
            .scan(&start, end.as_deref(), false, 1)
            .map_err(fault)?;
        let Some((k, _)) = first.into_iter().next() else {
            break;
        };
        let Some(name) = name_of(&k) else { break };
        let prefix = layout::row_prefix(&CONTRACT, &name);
        let pend = layout::prefix_successor(&prefix);
        let (mut n, mut from) = (0u64, prefix);
        loop {
            let chunk = store
                .scan(&from, pend.as_deref(), false, 1000)
                .map_err(fault)?;
            n += chunk.len() as u64;
            match chunk.last() {
                Some((lk, _)) if chunk.len() == 1000 => {
                    from = lk.clone();
                    from.push(0);
                }
                _ => break,
            }
        }
        out.insert(String::from_utf8_lossy(&name).into_owned(), n);
        match pend {
            Some(p) => start = p,
            None => break,
        }
    }
    Ok(out)
}

fn overview<S: KvStore>(sh: &Shared<S>) -> Reply {
    let st = sh.state.lock().unwrap();
    let store = st.occ.store();
    let tables = discover(store)?;
    let fields = sh.fields.read().unwrap();
    let mut t = Vec::new();
    for (name, n) in &tables {
        let idx: Vec<String> = sh
            .tables
            .index_names(&CONTRACT, name.as_bytes())
            .iter()
            .map(|i| js(i))
            .collect();
        let f = fields.get(name).map_or("null".to_string(), |f| {
            format!(
                "[{}]",
                f.iter().map(|x| js(x)).collect::<Vec<_>>().join(",")
            )
        });
        t.push(format!(
            "{{\"name\":{},\"rows\":{n},\"indexes\":[{}],\"fields\":{f}}}",
            js(name),
            idx.join(",")
        ));
    }
    let get = |k: Vec<u8>| store.get(&k).map_err(fault);
    let balance = u64_le(get(layout::balance_key(&CONTRACT))?);
    let usage = u64_le(get(layout::usage_key(&CONTRACT))?);
    let deposit = u64_le(get(layout::deposit_key(&CONTRACT))?);
    let skey = layout::storage_key(&CONTRACT, b"");
    let storage_keys = scan_prefix(store, &skey, 100_000)?.len();
    Ok(format!(
        "{{\"contract\":{},\"height\":{},\"balance\":{balance},\"storage_usage\":{usage},\"storage_deposit\":{deposit},\"deposit_per_byte\":{},\"storage_keys\":{storage_keys},\"merkle\":{{\"tracked\":false,\"root\":null,\"version\":{}}},\"tables\":[{}]}}",
        js(&hexs(&CONTRACT)),
        st.height,
        layout::DEPOSIT_PER_BYTE,
        st.height,
        t.join(",")
    ))
}

fn table_rows<S: KvStore>(sh: &Shared<S>, req: &Request, table: &str) -> Reply {
    let st = sh.state.lock().unwrap();
    let store = st.occ.store();
    let known = discover(store)?;
    let Some(total) = known.get(table) else {
        return Err(("404 Not Found", format!("no such table {table:?}")));
    };
    let page = scan_table(store, &CONTRACT, table.as_bytes(), &opts(req)?).map_err(fault)?;
    let fields = sh.fields.read().unwrap();
    let rows: Vec<String> = page
        .rows
        .iter()
        .map(|r| row_json(fields.get(table), r.id, &r.data))
        .collect();
    Ok(format!(
        "{{\"table\":{},\"total\":{total},\"rows\":[{}],\"next_cursor\":{}}}",
        js(table),
        rows.join(","),
        cursor_json(&page.next_cursor)
    ))
}

fn index_value(
    specs: &[IndexSpec],
    table: &str,
    index: &str,
    v: &str,
) -> Result<IndexValue, String> {
    let addr = specs
        .iter()
        .find(|i| i.table == table && i.index == index)
        .map(|i| i.addr);
    match (addr, v.len() == 64, v.parse::<u64>()) {
        (Some(true), _, _) | (None, true, _) => demo::unhex(v)
            .filter(|b| b.len() == 32)
            .map(IndexValue::Bytes)
            .ok_or_else(|| "address value must be 64 hex characters".to_string()),
        (_, _, Ok(n)) => Ok(IndexValue::U64(n)),
        _ => Err("value must be a u64 or a 64-hex-character address".into()),
    }
}

fn index_browse<S: KvStore>(sh: &Shared<S>, req: &Request, table: &str, index: &str) -> Reply {
    let st = sh.state.lock().unwrap();
    let store = st.occ.store();
    if !sh
        .tables
        .index_names(&CONTRACT, table.as_bytes())
        .iter()
        .any(|i| i == index)
    {
        return Err(("404 Not Found", "no such index".into()));
    }
    let o = opts(req)?;
    let fields = sh.fields.read().unwrap();
    if let Some(v) = req.query.get("value").filter(|v| !v.is_empty()) {
        let value = index_value(&sh.indexes, table, index, v).map_err(bad)?;
        let page =
            by_index_eq(store, &CONTRACT, table.as_bytes(), index, &value, &o).map_err(fault)?;
        let rows: Vec<String> = page
            .rows
            .iter()
            .map(|r| row_json(fields.get(table), r.id, &r.data))
            .collect();
        return Ok(format!(
            "{{\"table\":{},\"index\":{},\"value\":{},\"rows\":[{}],\"next_cursor\":{}}}",
            js(table),
            js(index),
            js(v),
            rows.join(","),
            cursor_json(&page.next_cursor)
        ));
    }
    // No value: list the index entries (encoded value as hex, row id), paged.
    let prefix = layout::index_prefix(&CONTRACT, table.as_bytes(), index);
    let end = layout::prefix_successor(&prefix);
    let desc = o.order == Order::Desc;
    let (mut start, mut stop) = (prefix.clone(), end);
    if let Some(c) = &o.cursor {
        if desc {
            stop = Some(c.0.clone());
        } else {
            start = c.0.clone();
            start.push(0);
        }
    }
    let mut entries = store
        .scan(&start, stop.as_deref(), desc, o.limit + 1)
        .map_err(fault)?;
    let more = entries.len() > o.limit;
    entries.truncate(o.limit);
    let next = match (more, entries.last()) {
        (true, Some((k, _))) => js(&hexs(k)),
        _ => "null".into(),
    };
    let items: Vec<String> = entries
        .iter()
        .map(|(k, _)| {
            format!(
                "{{\"value\":{},\"id\":{}}}",
                js(&hexs(&k[prefix.len()..k.len() - 8])),
                layout::id_from_key(k).unwrap_or(0)
            )
        })
        .collect();
    Ok(format!(
        "{{\"table\":{},\"index\":{},\"entries\":[{}],\"next_cursor\":{next}}}",
        js(table),
        js(index),
        items.join(",")
    ))
}

fn storage<S: KvStore>(sh: &Shared<S>, req: &Request) -> Reply {
    let st = sh.state.lock().unwrap();
    let store = st.occ.store();
    let prefix = layout::storage_key(&CONTRACT, b"");
    let (mut start, end) = (prefix.clone(), layout::prefix_successor(&prefix));
    if let Some(c) = cursor_of(req)? {
        start = c.0;
        start.push(0);
    }
    let limit = limit_of(req);
    let mut e = store
        .scan(&start, end.as_deref(), false, limit + 1)
        .map_err(fault)?;
    let more = e.len() > limit;
    e.truncate(limit);
    let next = match (more, e.last()) {
        (true, Some((k, _))) => js(&hexs(k)),
        _ => "null".into(),
    };
    let items: Vec<String> = e
        .iter()
        .map(|(k, v)| {
            let key = &k[prefix.len()..];
            format!(
                "{{\"key\":{},\"key_text\":{},\"hex\":{},\"bytes\":{}}}",
                js(&hexs(key)),
                js(&String::from_utf8_lossy(key)),
                js(&hexs(v)),
                v.len()
            )
        })
        .collect();
    Ok(format!(
        "{{\"keys\":[{}],\"next_cursor\":{next}}}",
        items.join(",")
    ))
}

fn tokens<S: KvStore>(sh: &Shared<S>) -> Reply {
    let st = sh.state.lock().unwrap();
    let store = st.occ.store();
    let mut out = Vec::new();
    for (k, v) in scan_prefix(store, &[layout::TAG_TOKEN], 1000)? {
        let Ok(id) = <[u8; 32]>::try_from(&k[1..]) else {
            continue;
        };
        let Some(info) = btb_vm::token::TokenInfo::decode(&v) else {
            continue;
        };
        let holders = scan_prefix(store, &layout::token_balance_prefix(&id), 100)?;
        let hs: Vec<String> = holders
            .iter()
            .map(|(hk, hv)| {
                let bal = u128::from_le_bytes(hv.as_slice().try_into().unwrap_or([0; 16]));
                format!(
                    "{{\"holder\":{},\"balance\":{}}}",
                    js(&hexs(&hk[33..])),
                    js(&bal.to_string())
                )
            })
            .collect();
        out.push(format!(
            "{{\"id\":{},\"name\":{},\"symbol\":{},\"decimals\":{},\"supply\":{},\"creator\":{},\"holders\":[{}]}}",
            js(&hexs(&id)),
            js(&info.name),
            js(&info.symbol),
            info.decimals,
            js(&info.supply.to_string()),
            js(&hexs(&info.creator)),
            hs.join(",")
        ));
    }
    Ok(format!("{{\"tokens\":[{}]}}", out.join(",")))
}

fn one_row<S: KvStore>(sh: &Shared<S>, table: &str, id: &str) -> Reply {
    let st = sh.state.lock().unwrap();
    let id: u64 = id.parse().map_err(|_| bad("id must be a number"))?;
    let row = by_id(st.occ.store(), &CONTRACT, table.as_bytes(), id).map_err(fault)?;
    let Some(r) = row else {
        return Err(("404 Not Found", "no such row".into()));
    };
    Ok(row_json(
        sh.fields.read().unwrap().get(table),
        r.id,
        &r.data,
    ))
}

/// Serve `GET /admin/...` (everything except the SSE stream).
pub fn route<S: KvStore>(sh: &Shared<S>, req: &Request) -> Reply {
    let parts: Vec<&str> = req.path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["admin"] | ["admin", "overview"] | ["admin", "tables"] => overview(sh),
        ["admin", "table", t] => table_rows(sh, req, t),
        ["admin", "table", t, id] => one_row(sh, t, id),
        ["admin", "index", t, i] => index_browse(sh, req, t, i),
        ["admin", "storage"] => storage(sh, req),
        ["admin", "tokens"] => tokens(sh),
        _ => Err(("404 Not Found", "not found".into())),
    }
}

/// What a committed batch touched, as an SSE payload.
pub fn commit_event(height: u64, batch: &WriteBatch) -> String {
    let (mut tables, mut storage, mut tokens, mut accounts) = (Vec::new(), false, false, false);
    for op in batch.ops() {
        let (BatchOp::Put(key, _) | BatchOp::Delete(key)) = op;
        match key.first() {
            Some(&(layout::TAG_ROW | layout::TAG_COUNTER | layout::TAG_INDEX))
                if key.get(1..33) == Some(&CONTRACT[..]) =>
            {
                let n = key
                    .get(33..37)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(|b| u32::from_be_bytes(b) as usize);
                if let Some(name) = n.and_then(|n| key.get(37..37 + n)) {
                    let name = String::from_utf8_lossy(name).into_owned();
                    if !tables.contains(&name) {
                        tables.push(name);
                    }
                }
            }
            Some(&layout::TAG_STORAGE) => storage = true,
            Some(&(layout::TAG_TOKEN | layout::TAG_TOKEN_BALANCE)) => tokens = true,
            _ => accounts = true,
        }
    }
    let t: Vec<String> = tables.iter().map(|t| js(t)).collect();
    format!(
        "{{\"height\":{height},\"tables\":[{}],\"storage\":{storage},\"tokens\":{tokens},\"accounts\":{accounts}}}",
        t.join(",")
    )
}
