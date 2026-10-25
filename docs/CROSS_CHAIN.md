# Cross-Chain Design

BTB contracts can read from and write to other chains — Ethereum, Bitcoin, Solana and more — **without deploying contracts there**. The logic always runs on BTB; other chains only see ordinary transactions.

## Writing: chain signatures

BTB validators jointly hold keys using threshold cryptography (MPC). No single validator ever has a full key, but when two-thirds agree they produce a valid signature together. Those keys control normal addresses on other chains:

- An Ethereum address, a Bitcoin address, a Solana account — each owned by a BTB contract.
- A contract calls something like `eth.send_tx(...)`; validators sign it; it lands on Ethereum as a normal transaction.
- The target chain does not know BTB exists, so nothing is deployed there.

Supported signature schemes: threshold ECDSA (Ethereum, Bitcoin legacy), Schnorr (Bitcoin Taproot), EdDSA (Solana).

NEAR (Chain Signatures) and the Internet Computer (Chain Fusion) already run this approach in production.

## Reading: validator-agreed observations

Users never supply RPC endpoints, because a single RPC can lie. Validators each observe other chains through their own nodes or several providers and agree on the result in consensus. Later, light clients that verify the other chain's proofs directly can replace trust in RPC data.

## Example: native cross-chain swap

1. A user sends real ETH to a BTB-controlled Ethereum vault address.
2. Validators observe the deposit and credit the user inside the BTB DEX.
3. The user swaps ETH for SOL on BTB — instant and cheap.
4. BTB signs a Solana transaction sending real SOL from its Solana vault to the user's wallet.

No wrapped tokens and no bridge contracts. A BTB contract can also call existing contracts on other chains, for example executing a swap on Uniswap from a BTB-owned Ethereum address.

## Limits and risks

| Limit | Implication |
| --- | --- |
| No custom logic on other chains | If logic must run inside Ethereum atomically with other Ethereum contracts, a contract is still needed there |
| Gas on each target chain | Vault addresses need ETH, SOL, etc. for fees; the protocol charges users and keeps vaults topped up |
| Speed of the slowest chain | Bitcoin deposits need several confirmations; Ethereum needs minutes for safe finality |
| Validator collusion | Two-thirds of validators control every vault. Vault value must stay below what slashable BTB stake covers — the classic bridge risk |
| Liquidity in vaults | Real assets must sit in each chain's vault for withdrawals to work |

## Rollout

Cross-chain is part of **Phase 5** in the [roadmap](../ROADMAP.md), starting with read-only observation of Ethereum, then signed withdrawals with conservative per-vault caps, then Bitcoin and Solana.
