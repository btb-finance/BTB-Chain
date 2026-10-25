//! Native multi-token module: user-created fungible tokens, separate from native BTB.
//!
//! Creating a token never touches BTB: the native balance (`token_balance` / `token_transfer`)
//! and its fixed supply are unaffected. A user token has its own id, balances (`u128`, held by
//! addresses, a contract's own address included) and a supply that only changes through
//! `token_mint` / `token_burn` by the token's mint authority, with checked math.
//!
//! The rules live here once, over the small [`TokenStore`] trait; `MemHost` and `RuntimeHost`
//! only provide storage. Every operation validates first and writes last, so a failed call
//! (an error status) changes nothing.

use crate::host::Address;
use btb_codec::{Address as CAddress, Decode, Encode};
use sha2::{Digest, Sha256};

/// 32-byte token id: `SHA256("btb.token.v1" || creator contract || nonce:u64be)`.
pub type TokenId = [u8; 32];

/// Holders with their balances, as returned by one page.
pub type HolderRows = Vec<(Address, u128)>;

pub const MAX_NAME_LEN: usize = 64;
pub const MAX_SYMBOL_LEN: usize = 12;
pub const MAX_DECIMALS: u8 = 38;
/// Largest `limit` honoured by `token_holders`.
pub const MAX_HOLDERS_LIMIT: u32 = 256;
/// Upper bound on an encoded `token_create` request.
pub const MAX_CREATE_REQ: usize = 4096;

/// Why a token operation failed. `code` is the value returned to the guest (see `abi::TOKEN_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// Bad name, symbol or decimals.
    Invalid,
    /// Balance too low.
    Insufficient,
    /// Unknown token id.
    NoToken,
    /// Caller is not the mint authority (or the token has none).
    Unauthorized,
    /// A supply, balance or nonce would overflow `u128` / `u64`.
    Overflow,
    /// The host has no token ledger.
    Unsupported,
}

impl TokenError {
    pub fn code(self) -> u64 {
        use crate::host::abi;
        match self {
            TokenError::Invalid => abi::TOKEN_INVALID,
            TokenError::Insufficient => abi::TOKEN_INSUFFICIENT,
            TokenError::NoToken => abi::TOKEN_NO_TOKEN,
            TokenError::Unauthorized => abi::TOKEN_UNAUTHORIZED,
            TokenError::Overflow => abi::TOKEN_OVERFLOW,
            TokenError::Unsupported => abi::TOKEN_UNSUPPORTED,
        }
    }
}

/// Parameters of `token_create`, encoded with `btb-codec` as the tuple
/// `(name: String, symbol: String, decimals: u8, initial_supply: u128, mint_authority: Option<Address>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateParams {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    /// Credited to the creating contract.
    pub initial_supply: u128,
    /// `None`: the supply is fixed forever. Otherwise only this address may mint and burn.
    pub mint_authority: Option<Address>,
}

impl CreateParams {
    pub fn encode(&self) -> Vec<u8> {
        (
            self.name.clone(),
            self.symbol.clone(),
            self.decimals,
            self.initial_supply,
            self.mint_authority.map(CAddress),
        )
            .encode()
    }

    /// `None` if malformed (including trailing bytes or an oversize request).
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() > MAX_CREATE_REQ {
            return None;
        }
        let (name, symbol, decimals, initial_supply, auth) =
            <(String, String, u8, u128, Option<CAddress>)>::decode_with_max(b, MAX_CREATE_REQ)
                .ok()?;
        Some(Self {
            name,
            symbol,
            decimals,
            initial_supply,
            mint_authority: auth.map(|a| a.0),
        })
    }

    fn validate(&self) -> Result<(), TokenError> {
        let sym_ok = !self.symbol.is_empty()
            && self.symbol.len() <= MAX_SYMBOL_LEN
            && self.symbol.bytes().all(|b| b.is_ascii_alphanumeric());
        if self.name.is_empty()
            || self.name.len() > MAX_NAME_LEN
            || !sym_ok
            || self.decimals > MAX_DECIMALS
        {
            return Err(TokenError::Invalid);
        }
        Ok(())
    }
}

/// What `token_info` returns, encoded with `btb-codec` as the tuple
/// `(name, symbol, decimals, supply: u128, mint_authority: Option<Address>, creator: Address)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub supply: u128,
    pub mint_authority: Option<Address>,
    pub creator: Address,
}

impl TokenInfo {
    pub fn encode(&self) -> Vec<u8> {
        (
            self.name.clone(),
            self.symbol.clone(),
            self.decimals,
            self.supply,
            self.mint_authority.map(CAddress),
            CAddress(self.creator),
        )
            .encode()
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let (name, symbol, decimals, supply, auth, creator) =
            <(String, String, u8, u128, Option<CAddress>, CAddress)>::decode_with_max(
                b,
                MAX_CREATE_REQ,
            )
            .ok()?;
        Some(Self {
            name,
            symbol,
            decimals,
            supply,
            mint_authority: auth.map(|a| a.0),
            creator: creator.0,
        })
    }
}

/// Deterministic token id of the `nonce`-th token created by `creator`.
pub fn derive_id(creator: &Address, nonce: u64) -> TokenId {
    let mut h = Sha256::new();
    h.update(b"btb.token.v1");
    h.update(creator);
    h.update(nonce.to_be_bytes());
    h.finalize().into()
}

