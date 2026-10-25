//! The `btb` command line: `init`, `build`, `dev` (hot reload) and `deploy`. See docs/CLI.md.
//!
//! Everything is a plain function here so tests can drive it; `main.rs` only parses arguments.

pub mod chain;
pub mod storage;

use btb_dev::{IndexSpec, Server};
use btb_vm::{Contract, EntryPoint};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// The RISC-V target contracts are built for.
pub const TARGET: &str = "riscv64imac-unknown-none-elf";

const T_CARGO: &str = include_str!("../templates/Cargo.toml");
const T_CONFIG: &str = include_str!("../templates/cargo-config.toml");
const T_LINK: &str = include_str!("../templates/link.ld");
const T_TOOLCHAIN: &str = include_str!("../templates/rust-toolchain.toml");
const T_MAIN: &str = include_str!("../templates/main.rs");
const T_VSCODE: &str = include_str!("../templates/vscode-settings.json");
const T_VSCODE_EXT: &str = include_str!("../templates/vscode-extensions.json");
const T_RA: &str = include_str!("../templates/rust-analyzer.toml");
const T_INDEX: &str = include_str!("../templates/index.html");
const T_GITIGNORE: &str = "/target\n";

/// Where the generated project finds `btb-sdk`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SdkSource {
    /// A directory containing the `btb-sdk` crate (relative paths are relative to the project).
    Path(String),
    /// A git repository containing `btb-sdk` (cargo finds the crate by name).
    Git(String),
}

impl SdkSource {
    /// The `BTB_SDK_PATH` variable if set, else the `crates/btb-sdk` of the checkout this `btb`
    /// was built from.
    pub fn default_source() -> SdkSource {
        match std::env::var("BTB_SDK_PATH") {
            Ok(p) if !p.is_empty() => SdkSource::Path(p),
            _ => SdkSource::Path(concat!(env!("CARGO_MANIFEST_DIR"), "/../btb-sdk").to_string()),
        }
    }

