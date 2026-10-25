//! BTB Chain hosting node (`docs/HOSTING.md`).
//!
//! A hosting node is not a validator. It follows the chain **without trusting its peers**: it
//! block-syncs from validators and checks every response against the validator set it derived
//! itself by executing the chain (`btb_node::sync::verify`), then executes every block to
//! rebuild the state. From that state it serves
//!
//! * read-only queries and live subscriptions, with the HTTP shape of `btb-dev`;
//! * Merkle proofs of storage keys, with the header and consensus evidence of the block that
//!   names the state root ([`proof`]);
//! * static websites that contract deployers publish on chain ([`btb_chain::site`]).
//!
//! Hosting nodes earn query fees through payment channels (`docs/TOKENOMICS.md`); this crate
//! serves the data, not the fee protocol yet.

mod build;
mod http;
pub mod proof;
mod site;
mod state;

pub use state::{HostState, Recording};

use btb_chain::Address;
use btb_node::config::{GenesisFile, NodeParams};
use btb_node::net::FollowerPolicy;
use btb_node::Node;
use ed25519_dalek::SigningKey;
use state::Shared;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Settings of a hosting node.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub genesis: GenesisFile,
    /// Validators to sync from: `/ip4/.../udp/.../quic-v1/p2p/<peer id>`. The peer id pins the
    /// validator's key, which must be in the genesis set (or a later epoch's).
    pub validators: Vec<String>,
    /// HTTP bind address. The API is read-only and public; put TLS and rate limits in front.
    pub http: SocketAddr,
    /// libp2p listen address (QUIC). Needed only to relay sync to followers of this node.
    pub listen: String,
    /// Network identity. `None`: a fresh random key (a hosting node has no validator key).
    pub key: Option<SigningKey>,
    /// Contract answered at the unprefixed `/query/<fn>` and `/subscribe/<fn>`.
    pub default_contract: Option<Address>,
    /// How often a caught-up node asks for new blocks, in milliseconds.
    pub poll_ms: u64,
    /// Serve block sync to followers of this node (opt-in, see `docs/HOSTING.md`).
    pub serve_followers: Option<FollowerPolicy>,
    /// Concurrent HTTP connections; more get a 503.
    pub max_connections: usize,
}

impl HostConfig {
    pub fn new(genesis: GenesisFile, validators: Vec<String>, http: SocketAddr) -> Self {
        HostConfig {
            genesis,
            validators,
            http,
            listen: "/ip4/127.0.0.1/udp/0/quic-v1".into(),
            key: None,
            default_contract: None,
            poll_ms: 100,
            serve_followers: None,
            max_connections: 256,
        }
    }
}

/// A running hosting node.
pub struct Host {
    node: Arc<Node>,
    shared: Arc<Shared>,
    http_addr: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl Host {
    pub fn start(cfg: HostConfig) -> Result<Host, String> {
        let key = cfg.key.clone().unwrap_or_else(|| {
            use rand::RngCore;
            let mut s = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut s);
            SigningKey::from_bytes(&s)
        });
        let kv = Recording::new();
        let written = kv.written();
        let state: HostState = btb_node::open_chain(kv, &cfg.genesis)?;
        let shared = Arc::new(Shared::new(
            written,
            cfg.default_contract,
            cfg.max_connections,
        ));
        let mut params = NodeParams::new(cfg.genesis.clone(), key);
        params.listen = cfg.listen.clone();
        params.peers = cfg.validators.clone();
        params.serve_followers = cfg.serve_followers;
        params.sync_poll_ms = Some(cfg.poll_ms.max(10));
        let hook_shared = shared.clone();
        let hook: btb_node::driver::BlockHook =
            Box::new(move |any, block| hook_shared.on_block(any, block));
        let node = Arc::new(Node::start_with_hook(params, state, Some(hook))?);

        let listener =
            TcpListener::bind(cfg.http).map_err(|e| format!("bind {}: {e}", cfg.http))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let http_addr = listener.local_addr().map_err(|e| e.to_string())?;
        let ctx = Arc::new(http::Ctx {
            node: node.clone(),
            shared: shared.clone(),
        });
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("btb-host-http".into())
            .spawn(move || {
                while !sh.stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            let _ = conn.set_nonblocking(false);
                            let ctx = ctx.clone();
                            let _ = std::thread::Builder::new()
                                .name("btb-host-conn".into())
                                .spawn(move || http::handle(&ctx, conn));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Host {
            node,
            shared,
            http_addr,
            thread: Some(thread),
        })
    }

    /// Where the HTTP API listens.
    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// The underlying node (status, network handle).
    pub fn node(&self) -> &Node {
        &self.node
    }

    /// Height of the last block this node executed.
    pub fn height(&self) -> u64 {
        self.shared.head.load(Ordering::SeqCst)
    }

    /// Subscriptions currently open.
    pub fn subscriptions(&self) -> usize {
        self.shared.subs.lock().map_or(0, |s| s.clients.len())
    }

    /// Block until Ctrl-C.
    pub fn run_until_stopped(&self) {
        self.node.run_until_stopped();
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}
