# Writing BTB Contracts: Rust First, Any Language Welcome

BTB contracts run on a metered **RISC-V VM**. Anything that compiles to RISC-V can run on BTB, the same "one VM, many languages" approach the Internet Computer uses with WebAssembly. **Rust is the first-class language**, with an SDK in the style of NEAR's `near-sdk`, and the developer experience is modeled on Convex, so contracts read like a backend that humans and AI agents can understand at a glance.

> Draft. APIs below are illustrative and will change during Phase 1.

## Languages

| Language | Status | How |
| --- | --- | --- |
| **Rust** | First-class (Phase 1) | `btb-sdk` crate with `#[btb::contract]`, `#[table]`, `#[query]`, `#[mutation]`, `#[cron]` macros |
| C / C++ | Supported via LLVM (Phase 3) | Header file with the host-call interface |
| Zig | Supported via LLVM (Phase 3) | Thin SDK over the host-call interface |
| TypeScript | Planned | Compiled or run in a RISC-V-hosted interpreter; the main draw for Convex developers |
| Others (Go via TinyGo, Nim, …) | Community | Any toolchain that emits RISC-V and follows the host-call ABI |
| BTB DSL | Later, optional | A small Rust-like language where tables and queries are syntax; compiles to the same target |

Every language talks to the chain through the same **host-call interface** (storage, tables, tokens, context, cross-chain), so features land for all languages at once.

## Design rules (all languages)

- **Deterministic.** No floats, no system time, no randomness outside a chain-provided source. The VM only implements the integer RISC-V instruction set.
- **Metered.** The VM charges gas per instruction and per host call; infinite loops cannot stall the chain.
- **Money-safe.** Tokens move only through host calls that check balances; the SDK models them as values that cannot be copied or dropped, and reentrancy is prevented by the runtime.
- **Database-native.** Tables, indexes, queries and mutations are part of the SDK and the host interface, not something each contract reinvents.

## The Convex-style project layout

A BTB app looks like a Convex app: backend functions live in one folder, next to the frontend.

```
my-dex/
├── btb/                 # backend: contracts compiled to RISC-V
│   ├── schema.rs        # tables and indexes
│   ├── pools.rs         # queries and mutations
│   └── crons.rs         # scheduled functions
├── web/                 # frontend, hosted on BTB
│   └── src/App.tsx
└── btb.toml             # project config
```

Running `btb dev` compiles the `btb/` folder, starts a local chain, hot-reloads on save and generates typed TypeScript bindings for the frontend.

## Example: a DEX in Rust

```rust
use btb_sdk::prelude::*;

#[btb::table]
pub struct Pool {
    #[primary] pub id: u64,
    #[index]   pub token_a: TokenId,
    #[index]   pub token_b: TokenId,
    pub reserve_a: u128,
    pub reserve_b: u128,
    #[index]   pub creator: Address,
}

#[btb::query]
pub fn price(pool: u64) -> Result<Ratio> {
    let p = Pool::get(pool)?;
    Ok(Ratio::new(p.reserve_b, p.reserve_a))
}

#[btb::query]
pub fn pools_by_creator(creator: Address) -> Vec<Pool> {
    Pool::by_creator(creator).collect()
}

#[btb::mutation]
pub fn swap(pool: u64, amount_in: u128, min_out: u128) -> Result<u128> {
    let mut p = Pool::get(pool)?;
    let out = amm_out(amount_in, p.reserve_a, p.reserve_b);
    ensure!(out >= min_out, "slippage too high");
    token::transfer_in(p.token_a, caller(), amount_in)?;
    token::transfer_out(p.token_b, caller(), out)?;
    p.reserve_a += amount_in;
    p.reserve_b -= out;
    p.save()?;
    Ok(out)
}

#[btb::cron(every = "1h")]
pub fn record_price_history() -> Result<()> {
    // snapshot prices for charts
    Ok(())
}
```

And the frontend, exactly like Convex:

```tsx
const price = useQuery(api.pools.price, { pool: 7 }); // live, no polling
const swap  = useMutation(api.pools.swap);
```

## Example: native token

```rust
let moon = token::create(TokenSpec {
    name: "Moon",
    symbol: "MOON",
    supply: 1_000_000,   // fixed at creation
    decimals: 18,
})?;
```

## Built for AI agents too

Because every app follows the same layout and the same three function kinds (query, mutation, cron), an AI coding agent can read a BTB project and know exactly where data is defined, what can change state and what runs on a schedule. The repository includes an [AGENTS.md](../AGENTS.md) describing these conventions for any agent working on the codebase.

## How it compiles

```
Rust / C / Zig / … source
  → standard compiler (rustc, clang, zig) targeting RISC-V rv64im
  → ELF binary with BTB entry points (one per query / mutation / cron)
  → btb deploy: binary + schema uploaded to the chain
  → BTB VM: verified, metered, recompiled to x86 / ARM on validators
```

No custom compiler is needed for Rust, C or Zig: their existing RISC-V backends do the work. The optional BTB DSL would add its own frontend on top of the same pipeline later.

## Why RISC-V

- About 40 base instructions: a small VM that is easier to secure.
- Mainstream compilers already target it, which is what makes "any language" practical.
- Translates efficiently to x86 and ARM, so contracts run close to native speed.
- ZK-ready: provers such as RISC Zero and SP1 prove RISC-V programs, enabling verifiable light clients later.
- Proven in production by Nervos CKB and Polkadot's PolkaVM, both open source in Rust.

See [PRIOR_ART.md](PRIOR_ART.md) for how other chains approach languages and VMs.
