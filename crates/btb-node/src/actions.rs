//! Node side of HTTPS actions (`docs/ORACLES.md`, section 3).
//!
//! For every pending request on the chain (`btb_chain::actions`), a validator
//!
//! 1. makes the HTTP request itself, under the SSRF rules of [`FetchPolicy`],
//! 2. runs the contract's `btb_transform_<name>` on the response (pure: no state, no clock, no
//!    randomness; out of gas or a trap means "transform failed"),
//! 3. signs the transformed bytes (`ActionVote`) and gossips the vote; the block proposer carries
//!    the votes into a block, where the chain counts them (`Executor`, `run_actions`).
//!
//! The work runs on a worker thread per request (at most [`FetchPolicy::max_inflight`] at a time)
//! so the driver thread never waits for the network.
//!
//! # What a validator refuses to do (SSRF)
//!
//! * Talk to anything but `https`, unless `allow_http` (tests, local setups).
//! * Connect to a private, loopback, link-local, multicast, reserved or otherwise non-public
//!   address, unless `allow_private`. The check runs on the addresses the connection will actually
//!   use (our own resolver hands them to the HTTP client), so DNS rebinding between the check and
//!   the connect is impossible; IPv4-mapped IPv6 and NAT64 forms are unwrapped first.
//! * Follow a redirect to a host that is not in the contract's allowlist, or to a different scheme
//!   than policy allows, or more than `max_redirects` of them.
//! * Read more than the request's `max_response_bytes` or wait longer than its `timeout_ms`.
//!
//! Failures are signed too, as `STATUS_FAILED` with one of a few fixed texts ([`FetchError::text`])
//! so that validators which hit the same problem still agree byte for byte.

use btb_chain::actions::{
    host_allowed, parse_url, ActionVote, Pending, MAX_RESULT_BYTES, MAX_VOTES_PER_BLOCK,
    STATUS_FAILED, STATUS_OK,
};
use btb_chain::Hash;
use btb_codec::http::{HttpRequest, HttpResponse, METHOD_POST};
use btb_codec::Encode;
use btb_vm::Contract;
use ed25519_dalek::SigningKey;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// How a validator fetches (the chain's allowlist and caps come on top of this).
#[derive(Debug, Clone)]
pub struct FetchPolicy {
    /// Allow plain `http://` URLs.
    pub allow_http: bool,
    /// Allow private, loopback and other non-public addresses. Tests and local setups only.
    pub allow_private: bool,
    /// Most redirects followed (each target must be on the contract's allowlist).
    pub max_redirects: u8,
    /// Gas limit of the transform function.
    pub transform_gas: u64,
    /// VM memory of the transform.
    pub vm_memory: usize,
    /// Requests fetched at the same time.
    pub max_inflight: usize,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        FetchPolicy {
            allow_http: false,
            allow_private: false,
            max_redirects: 3,
            transform_gas: 20_000_000,
            vm_memory: 1 << 20,
            max_inflight: 16,
        }
    }
}

impl FetchPolicy {
    /// The relaxed policy for tests: plain HTTP and local addresses allowed.
    pub fn test_mode() -> Self {
        FetchPolicy {
            allow_http: true,
            allow_private: true,
            ..Default::default()
        }
    }
}

/// Why a fetch did not produce a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// The policy or the allowlist forbids it (address, scheme, redirect target).
    Blocked,
    Timeout,
    TooLarge,
    /// Anything else: refused, reset, TLS failure, bad redirect, no HTTP support built in.
    Failed,
}

impl FetchError {
    /// The text validators sign: coarse on purpose, so that they agree on a failure.
    pub fn text(self) -> &'static str {
        match self {
            FetchError::Blocked => "blocked",
            FetchError::Timeout => "timeout",
            FetchError::TooLarge => "response too large",
            FetchError::Failed => "fetch failed",
        }
    }
}

// ---------------------------------------------------------------- addresses

/// Is `ip` an address a validator must not connect to on a contract's behalf?
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => blocked_v4(a),
        IpAddr::V6(a) => blocked_v6(a),
    }
}

fn blocked_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    a.is_unspecified()
        || a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_broadcast()
        || a.is_multicast()
        || o[0] == 0
        // 100.64.0.0/10 carrier-grade NAT
        || (o[0] == 100 && (o[1] & 0xC0) == 64)
        // 192.0.0.0/24 IETF protocol assignments, 192.0.2.0/24 TEST-NET-1
        || (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
        // 198.18.0.0/15 benchmarking, 198.51.100.0/24 TEST-NET-2
        || (o[0] == 198 && ((o[1] & 0xFE) == 18 || (o[1] == 51 && o[2] == 100)))
        // 203.0.113.0/24 TEST-NET-3
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        // 240.0.0.0/4 reserved
        || o[0] >= 240
}

