# 0002: Named accounts with hidden, rotating keys

Status: **Draft**. Issue: to be opened by a maintainer as a Design Proposal. Touches cryptography, transactions and state: needs the Safety section below and an external review before any phase ships to a public network.

## Summary

Separate **who an account is** from **which keys control it**. An account gets a permanent id and an optional name (`alice.btb`). It is controlled by a set of keys that the chain stores only as **hash commitments**, never as public keys. A key's public key is revealed only in the transaction that uses it, and that same transaction **retires the key and commits to the next one**, so a revealed public key never controls funds again. On top of this: passkey login with spending limits, several devices, guardian recovery, and post-quantum signature schemes as additional key types.

**Recommendation: build phases A and B (account record with hidden rotating keys, names) first; C to E after the public testnet.**

## Problem

Today ([CHAIN.md](../CHAIN.md#transactions), `crates/btb-chain/src/tx.rs`):

- An address is `SHA-256("btb.account.v1" || ed25519 public key)`. The key can never change: a lost key loses the account, a stolen key loses it to the thief.
- Every transaction carries the signer's **public key**. After an account's first transaction its public key is public forever. A future quantum computer that breaks Ed25519 (Shor's algorithm breaks every elliptic-curve scheme: Ed25519, secp256k1, P-256) could derive the private key of every account that has ever sent a transaction and take its funds. This is the same exposure Ethereum and Bitcoin have and are discussing migration plans for.
- Addresses are 64 hex characters with no checksum and no name: unreadable and easy to mistype.
- Users must keep a raw secret (a key file or, later, a seed phrase). Lost secrets are the most common way people lose crypto.

## Goals

1. An account survives losing or replacing any one key.
2. **No public key that controls funds is ever visible on chain or in state.** A public key becomes visible only in the transaction that spends with it, and that key is retired by the same transaction.
3. Sending to a person by name (`alice.btb`), with the address form for machines.
4. Normal login (passkeys: Face ID, fingerprint, Windows Hello) for everyday use, without seed phrases.
5. A path to post-quantum signatures without another migration.
6. Existing accounts keep working.

Non-goals: privacy of balances or transfers (transfers stay public; see [WHY_BTB.md](../WHY_BTB.md#where-btb-is-not-ahead-yet)); hiding *which* account sent a transaction.

## Design

### Account record

```
Account {
    id:        Address            // permanent; never changes when keys change
    name:      Option<Name>       // "alice.btb", see Names
    slots:     Vec<KeySlot>       // at most 8
    threshold: u8                 // slots needed for "admin" actions (add/remove slots, recovery)
    recovery:  Option<Recovery>
    nonce, balance, ...           // as today
}

KeySlot {
    commitment: Hash              // SHA-256("btb.key.v1" || scheme || public key)
    scheme:     Scheme            // Ed25519 | P256Passkey | MlDsa44 | SlhDsa128s
    role:       Role              // Admin | Spend { daily_limit } | Session-like scopes later
    rotate:     bool              // true: the key is one-time (see below)
}
```

State stores only `commitment`, never the public key. The account id for a new account is `SHA-256("btb.account.v2" || creator || nonce)` (like contract addresses), so it does not depend on any key.

### Spending with a hidden key

A transaction names the account and the slot and carries:

```
auth: { slot: u8, scheme, public_key, signature, next_commitment: Option<Hash> }
```

The chain checks, in order: `SHA-256("btb.key.v1" || scheme || public_key) == slots[slot].commitment`; the signature over the transaction body (which covers `next_commitment`); the slot's role allows this kind of transaction and amount. For a slot with `rotate = true`, `next_commitment` is **required** and replaces the slot's commitment in the same transaction. The revealed key can never sign for the account again.

Wallets derive one-time keys deterministically (`key_i = KDF(device secret, account, slot, i)`), so the user keeps one device secret and never sees the rotation.

**What this buys against a quantum attacker.** At rest, an account shows only hashes; Shor's algorithm cannot invert SHA-256, and Grover's algorithm only halves its strength (128-bit security remains). The exposure window is between broadcasting a transaction and its inclusion: an attacker would have to derive the private key from the public key in the mempool and get a *conflicting* transaction (same account and nonce) ordered first. BTB's finality is about a second ([NODE.md](../NODE.md#measured-finality)), the first valid transaction for a nonce wins in the mempool, and a proposer that orders the attacker's copy instead must also be the attacker or bribed. This is a large reduction, not elimination; **phase E removes the window** by using signature schemes quantum computers cannot break.

### Funds behind the name

Users who want a cold "vault" behind their everyday account use two accounts: a vault account whose only slots are rotating keys (never revealed until a withdrawal, then immediately replaced) or post-quantum keys, and an everyday account with a passkey and a daily limit, topped up from the vault. `alice.btb` can point to either. Nothing on chain links the vault's future keys to anything visible.

### Several keys, several devices

Each device adds its own slot (phone passkey, laptop passkey, a hardware key, an offline one-time key). Admin actions (adding or removing slots, changing the threshold or recovery) need `threshold` admin slots in one transaction (each with its own `auth` entry). Ordinary spending needs one `Spend` slot within its limit.

### Passkeys

A passkey signs with P-256 (WebAuthn). The chain verifies the WebAuthn assertion: `authenticatorData || SHA-256(clientDataJSON)` signed with P-256, where `clientDataJSON.challenge` must equal the transaction's signing hash and the origin must be in the account's allowed list. P-256 is **not** quantum safe and a passkey's public key is fixed by the device (it cannot rotate per use), so passkey slots are allowed only with role `Spend { daily_limit }`; admin rights and large amounts belong to rotating or post-quantum slots.

### Recovery

```
Recovery { guardians: Vec<Hash>, needed: u8, delay_blocks: u64 }
```

Guardians are commitments to other accounts' ids (friends, a second device's account, a recovery service). `needed` guardians sign a `RecoveryStart { new_slot }`; after `delay_blocks` (default about 48 hours) without the account cancelling with any admin slot, `RecoveryFinish` installs the new slot. The account can veto during the delay with a single admin slot, so stolen guardians alone cannot take it.

### Names

A native name registry (not a contract): `name -> account id`, one name per account, lowercase `a-z 0-9 -`, 3 to 32 characters, suffix `.btb`.

- Registering **locks a refundable deposit** (more for shorter names) in the account; no BTB is minted or burned, the supply invariant is unchanged. Releasing a name refunds the deposit after a delay.
- Transfers, calls and the SDK accept `alice.btb` and resolve it at execution, so a transaction says which name it meant; the receipt records the resolved id.
- **Names can be sold or given away.** A name is owned by its account and transferable:
  - `NameList { name, price, buyer: Option<Address>, expires_height }`: the owner offers it for sale (to anyone, or to one buyer). `NameUnlist` withdraws the offer.
  - `NameBuy { name, max_price }`: an atomic swap in one transaction: the buyer pays `price` to the seller, the name moves to the buyer, the seller's deposit is refunded and the buyer locks the deposit. If any part fails, nothing happens. No escrow contract, no middleman, no trust between the parties.
  - `NameGive { name, to }`: transfer without payment (the receiver locks the deposit).
  - Admin rights are needed to list, unlist or give a name (a passkey's spending slot cannot sell your name).
  - Offers are cancelled automatically when the name changes owner; a sale never moves funds already held by the account, only the name.
  - Optional marketplace fee: a marketplace contract can call `NameBuy` for the buyer and take its own fee; the protocol itself takes only gas.
- Display form for machines: `btb1…` (bech32m of the 32-byte id, with a checksum). Raw 64-hex stays accepted.

### Gas

Every verification has a fixed cost, measured against today's Ed25519 cost before the phase ships:

| Scheme | Public key | Signature | Relative verification cost (to measure) |
| --- | --- | --- | --- |
| Ed25519 | 32 B | 64 B | 1x (today) |
| P-256 passkey (WebAuthn) | 33 B | 64 B + authenticator data (~200 B) | ~3x |
| ML-DSA-44 (FIPS 204) | 1,312 B | 2,420 B | ~2x, plus bytes |
| SLH-DSA-128s (FIPS 205, hash-based) | 32 B | 7,856 B | ~10-20x, plus bytes |

Transaction size is charged per byte as today; the 25 MiB block payload leaves room for the larger signatures.

### Migration

Existing accounts are unchanged: an Ed25519 account is an account whose id is its old address and which has one implicit `Ed25519, Admin, rotate = false` slot. Its owner can add slots and then remove the implicit one; from then on the old public key (already visible) controls nothing. Wallets should prompt for this before a public mainnet.

## Phases

| Phase | Contents | Depends on |
| --- | --- | --- |
| **A** | Account record, `auth` with commitments, Ed25519 slots, rotation on use, admin threshold, migration of existing accounts | none |
| **B** | Name registry with deposits, selling and giving names (atomic swap), `btb1…` addresses, name resolution in transfers, calls, CLI and `@btb/client` | A |
| **C** | Passkey slots (P-256 WebAuthn) with daily limits; browser flow in `@btb/client` | A |
| **D** | Guardian recovery with delay and veto | A |
| **E** | Post-quantum slots: ML-DSA-44 and SLH-DSA-128s | A; a reviewed, permissively licensed implementation |

## Safety

- **Signature checks are consensus-critical.** Every validator must accept exactly the same signatures. Pin library versions, reject non-canonical encodings (P-256 high-S, malformed WebAuthn data), and test against official vectors (Wycheproof for P-256, NIST ACVP for ML-DSA and SLH-DSA).
- **Commitment domain separation.** The commitment includes the scheme, so a key cannot be reinterpreted under another scheme.
- **Rotation is atomic.** The new commitment is set in the same state change as the spend; a reverted transaction (VM failure) still rotates, because the public key was revealed either way.
- **Replay.** The account nonce is unchanged and shared by every slot; a signature covers chain id, account, slot and nonce.
- **Limits are enforced before execution** (admission and block validity), like session keys ([session.rs](../../crates/btb-chain/src/session.rs)): a spend over a passkey's daily limit is invalid, not reverted.
- **Recovery cannot be rushed.** The delay is a number of blocks, the veto needs one admin slot, and every step emits an event the wallet can alert on.
- **Names cannot redirect funds silently.** Resolution happens at execution and is recorded in the receipt; a name released and re-registered gets a new owner only after a delay.
- **No inflation.** Name deposits are locked balances; recovery and accounts mint nothing.
- **Quantum limits are stated honestly.** Phases A to D shrink exposure to the mempool window; only phase E removes it.

## Tests that prove it

- Commitment mismatch, wrong scheme, wrong slot, bad signature: invalid at admission and in blocks.
- A rotating slot used once cannot sign again with the same key (even in the same block, even after a revert).
- Missing `next_commitment` on a rotating slot: invalid.
- Admin action with fewer than `threshold` slots: invalid; with exactly `threshold`: applied.
- Passkey over its daily limit: invalid; the limit resets by block height, not wall clock.
- WebAuthn: wrong challenge, wrong origin, high-S signature, truncated authenticator data: invalid.
- Recovery: finishing before the delay fails; a veto cancels; stolen guardians without the delay cannot change slots.
- Names: duplicate registration, invalid characters, release and re-registration delay; deposit returned; total supply unchanged across every name operation.
- Name sales: a buy at a price below the listing, after expiry, by the wrong buyer, or with insufficient balance fails with no change; a successful buy moves exactly the price, the name and both deposits in one state change; a listing does not survive a transfer; a spending slot cannot list or give a name.
- State never contains a public key of a rotating or post-quantum slot (scan of the account prefix after a run).
- Old Ed25519 accounts sign as before; after removing the implicit slot, the old key is refused.
- Every validator in a cluster agrees on all of the above (`btb-node` cluster test).

## Files affected

`crates/btb-chain/src/tx.rs` (auth, account v2 ids), a new `crates/btb-chain/src/account.rs` (record, slots, rotation, recovery), `crates/btb-chain/src/names.rs`, `exec.rs` and `mempool.rs` (validity checks), `crates/btb-cli` (names, `btb1…`, key rotation in `call`/`deploy`), `sdk-ts` (passkeys, names), `docs/CHAIN.md`, `docs/TS_SDK.md`, `docs/CLI.md`, `docs/GLOSSARY.md`.

## Open questions

1. Name deposit schedule (by length) and whether expired names exist at all.
2. After a name is sold, should payments to it be held for a short period (for example a few hours) so senders who looked it up before the sale are not surprised, or is the receipt's resolved id enough?
3. Should transfers by name require the receiver's name to be older than some number of blocks (anti-phishing for look-alike names)?
4. Which ML-DSA / SLH-DSA implementation: RustCrypto's crates are permissively licensed but young; an audit budget decides.
5. Encrypted or threshold-decrypted mempool to close the remaining quantum window in phases A to D, or rely on phase E only?
6. Does a contract account get slots too (upgrade authority as a slot), or stay code-controlled only?
