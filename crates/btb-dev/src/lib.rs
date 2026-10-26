//! Single-node dev server for BTB contracts (DEV ONLY).
//!
//! Loads a contract ELF, keeps state in a `KvStore`, and serves it over plain HTTP:
//!
//! - `POST /mutation/<name>`: run a mutation (optimistic transaction), commit, then re-run every
//!   subscribed query whose read set was touched and push changed results.
//! - `GET /query/<name>`: run a query once.
//! - `GET /subscribe/<name>`: Server-Sent Events; one event with the current result, then one
//!   more whenever a commit changes the result (`SubscriptionManager`).
//! - `GET /`: the todo demo page.
//! - `GET /dashboard` and `GET /admin/...`: read-only table browser (see `admin`).
//!
//! Arguments: `?args=<hex of btb-codec bytes>`, or a POST body that is either that hex or (with
//! `Content-Type: application/json`) a flat JSON array, see `demo`. Results are hex, or JSON with
//! `?format=json`. The caller is the `X-BTB-Caller` header or `?caller=` (EventSource cannot set
//! headers): 64 hex characters are the address, any other text is hashed with SHA-256. There is
//! no authentication; never expose this server.

mod admin;
mod demo;

pub use admin::parse_fields;

use btb_runtime::schema::{IndexKind, Schema, Ty};
use btb_runtime::{
    layout, IndexDef, IndexValue, KvStore, Occ, OccError, ReadSet, RuntimeHost, SubId,
    SubscriptionManager, Tables, WriteBatch,
};
use btb_vm::{CallError, Contract, FunctionKind, Trap};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// The one contract a dev server hosts.
pub const CONTRACT: [u8; 32] = [0xDE; 32];
/// Gas limit per call and VM memory.
const GAS: u64 = 50_000_000;
const MEM: usize = 1 << 20;
/// BTB the dev contract starts with, to pay storage deposits.
const DEV_BALANCE: u64 = 1_000_000_000_000;
const MAX_BODY: usize = 1 << 20;
const MAX_RETRIES: u32 = 3;
const TODO_HTML: &str = include_str!("../../../examples/todo/web/index.html");

/// TEMPORARY schema declaration, `Table:index:kind@offset`: index `index` of `Table` is the
/// value at byte `offset` of the row, `addr` (32 bytes, as `IndexValue::Bytes`) or `u64`
/// (8 bytes LE). Contracts will declare indexes themselves later (docs/DEV_SERVER.md).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSpec {
    pub table: String,
    pub index: String,
    pub addr: bool,
    pub offset: usize,
}

impl IndexSpec {
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("bad --index {s:?}, expected Table:index:addr@offset or :u64@offset");
        let mut p = s.split(':');
        let (table, index, rest) = (p.next(), p.next(), p.next());
        let (Some(table), Some(index), Some(rest), None) = (table, index, rest, p.next()) else {
            return Err(bad());
        };
        let (kind, off) = rest.split_once('@').ok_or_else(bad)?;
        let addr = match kind {
            "addr" => true,
            "u64" => false,
            _ => return Err(bad()),
        };
        Ok(Self {
            table: table.into(),
            index: index.into(),
            addr,
            offset: off.parse().map_err(|_| bad())?,
        })
    }

    fn def(&self) -> IndexDef {
        let (addr, off) = (self.addr, self.offset);
        IndexDef::new(&self.index, move |row| {
            let n = if addr { 32 } else { 8 };
            let b = row
                .get(off..off + n)
                .ok_or_else(|| "row too short for index".to_string())?;
            Ok(Some(if addr {
                IndexValue::Bytes(b.to_vec())
            } else {
                IndexValue::U64(u64::from_le_bytes(b.try_into().expect("8 bytes")))
            }))
        })
    }
}

/// Register the indexes the contract declares in its ELF, plus the deprecated `--index` /
/// `Cargo.toml` specs for tables the ELF does not declare. Returns the ELF's schema.
fn register_tables(
    tables: &mut Tables,
    elf: &[u8],
    legacy: &[IndexSpec],
) -> Result<Schema, String> {
    let schema = Schema::from_elf(elf)?;
    for (t, defs) in schema.index_defs()? {
        tables
            .register(CONTRACT, t.as_bytes(), defs)
            .map_err(|e| e.to_string())?;
    }
    let mut by_table: BTreeMap<&str, Vec<IndexDef>> = BTreeMap::new();
    for i in legacy.iter().filter(|i| schema.table(&i.table).is_none()) {
        by_table.entry(&i.table).or_default().push(i.def());
    }
    for (t, defs) in by_table {
        tables
            .register(CONTRACT, t.as_bytes(), defs)
            .map_err(|e| e.to_string())?;
    }
    Ok(schema)
}

