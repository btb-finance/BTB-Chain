//! `btb init <name> | build | codegen | dev | deploy`, see docs/CLI.md.
use btb_cli::{chain, deploy, deploy_all, init, print_entries, sha256_hex, DevSession, SdkSource};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage: btb <command>\n\
  btb init <name> [--sdk-path DIR | --sdk-git URL]   create a contract project in ./<name>\n\
  btb build                                          build target/btb/<name>.elf and list entry points\n\
  btb codegen --ts DIR                               build and write the typed TypeScript client (api.ts, api.js, api.d.ts) to DIR\n\
  btb dev [--port N] [--data DIR]                    build, serve and hot reload (default port 8787)\n\
  btb deploy [--rpc URL --key FILE [--gas-limit N]] [--immutable | --upgrade-authority ADDR]   write a deploy bundle; with --rpc deploy it to a node\n\
  btb upgrade --rpc URL --key FILE <contract> [--budget-gas N] [--gas-limit N]   build and upgrade a deployed contract (migrations and index backfills run on chain; budget prepaid)\n\
  btb upgrade --rpc URL <contract> --status          progress of the latest upgrade (migration phase, rows done, gas)\n\
  btb call --rpc URL --key FILE [--gas-limit N] [--relayer HOST_URL] <contract> <fn> [args]   send a mutation (--relayer: submit via a hosting node, which earns 20% of the fee)\n\
  btb query --rpc URL <contract> <fn> [args]         run a read-only query\n\
  btb keygen <FILE>                                  write a new account key file\n\
  btb store <file> --rpc URL --key FILE [--k N --m N --periods N]   erasure-code a file onto storage nodes\n\
  btb fetch <root> --rpc URL [--out FILE]            fetch and verify a stored file\n\
  btb storage-node run --rpc URL --key FILE --dir DIR [--listen ADDR] [--endpoint URL] [--deposit UNITS]\n\
