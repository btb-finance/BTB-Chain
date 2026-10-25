//! HTTPS actions: the chain side (`docs/ORACLES.md`, section 3).
//!
//! An `#[btb::action]` contract function is not a transaction. A user sends an
//! [`ActionOp::Request`] (transaction tag 13): the chain runs the action's first phase read-only,
//! gets the [`HttpRequest`] it returns, checks it (host allowlist, sizes, headers), escrows the
//! caller's `max_fee` and stores a **pending request**. Validators then each make the request
//! off-chain, run the contract's `btb_transform_<name>` on their response and sign the
//! transformed result ([`ActionVote`], gossiped, then carried in blocks in `OracleInput::actions`).
//! When validators holding at least two thirds of the stake signed identical bytes, the chain
//! calls the contract's `<name>_callback` mutation with them in that very block. If the votes can
//! no longer reach two thirds the callback gets `STATUS_DISAGREEMENT`; if the deadline passes
//! first it gets `STATUS_TIMEOUT`. Either way the request ends: it is removed and the escrow is
//! settled.
//!
//! # State (tag `0x51`)
//!
//! | Key | Value |
//! | --- | --- |
//! | `51 01 contract` | the contract's host allowlist (`Vec<String>`), set by its deployer |
//! | `51 02` | number of pending requests, `u64` LE |
//! | `51 10 id` | the pending request ([`Pending`]); `id` is the id of the request transaction |
//! | `51 20 id key` | `outcome_hash` of the validator `key`'s vote (the first vote wins) |
//! | `51 30 id outcome_hash` | `status:u8 \|\| data` of an outcome some validator voted for |
//!
//! # Security argument
//!
//! * **Only validators speak.** A vote must carry a valid strict Ed25519 signature by the
//!   consensus key of a validator of the current epoch, over
//!   `SHA256("btb.action.vote.v1" || chain_id || request_id || status || SHA256(data))`.
//!   The chain id and the request id are in the digest, so a vote cannot be replayed on another
//!   chain or for another request, and once a request ends its votes are inert (the request is
//!   gone). A second vote by the same validator on a request is ignored (no changing one's mind).
//! * **Agreement is stake weighted and byte exact.** A result is delivered only if validators
//!   holding `>= 2/3` of the epoch's stake voted for the same `(status, data)`. With less than a
//!   third of the stake faulty, that result is one an honest validator computed.
//! * **No result is forced by a proposer.** The proposer can only omit votes (delaying, at worst
//!   to the deadline) or include genuine ones. Garbage is skipped, never an error, so a block with
//!   junk votes still executes.
//! * **Callbacks are system only.** The callback runs with caller [`CALLER`]; a transaction that
//!   names `<action>_callback` is refused (`ActionError::CallbackIsSystemOnly` at the call
//!   site in `exec.rs`), so nobody can feed a contract a made-up result.
//! * **Bounded state and work.** At most [`MAX_PENDING`] requests, at most
//!   [`MAX_PENDING_PER_REQUESTER`] per account; a result is at most
//!   [`MAX_RESULT_BYTES`]; a block takes at most [`MAX_VOTES_PER_BLOCK`] votes; every request ends
//!   within [`DEADLINE_BLOCKS`].

use crate::state::StateError;
use crate::tx::{Address, Hash};
use btb_codec::http::{HttpRequest, METHOD_GET, METHOD_POST};
use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_runtime::kv::{KvError, KvStore};
use btb_runtime::layout::prefix_successor;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

pub const TAG_ACTION: u8 = 0x51;
const SUB_HOSTS: u8 = 0x01;
const SUB_COUNT: u8 = 0x02;
const SUB_PENDING: u8 = 0x10;
const SUB_VOTE: u8 = 0x20;
const SUB_OUTCOME: u8 = 0x30;

