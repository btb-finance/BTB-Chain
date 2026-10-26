//! Transactions: format, signing, verification and the contract manifest.
//!
//! Wire format (all integers little-endian, via `btb-codec`):
//!
//! ```text
//! tx      = body || public_key[32] || signature[64]
//! body    = chain_id:u64 || nonce:u64 || tag:u8 || fields
//! Deploy   (tag 0): code:bytes || manifest
//! Call     (tag 1): contract[32] || kind:u8 || function:string || args:bytes
//!                   || gas_limit:u64 || max_fee:u64
//! Transfer (tag 2): to[32] || amount:u64
//! Stake    (tag 3): validator_key[32] || amount:u64
//! Unstake  (tag 4): amount:u64
//! RegisterValidator (tag 5): consensus_key[32] || network_addr:bytes
//! SubmitEvidence    (tag 6): epoch:u64 || evidence
//! Dkg               (tag 14): sub:u8 || fields (`dkg.rs`: 0 deal, 1 complain), sent by a consensus key
//! Cross             (tag 21): sub:u8 || fields (`cross.rs`: 0 deal, 1 complain, 2 signature, 3 evidence, 4 attest, 5 prepare, 6 withdraw)
//! Upgrade           (tag 23): contract[32] || code:bytes || manifest || budget_gas:u64 (`upgrade.rs`)
//! Session           (tag 22): sub:u8 || fields (`session.rs`: 0 grant, 1 revoke), signed by the account
//! Host              (tag 20): sub:u8 || fields (`host.rs`: 0 register url:bytes, 1 unregister)
//! Action            (tag 13): sub:u8 || fields (`actions.rs`: 0 request an HTTPS action, 1 set the contract's hosts)
//! Storage           (tag 12): sub:u8 || fields (`storage.rs`: 0 register, 1 add deposit, 2 withdraw, 3 store, 4 prove)
//! Dex               (tag 10): sub:u8 || fields (`dex.rs`: 0 place, 1 cancel)
//! Site              (tag 9): sub:u8 || fields (`site.rs`: 0 publish, 1 remove)
//! evidence = 0 || voter:u32 || round:u64 || (id[32] || sig[64]) x2     (DoubleVote)
//!          | 1 || proposer:u32 || round:u64 || (id[32] || sig[64]) x2  (DoubleProposal)
//! Channel  (tag 8): sub:u8 || fields   (see `channel.rs`: open 0, top-up 1, close 2, start-close 3, finalize 4)
//! manifest = name:string || crons:[(function:string, every_blocks:u64)] || indexes:[string]
//! ```
//!
//! **Versions.** A body may name the hosting node that relayed it. If so, the tag byte has its
//! high bit set (`tag | 0x80`) and the 32-byte relayer address follows it, before the fields.
//! A body without a relayer is byte-for-byte the original encoding, so old transactions still
//! decode, keep their signatures and ids, and execute (the fee then goes wholly to the
//! validator pool). The relayer is inside the signed body: changing it breaks the signature.
//!
//! A body may also say it is signed by a **session key** for an account (`session.rs`): the tag's
//! bit `0x40` is set and the 32-byte account address follows the relayer (if any). The
//! transaction's `public_key` is then the session key, the sender (nonce, fee payer, `caller()`)
//! is the account, and the chain checks the account's grant. The field is inside the signed body.
//! A body without a session is byte-for-byte what it was before.
//!
//! `kind` is the entry-point kind (0 query, 1 mutation, 2 cron, 3 action). All four decode so
//! the error is a clear "kind not allowed", but only mutation is valid in a `Call`: queries never
//! go through consensus, and an action is requested with `Action` (tag 13), not called.
//!
//! The signature is Ed25519 over `SHA256("btb.tx.sig.v1" || body)`. The body contains the
//! chain id, so a signature made for one chain is useless on another. The sender is
//! `SHA256("btb.account.v1" || public_key)`.

use btb_codec::{Decode, DecodeError, Encode, Reader};
use btb_consensus::{Evidence, Sig};
use btb_vm::FunctionKind;
pub use ed25519_dalek::SigningKey;
use ed25519_dalek::{Signature, Signer, VerifyingKey};

use crate::sha256;

pub type Address = [u8; 32];
pub type Hash = [u8; 32];

pub const MAX_CODE_BYTES: usize = 1 << 20;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_CRONS: usize = 16;
pub const MAX_INDEX_SPECS: usize = 64;
pub const MAX_CRON_EVERY: u64 = 1 << 32;
pub const MAX_ARG_BYTES: usize = 1 << 20;