fn blocked_v6(a: Ipv6Addr) -> bool {
    if let Some(v4) = a.to_ipv4_mapped() {
        return blocked_v4(v4);
    }
    let s = a.segments();
    // NAT64 64:ff9b::/96 and the local-use 64:ff9b:1::/48: the embedded address decides / blocked
    if s[0] == 0x64 && s[1] == 0xff9b {
        return s[2] != 0 || s[3] != 0 || s[4] != 0 || s[5] != 0 || {
            let v4 = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
            blocked_v4(v4)
        };
    }
    a.is_unspecified()
        || a.is_loopback()
        || a.is_multicast()
        // fc00::/7 unique local, fe80::/10 link local, fec0::/10 site local (deprecated)
        || (s[0] & 0xFE00) == 0xFC00
        || (s[0] & 0xFFC0) == 0xFE80
        || (s[0] & 0xFFC0) == 0xFEC0
        // 2001:db8::/32 documentation, 2001::/32 Teredo, 2002::/16 6to4 (embeds an IPv4 address)
        || (s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0))
        || s[0] == 0x2002
        // ::/8 reserved (includes IPv4-compatible forms)
        || (s[0] >> 8) == 0
}

// ---------------------------------------------------------------- fetching

#[cfg(feature = "https")]
fn resolve_checked(
    netloc: &str,
    allow_private: bool,
) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let all: Vec<_> = netloc.to_socket_addrs()?.collect();
    if allow_private {
        return Ok(all);
    }
    let ok: Vec<_> = all.into_iter().filter(|a| !is_blocked_ip(a.ip())).collect();
    if ok.is_empty() {
        // The marker text is what `classify` looks for.
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "btb-blocked-address",
        ));
    }
    Ok(ok)
}

#[cfg(feature = "https")]
struct CheckedResolver {
    allow_private: bool,
}

#[cfg(feature = "https")]
impl ureq::Resolver for CheckedResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        resolve_checked(netloc, self.allow_private)
    }
}

/// The URL `loc` (a `Location` header) names, relative to `base`.
fn join_url(base: &str, loc: &str) -> Option<String> {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return Some(loc.to_string());
    }
    let scheme_end = base.find("://")? + 3;
    let rest = &base[scheme_end..];
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let origin = &base[..scheme_end + auth_end];
    if let Some(r) = loc.strip_prefix("//") {
        return Some(format!("{}://{r}", &base[..scheme_end - 3]));
    }
    if loc.starts_with('/') {
        return Some(format!("{origin}{loc}"));
    }
    if loc.is_empty() || loc.starts_with('?') || loc.starts_with('#') {
        return None;
    }
    let path = &base[scheme_end + auth_end..];
    let dir = path
        .split(['?', '#'])
        .next()
        .and_then(|p| p.rfind('/').map(|i| &p[..=i]))
        .unwrap_or("/");
    Some(format!("{origin}{dir}{loc}"))
}

/// Make the request: allowlist and scheme checked before every hop, redirects followed only to
/// allowlisted hosts, the connection only to public addresses (unless the policy says
/// otherwise), time and size capped by the request's own limits.
#[cfg(feature = "https")]
pub fn fetch(
    req: &HttpRequest,
    hosts: &[String],
    policy: &FetchPolicy,
) -> Result<HttpResponse, FetchError> {
    use std::io::Read;
    let max = req.max_response_bytes as usize;
    let deadline = Instant::now() + Duration::from_millis(u64::from(req.timeout_ms));
    let agent_for = |left: Duration| {
        ureq::AgentBuilder::new()
            .timeout(left)
            .redirects(0)
            .user_agent("btb-validator")
            .resolver(CheckedResolver {
                allow_private: policy.allow_private,
            })
            .build()
    };
    let mut url = req.url.clone();
    let mut method_post = req.method == METHOD_POST;
    let mut hops = 0u8;
    loop {
        let parsed = parse_url(&url).map_err(|_| FetchError::Blocked)?;
        if !host_allowed(hosts, &parsed) || (!parsed.https && !policy.allow_http) {
            return Err(FetchError::Blocked);
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(FetchError::Timeout)?;
        let agent = agent_for(left);
        let mut r = if method_post {
            agent.post(&url)
        } else {
            agent.get(&url)
        };
        for (n, v) in &req.headers {
            r = r.set(n, v);
        }
        let result = if method_post {
            r.send_bytes(&req.body)
        } else {
            r.call()
        };
        let resp = match result {
            Ok(r) => r,
            // 4xx and 5xx are answers like any other: the transform decides what they mean
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => return Err(classify(&t)),
        };
        let status = resp.status();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let Some(loc) = resp.header("location").and_then(|l| join_url(&url, l)) else {
                return Err(FetchError::Failed);
            };
            hops += 1;
            if hops > policy.max_redirects {
                return Err(FetchError::Failed);
            }
            if (301..=303).contains(&status) {
                method_post = false;
            }
            url = loc;
            continue;
        }
        let mut body = Vec::new();
        resp.into_reader()
            .take(max as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                    FetchError::Timeout
                }
                _ => FetchError::Failed,
            })?;
        if body.len() > max {
            return Err(FetchError::TooLarge);
        }
        return Ok(HttpResponse { status, body });
    }
}

