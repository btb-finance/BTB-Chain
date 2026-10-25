//! `btb-dev <contract.elf> [--port N] [--index Table:index:addr@offset]... [--fields Table:addr,bool,string]... [--data DIR]`
//!
//! DEV ONLY: no authentication (the caller comes from a header), no TLS, one node.
use btb_dev::{parse_fields, IndexSpec, Server};
use std::process::ExitCode;

const USAGE: &str =
    "usage: btb-dev <contract.elf> [--port N] [--index Table:index:addr@offset]... [--fields Table:addr,bool,...]... [--data DIR]\n\
  --port N     listen on 127.0.0.1:N (default 8787; 0 = pick a free port)\n\
  --index SPEC temporary schema registration, e.g. Todo:owner:addr@0 (see docs/DEV_SERVER.md);\n\
               without any --index, Todo:owner:addr@0 is registered for the todo demo\n\
  --fields SPEC dashboard decode hint, e.g. Todo:addr,bool,string (default for the demo)\n\
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

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let (mut elf, mut port, mut data) = (None, 8787u16, None);
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
            "--data" => data = Some(args.next().ok_or("--data needs a directory")?),
            "-h" | "--help" => return Err(String::new()),
            _ if a.starts_with('-') => return Err(format!("unknown flag {a}")),
            _ if elf.is_none() => elf = Some(a),
            _ => return Err("only one contract file".into()),
        }
    }
    let elf = elf.ok_or("missing contract.elf")?;
    let bytes = std::fs::read(&elf).map_err(|e| format!("read {elf}: {e}"))?;
    if indexes.is_empty() {
        indexes.push(IndexSpec::parse("Todo:owner:addr@0")?);
    }
    if fields.is_empty() {
        fields.push(parse_fields("Todo:addr,bool,string")?);
    }
    let handle = match data {
        None => Server::start(
            &bytes,
            ("127.0.0.1", port),
            btb_runtime::MemKv::new(),
            indexes,
        )?,
        #[cfg(feature = "rocksdb")]
        Some(dir) => Server::start(
            &bytes,
            ("127.0.0.1", port),
            btb_runtime::rocks::RocksKv::open(dir).map_err(|e| e.to_string())?,
            indexes,
        )?,
        #[cfg(not(feature = "rocksdb"))]
        Some(_) => return Err("--data needs a build with --features rocksdb".into()),
    };
    handle.set_fields(fields);
    println!(
        "btb-dev listening on http://{} (dev only: no auth, caller from header)",
        handle.addr()
    );
    handle.join();
    Ok(())
}