/// High bit of the tag byte: a 32-byte relayer address follows the tag (tx encoding v2).
const RELAYER_FLAG: u8 = 0x80;
/// Second-highest bit of the tag byte: a 32-byte account address follows (after the relayer, if
/// any): the transaction is signed by a session key of that account (tx encoding v3).
const SESSION_FLAG: u8 = 0x40;

const SIG_DOMAIN: &[u8] = b"btb.tx.sig.v1";
const ID_DOMAIN: &[u8] = b"btb.tx.id.v1";
const ACCOUNT_DOMAIN: &[u8] = b"btb.account.v1";
const CONTRACT_DOMAIN: &[u8] = b"btb.contract.v1";

/// Why bytes are not a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxError {
    Decode(DecodeError),
    BadTag(u8),
    BadKind(u8),
}

impl From<DecodeError> for TxError {
    fn from(e: DecodeError) -> Self {
        TxError::Decode(e)
    }
}

/// What a deploy declares about the contract: its name, cron schedules and table indexes.
///
/// `crons` lists `(function, every_blocks)`: the `cron` entry point `function` runs every
/// `every_blocks` blocks (`every = "N blocks"`). `indexes` use the `btb dev` spec
/// `Table:index:addr@offset` or `Table:index:u64@offset`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub name: String,
    pub crons: Vec<(String, u64)>,
    pub indexes: Vec<String>,
}

impl Encode for Manifest {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.name.encode_to(out);
        self.crons.encode_to(out);
        self.indexes.encode_to(out);
    }
}

impl Decode for Manifest {
    fn decode_from(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Manifest {
            name: String::decode_from(r)?,
            crons: Vec::decode_from(r)?,
            indexes: Vec::decode_from(r)?,
        })
    }
}

/// Reserved `Manifest.indexes` entry that carries the upgrade policy (wire compatible: the
/// manifest encoding is unchanged, old manifests have no such entry).
pub const UPGRADE_ENTRY: &str = "!upgrade=";

/// Who may replace a contract's code (`TxKind::Upgrade`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradePolicy {
    /// The account that deployed it (the default).
    Deployer,
    /// Nobody: the contract is immutable.
    Immutable,
    /// This account.
    Authority(Address),
}

impl Manifest {
    /// The upgrade policy the manifest names; `None` if it names none (deployer on deploy,
    /// unchanged on upgrade).
    pub fn upgrade_policy(&self) -> Result<Option<UpgradePolicy>, String> {
        let mut found = None;
        for e in self
            .indexes
            .iter()
            .filter_map(|i| i.strip_prefix(UPGRADE_ENTRY))
        {
            if found.is_some() {
                return Err("upgrade policy given twice".into());
            }
            found = Some(match e {
                "deployer" => UpgradePolicy::Deployer,
                "none" => UpgradePolicy::Immutable,
                hex if hex.len() == 64 && hex.is_ascii() => {
                    let mut a = [0u8; 32];
                    for (i, b) in a.iter_mut().enumerate() {
                        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                            .map_err(|_| format!("bad upgrade authority {hex:?}"))?;
                    }
                    UpgradePolicy::Authority(a)
                }
                other => return Err(format!("bad upgrade policy {other:?}")),
            });
        }
        Ok(found)
    }

    /// Set (or replace) the upgrade policy entry.
    pub fn with_upgrade_policy(mut self, p: UpgradePolicy) -> Self {
        self.indexes.retain(|i| !i.starts_with(UPGRADE_ENTRY));
        self.indexes.push(format!(
            "{UPGRADE_ENTRY}{}",
            match p {
                UpgradePolicy::Deployer => "deployer".to_string(),
                UpgradePolicy::Immutable => "none".to_string(),
                UpgradePolicy::Authority(a) => a.iter().map(|b| format!("{b:02x}")).collect(),
            }
        ));
        self
    }
}

/// A parsed index declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    pub table: String,
    pub index: String,
    pub addr: bool,
    pub offset: usize,
}