/// Most requests waiting at once (one chain-wide queue).
pub const MAX_PENDING: u64 = 256;
/// Most requests one account may have waiting, so one account cannot hold the whole queue.
pub const MAX_PENDING_PER_REQUESTER: usize = 16;
/// Blocks a request has to gather its votes. Every request ends by then.
pub const DEADLINE_BLOCKS: u64 = 50;
/// Largest transformed result a validator may sign.
pub const MAX_RESULT_BYTES: usize = 1024;
/// Votes a block may carry.
pub const MAX_VOTES_PER_BLOCK: usize = 512;
pub const MAX_HOSTS: usize = 16;
pub const MAX_HOST_BYTES: usize = 128;
pub const MAX_URL_BYTES: usize = 2048;
pub const MAX_HEADERS: usize = 16;
pub const MAX_HEADER_BYTES: usize = 512;
pub const MAX_BODY_BYTES: usize = 4096;
pub const MAX_ARGS_BYTES: usize = 4096;
/// Caps a request may ask for; 0 means the default.
pub const MAX_RESPONSE_CAP: u32 = 1 << 20;
pub const MAX_TIMEOUT_MS: u32 = 10_000;
pub const DEFAULT_RESPONSE_BYTES: u32 = 16 * 1024;
pub const DEFAULT_TIMEOUT_MS: u32 = 5_000;

/// Gas of the request transaction before the action's first phase runs (plus per byte).
pub const REQUEST_GAS: u64 = 20_000;
/// Gas of a `SetHosts` transaction, plus [`HOSTS_BYTE_GAS`] per byte of hosts.
pub const HOSTS_GAS: u64 = 10_000;
pub const HOSTS_BYTE_GAS: u64 = 100;
/// What each validator that answered earns, in gas (times the gas price). The caller's escrow
/// must cover this for every validator of the epoch plus the callback's gas limit.
pub const RESPONSE_GAS: u64 = 100_000;

/// Callback status codes (`status` argument of `<name>_callback`).
pub const STATUS_OK: u8 = 0;
/// At least two thirds of the stake agree the request failed (the data is a short text).
pub const STATUS_FAILED: u8 = 1;
pub const STATUS_DISAGREEMENT: u8 = 2;
pub const STATUS_TIMEOUT: u8 = 3;

/// The caller address a callback runs with. No account or contract hashes to it.
pub const CALLER: Address = *b"btb.action.callback.caller.v1\0\0\0";

/// Where requests' escrows are held until the request ends.
pub fn escrow_pool() -> Address {
    crate::sha256(&[b"btb.pool.v1", b"actions-escrow"])
}

/// Why an action transaction was refused (the gas used is still charged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    NoSuchContract,
    /// `SetHosts` by someone who did not deploy the contract.
    NotDeployer,
    /// The contract has no `btb_action_<function>`.
    NoSuchAction,
    /// The action has no `<function>_callback` mutation to deliver to.
    NoCallback,
    /// The action's first phase returned something that is not an `HttpRequest`.
    BadRequestSpec,
    /// The URL, headers or body are not acceptable (the text says which).
    BadRequest(String),
    /// The URL's host is not in the contract's allowlist.
    HostNotAllowed(String),
    BadHosts(String),
    /// `max_fee` is below what the escrow must cover.
    EscrowTooLow {
        needed: u64,
        max_fee: u64,
    },
    TooManyPending,
    /// The chain has no validator set to answer (cannot happen on a staking chain).
    NoValidators,
    /// A transaction tried to run the `<action>_callback` of an action: only the delivery of an
    /// agreed result may.
    CallbackIsSystemOnly,
}

/// An action transaction (tag 13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOp {
    /// Ask the validators to run `function` (an action) of `contract` with `args`. `gas_limit`
    /// covers the transaction and the action's first phase; `max_fee` is escrowed to pay the
    /// validators and the callback (the unused part is refunded).
    Request {
        contract: Address,
        function: String,
        args: Vec<u8>,
        gas_limit: u64,
        max_fee: u64,
    },
    /// Set the hosts the contract's actions may call (deployer only). Entries are `host`,
    /// `host:port` or `*.suffix`.
    SetHosts {
        contract: Address,
        hosts: Vec<String>,
    },
}

impl ActionOp {
    /// Gas charged before the VM runs.
    pub fn gas(&self) -> u64 {
        match self {
            ActionOp::Request { function, args, .. } => REQUEST_GAS.saturating_add(
                crate::CALL_BYTE_GAS.saturating_mul((function.len() + args.len()) as u64),
            ),
            ActionOp::SetHosts { hosts, .. } => HOSTS_GAS.saturating_add(
                HOSTS_BYTE_GAS.saturating_mul(hosts.iter().map(|h| h.len() as u64).sum()),
            ),
        }
    }

    /// The most gas the transaction can use.
    pub fn gas_limit(&self) -> u64 {
        match self {
            ActionOp::Request { gas_limit, .. } => *gas_limit,
            _ => self.gas(),
        }
    }

