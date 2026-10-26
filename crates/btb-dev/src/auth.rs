//! `--auth signed`: real signatures on a development server.
//!
//! The default (`header`) mode takes the caller from a header and checks nothing. In `signed`
//! mode a mutation must carry `X-BTB-Tx: <hex of a signed btb-chain Call transaction>` for the
//! dev contract, and session grants are sent as signed `Session` transactions to
//! `POST /auth/tx`. The rules are the chain's (`btb_chain::session`): the nonce is the
//! account's, a session key acts as the account within its grant (contract, functions, expiry
//! in dev heights, fee budget), anything else is rejected. Dev has no balances, so the fee a
//! call is charged is its `gas_limit * gas_price` (the reservation). Queries stay open
//! (`X-BTB-Caller`): like on the chain, reads are not signed.

use btb_chain::session;
use btb_chain::{ChainConfig, Transaction, TxKind};
use btb_runtime::MemKv;
use std::collections::BTreeMap;

pub const DEV_CHAIN_ID: u64 = 1337;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AuthMode {
    /// Caller from a header, no checks.
    #[default]
    Header,
    /// Signed transactions and session keys.
    Signed,
}

impl AuthMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "header" => Ok(AuthMode::Header),
            "signed" => Ok(AuthMode::Signed),
            _ => Err(format!("--auth must be header or signed, got {s:?}")),
        }
    }
}

pub struct DevAuth {
    pub mode: AuthMode,
    kv: MemKv,
    nonces: BTreeMap<[u8; 32], u64>,
    cfg: ChainConfig,
}

impl Default for DevAuth {
    fn default() -> Self {
        DevAuth {
            mode: AuthMode::Header,
            kv: MemKv::new(),
            nonces: BTreeMap::new(),
            cfg: ChainConfig::new(DEV_CHAIN_ID),
        }
    }
}

/// A verified call: who it acts as and the call itself.
pub struct Verified {
    pub account: [u8; 32],
    pub via_session: bool,
    pub function: String,
    pub args: Vec<u8>,
}

impl DevAuth {
    /// The next nonce of `account`.
    pub fn nonce(&self, account: &[u8; 32]) -> u64 {
        self.nonces.get(account).copied().unwrap_or(0)
    }

    fn basic(&self, tx: &Transaction) -> Result<(), String> {
        if tx.body.chain_id != DEV_CHAIN_ID {
            return Err(format!("wrong chain id, dev chain id is {DEV_CHAIN_ID}"));
        }
        if !tx.verify_signature() {
            return Err("bad signature".into());
        }
        session::check_kind(tx).map_err(|e| format!("{e:?}"))?;
        let want = self.nonces.get(&tx.sender()).copied().unwrap_or(0);
        if tx.body.nonce != want {
            return Err(format!("bad nonce: expected {want}, got {}", tx.body.nonce));
        }
        Ok(())
    }

    /// `POST /auth/tx`: a signed session grant or revoke.
    pub fn session_tx(&mut self, bytes: &[u8], height: u64) -> Result<(), String> {
        let tx = Transaction::decode(bytes).map_err(|e| format!("bad transaction: {e:?}"))?;
        self.basic(&tx)?;
        let TxKind::Session(op) = &tx.body.kind else {
            return Err("only Session transactions go to /auth/tx".into());
        };
        if tx.body.session.is_some() {
            return Err("a session cannot grant or revoke sessions".into());
        }
        session::apply_to(&mut self.kv, height, &tx.sender(), op)?;
        *self.nonces.entry(tx.sender()).or_insert(0) += 1;
        Ok(())
    }

    /// Check a signed call before it runs; on success the nonce and the budget are spent
    /// (a call that then fails in the VM still costs them, like on the chain).
    pub fn call_tx(
        &mut self,
        bytes: &[u8],
        height: u64,
        contract: &[u8; 32],
    ) -> Result<Verified, String> {
        let tx = Transaction::decode(bytes).map_err(|e| format!("bad transaction: {e:?}"))?;
        self.basic(&tx)?;
        let TxKind::Call {
            contract: to,
            function,
            args,
            gas_limit,
            ..
        } = &tx.body.kind
        else {
            return Err("a mutation needs a Call transaction".into());
        };
        if to != contract {
            return Err("the transaction calls another contract".into());
        }
        let via = tx.body.session.is_some();
        if via {
            session::check_admission(&self.kv, &tx, height, &self.cfg)
                .map_err(|e| format!("session refused: {e:?}"))?;
            let fee = gas_limit.saturating_mul(self.cfg.gas_price);
            session::charge_to(&mut self.kv, &tx.sender(), &tx.public_key, fee)?;
        }
        *self.nonces.entry(tx.sender()).or_insert(0) += 1;
        Ok(Verified {
            account: tx.sender(),
            via_session: via,
            function: function.clone(),
            args: args.clone(),
        })
    }
}

thread_local! {
    static VIA_SESSION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark the mutation about to run on this thread as session-signed (`auth::via_session`).
pub fn set_via_session(v: bool) {
    VIA_SESSION.with(|c| c.set(v));
}

/// Whether the call being run on this thread came through a session key.
pub fn via_session() -> bool {
    VIA_SESSION.with(std::cell::Cell::get)
}