impl IndexSpec {
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad =
            || format!("bad index spec {s:?}, expected Table:index:addr@offset or :u64@offset");
        let mut p = s.split(':');
        let (Some(table), Some(index), Some(rest), None) = (p.next(), p.next(), p.next(), p.next())
        else {
            return Err(bad());
        };
        let (kind, off) = rest.split_once('@').ok_or_else(bad)?;
        let addr = match kind {
            "addr" => true,
            "u64" => false,
            _ => return Err(bad()),
        };
        if table.is_empty() || index.is_empty() {
            return Err(bad());
        }
        Ok(Self {
            table: table.into(),
            index: index.into(),
            addr,
            offset: off.parse().map_err(|_| bad())?,
        })
    }
}

impl Manifest {
    /// Check the manifest against itself (limits, schedules, index specs).
    pub fn validate(&self) -> Result<Vec<IndexSpec>, String> {
        if self.name.len() > MAX_NAME_BYTES {
            return Err("name too long".into());
        }
        if self.crons.len() > MAX_CRONS {
            return Err("too many crons".into());
        }
        for (i, (f, every)) in self.crons.iter().enumerate() {
            if !(1..=MAX_CRON_EVERY).contains(every) {
                return Err(format!(
                    "cron {f:?}: every must be 1..={MAX_CRON_EVERY} blocks"
                ));
            }
            if self.crons[..i].iter().any(|(g, _)| g == f) {
                return Err(format!("cron {f:?} listed twice"));
            }
        }
        if self.indexes.len() > MAX_INDEX_SPECS {
            return Err("too many indexes".into());
        }
        self.upgrade_policy()?;
        self.indexes
            .iter()
            .filter(|s| !s.starts_with(UPGRADE_ENTRY))
            .map(|s| IndexSpec::parse(s))
            .collect()
    }
}

/// What a transaction does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxKind {
    Deploy {
        code: Vec<u8>,
        manifest: Manifest,
    },
    Call {
        contract: Address,
        kind: FunctionKind,
        function: String,
        args: Vec<u8>,
        gas_limit: u64,
        max_fee: u64,
    },
    Transfer {
        to: Address,
        amount: u64,
    },
    /// Bond `amount` of the sender's balance behind the validator `validator` (an account
    /// public key; today it must be the sender's own key, delegation is not supported).
    Stake {
        validator: [u8; 32],
        amount: u64,
    },
    /// Start unbonding `amount` of the sender's bonded stake.
    Unstake {
        amount: u64,
    },
    /// Register or update the sender's consensus key and network address.
    RegisterValidator {
        consensus_key: [u8; 32],
        network_addr: Vec<u8>,
    },
    /// Prove double signing in `epoch` (see `btb_consensus::Evidence`).
    SubmitEvidence {
        epoch: u64,
        evidence: Evidence,
    },
    /// A payment-channel operation (`crate::channel`).
    Channel(crate::channel::ChannelOp),
    /// Publish or remove a static site file of a contract (`crate::site`, deployer only).
    Site(crate::site::SiteOp),
    /// A DEX order or cancellation (`crate::dex`).
    Dex(crate::dex::DexOp),
    /// A storage-node or stored-file operation (`crate::storage`).
    Storage(crate::storage::StorageOp),
    /// A beacon key-generation message (`crate::dkg`), signed by a validator's consensus key.
    Dkg(crate::dkg::DkgOp),
    /// A cross-chain key, signature or vault operation (`crate::cross`).
    Cross(crate::cross::CrossOp),
    /// An HTTPS action request, or the contract's host allowlist (`crate::actions`).
    Action(crate::actions::ActionOp),
    /// Register or remove the sender as a hosting node that may be paid as relayer (`crate::host`).
    Host(crate::host::HostOp),
    /// Replace the code of a deployed contract (`crate::upgrade`), by its upgrade authority.
    /// `budget_gas` is prepaid (at the gas price) for the migration and backfill work.
    Upgrade {
        contract: Address,
        code: Vec<u8>,
        manifest: Manifest,
        budget_gas: u64,
    },
    /// Grant or revoke a session key (`crate::session`), signed by the account itself.
    Session(crate::session::SessionOp),
}

fn encode_evidence(e: &Evidence, out: &mut Vec<u8>) {
    let (tag, who, round, a, b) = match e {
        Evidence::DoubleVote { voter, round, a, b } => (0u8, *voter, *round, a, b),
        Evidence::DoubleProposal {
            proposer,
            round,
            a,
            b,
        } => (1u8, *proposer, *round, a, b),
    };
    tag.encode_to(out);
    who.encode_to(out);
    round.encode_to(out);
    for (id, sig) in [a, b] {
        id.encode_to(out);
        sig.0.encode_to(out);
    }
}