    /// Funds the transaction moves out of the sender's balance besides the gas fee.
    pub fn debit(&self) -> u64 {
        match self {
            ActionOp::Request { max_fee, .. } => *max_fee,
            ActionOp::SetHosts { .. } => 0,
        }
    }

    /// Wire form: `sub:u8 || fields` (the transaction tag is written by `tx.rs`).
    pub(crate) fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            ActionOp::Request {
                contract,
                function,
                args,
                gas_limit,
                max_fee,
            } => {
                0u8.encode_to(out);
                contract.encode_to(out);
                function.encode_to(out);
                args.encode_to(out);
                gas_limit.encode_to(out);
                max_fee.encode_to(out);
            }
            ActionOp::SetHosts { contract, hosts } => {
                1u8.encode_to(out);
                contract.encode_to(out);
                hosts.encode_to(out);
            }
        }
    }

    /// `Ok(Err(sub))` is an unknown sub-tag.
    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Result<Result<Self, u8>, DecodeError> {
        Ok(Ok(match u8::decode_from(r)? {
            0 => ActionOp::Request {
                contract: <[u8; 32]>::decode_from(r)?,
                function: String::decode_from(r)?,
                args: Vec::decode_from(r)?,
                gas_limit: u64::decode_from(r)?,
                max_fee: u64::decode_from(r)?,
            },
            1 => ActionOp::SetHosts {
                contract: <[u8; 32]>::decode_from(r)?,
                hosts: Vec::decode_from(r)?,
            },
            t => return Ok(Err(t)),
        }))
    }

    /// Checks that need no state: sizes and allowlist syntax.
    pub fn check_static(&self) -> Result<(), ActionError> {
        match self {
            ActionOp::Request { function, args, .. } => {
                if function.len() > crate::tx::MAX_NAME_BYTES || args.len() > MAX_ARGS_BYTES {
                    return Err(ActionError::BadRequest(
                        "function or arguments too large".into(),
                    ));
                }
                Ok(())
            }
            ActionOp::SetHosts { hosts, .. } => check_hosts(hosts),
        }
    }
}

// ---------------------------------------------------------------- URLs and the allowlist

/// A URL reduced to what the allowlist and the SSRF rules look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub https: bool,
    /// Lower-case host name or IPv4 literal.
    pub host: String,
    pub port: u16,
}

fn host_chars_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Parse `http(s)://host[:port]/...`. Strict: printable ASCII only, no user info, no IPv6
/// literal, no backslash, a plain DNS name or IPv4 address.
pub fn parse_url(url: &str) -> Result<ParsedUrl, ActionError> {
    let bad = |m: &str| ActionError::BadRequest(m.to_string());
    if url.len() > MAX_URL_BYTES {
        return Err(bad("url too long"));
    }
    if !url
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b) && b != b'\\')
    {
        return Err(bad("url has characters that are not allowed"));
    }
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(bad("url must start with https:// or http://"));
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') || authority.contains('[') {
        return Err(bad("url must not carry user info or an IPv6 literal"));
    }
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => {
            if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad("bad port"));
            }
            let n: u32 = p.parse().map_err(|_| bad("bad port"))?;
            if n == 0 || n > 65_535 {
                return Err(bad("bad port"));
            }
            (h, n as u16)
        }
        None => (authority, if https { 443 } else { 80 }),
    };
    // Reject hosts written in tricky ways (hex, octal, decimal-integer forms) that some stacks
    // read as addresses: either a clean dotted quad or a name with a non-numeric label.
    let host = host.to_ascii_lowercase();
    if !host_chars_ok(&host) {
        return Err(bad("bad host"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    let all_numeric = labels.iter().all(|l| l.bytes().all(|b| b.is_ascii_digit()));
    if all_numeric
        && (labels.len() != 4
            || labels.iter().any(|l| {
                l.len() > 3
                    || (l.len() > 1 && l.starts_with('0'))
                    || l.parse::<u16>().is_ok_and(|n| n > 255)
            }))
    {
        return Err(bad("bad IPv4 literal"));
    }
    if !all_numeric
        && labels
            .last()
            .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(bad("bad host"));
    }
    Ok(ParsedUrl { https, host, port })
}

fn check_hosts(hosts: &[String]) -> Result<(), ActionError> {
    let bad = |m: String| ActionError::BadHosts(m);
    if hosts.len() > MAX_HOSTS {
        return Err(bad("too many hosts".into()));
    }
    for (i, h) in hosts.iter().enumerate() {
        if h.len() > MAX_HOST_BYTES || hosts[..i].contains(h) {
            return Err(bad(format!("host {h:?} too long or listed twice")));
        }
        let body = h.strip_prefix("*.").unwrap_or(h);
        let (name, port) = match body.split_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (body, None),
        };
        let port_ok = port.is_none_or(|p| {
            !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) && p != "0"
        });
        if !host_chars_ok(name) || !port_ok || (h.starts_with("*.") && port.is_some()) {
            return Err(bad(format!(
                "host {h:?} is not host, host:port or *.suffix"
            )));
        }
        // A wildcard needs at least two labels behind it, so `*.com` cannot be listed.
        if h.starts_with("*.") && !name.contains('.') {
            return Err(bad(format!("wildcard {h:?} is too broad")));
        }
    }
    Ok(())
}