#[cfg(feature = "https")]
fn classify(t: &ureq::Transport) -> FetchError {
    use std::error::Error;
    let mut src: Option<&(dyn Error + 'static)> = Some(t);
    while let Some(e) = src {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.to_string().contains("btb-blocked-address") {
                return FetchError::Blocked;
            }
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return FetchError::Timeout;
            }
        }
        src = e.source();
    }
    FetchError::Failed
}

/// Built without the `https` feature: every fetch fails, identically on every such node.
#[cfg(not(feature = "https"))]
pub fn fetch(
    _req: &HttpRequest,
    _hosts: &[String],
    _policy: &FetchPolicy,
) -> Result<HttpResponse, FetchError> {
    Err(FetchError::Failed)
}

// ---------------------------------------------------------------- the whole answer

/// Run the contract's transform on `resp` (or, without one, take the body of a 2xx response).
fn transform(
    code: &[u8],
    name: &str,
    resp: &HttpResponse,
    policy: &FetchPolicy,
) -> Result<Vec<u8>, String> {
    let contract = Contract::from_elf(code).map_err(|_| "contract invalid".to_string())?;
    if contract.has_transform(name) {
        return contract
            .call_transform(name, &resp.encode(), policy.transform_gas, policy.vm_memory)
            .map(|o| o.output)
            .map_err(|_| "transform failed".to_string());
    }
    if (200..300).contains(&resp.status) {
        Ok(resp.body.clone())
    } else {
        Err(format!("http status {}", resp.status))
    }
}

/// Fetch, transform and size-check: the `(status, data)` this validator signs.
pub fn perform(p: &Pending, code: &[u8], policy: &FetchPolicy) -> (u8, Vec<u8>) {
    fn fail(text: &str) -> (u8, Vec<u8>) {
        (STATUS_FAILED, text.as_bytes().to_vec())
    }
    let Some(req) = p.http() else {
        return fail("bad request");
    };
    let resp = match fetch(&req, &p.hosts, policy) {
        Ok(r) => r,
        Err(e) => return fail(e.text()),
    };
    match transform(code, &p.function, &resp, policy) {
        Ok(data) if data.len() <= MAX_RESULT_BYTES => (STATUS_OK, data),
        Ok(_) => fail("result too large"),
        Err(m) => fail(&m),
    }
}

// ---------------------------------------------------------------- the driver's bookkeeping

/// Votes are kept this long while their request is unknown or already settled.
const VOTE_TTL: Duration = Duration::from_secs(60);
/// Most votes held at once (bounded memory against a flood).
const MAX_POOL: usize = 4096;

/// Everything the driver keeps for HTTPS actions: which requests this validator has started,
/// the worker threads' results and the votes waiting to go into a block.
pub struct ActionNode {
    chain_id: u64,
    key: SigningKey,
    policy: FetchPolicy,
    started: BTreeSet<Hash>,
    inflight: usize,
    done_tx: Sender<ActionVote>,
    done_rx: Receiver<ActionVote>,
    pool: BTreeMap<(Hash, [u8; 32]), (Instant, ActionVote)>,
    /// Votes that failed verification or came from a non-validator (statistics).
    pub rejected: u64,
}

impl ActionNode {
    pub fn new(chain_id: u64, key: SigningKey, policy: FetchPolicy) -> Self {
        let (done_tx, done_rx) = channel();
        ActionNode {
            chain_id,
            key,
            policy,
            started: BTreeSet::new(),
            inflight: 0,
            done_tx,
            done_rx,
            pool: BTreeMap::new(),
            rejected: 0,
        }
    }

    pub fn inflight(&self) -> usize {
        self.inflight
    }

    pub fn pooled(&self) -> usize {
        self.pool.len()
    }

