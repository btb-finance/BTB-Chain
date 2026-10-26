# nft

Example BTB contract: NFTs and a marketplace that sells them for a user token (`btb_sdk::token::Token`), with an escrow ledger. Web page: [web/index.html](web/index.html), served by `btb dev`.

| Kind | Function |
| --- | --- |
| mutation | `mint(uri) -> u64`: new NFT owned by the caller (`uri` 1 to 256 bytes) |
| mutation | `transfer(id, to)`: owner only; aborts while the NFT is listed |
| mutation | `list(id, token, price) -> u64`: owner only, one open listing per NFT, `price > 0`, token must exist; returns the listing id |
| mutation | `mint_and_list(uri, token, price) -> (u64, u64)`: both or neither |
| mutation | `cancel(listing)`: seller only |
| mutation | `buy(listing)`: pays the seller from the buyer's escrow, moves the NFT, closes the listing |
| mutation | `deposit(token, amount)`, `withdraw(token, amount)`: the escrow ledger, below |
| mutation | `create_currency(name, symbol, decimals, supply) -> Token`: demo helper, the contract holds the supply |
| query | `my_nfts() -> Vec<(id, uri)>` (read through the `owner` index), `nft(id) -> (owner, uri)`, `listing_of(nft) -> Option<(listing id, token, price)>` (primary-key lookup) |
| query | `listings(cursor, limit) -> (Vec<(listing, nft, seller, token, price, uri)>, Option<cursor>)`, ascending listing id, `limit` 1 to 50, start with cursor 0 |
| query | `escrow_of(token, who)`, `escrowed(token)`, `unclaimed(token)`, `balance_of(token, who)`, `info(token)` |

Tables: `Nft { #[index] owner, uri }` and `Listing { #[primary] nft, seller, token, price }` (deleted on cancel and on sale). The indexes are declared on the fields with `#[btb::table]`; `btb dev` and the chain read them from the contract, and the unique primary key on `nft` is what allows only one open listing per NFT.

## The escrow model (contract-as-sender)

`Token::transfer` moves tokens out of the **calling contract's own balance**; there are no user-signed token transfers or attached funds yet. A buyer therefore cannot pay from a wallet inside `buy`. Instead:

1. Tokens sit in the market contract. The **escrow ledger** (`e:<token><owner> -> u128`, plus `t:<token> -> u128`, the sum per token) records who may spend how much of them.
2. `deposit(token, amount)` credits the caller with `amount` out of the **unclaimed surplus**, `balance_of(contract) - escrowed(token)`, and aborts if the surplus is smaller. The contract cannot see who sent tokens to it, only its balance, so this is first come, first served and front-runnable. It is a placeholder until a transfer carries its sender. In the demo, `create_currency` makes a token whose whole supply is unclaimed, so anyone can fund themselves.
3. `buy` debits the buyer's escrow (`insufficient escrow` if short) and sends `price` to the seller with a real `Token::transfer`. The seller receives tokens at their own address, not into escrow.
4. `withdraw` sends unused escrow back to the caller.

Invariant per token: the contract's balance is at least the escrowed total, and `escrowed + unclaimed == contract balance`. Aborts: `not the owner`, `not the seller`, `nft is listed`, `already listed`, `price must be positive`, `unknown token`, `no such nft`, `no such listing`, `cannot buy your own listing`, `insufficient escrow`, `not enough unclaimed tokens`, `zero amount`, `overflow`. A failed call changes nothing (the tests compare the whole store before and after).

```
./build.sh                          # writes nft.elf (git-ignored)
cargo test -p btb-tests --test nft
cargo build -p btb-cli && (cd examples/nft && ../../target/debug/btb dev)   # open the printed URL in two tabs
node examples/nft/web/smoke.js      # Playwright check, see web/smoke.js
```

Same build setup as [counter](../counter/README.md). The page embeds `@btb/client`; see [../README-web.md](../README-web.md) for how it is refreshed.
