//! User tokens: the native multi-token module (`Token::create`, transfers, holder queries).
//!
//! These are separate from native BTB (`host::token_balance` / `host::token_transfer`, fixed
//! supply): creating a user token never mints BTB. Balances are `u128` and held by addresses; a
//! contract moves tokens from its own balance. See `docs/TOKENOMICS.md`.
//!
//! Failures that a contract can plausibly handle return [`TokenError`]; call `.unwrap_or_abort()`
//! to abort the call (and discard its state changes) instead.

use crate::host;
use alloc::string::String;
use alloc::vec::Vec;
use btb_codec::{Address, Decode, Encode};

/// Why a token call failed (mirrors the `TOKEN_*` status codes of the host ABI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// Bad name, symbol or decimals.
    Invalid,
    /// Balance too low.
    Insufficient,
    /// Unknown token.
    NoToken,
    /// Not the mint authority.
    Unauthorized,
    /// Supply, balance or nonce overflow.
    Overflow,
    /// The host has no token ledger.
    Unsupported,
}

impl TokenError {
    fn from_code(c: u64) -> Self {
        match c {
            1 => TokenError::Invalid,
            2 => TokenError::Insufficient,
            3 => TokenError::NoToken,
            4 => TokenError::Unauthorized,
            5 => TokenError::Overflow,
            _ => TokenError::Unsupported,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            TokenError::Invalid => "invalid token parameters",
            TokenError::Insufficient => "insufficient token balance",
            TokenError::NoToken => "unknown token",
            TokenError::Unauthorized => "not the mint authority",
            TokenError::Overflow => "token overflow",
            TokenError::Unsupported => "tokens unsupported",
        }
    }
}

/// Abort the call with the error message instead of handling it.
pub trait UnwrapOrAbort<T> {
    fn unwrap_or_abort(self) -> T;
}

impl<T> UnwrapOrAbort<T> for Result<T, TokenError> {
    fn unwrap_or_abort(self) -> T {
        match self {
            Ok(v) => v,
            Err(e) => host::abort(e.message()),
        }
    }
}

fn check(code: u64) -> Result<(), TokenError> {
    if code == 0 {
        Ok(())
    } else {
        Err(TokenError::from_code(code))
    }
}

/// What `Token::info` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub supply: u128,
    pub mint_authority: Option<Address>,
    pub creator: Address,
}

/// A page of holders, ordered by address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HolderPage {
    pub holders: Vec<(Address, u128)>,
    /// Pass back as `cursor` for the next page; `None` when there are no more holders.
    pub next_cursor: Option<Address>,
}

/// Handle to a user token. Encodes as its 32 raw id bytes, so it can be a call argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Token(pub [u8; 32]);

impl Encode for Token {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }
}

impl Decode for Token {
    fn decode_from(r: &mut btb_codec::Reader<'_>) -> Result<Self, btb_codec::DecodeError> {
        Ok(Token(Address::decode_from(r)?.0))
    }
}

impl Token {
    /// Create a token. `initial_supply` is credited to this contract. With `mint_authority:
    /// None` the supply is fixed forever; otherwise only that address may `mint` / `burn`
    /// (calls run as the authority contract). The id is `SHA256("btb.token.v1" || this contract
    /// || creation nonce)`. Mutations and crons only. Never mints native BTB.
    pub fn create(
        name: &str,
        symbol: &str,
        decimals: u8,
        initial_supply: u128,
        mint_authority: Option<Address>,
    ) -> Result<Token, TokenError> {
        let req = (
            String::from(name),
            String::from(symbol),
            decimals,
            initial_supply,
            mint_authority,
        )
            .encode();
        host::token_create_raw(&req)
            .map(Token)
            .map_err(TokenError::from_code)
    }

    /// Move `amount` from this contract's balance to `to`. Mutations and crons only.
    pub fn transfer(&self, to: &Address, amount: u128) -> Result<(), TokenError> {
        check(host::token_transfer_of_raw(&self.0, to, amount))
    }

    /// Balance of `holder` (zero if none).
    pub fn balance_of(&self, holder: &Address) -> Result<u128, TokenError> {
        host::token_balance_of_raw(&self.0, holder).map_err(TokenError::from_code)
    }

    /// Name, symbol, decimals, supply, authority and creator; `None` for an unknown token.
    pub fn info(&self) -> Option<TokenInfo> {
        let raw = host::token_info_raw(&self.0)?;
        match <(String, String, u8, u128, Option<Address>, Address)>::decode(&raw) {
            Ok((name, symbol, decimals, supply, mint_authority, creator)) => Some(TokenInfo {
                name,
                symbol,
                decimals,
                supply,
                mint_authority,
                creator,
            }),
            Err(_) => host::abort("corrupt token info"),
        }
    }

    /// Holders with a non-zero balance, ascending by address, at most `limit` (capped at 256)
    /// per page. Pass the previous page's `next_cursor` as `cursor`. Allowed in queries.
    pub fn holders(&self, cursor: Option<&Address>, limit: u32) -> Result<HolderPage, TokenError> {
        host::token_holders_raw(&self.0, cursor, limit)
            .map(|(holders, next_cursor)| HolderPage {
                holders,
                next_cursor,
            })
            .ok_or(TokenError::NoToken)
    }

    /// Mint `amount` to `to`. Only the mint authority (this contract must be it).
    pub fn mint(&self, to: &Address, amount: u128) -> Result<(), TokenError> {
        check(host::token_mint_raw(&self.0, to, amount))
    }

    /// Burn `amount` from this contract's own balance. Only the mint authority.
    pub fn burn(&self, amount: u128) -> Result<(), TokenError> {
        check(host::token_burn_raw(&self.0, amount))
    }
}