fn decode_evidence(r: &mut Reader<'_>) -> Result<Evidence, TxError> {
    let tag = u8::decode_from(r)?;
    let who = u32::decode_from(r)?;
    let round = u64::decode_from(r)?;
    let mut pair = || -> Result<([u8; 32], Sig), DecodeError> {
        Ok((
            <[u8; 32]>::decode_from(r)?,
            Sig(<[u8; 64]>::decode_from(r)?),
        ))
    };
    let (a, b) = (pair()?, pair()?);
    match tag {
        0 => Ok(Evidence::DoubleVote {
            voter: who,
            round,
            a,
            b,
        }),
        1 => Ok(Evidence::DoubleProposal {
            proposer: who,
            round,
            a,
            b,
        }),
        t => Err(TxError::BadTag(t)),
    }
}

fn kind_byte(k: FunctionKind) -> u8 {
    match k {
        FunctionKind::Query => 0,
        FunctionKind::Mutation => 1,
        FunctionKind::Cron => 2,
        FunctionKind::Action => 3,
    }
}

fn byte_kind(b: u8) -> Result<FunctionKind, TxError> {
    Ok(match b {
        0 => FunctionKind::Query,
        1 => FunctionKind::Mutation,
        2 => FunctionKind::Cron,
        3 => FunctionKind::Action,
        _ => return Err(TxError::BadKind(b)),
    })
}

/// The signed part of a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxBody {
    pub chain_id: u64,
    pub nonce: u64,
    /// The hosting node that relayed the transaction and earns `RELAY_SHARE_BPS` of its fee
    /// (see `host.rs`). Chosen by the sender and covered by the signature, so no relay or
    /// proposer can change it. `None` encodes exactly as before this field existed.
    pub relayer: Option<Address>,
    /// `Some(account)`: signed by a session key of `account` and acting as it (`session.rs`).
    /// `None` encodes exactly as before this field existed.
    pub session: Option<Address>,
    pub kind: TxKind,
}

impl Encode for TxBody {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.chain_id.encode_to(out);
        self.nonce.encode_to(out);
        let tag_pos = out.len();
        match &self.kind {
            TxKind::Deploy { code, manifest } => {
                0u8.encode_to(out);
                code.encode_to(out);
                manifest.encode_to(out);
            }
            TxKind::Call {
                contract,
                kind,
                function,
                args,
                gas_limit,
                max_fee,
            } => {
                1u8.encode_to(out);
                contract.encode_to(out);
                kind_byte(*kind).encode_to(out);
                function.encode_to(out);
                args.encode_to(out);
                gas_limit.encode_to(out);
                max_fee.encode_to(out);
            }
            TxKind::Transfer { to, amount } => {
                2u8.encode_to(out);
                to.encode_to(out);
                amount.encode_to(out);
            }
            TxKind::Stake { validator, amount } => {
                3u8.encode_to(out);
                validator.encode_to(out);
                amount.encode_to(out);
            }
            TxKind::Unstake { amount } => {
                4u8.encode_to(out);
                amount.encode_to(out);
            }
            TxKind::RegisterValidator {
                consensus_key,
                network_addr,
            } => {
                5u8.encode_to(out);
                consensus_key.encode_to(out);
                network_addr.encode_to(out);
            }
            TxKind::SubmitEvidence { epoch, evidence } => {
                6u8.encode_to(out);
                epoch.encode_to(out);
                encode_evidence(evidence, out);
            }
            TxKind::Channel(op) => {
                8u8.encode_to(out);
                op.encode_to(out);
            }
            TxKind::Site(op) => {
                9u8.encode_to(out);
                op.encode_to(out);
            }
            TxKind::Dex(op) => {
                10u8.encode_to(out);
                op.encode_to(out);
            }
            TxKind::Storage(op) => {
                12u8.encode_to(out);
                op.encode_to(out);
            }
            TxKind::Dkg(op) => {
                14u8.encode_to(out);
                op.encode_to(out);
            }
            TxKind::Cross(op) => {
                21u8.encode_to(out);
                op.encode_to(out);
            }
            TxKind::Action(op) => {
                13u8.encode_to(out);
                op.encode_to(out);
            }
            TxKind::Host(op) => {
                20u8.encode_to(out);
                op.encode_to(out);
            }
            TxKind::Upgrade {
                contract,
                code,
                manifest,
                budget_gas,
            } => {
                23u8.encode_to(out);
                contract.encode_to(out);
                code.encode_to(out);
                manifest.encode_to(out);
                budget_gas.encode_to(out);
            }
            TxKind::Session(op) => {
                22u8.encode_to(out);
                op.encode_to(out);
            }
        }
        // v2: a relayer sets the tag's high bit and follows it as 32 bytes; v3: a session
        // account sets bit 0x40 and follows the relayer.
        let mut extra: Vec<u8> = Vec::new();
        if let Some(relayer) = &self.relayer {
            out[tag_pos] |= RELAYER_FLAG;
            extra.extend_from_slice(relayer);
        }
        if let Some(account) = &self.session {
            out[tag_pos] |= SESSION_FLAG;
            extra.extend_from_slice(account);
        }
        out.splice(tag_pos + 1..tag_pos + 1, extra);
    }
}