struct Client {
    caller: [u8; 32],
    tx: Sender<Vec<u8>>,
}

struct State<S: KvStore> {
    occ: Occ<S>,
    subs: SubscriptionManager,
    clients: BTreeMap<SubId, Client>,
    height: u64,
    /// Dashboard event streams (`/admin/events`).
    admin: Vec<Sender<String>>,
}

struct Shared<S: KvStore> {
    /// The running contract; swapped by `Server::reload` (state is kept).
    contract: RwLock<Arc<Contract>>,
    /// Page served at `/`; `None` serves the built-in todo demo.
    page: RwLock<Option<String>>,
    tables: RwLock<Tables>,
    /// What the contract's ELF declares (`#[btb::table]`, function signatures).
    schema: RwLock<Schema>,
    /// Index declarations (the dashboard needs their value kinds).
    indexes: Vec<IndexSpec>,
    /// Field-type hints per table for the dashboard (`Table` -> `["addr", "bool", ...]`).
    fields: RwLock<BTreeMap<String, Vec<String>>>,
    state: Mutex<State<S>>,
    stop: AtomicBool,
}

impl<S: KvStore> Shared<S> {
    /// Dashboard field hints of `table`: from `--fields` / `Cargo.toml` if given, else derived
    /// from the contract's schema when every field has a dashboard type.
    fn hint(&self, table: &str) -> Option<Vec<String>> {
        if let Some(h) = self.fields.read().unwrap().get(table) {
            return Some(h.clone());
        }
        let schema = self.schema.read().unwrap();
        schema
            .table(table)?
            .fields
            .iter()
            .map(|(_, ty)| match ty {
                Ty::Address => Some("addr".to_string()),
                Ty::Str => Some("string".into()),
                Ty::Bytes => Some("bytes".into()),
                Ty::Bool
                | Ty::U8
                | Ty::U16
                | Ty::U32
                | Ty::U64
                | Ty::U128
                | Ty::I8
                | Ty::I16
                | Ty::I32
                | Ty::I64
                | Ty::I128 => Some(ty.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Is index `index` of `table` made of 32-byte addresses (`Some(true)`), numbers
    /// (`Some(false)`), or unknown to the contract and the legacy specs (`None`)?
    fn index_is_addr(&self, table: &str, index: &str) -> Option<bool> {
        if let Some(k) = self
            .schema
            .read()
            .unwrap()
            .table(table)
            .and_then(|t| t.index_kind(index))
        {
            return Some(k == IndexKind::Bytes);
        }
        self.indexes
            .iter()
            .find(|i| i.table == table && i.index == index)
            .map(|i| i.addr)
    }
}

/// Dashboard field hints: `(table, field types)`.
pub type FieldHints = Vec<(String, Vec<String>)>;

type ReloadFn = Arc<dyn Fn(&[u8]) -> Result<Vec<btb_vm::EntryPoint>, String> + Send + Sync>;

pub struct Server {
    addr: SocketAddr,
    stop: Arc<dyn Fn() + Send + Sync>,
    reload: ReloadFn,
    page: Arc<dyn Fn(Option<String>) + Send + Sync>,
    fields: Arc<dyn Fn(FieldHints) + Send + Sync>,
    thread: JoinHandle<()>,
}

impl Server {
    /// Start serving on `bind` (port 0 picks a free port).
    pub fn start<S: KvStore + Send + 'static>(
        elf: &[u8],
        bind: impl ToSocketAddrs,
        mut store: S,
        indexes: Vec<IndexSpec>,
    ) -> Result<Server, String> {
        let contract = Contract::from_elf(elf).map_err(|e| format!("bad contract: {e:?}"))?;
        let mut tables = Tables::new();
        let schema = register_tables(&mut tables, elf, &indexes)?;
        let bal = layout::balance_key(&CONTRACT);
        if store.get(&bal).map_err(|e| e.to_string())?.is_none() {
            store
                .put(&bal, &DEV_BALANCE.to_le_bytes())
                .map_err(|e| e.to_string())?;
        }
        let listener = TcpListener::bind(bind).map_err(|e| format!("bind: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared {
            contract: RwLock::new(Arc::new(contract)),
            page: RwLock::new(None),
            tables: RwLock::new(tables),
            schema: RwLock::new(schema),
            indexes,
            fields: RwLock::new(BTreeMap::new()),
            state: Mutex::new(State {
                occ: Occ::new(store),
                subs: SubscriptionManager::new(),
                clients: BTreeMap::new(),
                height: 1,
                admin: Vec::new(),
            }),
            stop: AtomicBool::new(false),
        });
        let sh = shared.clone();
        let thread = std::thread::spawn(move || {
            for conn in listener.incoming() {
                if sh.stop.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(conn) = conn {
                    let sh = sh.clone();
                    std::thread::spawn(move || {
                        let _ = handle(&sh, conn);
                    });
                }
            }
        });
        let (sh_reload, sh_page, sh_fields) = (shared.clone(), shared.clone(), shared.clone());
        let stop = Arc::new(move || {
            shared.stop.store(true, Ordering::SeqCst);
            // Wake the blocked accept.
            let _ = TcpStream::connect(addr);
        });
        let reload = Arc::new(move |elf: &[u8]| reload_contract(&sh_reload, elf));
        let page = Arc::new(move |p| *sh_page.page.write().unwrap() = p);
        let fields = Arc::new(move |f: FieldHints| {
            *sh_fields.fields.write().unwrap() = f.into_iter().collect();
        });
        Ok(Server {
            addr,
            stop,
            reload,
            page,
            fields,
            thread,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Swap in a new build of the contract while keeping all state. The ELF is validated first
    /// (an error leaves the old contract running). Every live subscription is then re-run
    /// against the new code and pushes its result if it changed. Returns the entry points.
    pub fn reload(&self, elf: &[u8]) -> Result<Vec<btb_vm::EntryPoint>, String> {
        (self.reload)(elf)
    }

    /// Serve this HTML at `/` instead of the built-in todo page (`None` restores it).
    pub fn set_page(&self, html: Option<String>) {
        (self.page)(html)
    }

    /// Field-type hints for the dashboard, `(table, ["addr", "bool", "string"])` (see
    /// [`parse_fields`]). Replaces earlier hints.
    pub fn set_fields(&self, fields: FieldHints) {
        (self.fields)(fields)
    }

    /// Stop accepting; open event streams end within a second.
    pub fn shutdown(self) {
        (self.stop)();
        let _ = self.thread.join();
    }

    /// Block until the server stops (forever, for the binary).
    pub fn join(self) {
        let _ = self.thread.join();
    }
}

fn reload_contract<S: KvStore + Send + 'static>(
    sh: &Arc<Shared<S>>,
    elf: &[u8],
) -> Result<Vec<btb_vm::EntryPoint>, String> {
    let contract = Contract::from_elf(elf).map_err(|e| format!("bad contract: {e:?}"))?;
    let entries = contract.entry_points().to_vec();
    // New or changed tables and indexes apply to new writes; existing rows are not re-indexed.
    let mut tables = sh.tables.read().unwrap().clone();
    let schema = register_tables(&mut tables, elf, &sh.indexes)?;
    *sh.tables.write().unwrap() = tables;
    *sh.schema.write().unwrap() = schema;
    let mut guard = sh.state.lock().unwrap();
    *sh.contract.write().unwrap() = Arc::new(contract);
    let st = &mut *guard;
    let (height, mut dead) = (st.height, Vec::new());
    let ids: Vec<SubId> = st.clients.keys().copied().collect();
    for id in ids {
        let client = &st.clients[&id];
        let c = client.caller;
        let store = st.occ.store();
        let change = st.subs.refresh(id, |_, f, a| {
            run(sh, store, FunctionKind::Query, f, a, c, height).map(|(o, _, r)| (o, r))
        });
        match change {
            Ok(Some(ch)) => {
                if client.tx.send(ch.result).is_err() {
                    dead.push(id);
                }
            }
            Ok(None) => {}
            // The new code no longer serves this query: end the stream.
            Err(_) => dead.push(id),
        }
    }
    for id in dead {
        st.subs.unsubscribe(id);
        st.clients.remove(&id);
    }
    Ok(entries)
}

fn caller_of(s: &str) -> [u8; 32] {
    match demo::unhex(s) {
        Some(b) if b.len() == 32 => b.try_into().expect("32 bytes"),
        _ => Sha256::digest(s.as_bytes()).into(),
    }
}

struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut o, mut i) = (Vec::new(), 0);
    while i < b.len() {
        match b[i] {
            b'%' if s.get(i + 1..i + 3).and_then(demo::unhex).is_some() => {
                o.push(demo::unhex(&s[i + 1..i + 3]).expect("checked")[0]);
                i += 3;
                continue;
            }
            b'+' => o.push(b' '),
            c => o.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&o).into_owned()
}

fn read_request(conn: &mut TcpStream) -> Result<Request, String> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut r = BufReader::new(conn.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    r.by_ref()
        .take(8192)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let mut it = line.split_whitespace();
    let (method, target) = (
        it.next().ok_or("empty request")?.to_string(),
        it.next().ok_or("no target")?.to_string(),
    );
    let mut headers = BTreeMap::new();
    for _ in 0..100 {
        let mut h = String::new();
        r.by_ref()
            .take(8192)
            .read_line(&mut h)
            .map_err(|e| e.to_string())?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err("body too large".into());
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).map_err(|e| e.to_string())?;
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let query = qs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect();
    Ok(Request {
        method,
        path: pct_decode(path),
        query,
        headers,
        body,
    })
}

fn respond(conn: &mut TcpStream, status: &str, ctype: &str, body: &str) -> std::io::Result<()> {
    write!(
        conn,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn err_json(conn: &mut TcpStream, status: &str, msg: &str) -> std::io::Result<()> {
    respond(
        conn,
        status,
        "application/json",
        &format!("{{\"error\":{}}}", demo::json_str(msg)),
    )
}

fn call_error(e: &CallError) -> (&'static str, String) {
    match e {
        CallError::NoSuchEntry { .. } => ("404 Not Found", "no such function".into()),
        CallError::Trap {
            trap: Trap::Abort(m),
            ..
        } => ("400 Bad Request", format!("aborted: {m}")),
        other => ("400 Bad Request", format!("{other:?}")),
    }
}

/// Run `function` as `kind`; returns the output and what it read and wrote.
fn run<S: KvStore>(
    sh: &Shared<S>,
    store: &S,
    kind: FunctionKind,
    function: &str,
    args: &[u8],
    caller: [u8; 32],
    height: u64,
) -> Result<(Vec<u8>, WriteBatch, ReadSet), (&'static str, String)> {
    let tables = sh.tables.read().unwrap();
    let mut host = RuntimeHost::new(store, &tables, CONTRACT, caller, height);
    if kind != FunctionKind::Query {
        // No beacon on a development server: marked functions get numbers from a fixed seed so
        // they can be tried out. Different on every call; not random in any security sense.
        static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut id = [0u8; 32];
        id[..8].copy_from_slice(
            &CALLS
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_le_bytes(),
        );
        id[8..].copy_from_slice(&caller[..24]);
        host.enable_dev_randomness(*b"btb.dev.randomness.not.secure!!!", id);
    }
    let contract = sh.contract.read().unwrap().clone();
    let out = contract
        .call(kind, function, args, &mut host, GAS, MEM)
        .map_err(|e| call_error(&e))?;
    let (batch, reads) = host
        .finish_tracked()
        .map_err(|f| ("500 Internal Server Error", format!("{f:?}")))?;
    Ok((out.output, batch, reads))
}

fn render(function: &str, out: &[u8], json: bool) -> Vec<u8> {
    if json {
        demo::result_json(function, out).into_bytes()
    } else {
        demo::hex(out).into_bytes()
    }
}

fn handle<S: KvStore + Send + 'static>(
    sh: &Arc<Shared<S>>,
    mut conn: TcpStream,
) -> std::io::Result<()> {
    let req = match read_request(&mut conn) {
        Ok(r) => r,
        Err(e) => return err_json(&mut conn, "400 Bad Request", &e),
    };
    if req.method == "GET" && (req.path == "/" || req.path == "/index.html") {
        let page = sh.page.read().unwrap().clone();
        let html = page.as_deref().unwrap_or(TODO_HTML);
        return respond(&mut conn, "200 OK", "text/html; charset=utf-8", html);
    }
    if req.method == "GET" && req.path == "/dashboard" {
        return respond(
            &mut conn,
            "200 OK",
            "text/html; charset=utf-8",
            admin::DASHBOARD_HTML,
        );
    }
    if req.path == "/admin" || req.path.starts_with("/admin/") {
        if req.method != "GET" {
            return err_json(
                &mut conn,
                "405 Method Not Allowed",
                "admin is read-only: use GET",
            );
        }
        if req.path == "/admin/events" {
            return admin_events(sh, &mut conn);
        }
        return match admin::route(sh, &req) {
            Ok(body) => respond(&mut conn, "200 OK", "application/json", &body),
            Err((s, m)) => err_json(&mut conn, s, &m),
        };
    }
    let Some((kind, function)) = req.path.strip_prefix('/').and_then(|p| p.split_once('/')) else {
        return err_json(&mut conn, "404 Not Found", "not found");
    };
    let (kind, want_method) = match kind {
        "mutation" => (FunctionKind::Mutation, "POST"),
        "query" | "subscribe" => (FunctionKind::Query, "GET"),
        _ => return err_json(&mut conn, "404 Not Found", "not found"),
    };
    if req.method != want_method {
        return err_json(
            &mut conn,
            "405 Method Not Allowed",
            &format!("use {want_method}"),
        );
    }
    let args = match request_args(&req) {
        Ok(a) => a,
        Err(e) => return err_json(&mut conn, "400 Bad Request", &e),
    };
    let caller = caller_of(
        req.headers
            .get("x-btb-caller")
            .or_else(|| req.query.get("caller"))
            .map_or("anonymous", String::as_str),
    );
    let json = req.query.get("format").is_some_and(|f| f == "json");
    let function = function.to_string();
    match req.path.split('/').nth(1) {
        Some("mutation") => mutation(sh, &mut conn, &function, &args, caller, json),
        Some("query") => {
            let st = sh.state.lock().unwrap();
            match run(
                sh,
                st.occ.store(),
                kind,
                &function,
                &args,
                caller,
                st.height,
            ) {
                Ok((out, _, _)) => {
                    drop(st);
                    let ctype = if json {
                        "application/json"
                    } else {
                        "text/plain"
                    };
                    respond(
                        &mut conn,
                        "200 OK",
                        ctype,
                        &String::from_utf8_lossy(&render(&function, &out, json)),
                    )
                }
                Err((s, m)) => {
                    drop(st);
                    err_json(&mut conn, s, &m)
                }
            }
        }
        _ => subscribe(sh, &mut conn, &function, args, caller, json),
    }
}

fn request_args(req: &Request) -> Result<Vec<u8>, String> {
    let is_json = req
        .headers
        .get("content-type")
        .is_some_and(|c| c.starts_with("application/json"));
    let body = String::from_utf8_lossy(&req.body);
    if is_json && !req.body.is_empty() {
        return demo::args_from_json(&body);
    }
    let text = req
        .query
        .get("args")
        .map(String::as_str)
        .unwrap_or_else(|| body.trim());
    demo::unhex(text).ok_or_else(|| "args must be hex of btb-codec bytes".to_string())
}

fn mutation<S: KvStore + Send + 'static>(
    sh: &Arc<Shared<S>>,
    conn: &mut TcpStream,
    function: &str,
    args: &[u8],
    caller: [u8; 32],
    json: bool,
) -> std::io::Result<()> {
    let mut guard = sh.state.lock().unwrap();
    let st = &mut *guard;
    let height = st.height;
    let r = st
        .occ
        .execute_with_retry::<_, (&'static str, String)>(MAX_RETRIES, |store, _| {
            let (out, batch, reads) = run(
                sh,
                store,
                FunctionKind::Mutation,
                function,
                args,
                caller,
                height,
            )?;
            Ok((out, batch, reads))
        });
    let committed = match r {
        Ok(c) => c,
        Err(OccError::Failed((s, m))) => {
            drop(guard);
            return err_json(conn, s, &m);
        }
        Err(e) => {
            drop(guard);
            return err_json(conn, "409 Conflict", &format!("{e:?}"));
        }
    };
    st.height += 1;
    if !committed.batch.is_empty() {
        let ev = admin::commit_event(st.height, &committed.batch);
        st.admin.retain(|tx| tx.send(ev.clone()).is_ok());
    }
    // Push changed results of affected subscriptions (a subscription runs as its own caller).
    let mut dead = Vec::new();
    for id in st.subs.affected(&committed.batch) {
        let Some(client) = st.clients.get(&id) else {
            continue;
        };
        let (c, height) = (client.caller, st.height);
        let store = st.occ.store();
        let change = st.subs.refresh(id, |_, f, a| {
            run(sh, store, FunctionKind::Query, f, a, c, height).map(|(o, _, r)| (o, r))
        });
        match change {
            Ok(Some(ch)) => {
                if client.tx.send(ch.result).is_err() {
                    dead.push(id);
                }
            }
            Ok(None) => {}
            Err(_) => dead.push(id),
        }
    }
    for id in dead {
        st.subs.unsubscribe(id);
        st.clients.remove(&id);
    }
    drop(guard);
    let ctype = if json {
        "application/json"
    } else {
        "text/plain"
    };
    respond(
        conn,
        "200 OK",
        ctype,
        &String::from_utf8_lossy(&render(function, &committed.output, json)),
    )
}

fn event(conn: &mut TcpStream, function: &str, out: &[u8], json: bool) -> std::io::Result<()> {
    let data = String::from_utf8_lossy(&render(function, out, json)).into_owned();
    write!(conn, "data: {data}\n\n")?;
    conn.flush()
}

fn subscribe<S: KvStore + Send + 'static>(
    sh: &Arc<Shared<S>>,
    conn: &mut TcpStream,
    function: &str,
    args: Vec<u8>,
    caller: [u8; 32],
    json: bool,
) -> std::io::Result<()> {
    let (tx, rx) = channel();
    let (id, first) = {
        let mut guard = sh.state.lock().unwrap();
        let st = &mut *guard;
        match run(
            sh,
            st.occ.store(),
            FunctionKind::Query,
            function,
            &args,
            caller,
            st.height,
        ) {
            Ok((out, _, reads)) => {
                let id = st.subs.subscribe(CONTRACT, function, &args, reads, &out);
                st.clients.insert(id, Client { caller, tx });
                (id, out)
            }
            Err((s, m)) => {
                drop(guard);
                return err_json(conn, s, &m);
            }
        }
    };
    conn.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
    )?;
    let result = (|| {
        event(conn, function, &first, json)?;
        let mut idle = 0;
        while !sh.stop.load(Ordering::SeqCst) {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(out) => {
                    idle = 0;
                    event(conn, function, &out, json)?;
                }
                Err(RecvTimeoutError::Timeout) => {
                    idle += 1;
                    if idle % 60 == 0 {
                        conn.write_all(b": ping\n\n")?;
                        conn.flush()?;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    })();
    let mut st = sh.state.lock().unwrap();
    st.subs.unsubscribe(id);
    st.clients.remove(&id);
    result
}

/// `GET /admin/events`: one `data:` event per commit that wrote anything, with the tables it
/// touched. The first event is the current height.
fn admin_events<S: KvStore + Send + 'static>(
    sh: &Arc<Shared<S>>,
    conn: &mut TcpStream,
) -> std::io::Result<()> {
    let (tx, rx) = channel();
    let height = {
        let mut st = sh.state.lock().unwrap();
        st.admin.push(tx);
        st.height
    };
    conn.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
    )?;
    write!(
        conn,
        "data: {{\"height\":{height},\"tables\":[],\"storage\":false,\"tokens\":false,\"accounts\":false}}\n\n"
    )?;
    conn.flush()?;
    let mut idle = 0;
    while !sh.stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => {
                idle = 0;
                write!(conn, "data: {ev}\n\n")?;
                conn.flush()?;
            }
            Err(RecvTimeoutError::Timeout) => {
                idle += 1;
                if idle % 60 == 0 {
                    conn.write_all(b": ping\n\n")?;
                    conn.flush()?;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}