\n\
Run build, dev and deploy inside a project. The sdk path defaults to $BTB_SDK_PATH, else the\n\
crates/btb-sdk of the checkout this btb was built from. See docs/CLI.md.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("btb: {e}");
            }
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().ok_or("")?;
    let rest: Vec<String> = args.collect();
    let project = PathBuf::from(".");
    match cmd.as_str() {
        "init" => {
            let (mut name, mut sdk) = (None, SdkSource::default_source());
            let mut it = rest.into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--sdk-path" => {
                        sdk = SdkSource::Path(it.next().ok_or("--sdk-path needs a directory")?)
                    }
                    "--sdk-git" => sdk = SdkSource::Git(it.next().ok_or("--sdk-git needs a URL")?),
                    _ if a.starts_with('-') => return Err(format!("unknown flag {a}")),
                    _ if name.is_none() => name = Some(a),
                    _ => return Err("only one project name".into()),
                }
            }
            let name = name.ok_or("missing project name")?;
            let root = init(&PathBuf::from("."), &name, &sdk)?;
            println!("created {}\nnext: cd {name} && btb dev", root.display());
            Ok(())
        }
        "build" => {
            no_args(&rest)?;
            let dirs = btb_cli::project_contracts(&project)?;
            let dirs = if dirs.is_empty() { vec![project] } else { dirs };
            for d in dirs {
                let b = btb_cli::build(&d)?;
                print_entries(&b);
                println!("wrote {}", b.elf_path.display());
            }
            Ok(())
        }
        "codegen" => {
            let mut out = None;
            let mut it = rest.into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--ts" => out = Some(it.next().ok_or("--ts needs an output directory")?),
                    _ => return Err(format!("unexpected argument {a}")),
                }
            }
            let out = out.ok_or("usage: btb codegen --ts <out-dir>")?;
            let (files, warnings) = btb_cli::codegen_ts(&project, &PathBuf::from(out))?;
            for w in warnings {
                eprintln!("btb: warning: {w}");
            }
            for f in files {
                println!("wrote {}", f.display());
            }
            Ok(())
        }
        "deploy" => {
            let f = Flags::parse(rest)?;
            if !f.positional.is_empty() {
                return Err(format!("unexpected argument {}", f.positional[0]));
            }
            let bundles = deploy_all(&project)?;
            for (b, dir) in &bundles {
                print_entries(b);
                println!("code hash (sha256): {}", sha256_hex(&b.elf));
                println!("wrote deploy bundle to {}", dir.display());
            }
            match f.rpc {
                None => {
                    if f.key.is_some() || f.gas_limit.is_some() {
                        return Err("--key and --gas-limit need --rpc".into());
                    }
                    println!("note: pass --rpc URL --key FILE to deploy this bundle to a node.");
                }
                Some(_) => {
                    let s = f.session()?;
                    for (b, dir) in &bundles {
                        let addr = s.deploy_bundle(dir, f.gas_limit)?;
                        println!(
                            "deployed: contract {} ({})",
                            chain::hex_encode(&addr),
                            b.name
                        );
                    }
                }
            }
            Ok(())
        }
        "upgrade" => {
            let f = Flags::parse(rest)?;
            let [c] = f.positional.as_slice() else {
                return Err(
                    "usage: btb upgrade --rpc URL --key FILE <contract> [--budget-gas N]".into(),
                );
            };
            let contract = chain::parse_address(c)?;
            if f.status {
                let rpc = f.rpc.as_deref().ok_or("--rpc is required")?;
                let r = chain::Rpc::new(rpc).call(
                    "btb_upgradeStatus",
                    serde_json::json!([chain::hex_encode(&contract)]),
                )?;
                println!(
                    "{}",
                    if r.is_null() {
                        "no upgrade job".into()
                    } else {
                        serde_json::to_string_pretty(&r).unwrap_or_default()
                    }
                );
                return Ok(());
            }
            let (b, dir) = deploy(&project)?;
            print_entries(&b);
            println!("code hash (sha256): {}", sha256_hex(&b.elf));
            let s = f.session()?;
            let rc = s.upgrade_bundle(contract, &dir, f.budget_gas.unwrap_or(5_000_000))?;
            println!("upgraded: contract {}", chain::hex_encode(&contract));
            println!("gas_used {} height {}", rc["gas_used"], rc["height"]);
            let st = s.upgrade_status(contract)?;
            if !st.is_null() {
                println!(
                    "migration: {} (btb upgrade <contract> --rpc URL --status)",
                    st["phase"]
                );
            }
            Ok(())
        }
        "keygen" => {
            let f = Flags::parse(rest)?;
            let [path] = f.positional.as_slice() else {
                return Err("usage: btb keygen <FILE>".into());
            };
            let addr = chain::keygen(&PathBuf::from(path))?;
            println!("wrote {path}\naccount {}", chain::hex_encode(&addr));
            Ok(())
        }
        "call" | "query" => {
            let f = Flags::parse(rest)?;
            let (Some(c), Some(func)) = (f.positional.first(), f.positional.get(1)) else {
                return Err(format!("usage: btb {cmd} --rpc URL <contract> <fn> [args]"));
            };
            if f.positional.len() > 3 {
                return Err("too many arguments".into());
            }
            let contract = chain::parse_address(c)?;
            let args = chain::parse_args(f.positional.get(2).map_or("", String::as_str))?;
            if cmd == "call" {
                let rc = f.session()?.call(
                    contract,
                    func,
                    args,
                    f.gas_limit.unwrap_or(chain::DEFAULT_CALL_GAS),
                )?;
                println!(
                    "output {}\ngas_used {}\nheight {}",
                    rc["output"].as_str().unwrap_or(""),
                    rc["gas_used"],
                    rc["height"]
                );
            } else {
                let rpc = f.rpc.as_deref().ok_or("--rpc is required")?;
                let r = chain::Rpc::new(rpc).call(
                    "btb_query",
                    serde_json::json!([
                        chain::hex_encode(&contract),
                        func,
                        chain::hex_encode(&args)
                    ]),
                )?;
                println!(
                    "output {}\ngas_used {}",
                    r["output"].as_str().unwrap_or(""),
                    r["gas_used"]
                );
            }
            Ok(())
        }
        "dev" => {
            let (mut port, mut data) = (8787u16, None);
            let mut it = rest.into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--port" => {
                        port = it
                            .next()
                            .and_then(|v| v.parse().ok())
                            .ok_or("--port needs a number")?
                    }
                    "--data" => data = Some(it.next().ok_or("--data needs a directory")?),
                    _ => return Err(format!("unknown argument {a}")),
                }
            }
            let s = DevSession::start(
                &project,
                ("127.0.0.1", port),
                Duration::from_millis(500),
                data.as_deref(),
            )?;
            println!(
                "btb dev: http://{}/ (dev only: no auth). Edit src/ to hot reload; Ctrl-C to stop.",
                s.addr()
            );
            if s.contracts().len() > 1 {
                for (name, addr) in s.contracts() {
                    println!(
                        "  contract {name}: {}  (/c/{name}/...)",
                        chain::hex_encode(&addr)
                    );
                }
                println!("  dashboard: http://{}/dashboard", s.addr());
            }
            s.join();
            Ok(())
        }
        "store" | "fetch" | "storage-node" => btb_cli::storage::run(&cmd, rest),
        "-h" | "--help" | "help" => Err(String::new()),
        other => Err(format!("unknown command {other}")),
    }
}