    fn dependency(&self) -> String {
        match self {
            SdkSource::Path(p) => format!(
                "btb-sdk = {{ path = {}, features = [\"runtime\"] }}",
                toml_str(p)
            ),
            SdkSource::Git(u) => format!(
                "btb-sdk = {{ git = {}, features = [\"runtime\"] }}",
                toml_str(u)
            ),
        }
    }
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Scaffold a contract project in `dir/<name>` (`dir` is created if missing). Fails if the
/// target directory exists and is not empty. Returns the project directory.
pub fn init(dir: &Path, name: &str, sdk: &SdkSource) -> Result<PathBuf, String> {
    let valid = name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !valid {
        return Err(format!(
            "bad project name {name:?}: use lowercase letters, digits, '-' and '_', starting with a letter"
        ));
    }
    let root = dir.join(name);
    if root.exists() && fs::read_dir(&root).map_err(io)?.next().is_some() {
        return Err(format!(
            "{} already exists and is not empty",
            root.display()
        ));
    }
    let sub = |t: &str| {
        t.replace("{{name}}", name)
            .replace("{{sdk}}", &sdk.dependency())
    };
    let files: [(&str, String); 11] = [
        ("Cargo.toml", sub(T_CARGO)),
        (".cargo/config.toml", T_CONFIG.into()),
        ("link.ld", T_LINK.into()),
        ("rust-toolchain.toml", T_TOOLCHAIN.into()),
        (".vscode/settings.json", T_VSCODE.into()),
        (".vscode/extensions.json", T_VSCODE_EXT.into()),
        ("rust-analyzer.toml", T_RA.into()),
        ("src/main.rs", sub(T_MAIN)),
        ("web/index.html", sub(T_INDEX)),
        (".gitignore", T_GITIGNORE.into()),
        ("README.md", format!("# {name}\n\nA BTB contract.\n\n```\nbtb dev      # build, serve and hot reload at http://127.0.0.1:8787/\nbtb build    # target/btb/{name}.elf\nbtb deploy   # bundle in target/btb/deploy/\n```\n")),
    ];
    for (rel, text) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().expect("has parent")).map_err(io)?;
        fs::write(&p, text).map_err(|e| format!("write {}: {e}", p.display()))?;
    }
    Ok(root)
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// The `name = "..."` of the `[package]` section of `<project>/Cargo.toml`.
pub fn package_name(project: &Path) -> Result<String, String> {
    let text = read_manifest(project)?;
    let mut in_pkg = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_pkg = l == "[package]";
        } else if in_pkg {
            if let Some(v) = l.strip_prefix("name").map(str::trim_start) {
                if let Some(v) = v.strip_prefix('=') {
                    return Ok(v.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    Err(format!(
        "no [package] name in {}/Cargo.toml",
        project.display()
    ))
}

fn read_manifest(project: &Path) -> Result<String, String> {
    let p = project.join("Cargo.toml");
    fs::read_to_string(&p).map_err(|e| {
        format!(
            "{}: {e} (run btb inside a project made by `btb init`)",
            p.display()
        )
    })
}

/// The strings of `[package.metadata.btb] <key> = [...]`.
fn metadata_list(project: &Path, key: &str) -> Result<Vec<String>, String> {
    let text = read_manifest(project)?;
    let (mut in_sec, mut in_list, mut out) = (false, false, Vec::new());
    for line in text.lines() {
        let l = line.split('#').next().unwrap_or("").trim();
        if l.starts_with('[') && !in_list {
            in_sec = l == "[package.metadata.btb]";
            continue;
        }
        if in_sec && l.starts_with(key) && l[key.len()..].trim_start().starts_with('=') {
            in_list = true;
        }
        if in_list {
            out.extend(l.split('"').skip(1).step_by(2).map(String::from));
            if l.contains(']') {
                in_list = false;
            }
        }
    }
    Ok(out)
}

/// The index declarations of `[package.metadata.btb] indexes = [...]` (Table:index:kind@offset).
pub fn project_indexes(project: &Path) -> Result<Vec<IndexSpec>, String> {
    metadata_list(project, "indexes")?
        .iter()
        .map(|s| IndexSpec::parse(s))
        .collect()
}

/// The dashboard field hints of `[package.metadata.btb] fields = ["Table:addr,bool,string"]`.
pub fn project_fields(project: &Path) -> Result<Vec<(String, Vec<String>)>, String> {
    metadata_list(project, "fields")?
        .iter()
        .map(|s| btb_dev::parse_fields(s))
        .collect()
}

/// A built and validated contract.
pub struct Built {
    pub name: String,
    pub elf_path: PathBuf,
    pub elf: Vec<u8>,
    pub entry_points: Vec<EntryPoint>,
    /// `(function, every_blocks)` from the `#[btb::cron(every = "N blocks")]` marker symbols.
    pub crons: Vec<(String, u64)>,
}

/// Cron schedules from the `btb_every_<N>_<name>` marker symbols, sorted by function.
/// Each must name an existing cron entry point and be within the chain's limits.
pub fn cron_schedules(elf: &[u8], entries: &[EntryPoint]) -> Result<Vec<(String, u64)>, String> {
    let mut out: Vec<(String, u64)> = Vec::new();
    let syms = btb_vm::elf::export_list(elf).map_err(|e| format!("invalid contract: {e:?}"))?;
    for (sym, _) in syms {
        let Some(rest) = sym.strip_prefix("btb_every_") else {
            continue;
        };
        let (n, name) = rest
            .split_once('_')
            .ok_or_else(|| format!("bad schedule symbol {sym:?}"))?;
        let every: u64 = n
            .parse()
            .map_err(|_| format!("bad schedule symbol {sym:?}"))?;
        if !(1..=MAX_CRON_EVERY).contains(&every) {
            return Err(format!(
                "cron {name:?}: every must be 1..={MAX_CRON_EVERY} blocks"
            ));
        }
        if !entries
            .iter()
            .any(|e| e.name == name && format!("{:?}", e.kind) == "Cron")
        {
            return Err(format!("schedule for {name:?} but no cron entry point"));
        }
        if out.iter().any(|(f, _)| f == name) {
            return Err(format!("cron {name:?} scheduled twice"));
        }
        out.push((name.to_string(), every));
    }
    if out.len() > MAX_CRONS {
        return Err(format!("at most {MAX_CRONS} scheduled crons"));
    }
    out.sort();
    Ok(out)
}

/// Same limits as `btb-chain`'s `Manifest::validate`.
const MAX_CRON_EVERY: u64 = 1 << 32;
const MAX_CRONS: usize = 16;

/// Run `cargo build --release` for the RISC-V target (same environment as the examples'
/// build.sh), copy the ELF to `target/btb/<name>.elf` and validate it with the VM's loader.
pub fn build(project: &Path) -> Result<Built, String> {
    let name = package_name(project)?;
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release"])
        .current_dir(project)
        // Env flags would override .cargo/config.toml (the rv64im feature flags live there).
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTC_BOOTSTRAP", "1")
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err(format!(
            "cargo build failed ({status}); is the {TARGET} target installed (rustup target add {TARGET})?"
        ));
    }
    let built = project
        .join("target")
        .join(TARGET)
        .join("release")
        .join(&name);
    let elf = fs::read(&built).map_err(|e| format!("read {}: {e}", built.display()))?;
    let contract = Contract::from_elf(&elf).map_err(|e| format!("invalid contract: {e:?}"))?;
    let out_dir = project.join("target/btb");
    fs::create_dir_all(&out_dir).map_err(io)?;
    let elf_path = out_dir.join(format!("{name}.elf"));
    fs::write(&elf_path, &elf).map_err(io)?;
    let entry_points = contract.entry_points().to_vec();
    let crons = cron_schedules(&elf, &entry_points)?;
    Ok(Built {
        name,
        elf_path,
        elf,
        entry_points,
        crons,
    })
}

/// `kind name` per entry point, e.g. `query list_mine`.
pub fn describe(entries: &[EntryPoint]) -> Vec<String> {
    entries
        .iter()
        .map(|e| format!("{} {}", kind_name(e), e.name))
        .collect()
}

fn kind_name(e: &EntryPoint) -> String {
    format!("{:?}", e.kind).to_lowercase()
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The manifest JSON of a deploy bundle.
pub fn manifest_json(b: &Built, indexes: &[IndexSpec]) -> String {
    let entries: Vec<String> = b
        .entry_points
        .iter()
        .map(|e| {
            format!(
                "    {{\"kind\": {}, \"name\": {}}}",
                json_str(&kind_name(e)),
                json_str(&e.name)
            )
        })
        .collect();
    let crons: Vec<String> = b
        .crons
        .iter()
        .map(|(f, n)| format!("{{\"function\": {}, \"every_blocks\": {n}}}", json_str(f)))
        .collect();
    let idx: Vec<String> = indexes
        .iter()
        .map(|i| {
            format!(
                "    {}",
                json_str(&format!(
                    "{}:{}:{}@{}",
                    i.table,
                    i.index,
                    if i.addr { "addr" } else { "u64" },
                    i.offset
                ))
            )
        })
        .collect();
    format!(
        "{{\n  \"name\": {},\n  \"elf\": {},\n  \"size\": {},\n  \"code_hash\": {},\n  \"hash_algorithm\": \"sha256\",\n  \"entry_points\": [\n{}\n  ],\n  \"crons\": [{}],\n  \"indexes\": [{}{}{}]\n}}\n",
        json_str(&b.name),
        json_str(&format!("{}.elf", b.name)),
        b.elf.len(),
        json_str(&sha256_hex(&b.elf)),
        entries.join(",\n"),
        crons.join(", "),
        if idx.is_empty() { "" } else { "\n" },
        idx.join(",\n"),
        if idx.is_empty() { "" } else { "\n  " },
    )
}

/// Build, then write the deploy bundle (`<name>.elf` and `manifest.json`) to
/// `target/btb/deploy/`. There is no chain to deploy to yet (Phase 4).
pub fn deploy(project: &Path) -> Result<(Built, PathBuf), String> {
    let b = build(project)?;
    let indexes = project_indexes(project)?;
    let dir = project.join("target/btb/deploy");
    fs::create_dir_all(&dir).map_err(io)?;
    fs::write(dir.join(format!("{}.elf", b.name)), &b.elf).map_err(io)?;
    fs::write(dir.join("manifest.json"), manifest_json(&b, &indexes)).map_err(io)?;
    Ok((b, dir))
}

/// Fingerprint of everything that should trigger a rebuild: mtime and size of `src/**`,
/// `Cargo.toml` and `web/index.html`.
fn fingerprint(project: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, SystemTime, u64)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            match e.metadata() {
                Ok(m) if m.is_dir() => walk(&p, out),
                Ok(m) => out.push((p, m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len())),
                Err(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&project.join("src"), &mut out);
    for f in ["Cargo.toml", "web/index.html"] {
        if let Ok(m) = fs::metadata(project.join(f)) {
            out.push((
                project.join(f),
                m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                m.len(),
            ));
        }
    }
    out.sort();
    out
}

/// A running `btb dev`: the server plus a thread that polls the sources and hot reloads.
pub struct DevSession {
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    watcher: JoinHandle<()>,
    /// Number of successful reloads so far (the first build does not count).
    reloads: Arc<std::sync::atomic::AtomicU64>,
}

impl DevSession {
    /// Build the project, start the server on `bind` and begin watching. `poll` is how often
    /// the sources are checked. Rebuild errors are printed and the old contract keeps running.
    pub fn start(
        project: &Path,
        bind: impl std::net::ToSocketAddrs,
        poll: Duration,
        data: Option<&str>,
    ) -> Result<DevSession, String> {
        let project = project.to_path_buf();
        let mut indexes = project_indexes(&project)?;
        if indexes.is_empty() {
            eprintln!(
                "btb: no [package.metadata.btb] indexes in Cargo.toml; no index is registered"
            );
        }
        let before = fingerprint(&project);
        let b = build(&project)?;
        print_entries(&b);
        let server = match data {
            None => Server::start(
                &b.elf,
                bind,
                btb_runtime::MemKv::new(),
                std::mem::take(&mut indexes),
            )?,
            #[cfg(feature = "rocksdb")]
            Some(d) => Server::start(
                &b.elf,
                bind,
                btb_runtime::rocks::RocksKv::open(d).map_err(|e| e.to_string())?,
                std::mem::take(&mut indexes),
            )?,
            #[cfg(not(feature = "rocksdb"))]
            Some(_) => return Err("--data needs a build with --features rocksdb".into()),
        };
        load_page(&server, &project);
        server.set_fields(project_fields(&project)?);
        let server = Arc::new(server);
        let stop = Arc::new(AtomicBool::new(false));
        let reloads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (s, st, rl) = (server.clone(), stop.clone(), reloads.clone());
        let watcher = std::thread::spawn(move || {
            let mut last = before;
            while !st.load(Ordering::SeqCst) {
                std::thread::sleep(poll);
                let now = fingerprint(&project);
                if now == last {
                    continue;
                }
                // Fingerprint first: edits made while building trigger another round.
                last = now;
                eprintln!("btb: change detected, rebuilding...");
                match build(&project) {
                    Ok(b) => match s.reload(&b.elf) {
                        Ok(_) => {
                            load_page(&s, &project);
                            if let Ok(f) = project_fields(&project) {
                                s.set_fields(f);
                            }
                            rl.fetch_add(1, Ordering::SeqCst);
                            eprintln!("btb: reloaded (state kept)");
                            print_entries(&b);
                        }
                        Err(e) => eprintln!("btb: reload failed, keeping the old build: {e}"),
                    },
                    Err(e) => eprintln!("btb: build failed, keeping the old build: {e}"),
                }
            }
        });
        Ok(DevSession {
            server,
            stop,
            watcher,
            reloads,
        })
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.server.addr()
    }

    /// How many hot reloads have succeeded.
    pub fn reload_count(&self) -> u64 {
        self.reloads.load(Ordering::SeqCst)
    }

    /// Block until the watcher ends (forever, for the binary).
    pub fn join(self) {
        let _ = self.watcher.join();
    }

    /// Stop watching and serving.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.watcher.join();
        if let Ok(server) = Arc::try_unwrap(self.server) {
            server.shutdown();
        }
    }
}

fn load_page(server: &Server, project: &Path) {
    server.set_page(fs::read_to_string(project.join("web/index.html")).ok());
}

/// Print the entry points of a build.
pub fn print_entries(b: &Built) {
    println!("{} ({} bytes), entry points:", b.name, b.elf.len());
    for l in describe(&b.entry_points) {
        println!("  {l}");
    }
}
