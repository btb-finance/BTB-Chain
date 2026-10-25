//! `btb-host`: run a BTB Chain hosting node.
//!
//! ```text
//! btb-host --genesis genesis.json --validator /ip4/.../udp/9000/quic-v1/p2p/<id> \
//!          [--validator ...] [--http 127.0.0.1:8788] [--default-contract <hex>] \
//!          [--listen /ip4/0.0.0.0/udp/9100/quic-v1] [--serve-followers] \
//!          [--relayer <address hex> --relay-rpc HOST:PORT [--relay-rpc ...]]
//! ```

use btb_host::{Host, HostConfig};
use btb_node::config::GenesisFile;
use std::path::PathBuf;

fn main() {
    if let Err(e) = run() {
        eprintln!("btb-host: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut genesis: Option<PathBuf> = None;
    let mut validators = Vec::new();
    let mut http = "127.0.0.1:8788".to_string();
    let mut listen: Option<String> = None;
    let mut default_contract = None;
    let mut serve = false;
    let mut poll = None;
    let (mut relayer, mut relay_rpc) = (None, Vec::new());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--genesis" => genesis = Some(val("--genesis")?.into()),
            "--validator" => validators.push(val("--validator")?),
            "--http" => http = val("--http")?,
            "--listen" => listen = Some(val("--listen")?),
            "--default-contract" => {
                default_contract = Some(btb_node::hex::decode32(&val("--default-contract")?)?)
            }
            "--poll-ms" => poll = Some(val("--poll-ms")?.parse().map_err(|_| "--poll-ms: number")?),
            "--serve-followers" => serve = true,
            "--relayer" => relayer = Some(btb_node::hex::decode32(&val("--relayer")?)?),
            "--relay-rpc" => relay_rpc.push(val("--relay-rpc")?),
            "-h" | "--help" => {
                println!("{}", include_str!("usage.txt"));
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    let genesis = GenesisFile::load(&genesis.ok_or("--genesis <file> is required")?)?;
    if validators.is_empty() {
        return Err("at least one --validator <multiaddr/p2p/id> is required".into());
    }
    let mut cfg = HostConfig::new(
        genesis,
        validators,
        http.parse().map_err(|e| format!("--http: {e}"))?,
    );
    if let Some(l) = listen {
        cfg.listen = l;
    }
    cfg.default_contract = default_contract;
    if let Some(p) = poll {
        cfg.poll_ms = p;
    }
    if serve {
        cfg.serve_followers = Some(Default::default());
    }
    match (relayer, relay_rpc.is_empty()) {
        (Some(relayer), false) => {
            cfg.relay = Some(btb_host::RelayConfig {
                relayer,
                rpc: relay_rpc,
            })
        }
        (None, true) => {}
        _ => return Err("--relayer and --relay-rpc go together".into()),
    }
    let host = Host::start(cfg)?;
    println!("btb-host listening on http://{}", host.http_addr());
    host.run_until_stopped();
    host.shutdown();
    Ok(())
}