/// Is `u` allowed by `hosts`? Entries: `host` (any port), `host:port`, `*.suffix` (any
/// subdomain, not the bare suffix).
pub fn host_allowed(hosts: &[String], u: &ParsedUrl) -> bool {
    hosts.iter().any(|e| {
        if let Some(suffix) = e.strip_prefix("*.") {
            u.host.len() > suffix.len() + 1
                && u.host.ends_with(suffix)
                && u.host.as_bytes()[u.host.len() - suffix.len() - 1] == b'.'
        } else if let Some((h, p)) = e.split_once(':') {
            h == u.host && p.parse::<u16>().is_ok_and(|p| p == u.port)
        } else {
            *e == u.host
        }
    })
}

/// Check and normalise the request an action returned: allowlisted host, sane headers and
/// sizes, limits filled in. The result is what is stored and what validators fetch.
pub fn validate_request(mut r: HttpRequest, hosts: &[String]) -> Result<HttpRequest, ActionError> {
    let bad = |m: &str| ActionError::BadRequest(m.to_string());
    if r.method != METHOD_GET && r.method != METHOD_POST {
        return Err(bad("method must be GET or POST"));
    }
    if r.method == METHOD_GET && !r.body.is_empty() {
        return Err(bad("a GET has no body"));
    }
    if r.body.len() > MAX_BODY_BYTES {
        return Err(bad("body too large"));
    }
    let u = parse_url(&r.url)?;
    if !host_allowed(hosts, &u) {
        return Err(ActionError::HostNotAllowed(format!(
            "{}:{}",
            u.host, u.port
        )));
    }
    if r.headers.len() > MAX_HEADERS {
        return Err(bad("too many headers"));
    }
    for (n, v) in &r.headers {
        let name_ok = !n.is_empty()
            && n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let value_ok = v.bytes().all(|b| (0x20..=0x7e).contains(&b));
        if n.len() + v.len() > MAX_HEADER_BYTES || !name_ok || !value_ok {
            return Err(bad("bad header"));
        }
        let l = n.to_ascii_lowercase();
        if matches!(
            l.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding" | "upgrade" | "te"
        ) || l.starts_with("proxy-")
        {
            return Err(bad("this header is set by the validator"));
        }
    }
    if r.max_response_bytes == 0 {
        r.max_response_bytes = DEFAULT_RESPONSE_BYTES;
    }
    if r.timeout_ms == 0 {
        r.timeout_ms = DEFAULT_TIMEOUT_MS;
    }
    if r.max_response_bytes > MAX_RESPONSE_CAP || r.timeout_ms > MAX_TIMEOUT_MS {
        return Err(bad("limits above the chain's caps"));
    }
    Ok(r)
}

// ---------------------------------------------------------------- votes

/// A validator's signed answer to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionVote {
    pub request: Hash,
    /// `STATUS_OK` (data is the transformed result) or `STATUS_FAILED` (data is a short text).
    pub status: u8,
    pub data: Vec<u8>,
    /// The validator's Ed25519 consensus key.
    pub key: [u8; 32],
    pub sig: [u8; 64],
}

/// `SHA256("btb.action.outcome.v1" || status || data)`: what votes are grouped by.
pub fn outcome_hash(status: u8, data: &[u8]) -> Hash {
    crate::sha256(&[b"btb.action.outcome.v1", &[status], data])
}