/// Storage behind the token rules. Implementations never fail the rules; a backend fault is
/// recorded by the implementor (see `RuntimeHost`).
pub trait TokenStore {
    /// The contract that is running (the creator, sender and mint-authority candidate).
    fn contract(&self) -> Address;
    fn info(&mut self, id: &TokenId) -> Option<TokenInfo>;
    fn put_info(&mut self, id: &TokenId, info: &TokenInfo);
    fn balance(&mut self, id: &TokenId, holder: &Address) -> u128;
    /// Zero removes the holder entry.
    fn put_balance(&mut self, id: &TokenId, holder: &Address, v: u128);
    fn nonce(&mut self, creator: &Address) -> u64;
    fn put_nonce(&mut self, creator: &Address, n: u64);
    /// Up to `max` holders with a non-zero balance and address > `after`, ascending by address.
    fn holders(&mut self, id: &TokenId, after: Option<&Address>, max: usize) -> HolderRows;
}

pub fn create(s: &mut dyn TokenStore, p: &CreateParams) -> Result<TokenId, TokenError> {
    p.validate()?;
    let creator = s.contract();
    let nonce = s.nonce(&creator);
    let next = nonce.checked_add(1).ok_or(TokenError::Overflow)?;
    let id = derive_id(&creator, nonce);
    s.put_nonce(&creator, next);
    s.put_info(
        &id,
        &TokenInfo {
            name: p.name.clone(),
            symbol: p.symbol.clone(),
            decimals: p.decimals,
            supply: p.initial_supply,
            mint_authority: p.mint_authority,
            creator,
        },
    );
    s.put_balance(&id, &creator, p.initial_supply);
    Ok(id)
}

/// Move `amount` from the running contract to `to`.
pub fn transfer(
    s: &mut dyn TokenStore,
    id: &TokenId,
    to: &Address,
    amount: u128,
) -> Result<(), TokenError> {
    s.info(id).ok_or(TokenError::NoToken)?;
    let from = s.contract();
    let have = s.balance(id, &from);
    let rest = have.checked_sub(amount).ok_or(TokenError::Insufficient)?;
    if *to == from {
        return Ok(());
    }
    let new_to = s
        .balance(id, to)
        .checked_add(amount)
        .ok_or(TokenError::Overflow)?;
    s.put_balance(id, &from, rest);
    s.put_balance(id, to, new_to);
    Ok(())
}

pub fn balance_of(
    s: &mut dyn TokenStore,
    id: &TokenId,
    holder: &Address,
) -> Result<u128, TokenError> {
    s.info(id).ok_or(TokenError::NoToken)?;
    Ok(s.balance(id, holder))
}

fn authorised(s: &mut dyn TokenStore, id: &TokenId) -> Result<TokenInfo, TokenError> {
    let info = s.info(id).ok_or(TokenError::NoToken)?;
    if info.mint_authority == Some(s.contract()) {
        Ok(info)
    } else {
        Err(TokenError::Unauthorized)
    }
}

/// Mint `amount` to `to`. Only the mint authority; supply and balance use checked math.
pub fn mint(
    s: &mut dyn TokenStore,
    id: &TokenId,
    to: &Address,
    amount: u128,
) -> Result<(), TokenError> {
    let mut info = authorised(s, id)?;
    info.supply = info
        .supply
        .checked_add(amount)
        .ok_or(TokenError::Overflow)?;
    let new_to = s
        .balance(id, to)
        .checked_add(amount)
        .ok_or(TokenError::Overflow)?;
    s.put_info(id, &info);
    s.put_balance(id, to, new_to);
    Ok(())
}

/// Burn `amount` from the running contract's own balance. Only the mint authority.
pub fn burn(s: &mut dyn TokenStore, id: &TokenId, amount: u128) -> Result<(), TokenError> {
    let mut info = authorised(s, id)?;
    let me = s.contract();
    let rest = s
        .balance(id, &me)
        .checked_sub(amount)
        .ok_or(TokenError::Insufficient)?;
    // Supply >= any balance by construction; checked anyway.
    info.supply = info
        .supply
        .checked_sub(amount)
        .ok_or(TokenError::Insufficient)?;
    s.put_info(id, &info);
    s.put_balance(id, &me, rest);
    Ok(())
}

/// One page of holders ordered by address; `next` (the last address returned) is set when more
/// holders exist and is passed back as `after`.
pub fn holders(
    s: &mut dyn TokenStore,
    id: &TokenId,
    after: Option<&Address>,
    limit: u32,
) -> Result<(HolderRows, Option<Address>), TokenError> {
    s.info(id).ok_or(TokenError::NoToken)?;
    let limit = limit.min(MAX_HOLDERS_LIMIT) as usize;
    let mut page = s.holders(id, after, limit + 1);
    let more = page.len() > limit;
    page.truncate(limit);
    let next = if more {
        page.last().map(|(a, _)| *a)
    } else {
        None
    };
    Ok((page, next))
}

/// Decoded `token_holders` request: `id[32] || limit:u32le || cursor (empty or 32 bytes)`.
pub fn parse_holders_req(req: &[u8]) -> Option<(TokenId, u32, Option<Address>)> {
    let id: TokenId = req.get(..32)?.try_into().ok()?;
    let limit = u32::from_le_bytes(req.get(32..36)?.try_into().ok()?);
    let cursor = match req.len() {
        36 => None,
        68 => Some(req[36..68].try_into().ok()?),
        _ => return None,
    };
    Some((id, limit, cursor))
}