impl TxBody {
    fn decode_body(r: &mut Reader<'_>) -> Result<Self, TxError> {
        let chain_id = u64::decode_from(r)?;
        let nonce = u64::decode_from(r)?;
        let tag = u8::decode_from(r)?;
        let relayer = if tag & RELAYER_FLAG != 0 {
            Some(<[u8; 32]>::decode_from(r)?)
        } else {
            None
        };
        let session = if tag & SESSION_FLAG != 0 {
            Some(<[u8; 32]>::decode_from(r)?)
        } else {
            None
        };
        let kind = match tag & !(RELAYER_FLAG | SESSION_FLAG) {
            0 => TxKind::Deploy {
                code: Vec::decode_from(r)?,
                manifest: Manifest::decode_from(r)?,
            },
            1 => TxKind::Call {
                contract: <[u8; 32]>::decode_from(r)?,
                kind: byte_kind(u8::decode_from(r)?)?,
                function: String::decode_from(r)?,
                args: Vec::decode_from(r)?,
                gas_limit: u64::decode_from(r)?,
                max_fee: u64::decode_from(r)?,
            },
            2 => TxKind::Transfer {
                to: <[u8; 32]>::decode_from(r)?,
                amount: u64::decode_from(r)?,
            },
            3 => TxKind::Stake {
                validator: <[u8; 32]>::decode_from(r)?,
                amount: u64::decode_from(r)?,
            },
            4 => TxKind::Unstake {
                amount: u64::decode_from(r)?,
            },
            5 => TxKind::RegisterValidator {
                consensus_key: <[u8; 32]>::decode_from(r)?,
                network_addr: Vec::decode_from(r)?,
            },
            6 => TxKind::SubmitEvidence {
                epoch: u64::decode_from(r)?,
                evidence: decode_evidence(r)?,
            },
            8 => match crate::channel::ChannelOp::decode_from(r)? {
                Ok(op) => TxKind::Channel(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            9 => match crate::site::SiteOp::decode_from(r)? {
                Ok(op) => TxKind::Site(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            10 => match crate::dex::DexOp::decode_from(r)? {
                Ok(op) => TxKind::Dex(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            12 => match crate::storage::StorageOp::decode_from(r)? {
                Ok(op) => TxKind::Storage(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            14 => match crate::dkg::DkgOp::decode_from(r)? {
                Ok(op) => TxKind::Dkg(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            21 => match crate::cross::CrossOp::decode_from(r)? {
                Ok(op) => TxKind::Cross(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            13 => match crate::actions::ActionOp::decode_from(r)? {
                Ok(op) => TxKind::Action(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            20 => match crate::host::HostOp::decode_from(r)? {
                Ok(op) => TxKind::Host(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            23 => TxKind::Upgrade {
                contract: <[u8; 32]>::decode_from(r)?,
                code: Vec::decode_from(r)?,
                manifest: Manifest::decode_from(r)?,
                budget_gas: u64::decode_from(r)?,
            },
            22 => match crate::session::SessionOp::decode_from(r)? {
                Ok(op) => TxKind::Session(op),
                Err(t) => return Err(TxError::BadTag(t)),
            },
            t => return Err(TxError::BadTag(t)),
        };
        Ok(TxBody {
            chain_id,
            nonce,
            relayer,
            session,
            kind,
        })
    }

    /// The hash that is signed.
    pub fn signing_hash(&self) -> Hash {
        sha256(&[SIG_DOMAIN, &self.encode()])
    }
}

/// A signed transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub body: TxBody,
    pub public_key: [u8; 32],
    pub signature: [u8; 64],
}

/// Account address of a public key.
pub fn account_address(public_key: &[u8; 32]) -> Address {
    sha256(&[ACCOUNT_DOMAIN, public_key])
}

/// Address of the contract deployed by `deployer` with transaction nonce `nonce`.
pub fn contract_address(deployer: &Address, nonce: u64) -> Address {
    sha256(&[CONTRACT_DOMAIN, deployer, &nonce.to_be_bytes()])
}

impl Transaction {
    pub fn sign(body: TxBody, key: &SigningKey) -> Self {
        let sig = key.sign(&body.signing_hash());
        Transaction {
            public_key: key.verifying_key().to_bytes(),
            signature: sig.to_bytes(),
            body,
        }
    }

    /// The account the transaction acts as: the session's account if a session key signed it,
    /// else the address of the signing key. Nonce, fee and `caller()` are this account's.
    pub fn sender(&self) -> Address {
        self.body.session.unwrap_or_else(|| self.signer())
    }

    /// The address of the key that signed (the session key's, for a session transaction).
    pub fn signer(&self) -> Address {
        account_address(&self.public_key)
    }

    /// Whether the signature is a valid (strict) Ed25519 signature of the body by `public_key`.
    pub fn verify_signature(&self) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(&self.public_key) else {
            return false;
        };
        let sig = Signature::from_bytes(&self.signature);
        vk.verify_strict(&self.body.signing_hash(), &sig).is_ok()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.body.encode();
        out.extend_from_slice(&self.public_key);
        out.extend_from_slice(&self.signature);
        out
    }

    pub fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.encode());
    }

    pub fn decode_from(r: &mut Reader<'_>) -> Result<Self, TxError> {
        let body = TxBody::decode_body(r)?;
        Ok(Transaction {
            body,
            public_key: <[u8; 32]>::decode_from(r)?,
            signature: <[u8; 64]>::decode_from(r)?,
        })
    }

    /// Decode exactly one transaction (trailing bytes are an error).
    pub fn decode(bytes: &[u8]) -> Result<Self, TxError> {
        let mut r = Reader::new(bytes, MAX_CODE_BYTES);
        let tx = Self::decode_from(&mut r)?;
        if r.remaining() != 0 {
            return Err(DecodeError::TrailingBytes.into());
        }
        Ok(tx)
    }

    /// Transaction id: `SHA256("btb.tx.id.v1" || wire bytes)`.
    pub fn id(&self) -> Hash {
        sha256(&[ID_DOMAIN, &self.encode()])
    }

    /// Gas charged before the VM runs. For a deploy or transfer this is the whole gas, and it
    /// is charged in full.
    pub fn intrinsic_gas(&self) -> u64 {
        use crate::*;
        match &self.body.kind {
            TxKind::Transfer { .. } => TRANSFER_GAS,
            TxKind::Stake { .. } => STAKE_GAS,
            TxKind::Unstake { .. } => UNSTAKE_GAS,
            TxKind::RegisterValidator { .. } => REGISTER_VALIDATOR_GAS,
            TxKind::SubmitEvidence { .. } => EVIDENCE_GAS,
            TxKind::Channel(op) => op.gas(),
            TxKind::Site(op) => op.gas(),
            TxKind::Dex(op) => op.gas(),
            TxKind::Storage(op) => op.gas(),
            TxKind::Dkg(op) => op.gas(),
            TxKind::Cross(op) => op.gas(),
            TxKind::Action(op) => op.gas(),
            TxKind::Host(op) => op.gas(),
            TxKind::Session(op) => op.gas(),
            TxKind::Upgrade { code, .. } => crate::upgrade::UPGRADE_BASE_GAS
                .saturating_add(DEPLOY_BYTE_GAS.saturating_mul(code.len() as u64)),
            TxKind::Deploy { code, .. } => {
                DEPLOY_BASE_GAS.saturating_add(DEPLOY_BYTE_GAS.saturating_mul(code.len() as u64))
            }
            TxKind::Call { function, args, .. } => CALL_BASE_GAS
                .saturating_add(CALL_BYTE_GAS.saturating_mul((function.len() + args.len()) as u64)),
        }
    }

    /// The most gas the transaction can use.
    pub fn gas_limit(&self) -> u64 {
        match &self.body.kind {
            TxKind::Call { gas_limit, .. } => *gas_limit,
            TxKind::Action(op) => op.gas_limit(),
            _ => self.intrinsic_gas(),
        }
    }
}