fn vote_digest(chain_id: u64, request: &Hash, status: u8, data: &[u8]) -> Hash {
    crate::sha256(&[
        b"btb.action.vote.v1",
        &chain_id.to_be_bytes(),
        request,
        &[status],
        &Sha256::digest(data),
    ])
}

impl ActionVote {
    pub fn sign(
        chain_id: u64,
        key: &SigningKey,
        request: Hash,
        status: u8,
        data: Vec<u8>,
    ) -> ActionVote {
        let sig = key.sign(&vote_digest(chain_id, &request, status, &data));
        ActionVote {
            request,
            status,
            data,
            key: key.verifying_key().to_bytes(),
            sig: sig.to_bytes(),
        }
    }

    /// Strict Ed25519 verification; also checks the status and the data size.
    pub fn verify(&self, chain_id: u64) -> bool {
        if self.status > STATUS_FAILED || self.data.len() > MAX_RESULT_BYTES {
            return false;
        }
        let Ok(vk) = VerifyingKey::from_bytes(&self.key) else {
            return false;
        };
        vk.verify_strict(
            &vote_digest(chain_id, &self.request, self.status, &self.data),
            &Signature::from_bytes(&self.sig),
        )
        .is_ok()
    }

    pub fn outcome(&self) -> Hash {
        outcome_hash(self.status, &self.data)
    }
}

impl Encode for ActionVote {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.request.encode_to(out);
        self.status.encode_to(out);
        self.data.encode_to(out);
        self.key.encode_to(out);
        self.sig.encode_to(out);
    }
}

impl Decode for ActionVote {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let v = ActionVote {
            request: <[u8; 32]>::decode_from(r)?,
            status: u8::decode_from(r)?,
            data: Vec::decode_from(r)?,
            key: <[u8; 32]>::decode_from(r)?,
            sig: <[u8; 64]>::decode_from(r)?,
        };
        if v.data.len() > MAX_RESULT_BYTES {
            return Err(DecodeError::LengthTooLarge {
                len: v.data.len() as u64,
                max: MAX_RESULT_BYTES,
            });
        }
        Ok(v)
    }
}

// ---------------------------------------------------------------- pending requests

/// A request waiting for votes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub requester: Address,
    pub contract: Address,
    /// The action's name; the callback is `<function>_callback`.
    pub function: String,
    /// The arguments the requester gave; passed back to the callback.
    pub args: Vec<u8>,
    /// The validated, normalised `HttpRequest`, encoded.
    pub request: Vec<u8>,
    /// The contract's allowlist when the request was made (redirects are checked against it).
    pub hosts: Vec<String>,
    pub created: u64,
    pub deadline: u64,
    /// Funds held in [`escrow_pool`] for this request.
    pub escrow: u64,
}

impl Pending {
    pub fn http(&self) -> Option<HttpRequest> {
        HttpRequest::decode(&self.request).ok()
    }

    fn encode(&self) -> Vec<u8> {
        (
            self.requester,
            self.contract,
            self.function.clone(),
            self.args.clone(),
            self.request.clone(),
            self.hosts.clone(),
            (self.created, self.deadline, self.escrow),
        )
            .encode()
    }

    fn decode(b: &[u8]) -> Result<Pending, DecodeError> {
        let (requester, contract, function, args, request, hosts, (created, deadline, escrow)) =
            <(
                [u8; 32],
                [u8; 32],
                String,
                Vec<u8>,
                Vec<u8>,
                Vec<String>,
                (u64, u64, u64),
            )>::decode(b)?;
        Ok(Pending {
            requester,
            contract,
            function,
            args,
            request,
            hosts,
            created,
            deadline,
            escrow,
        })
    }
}

fn key1(sub: u8, tail: &[u8]) -> Vec<u8> {
    let mut k = vec![TAG_ACTION, sub];
    k.extend_from_slice(tail);
    k
}

fn hosts_key(contract: &Address) -> Vec<u8> {
    key1(SUB_HOSTS, contract)
}

fn pending_key(id: &Hash) -> Vec<u8> {
    key1(SUB_PENDING, id)
}

fn vote_key(id: &Hash, key: &[u8; 32]) -> Vec<u8> {
    let mut k = key1(SUB_VOTE, id);
    k.extend_from_slice(key);
    k
}

fn outcome_key(id: &Hash, h: &Hash) -> Vec<u8> {
    let mut k = key1(SUB_OUTCOME, id);
    k.extend_from_slice(h);
    k
}

