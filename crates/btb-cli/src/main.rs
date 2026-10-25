//! `btb init <name> | build | dev | deploy`, see docs/CLI.md.
use btb_cli::{deploy, init, print_entries, sha256_hex, DevSession, SdkSource};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage: btb <command>\n\
  btb init <name> [--sdk-path DIR | --sdk-git URL]   create a contract project in ./<name>\n\
  btb build                                          build target/btb/<name>.elf and list entry points\n\
  btb dev [--port N] [--data DIR]                    build, serve and hot reload (default port 8787)\n\
  btb deploy                                         write a deploy bundle to target/btb/deploy/\n\
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
            let b = btb_cli::build(&project)?;
            print_entries(&b);
            println!("wrote {}", b.elf_path.display());
            Ok(())
        }
        "deploy" => {
            no_args(&rest)?;
            let (b, dir) = deploy(&project)?;
            print_entries(&b);
            println!("code hash (sha256): {}", sha256_hex(&b.elf));
            println!("wrote deploy bundle to {}", dir.display());
            println!("note: no chain exists yet; deploying to a network arrives in Phase 4.");
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
            s.join();
            Ok(())
        }
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
