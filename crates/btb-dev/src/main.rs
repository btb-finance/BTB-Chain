//! `btb-dev <contract.elf>... [--port N] [--index Table:index:addr@offset]... [--fields Table:addr,bool,string]... [--data DIR]`
//!
//! DEV ONLY: no authentication (the caller comes from a header), no TLS, one node.
use btb_dev::{parse_fields, IndexSpec, Server};
use std::process::ExitCode;

const USAGE: &str =
    "usage: btb-dev <contract.elf> [--port N] [--index Table:index:addr@offset]... [--fields Table:addr,bool,...]... [--data DIR]\n\
  --port N     listen on 127.0.0.1:N (default 8787; 0 = pick a free port)\n\
  --index SPEC DEPRECATED, e.g. Todo:owner:addr@0: contracts declare indexes with #[index] on\n\
               a #[btb::table] field; this only applies to tables the ELF does not declare\n\
  --fields SPEC dashboard decode hint, e.g. Todo:addr,bool,string (derived from the schema)\n\
  --auth MODE  header (default: caller from X-BTB-Caller, no checks) or signed (signed transactions\n\
               in X-BTB-Tx, session grants at POST /auth/tx)\n\
  --files DIR  keep uploaded files in DIR (default: a temporary directory)\n\
  --data DIR   keep state in RocksDB at DIR (needs --features rocksdb); default is memory";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("btb-dev: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// One contract keeps the classic address; several get stable per-name addresses.
fn start<S: btb_runtime::KvStore + Send + 'static>(
    specs: &[btb_dev::ContractSpec],
    port: u16,
    store: S,
    indexes: Vec<IndexSpec>,
    files: Option<String>,
) -> Result<Server, String> {
    if specs.len() == 1 && files.is_none() {
        return Server::start(&specs[0].elf, ("127.0.0.1", port), store, indexes);
    }
    Server::start_multi(
        specs,
        ("127.0.0.1", port),
        store,
        indexes,
        files.map(std::path::PathBuf::from),
    )
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let (mut elfs, mut port, mut data, mut files_dir) = (Vec::new(), 8787u16, None, None);
    let mut auth = btb_dev::auth::AuthMode::Header;
    let (mut indexes, mut fields) = (Vec::new(), Vec::new());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--port needs a number")?
            }
            "--index" => indexes.push(IndexSpec::parse(
                &args.next().ok_or("--index needs a spec")?,
            )?),
            "--fields" => fields.push(parse_fields(&args.next().ok_or("--fields needs a spec")?)?),
            "--auth" => {
                auth = btb_dev::auth::AuthMode::parse(&args.next().ok_or("--auth needs a mode")?)?
            }
            "--files" => files_dir = Some(args.next().ok_or("--files needs a directory")?),
            "--data" => data = Some(args.next().ok_or("--data needs a directory")?),
            "-h" | "--help" => return Err(String::new()),
            _ if a.starts_with('-') => return Err(format!("unknown flag {a}")),
            _ => elfs.push(a),
        }
    }
    if elfs.is_empty() {
        return Err("missing contract.elf".into());
    }
    let mut specs = Vec::new();
    for elf in &elfs {
        let elf_bytes = std::fs::read(elf).map_err(|e| format!("read {elf}: {e}"))?;
        let name = std::path::Path::new(elf)
            .file_stem()
            .map_or_else(|| elf.clone(), |s| s.to_string_lossy().into_owned());
        specs.push(btb_dev::ContractSpec {
            name,
            elf: elf_bytes,
        });
    }
    let bytes = specs[0].elf.clone();
    // Contracts declare their tables and indexes (`#[btb::table]`); the old todo defaults only
    // apply to a contract that declares nothing.
    let declared = !btb_runtime::schema::Schema::from_elf(&bytes)?
        .tables
        .is_empty();
    if indexes.is_empty() && !declared {
        indexes.push(IndexSpec::parse("Todo:owner:addr@0")?);
    }
    if fields.is_empty() && !declared {
        fields.push(parse_fields("Todo:addr,bool,string")?);
    }
    let handle = match data {
        None => start(&specs, port, btb_runtime::MemKv::new(), indexes, files_dir)?,
        #[cfg(feature = "rocksdb")]
        Some(dir) => start(
            &specs,
            port,
            btb_runtime::rocks::RocksKv::open(dir).map_err(|e| e.to_string())?,
            indexes,
            files_dir,
        )?,
        #[cfg(not(feature = "rocksdb"))]
        Some(_) => return Err("--data needs a build with --features rocksdb".into()),
    };
    handle.set_fields(fields);
    handle.set_auth(auth);
    println!(
        "btb-dev listening on http://{} (dev only: no auth, caller from header)",
        handle.addr()
    );
    handle.join();
    Ok(())
}