pub fn hosts_of(kv: &impl KvStore, contract: &Address) -> Result<Vec<String>, StateError> {
    match kv.get(&hosts_key(contract))? {
        None => Ok(Vec::new()),
        Some(b) => Ok(Vec::<String>::decode(&b).map_err(StateError::from)?),
    }
}

pub(crate) fn set_hosts(
    kv: &mut impl KvStore,
    contract: &Address,
    hosts: &[String],
) -> Result<(), KvError> {
    if hosts.is_empty() {
        kv.delete(&hosts_key(contract))
    } else {
        kv.put(&hosts_key(contract), &hosts.to_vec().encode())
    }
}

pub fn pending_count(kv: &impl KvStore) -> Result<u64, KvError> {
    Ok(kv
        .get(&key1(SUB_COUNT, &[]))?
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map_or(0, u64::from_le_bytes))
}

fn set_count(kv: &mut impl KvStore, n: u64) -> Result<(), KvError> {
    kv.put(&key1(SUB_COUNT, &[]), &n.to_le_bytes())
}

pub fn pending_get(kv: &impl KvStore, id: &Hash) -> Result<Option<Pending>, StateError> {
    match kv.get(&pending_key(id))? {
        None => Ok(None),
        Some(b) => Ok(Some(Pending::decode(&b).map_err(StateError::from)?)),
    }
}

pub(crate) fn pending_put(kv: &mut impl KvStore, id: &Hash, p: &Pending) -> Result<(), KvError> {
    let n = pending_count(kv)?;
    kv.put(&pending_key(id), &p.encode())?;
    set_count(kv, n + 1)
}

/// All pending requests in id order (at most [`MAX_PENDING`]).
pub fn pending_list(kv: &impl KvStore) -> Result<Vec<(Hash, Pending)>, StateError> {
    let start = key1(SUB_PENDING, &[]);
    let end = key1(SUB_PENDING + 1, &[]);
    let mut out = Vec::new();
    for (k, v) in kv.scan(&start, Some(&end), false, MAX_PENDING as usize + 1)? {
        let id: Hash = k
            .get(2..34)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| StateError("bad pending key".into()))?;
        out.push((id, Pending::decode(&v).map_err(StateError::from)?));
    }
    Ok(out)
}

/// The votes recorded for `id`: `(validator key, outcome hash)`.
pub fn votes_of(kv: &impl KvStore, id: &Hash) -> Result<Vec<([u8; 32], Hash)>, StateError> {
    let start = key1(SUB_VOTE, id);
    let end = prefix_successor(&start);
    let mut out = Vec::new();
    for (k, v) in kv.scan(&start, end.as_deref(), false, 4096)? {
        let key: [u8; 32] = k
            .get(34..66)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| StateError("bad vote key".into()))?;
        let h: Hash = v
            .as_slice()
            .try_into()
            .map_err(|_| StateError("bad vote value".into()))?;
        out.push((key, h));
    }
    Ok(out)
}

/// The `(status, data)` stored for an outcome hash.
pub fn outcome_get(
    kv: &impl KvStore,
    id: &Hash,
    h: &Hash,
) -> Result<Option<(u8, Vec<u8>)>, KvError> {
    Ok(kv
        .get(&outcome_key(id, h))?
        .and_then(|b| Some((*b.first()?, b[1..].to_vec()))))
}

/// Record a vote. Returns false if the validator already voted on this request.
pub(crate) fn record_vote(kv: &mut impl KvStore, v: &ActionVote) -> Result<bool, KvError> {
    let k = vote_key(&v.request, &v.key);
    if kv.get(&k)?.is_some() {
        return Ok(false);
    }
    let h = v.outcome();
    kv.put(&k, &h)?;
    let ok = outcome_key(&v.request, &h);
    if kv.get(&ok)?.is_none() {
        let mut val = vec![v.status];
        val.extend_from_slice(&v.data);
        kv.put(&ok, &val)?;
    }
    Ok(true)
}

/// Remove a request and everything stored for it.
pub(crate) fn remove(kv: &mut impl KvStore, id: &Hash) -> Result<(), KvError> {
    for sub in [SUB_VOTE, SUB_OUTCOME] {
        let start = key1(sub, id);
        let end = prefix_successor(&start);
        for (k, _) in kv.scan(&start, end.as_deref(), false, 8192)? {
            kv.delete(&k)?;
        }
    }
    kv.delete(&pending_key(id))?;
    let n = pending_count(kv)?;
    set_count(kv, n.saturating_sub(1))
}

