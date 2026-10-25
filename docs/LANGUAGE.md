# The BTB Contract Language

BTB contracts are written in BTB's own Rust-like language and compiled to RISC-V. The language exists for **control and safety**: determinism, gas metering and money-safety rules are built in, and database concepts are first-class syntax. Speed comes from compiling to near-native RISC-V code.

> Draft. Syntax below is illustrative and will change during Phase 1. The language still needs a name.

## Design goals

- **Deterministic.** Every validator gets bit-identical results: integer-only math, no floats, no system time, no randomness outside a chain-provided source.
- **Metered.** The compiler inserts gas accounting; infinite loops cannot stall the chain.
- **Money-safe.** Tokens are a built-in type that cannot be copied or silently dropped; reentrancy is impossible by design.
- **Database-native.** Tables, indexes, queries and mutations are language constructs.
- **Familiar.** Rust-like syntax, `Result`/`Option` instead of null, pattern matching.

## Building blocks

| Construct | Purpose |
| --- | --- |
| `contract Name { ... }` | A deployable unit: tables, functions, schedules |
| `table T { field: Type @primary, other: Type @index }` | Persistent, indexed storage |
| `query fn` | Read-only; served by hosting nodes without consensus; can be subscribed to |
| `mutation fn` | Changes state; goes through consensus; pays gas |
| `@cron(every = 1 hour)` | Scheduled mutation, run by the block executor |
| `Token`, `transfer(...)` | Native token creation and safe transfers |
| `require(cond, "msg")?` | Abort with an error; all changes in the mutation are rolled back |

## Example: NFT market

```rust
contract NftMarket {
    table Nft {
        id: u64 @primary,
        owner: Address @index,
        creator: Address @index,
        price: Option<u128>,
        listed_at: Option<u64>,
    }

    query fn owned_by(user: Address) -> List<Nft> {
        Nft.where(owner == user)
    }

    mutation fn list(id: u64, price: u128) {
        let nft = Nft.get(id)?;
        require(nft.owner == caller(), "not the owner")?;
        Nft.update(id, { price: Some(price), listed_at: Some(block_time()) });
    }

    mutation fn buy(id: u64) {
        let nft = Nft.get(id)?;
        let price = nft.price.ok_or("not for sale")?;
        transfer(caller(), nft.owner, price)?;
        Nft.update(id, { owner: caller(), price: None, listed_at: None });
    }

    @cron(every = 1 day)
    mutation fn expire_old_listings() {
        Nft.where(price != None && listed_at < block_time() - 30 days)
           .update({ price: None, listed_at: None });
    }
}
```

## Example: native token

```rust
let token = Token.create({
    name: "Moon",
    symbol: "MOON",
    supply: 1_000_000,   // fixed at creation
    decimals: 18,
})?;
```

## Compilation pipeline

```
source (.btb)
  → lexer / parser          (ours)
  → type checker + BTB rules (ours)
  → IR: LLVM or Cranelift    (borrowed in Phase 1)
  → RISC-V rv64im machine code
  → BTB VM: metered, recompiled to x86 / ARM on validators
```

1. **Frontend (ours):** lexer, parser, type checker, and BTB-specific rules (tables, `@cron`, `query`/`mutation`, safe transfers).
2. **Backend (borrowed at first):** lower to LLVM or Cranelift IR, which already emit RISC-V.
3. **Later:** a custom backend optimized for gas costs.

## Why RISC-V

- About 40 base instructions: a small VM that is easier to secure.
- Translates efficiently to x86 and ARM, so contracts run close to native speed.
- ZK-ready: provers such as RISC Zero and SP1 prove RISC-V programs, enabling verifiable light clients later.
- Proven in production by Nervos CKB and Polkadot's PolkaVM, both open source in Rust.

Only the integer instruction set is used: floating point can produce slightly different results on different hardware, and validators must agree exactly.
