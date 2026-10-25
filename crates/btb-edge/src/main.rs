//! `btb-edge`: run an edge caching node. See `docs/EDGE.md`.

use btb_chain::{account_address, SigningKey};
use btb_edge::chain::{ChainStorage, HttpRpc, Rpc, RpcChain, RpcSubmitter, Submitter};
use btb_edge::pay::PayConfig;
use btb_edge::upstream::HttpUpstream;
use btb_edge::{Edge, EdgeConfig, Services};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "btb-edge --upstream HOST:PORT[,HOST:PORT..] [--listen 127.0.0.1:8090]
         [--rpc URL[,URL..]] [--mem-mb 256] [--disk-dir DIR --disk-mb 4096]
         [--site-ttl-secs 5] [--max-object-mb 1024] [--max-subscriptions 1024]
         [--key FILE --price UNITS [--free-requests 100 --free-window-secs 3600]
          [--redeem-min UNITS] [--redeem-every-secs 3600]]
         [--relayer ADDRESS_HEX]

--upstream  hosting nodes, tried in order with failover
--rpc       node JSON-RPC: storage lookups for large files and payment channels
--relayer   relay client-signed transactions (POST /tx) to the --rpc nodes; clients sign this
            account into the transaction as relayer and it earns 20% of the fee. Register the
            account on chain with a Host transaction first.
--key       the edge's account key file ({\"secret\": hex}); vouchers must pay its address.
            With --price, requests beyond the free quota need an X-BTB-Voucher header.";

fn main() {
    if let Err(e) = run() {
        eprintln!("btb-edge: {e}");
        std::process::exit(1);
    }
}

fn split(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

fn run() -> Result<(), String> {
    let mut upstream = Vec::new();
    let mut rpc_urls = Vec::new();
    let mut listen = "127.0.0.1:8090".to_string();
    let (mut mem_mb, mut disk_mb, mut disk_dir) = (256usize, 4096u64, None::<String>);
    let (mut ttl, mut max_mb, mut max_subs) = (5u64, 1024usize, 1024usize);
    let (mut key, mut price) = (None::<String>, None::<u64>);
    let mut relayer = None::<[u8; 32]>;
    let (mut free, mut window, mut redeem_min, mut redeem_every) = (100u64, 3600u64, 1u64, 3600u64);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |n: &str| args.next().ok_or(format!("{n} needs a value"));
        fn num<T: std::str::FromStr>(n: &str, v: String) -> Result<T, String> {
            v.parse().map_err(|_| format!("{n}: not a number"))
        }
        match a.as_str() {
            "--upstream" => upstream.extend(split(&val("--upstream")?)),
            "--rpc" => rpc_urls.extend(split(&val("--rpc")?)),
            "--listen" => listen = val("--listen")?,
            "--mem-mb" => mem_mb = num("--mem-mb", val("--mem-mb")?)?,
            "--disk-dir" => disk_dir = Some(val("--disk-dir")?),
            "--disk-mb" => disk_mb = num("--disk-mb", val("--disk-mb")?)?,
            "--site-ttl-secs" => ttl = num("--site-ttl-secs", val("--site-ttl-secs")?)?,
            "--max-object-mb" => max_mb = num("--max-object-mb", val("--max-object-mb")?)?,
            "--max-subscriptions" => {
                max_subs = num("--max-subscriptions", val("--max-subscriptions")?)?
            }
            "--relayer" => relayer = Some(btb_storage::unhex32(&val("--relayer")?)?),
            "--key" => key = Some(val("--key")?),
            "--price" => price = Some(num("--price", val("--price")?)?),
            "--free-requests" => free = num("--free-requests", val("--free-requests")?)?,
            "--free-window-secs" => window = num("--free-window-secs", val("--free-window-secs")?)?,
            "--redeem-min" => redeem_min = num("--redeem-min", val("--redeem-min")?)?,
            "--redeem-every-secs" => {
                redeem_every = num("--redeem-every-secs", val("--redeem-every-secs")?)?
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            o => return Err(format!("unknown argument {o:?} (try --help)")),
        }
    }
    if upstream.is_empty() {
        return Err("at least one --upstream is required".into());
    }
    let mut cfg = EdgeConfig::new(listen.parse().map_err(|e| format!("--listen: {e}"))?);
    cfg.mem_bytes = mem_mb << 20;
    cfg.disk = disk_dir.map(|d| (d.into(), disk_mb << 20));
    cfg.site_ttl = Duration::from_secs(ttl);
    cfg.max_object_bytes = max_mb << 20;
    cfg.max_subscriptions = max_subs;
    let up = HttpUpstream::new(
        upstream,
        Duration::from_secs(30),
        Duration::from_secs(5),
        cfg.max_object_bytes,
    );
    let rpc: Option<Arc<dyn Rpc>> = (!rpc_urls.is_empty())
        .then(|| Arc::new(HttpRpc::new(rpc_urls, Duration::from_secs(30))) as Arc<dyn Rpc>);
    if let Some(relayer) = relayer {
        if rpc.is_none() {
            return Err("--relayer needs --rpc (transactions are forwarded to the node)".into());
        }
        cfg.relay = Some(btb_edge::RelayConfig {
            relayer,
            chain_id: None,
        });
    }
    let mut svc = Services {
        upstream: Arc::new(up),
        storage: rpc
            .clone()
            .map(|r| Arc::new(ChainStorage(Arc::new(RpcChain(r)))) as _),
        rpc: rpc.clone(),
        submitter: None,
    };
    if let Some(price) = price {
        let rpc = rpc.ok_or("--price needs --rpc (channels are looked up on chain)")?;
        let file = key.ok_or("--price needs --key")?;
        let text = std::fs::read_to_string(&file).map_err(|e| format!("read {file}: {e}"))?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("key: {e}"))?;
        let secret = btb_storage::unhex32(v["secret"].as_str().ok_or("key file has no secret")?)?;
        let sk = SigningKey::from_bytes(&secret);
        let payee = account_address(&sk.verifying_key().to_bytes());
        cfg.pay = Some(PayConfig {
            payee,
            chain_id: None,
            free_requests: free,
            window: Duration::from_secs(window),
            price,
            redeem_min,
        });
        cfg.redeem_every = Some(Duration::from_secs(redeem_every));
        svc.submitter = Some(Arc::new(RpcSubmitter { rpc, key: sk }) as Arc<dyn Submitter>);
        println!("payee: {}", btb_storage::hex(&payee));
    }
    let edge = Edge::start(cfg, svc)?;
    println!("btb-edge listening on http://{}", edge.addr());
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