// ---------------------------------------------------------------- the decision

/// What the votes on a request add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Keep waiting.
    Wait,
    /// At least two thirds of the stake voted for this outcome.
    Agreed(Hash),
    /// No outcome can reach two thirds any more.
    Disagreed,
    /// The deadline passed.
    TimedOut,
}

/// Decide from `(outcome, stake)` tallies. `total` is the stake of the epoch's validators;
/// `height >= deadline` ends the request.
pub fn decide(height: u64, deadline: u64, total: u128, tallies: &[(Hash, u128)]) -> Verdict {
    let voted: u128 = tallies.iter().map(|(_, s)| *s).sum();
    if let Some((h, _)) = tallies
        .iter()
        .find(|(_, s)| *s * 3 >= total * 2 && total > 0)
    {
        return Verdict::Agreed(*h);
    }
    if height >= deadline {
        return Verdict::TimedOut;
    }
    let best = tallies.iter().map(|(_, s)| *s).max().unwrap_or(0);
    // Even if every validator that has not voted joined the leading outcome, it would fall short.
    if (best + total.saturating_sub(voted)) * 3 < total * 2 {
        return Verdict::Disagreed;
    }
    Verdict::Wait
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Hash {
        [n; 32]
    }

    #[test]
    fn urls_are_parsed_strictly() {
        let ok = |u: &str| parse_url(u).unwrap();
        assert_eq!(
            ok("https://API.Example.com/a?b=1"),
            ParsedUrl {
                https: true,
                host: "api.example.com".into(),
                port: 443
            }
        );
        assert_eq!(ok("http://127.0.0.1:8080/x").port, 8080);
        assert_eq!(ok("http://10.0.0.1").host, "10.0.0.1");
        for bad in [
            "ftp://a.example/",
            "https://user@a.example/",
            "https://[::1]/",
            "https://a.example:0/",
            "https://a.example:99999/",
            "https://a.example:x/",
            "https:///path",
            "https://a b.example/",
            "https://a.example\\@evil.com/",
            "https://2130706433/", // decimal integer form of 127.0.0.1
            "https://0x7f.0.0.1/", // hex
            "https://0177.0.0.1/", // octal
            "https://127.1/",      // short form
            "https://a..example/",
            "https://-a.example/",
            "https://exa_mple.com/",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
        assert!(parse_url(&format!("https://a.example/{}", "x".repeat(MAX_URL_BYTES))).is_err());
    }

    #[test]
    fn the_allowlist_matches_hosts_ports_and_subdomains() {
        let hosts: Vec<String> = ["api.example.com", "*.cdn.example.net", "127.0.0.1:8080"]
            .map(String::from)
            .to_vec();
        assert!(check_hosts(&hosts).is_ok());
        let m = |u: &str| host_allowed(&hosts, &parse_url(u).unwrap());
        assert!(m("https://api.example.com/x"));
        assert!(
            m("https://api.example.com:8443/x"),
            "a bare host allows any port"
        );
        assert!(!m("https://evil.api.example.com/"));
        assert!(!m("https://example.com/"));
        assert!(m("https://a.cdn.example.net/"));
        assert!(m("https://a.b.cdn.example.net/"));
        assert!(
            !m("https://cdn.example.net/"),
            "the bare suffix is not a subdomain"
        );
        assert!(
            !m("https://xcdn.example.net/"),
            "suffix must start at a label"
        );
        assert!(m("http://127.0.0.1:8080/"));
        assert!(!m("http://127.0.0.1:9/"));
        assert!(!m("http://127.0.0.1/"));
        // bad lists
        for bad in [
            vec!["*.com"],
            vec!["a b"],
            vec!["*.a.com:80"],
            vec!["a.com", "a.com"],
            vec![""],
            vec!["a.com:0"],
        ] {
            let l: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert!(check_hosts(&l).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn requests_are_validated_and_normalised() {
        let hosts = vec!["a.example".to_string()];
        let base = HttpRequest::get("https://a.example/p");
        let v = validate_request(base.clone(), &hosts).unwrap();
        assert_eq!(v.timeout_ms, 5_000);
        let zero = HttpRequest {
            max_response_bytes: 0,
            timeout_ms: 0,
            ..base.clone()
        };
        let v = validate_request(zero, &hosts).unwrap();
        assert_eq!(
            (v.max_response_bytes, v.timeout_ms),
            (DEFAULT_RESPONSE_BYTES, DEFAULT_TIMEOUT_MS)
        );
        let bad = |r: HttpRequest| validate_request(r, &hosts).unwrap_err();
        assert!(matches!(
            bad(HttpRequest::get("https://b.example/")),
            ActionError::HostNotAllowed(_)
        ));
        assert!(matches!(
            bad(HttpRequest {
                method: 9,
                ..base.clone()
            }),
            ActionError::BadRequest(_)
        ));
        assert!(matches!(
            bad(HttpRequest {
                body: vec![1],
                ..base.clone()
            }),
            ActionError::BadRequest(_)
        ));
        assert!(matches!(
            bad(base.clone().limits(MAX_RESPONSE_CAP + 1, 1)),
            ActionError::BadRequest(_)
        ));
        assert!(matches!(
            bad(base.clone().limits(1, MAX_TIMEOUT_MS + 1)),
            ActionError::BadRequest(_)
        ));
        for (n, v) in [
            ("host", "x"),
            ("Content-Length", "1"),
            ("x y", "1"),
            ("x", "a\r\nb"),
            ("Proxy-Authorization", "x"),
        ] {
            assert!(
                matches!(bad(base.clone().header(n, v)), ActionError::BadRequest(_)),
                "{n}"
            );
        }
        assert!(validate_request(base.header("accept", "application/json"), &hosts).is_ok());
    }

    #[test]
    fn votes_sign_verify_and_round_trip() {
        let k = SigningKey::from_bytes(&[3; 32]);
        let v = ActionVote::sign(7, &k, h(1), STATUS_OK, b"42".to_vec());
        assert!(v.verify(7));
        assert!(!v.verify(8), "another chain");
        let mut w = v.clone();
        w.request = h(2);
        assert!(!w.verify(7), "another request");
        let mut w = v.clone();
        w.data = b"43".to_vec();
        assert!(!w.verify(7));
        let mut w = v.clone();
        w.status = STATUS_FAILED;
        assert!(!w.verify(7));
        let mut w = v.clone();
        w.status = STATUS_TIMEOUT;
        assert!(!w.verify(7), "only validators' outcomes can be signed");
        let big = ActionVote::sign(7, &k, h(1), STATUS_OK, vec![0; MAX_RESULT_BYTES + 1]);
        assert!(!big.verify(7));
        assert_eq!(ActionVote::decode(&v.encode()).unwrap(), v);
        assert!(ActionVote::decode(&big.encode()).is_err());
        assert_ne!(
            outcome_hash(STATUS_OK, b"x"),
            outcome_hash(STATUS_FAILED, b"x")
        );
    }

    #[test]
    fn decisions_follow_two_thirds_of_the_stake() {
        let (a, b) = (h(1), h(2));
        // 4 validators of 10 each
        assert_eq!(decide(1, 50, 40, &[]), Verdict::Wait);
        assert_eq!(decide(1, 50, 40, &[(a, 20)]), Verdict::Wait);
        assert_eq!(
            decide(1, 50, 40, &[(a, 30)]),
            Verdict::Agreed(a),
            "30/40 >= 2/3"
        );
        assert_eq!(
            decide(1, 50, 40, &[(a, 20), (b, 10)]),
            Verdict::Wait,
            "10 left could still help"
        );
        assert_eq!(decide(1, 50, 40, &[(a, 20), (b, 20)]), Verdict::Disagreed);
        assert_eq!(
            decide(1, 50, 40, &[(a, 10), (b, 10), (h(3), 10)]),
            Verdict::Disagreed
        );
        assert_eq!(decide(50, 50, 40, &[(a, 20)]), Verdict::TimedOut);
        assert_eq!(
            decide(50, 50, 40, &[(a, 30), (b, 10)]),
            Verdict::Agreed(a),
            "agreement wins over the deadline"
        );
        // 27/40 = 0.675 >= 2/3 but 26 would not be
        assert_eq!(decide(1, 50, 40, &[(a, 26)]), Verdict::Wait);
        assert_eq!(decide(1, 50, 40, &[(a, 27)]), Verdict::Agreed(a));
        assert_eq!(decide(1, 50, 0, &[]), Verdict::Wait);
    }
}
