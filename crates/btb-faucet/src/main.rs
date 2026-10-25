//! `btb-faucet --key faucet.key --rpc 127.0.0.1:8545 [--bind 127.0.0.1:8080] ...`
//! See docs/TESTNET.md.
use btb_faucet::{limits::LimitConfig, load_key, server, to_hex, Config, Faucet, UNITS_PER_BTB};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "usage: btb-faucet --key FILE --rpc HOST:PORT [--rpc HOST:PORT ...] [options]\n\
\n\
  --key FILE               faucet key file ({\"secret\": hex}, as written by `btb-node init-testnet`)\n\
  --rpc HOST:PORT          node JSON-RPC endpoint; repeat for failover (default 127.0.0.1:8545)\n\
  --bind ADDR              listen address (default 127.0.0.1:8080; put a reverse proxy in front)\n\
  --amount BTB             default drip (default 100)\n\
  --max-amount BTB         largest drip a request may ask for (default 1000)\n\
  --address-cooldown SECS  one drip per address per this many seconds (default 86400)\n\
  --ip-max N               drips per IP per window (default 10)\n\
  --ip-window SECS         window for --ip-max (default 86400)\n\
  --pow-bits N             proof-of-work difficulty in leading zero bits, 0 = off (default 0)\n\
  --trust-proxy            take the client IP from the last X-Forwarded-For entry";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("btb-faucet: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut key, mut rpc) = (None, Vec::new());
    let mut cfg = Config::new(Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        let num = |v: String| v.parse::<u64>().map_err(|_| format!("{a} needs a number"));
        match a.as_str() {
            "--key" => key = Some(val()?),
            "--rpc" => rpc.push(val()?),
            "--bind" => {
                cfg.bind = val()?
                    .parse::<SocketAddr>()
                    .map_err(|e| format!("--bind: {e}"))?
            }
            "--amount" => cfg.default_amount = num(val()?)?.saturating_mul(UNITS_PER_BTB),
            "--max-amount" => cfg.max_amount = num(val()?)?.saturating_mul(UNITS_PER_BTB),
            "--address-cooldown" => cfg.limits.address_cooldown = Duration::from_secs(num(val()?)?),
            "--ip-max" => cfg.limits.ip_max = num(val()?)? as usize,
            "--ip-window" => cfg.limits.ip_window = Duration::from_secs(num(val()?)?),
            "--pow-bits" => cfg.pow_bits = num(val()?)?.min(40) as u32,
            "--trust-proxy" => cfg.trust_proxy = true,
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    let _: &LimitConfig = &cfg.limits;
    if rpc.is_empty() {
        rpc.push("127.0.0.1:8545".into());
    }
    cfg.rpc = rpc;
    if cfg.default_amount > cfg.max_amount {
        return Err("--amount is larger than --max-amount".into());
    }
    let key = load_key(std::path::Path::new(&key.ok_or("--key is required")?))?;
    let bind = cfg.bind;
    if !bind.ip().is_loopback() {
        eprintln!(
            "btb-faucet: warning: {bind} is not loopback; there is no TLS here, run behind a reverse proxy"
        );
    }
    let faucet = Arc::new(Faucet::new(cfg, key));
    let handle = server::serve(faucet.clone(), bind).map_err(|e| format!("bind {bind}: {e}"))?;
    println!(
        "btb-faucet: listening on http://{}, account {}",
        handle.local_addr(),
        to_hex(&faucet.address())
    );
    handle.wait();
    Ok(())
}