    /// Start a worker for every pending request this validator has not answered yet.
    /// `code_of` gives the contract code of a request.
    pub fn start(
        &mut self,
        pending: &[(Hash, Pending)],
        code_of: impl Fn(&Pending) -> Option<Vec<u8>>,
    ) {
        for (id, p) in pending {
            if self.inflight >= self.policy.max_inflight.max(1) {
                break;
            }
            if self.started.contains(id) {
                continue;
            }
            self.started.insert(*id);
            let (id, p) = (*id, p.clone());
            let code = code_of(&p);
            let (key, policy, chain_id, tx) = (
                self.key.clone(),
                self.policy.clone(),
                self.chain_id,
                self.done_tx.clone(),
            );
            self.inflight += 1;
            std::thread::spawn(move || {
                let (status, data) = match code {
                    Some(code) => perform(&p, &code, &policy),
                    None => (STATUS_FAILED, b"contract missing".to_vec()),
                };
                let _ = tx.send(ActionVote::sign(chain_id, &key, id, status, data));
            });
        }
    }

    /// Collect finished answers. Returns the new votes (to gossip).
    pub fn drain(&mut self) -> Vec<ActionVote> {
        let mut out = Vec::new();
        while let Ok(v) = self.done_rx.try_recv() {
            self.inflight = self.inflight.saturating_sub(1);
            self.pool
                .insert((v.request, v.key), (Instant::now(), v.clone()));
            out.push(v);
        }
        out
    }

    /// A vote from the network: kept if it verifies and comes from a validator of the epoch.
    pub fn on_vote(&mut self, v: ActionVote, validators: &[[u8; 32]]) -> bool {
        if !validators.contains(&v.key) || !v.verify(self.chain_id) {
            self.rejected += 1;
            return false;
        }
        if self.pool.contains_key(&(v.request, v.key)) || self.pool.len() >= MAX_POOL {
            return false;
        }
        self.pool.insert((v.request, v.key), (Instant::now(), v));
        true
    }

    /// Votes for the next proposal: those for requests still pending whose validator has no vote
    /// recorded on chain yet (`recorded(request)` lists the validators that have one).
    pub fn for_proposal(
        &self,
        pending: &BTreeSet<Hash>,
        recorded: impl Fn(&Hash) -> BTreeSet<[u8; 32]>,
    ) -> Vec<ActionVote> {
        let mut by_request: BTreeMap<Hash, BTreeSet<[u8; 32]>> = BTreeMap::new();
        let mut out = Vec::new();
        for ((id, key), (_, v)) in &self.pool {
            if !pending.contains(id) {
                continue;
            }
            let done = by_request.entry(*id).or_insert_with(|| recorded(id));
            if !done.contains(key) {
                out.push(v.clone());
                if out.len() >= MAX_VOTES_PER_BLOCK {
                    break;
                }
            }
        }
        out
    }

    /// Forget requests that are settled and votes nobody can use any more.
    pub fn prune(&mut self, pending: &BTreeSet<Hash>) {
        let now = Instant::now();
        self.started.retain(|id| pending.contains(id));
        self.pool
            .retain(|(id, _), (t, _)| pending.contains(id) || now.duration_since(*t) < VOTE_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_and_special_addresses_are_blocked() {
        for s in [
            "127.0.0.1",
            "127.255.255.254",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "0.0.0.0",
            "0.1.2.3",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.7",
            "203.0.113.9",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} must be blocked");
        }
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "172.15.0.1",
            "172.32.0.1",
            "100.63.0.1",
            "100.128.0.1",
            "198.17.0.1",
            "198.20.0.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2a00:1450:4001::200e",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s} is public");
        }
    }

    #[test]
    fn redirect_locations_are_joined_to_the_current_url() {
        let b = "https://a.example:8443/x/y/z?q=1#f";
        assert_eq!(
            join_url(b, "https://b.example/p").unwrap(),
            "https://b.example/p"
        );
        assert_eq!(join_url(b, "/root").unwrap(), "https://a.example:8443/root");
        assert_eq!(
            join_url(b, "next").unwrap(),
            "https://a.example:8443/x/y/next"
        );
        assert_eq!(join_url(b, "//c.example/q").unwrap(), "https://c.example/q");
        assert_eq!(
            join_url("http://a.example", "k").unwrap(),
            "http://a.example/k"
        );
        assert!(join_url(b, "").is_none());
        assert!(join_url(b, "?only=query").is_none());
    }

    #[test]
    fn failure_texts_are_fixed() {
        let all = [
            FetchError::Blocked,
            FetchError::Timeout,
            FetchError::TooLarge,
            FetchError::Failed,
        ];
        let texts: BTreeSet<&str> = all.iter().map(|e| e.text()).collect();
        assert_eq!(texts.len(), 4);
    }
}