fn no_args(rest: &[String]) -> Result<(), String> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(format!("unexpected argument {}", rest[0]))
    }
}

/// `--rpc URL --key FILE --gas-limit N` plus positional arguments, in any order.
#[derive(Default)]
struct Flags {
    rpc: Option<String>,
    key: Option<String>,
    /// A hosting or edge node to submit through (it earns the relay share of the fee).
    relayer: Option<String>,
    gas_limit: Option<u64>,
    /// `btb upgrade`: gas prepaid for migrations and index backfills.
    budget_gas: Option<u64>,
    status: bool,
    /// `btb deploy`: nobody can upgrade the contract / this account can.
    immutable: bool,
    authority: Option<String>,
    positional: Vec<String>,
}

impl Flags {
    fn parse(args: Vec<String>) -> Result<Flags, String> {
        let mut f = Flags::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--rpc" => f.rpc = Some(it.next().ok_or("--rpc needs a URL")?),
                "--key" => f.key = Some(it.next().ok_or("--key needs a file")?),
                "--relayer" => {
                    f.relayer = Some(it.next().ok_or("--relayer needs a hosting node URL")?)
                }
                "--budget-gas" => {
                    f.budget_gas = Some(
                        it.next()
                            .and_then(|v| v.parse().ok())
                            .ok_or("--budget-gas needs a number")?,
                    )
                }
                "--status" => f.status = true,
                "--immutable" => f.immutable = true,
                "--upgrade-authority" => {
                    f.authority = Some(it.next().ok_or("--upgrade-authority needs an address")?)
                }
                "--gas-limit" => {
                    f.gas_limit = Some(
                        it.next()
                            .and_then(|v| v.parse().ok())
                            .ok_or("--gas-limit needs a number")?,
                    )
                }
                _ if a.starts_with("--") => return Err(format!("unknown flag {a}")),
                _ => f.positional.push(a),
            }
        }
        Ok(f)
    }

    fn session(&self) -> Result<chain::Session, String> {
        let rpc = self.rpc.as_deref().ok_or("--rpc is required")?;
        let key = self.key.as_deref().ok_or("--key is required")?;
        let s = chain::Session::open(rpc, &PathBuf::from(key))?;
        match &self.relayer {
            Some(host) => s.via_relay(host),
            None => Ok(s),
        }
    }
}
