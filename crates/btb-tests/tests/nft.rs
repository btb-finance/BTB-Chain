//! End to end: build `examples/nft` (NFTs, escrow ledger, marketplace) for riscv64 and run it in
//! `btb-vm` over `RuntimeHost`/`MemKv`. Every call is its own transaction; a failed call must
//! leave the store byte-for-byte unchanged.

use btb_codec::{Address, Decode, Encode};
use btb_runtime::{layout, IndexDef, IndexValue, KvStore, MemKv, RuntimeHost, Tables};
use btb_sdk::token::Token;
use btb_vm::{CallError, Contract, FunctionKind};
use std::collections::BTreeMap;
use std::sync::OnceLock;

mod common;
use common::{abort_msg, GAS, MEM};

const CONTRACT: [u8; 32] = [8; 32];

fn addr(n: u8) -> Address {
    Address([0x40 + n; 32])
}

fn elf() -> &'static [u8] {
    static ELF: OnceLock<Vec<u8>> = OnceLock::new();
    ELF.get_or_init(|| common::build_example("nft"))
}

type Row = (u64, u64, Address, Token, u128, String);
type Page = (Vec<Row>, Option<u64>);

struct Chain {
    c: Contract,
    kv: MemKv,
    tables: Tables,
}

impl Chain {
    fn new() -> Self {
        let mut kv = MemKv::new();
        kv.put(&layout::balance_key(&CONTRACT), &1_000_000u64.to_le_bytes())
            .unwrap();
        let mut tables = Tables::new();
        tables
            .register(
                CONTRACT,
                b"Nft",
                vec![IndexDef::new("owner", |row| {
                    Ok(Some(IndexValue::Bytes(row[..32].to_vec())))
                })],
            )
            .unwrap();
        Chain {
            c: Contract::from_elf(elf()).expect("valid contract"),
            kv,
            tables,
        }
    }

    fn run(
        &mut self,
        kind: FunctionKind,
        name: &str,
        who: Address,
        input: &impl Encode,
    ) -> Result<Vec<u8>, CallError> {
        let mut h = RuntimeHost::new(&self.kv, &self.tables, CONTRACT, who.0, 1);
        let out = self.c.call(kind, name, &input.encode(), &mut h, GAS, MEM)?;
        let batch = h.finish().expect("no faults");
        self.kv.commit(batch).unwrap();
        Ok(out.output)
    }

    fn snapshot(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.kv
            .entries()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Run a mutation that must abort with `msg` and change nothing at all.
    fn fails(&mut self, name: &str, who: Address, input: &impl Encode, msg: &str) {
        let before = self.snapshot();
        let e = self
            .run(FunctionKind::Mutation, name, who, input)
            .expect_err(name);
        assert_eq!(abort_msg(e), msg, "{name}");
        assert_eq!(self.snapshot(), before, "{name} left a trace");
    }

    fn mutate<T: Decode>(&mut self, name: &str, who: Address, input: &impl Encode) -> T {
        let out = self
            .run(FunctionKind::Mutation, name, who, input)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        T::decode(&out).unwrap()
    }

    fn query<T: Decode>(&mut self, name: &str, who: Address, input: &impl Encode) -> T {
        let out = self
            .run(FunctionKind::Query, name, who, input)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        T::decode(&out).unwrap()
    }

    fn mint(&mut self, who: Address, uri: &str) -> u64 {
        self.mutate("mint", who, &(uri.to_string(),))
    }

    fn currency(&mut self, supply: u128) -> Token {
        let input = ("Gold".to_string(), "GLD".to_string(), 0u8, supply);
        self.mutate("create_currency", addr(0), &input)
    }

    fn fund(&mut self, who: Address, t: Token, amount: u128) {
        self.mutate::<()>("deposit", who, &(t, amount));
    }

    fn my_nfts(&mut self, who: Address) -> Vec<(u64, String)> {
        self.query("my_nfts", who, &())
    }

    fn owner_of(&mut self, id: u64) -> Address {
        self.query::<(Address, String)>("nft", addr(0), &(id,)).0
    }

    fn escrow(&mut self, t: Token, who: Address) -> u128 {
        self.query("escrow_of", addr(0), &(t, who))
    }

    fn tokens(&mut self, t: Token, who: Address) -> u128 {
        self.query("balance_of", addr(0), &(t, who))
    }

    fn listings(&mut self, cursor: u64, limit: u32) -> Page {
        self.query("listings", addr(0), &(cursor, limit))
    }

    fn contract_tokens(&mut self, t: Token) -> u128 {
        self.tokens(t, Address(CONTRACT))
    }
}

const ALICE: u8 = 1;
const BOB: u8 = 2;
const CAROL: u8 = 3;

#[test]
fn entry_points_are_exported() {
    let c = Contract::from_elf(elf()).unwrap();
    let names: Vec<_> = c
        .entry_points()
        .iter()
        .map(|e| (e.kind, e.name.as_str()))
        .collect();
    for want in [
        (FunctionKind::Mutation, "mint"),
        (FunctionKind::Mutation, "transfer"),
        (FunctionKind::Mutation, "list"),
        (FunctionKind::Mutation, "cancel"),
        (FunctionKind::Mutation, "buy"),
        (FunctionKind::Mutation, "deposit"),
        (FunctionKind::Mutation, "withdraw"),
        (FunctionKind::Query, "my_nfts"),
        (FunctionKind::Query, "listings"),
    ] {
        assert!(names.contains(&want), "missing {want:?} in {names:?}");
    }
    common::assert_only_rv64im(elf());
}

#[test]
fn mint_transfer_and_my_nfts() {
    let mut ch = Chain::new();
    let a = ch.mint(addr(ALICE), "ipfs://a");
    let b = ch.mint(addr(ALICE), "ipfs://b");
    let c = ch.mint(addr(BOB), "ipfs://c");
    assert_eq!((a, b, c), (0, 1, 2));
    assert_eq!(
        ch.my_nfts(addr(ALICE)),
        vec![(0, "ipfs://a".into()), (1, "ipfs://b".into())]
    );
    assert_eq!(ch.my_nfts(addr(BOB)), vec![(2, "ipfs://c".into())]);
    assert!(ch.my_nfts(addr(CAROL)).is_empty());

    ch.mutate::<()>("transfer", addr(ALICE), &(a, addr(CAROL)));
    assert_eq!(ch.owner_of(a), addr(CAROL));
    assert_eq!(ch.my_nfts(addr(ALICE)), vec![(1, "ipfs://b".into())]);
    assert_eq!(ch.my_nfts(addr(CAROL)), vec![(0, "ipfs://a".into())]);

    // Failures change nothing.
    ch.fails("transfer", addr(ALICE), &(a, addr(BOB)), "not the owner");
    ch.fails("transfer", addr(BOB), &(b, addr(BOB)), "not the owner");
    ch.fails("transfer", addr(ALICE), &(99u64, addr(BOB)), "no such nft");
    ch.fails(
        "mint",
        addr(ALICE),
        &(String::new(),),
        "uri must be 1..=256 bytes",
    );
    ch.fails(
        "mint",
        addr(ALICE),
        &("x".repeat(257),),
        "uri must be 1..=256 bytes",
    );
}

#[test]
fn list_buy_pays_the_seller_through_escrow() {
    let mut ch = Chain::new();
    let gold = ch.currency(10_000);
    let nft = ch.mint(addr(ALICE), "ipfs://art");
    ch.fund(addr(BOB), gold, 1_500);
    assert_eq!(ch.escrow(gold, addr(BOB)), 1_500);
    assert_eq!(ch.query::<u128>("escrowed", addr(0), &(gold,)), 1_500);
    assert_eq!(ch.query::<u128>("unclaimed", addr(0), &(gold,)), 8_500);

    let lid: u64 = ch.mutate("list", addr(ALICE), &(nft, gold, 1_000u128));
    let (rows, next) = ch.listings(0, 10);
    assert_eq!(next, None);
    assert_eq!(
        rows,
        vec![(lid, nft, addr(ALICE), gold, 1_000, "ipfs://art".into())]
    );

    ch.mutate::<()>("buy", addr(BOB), &(lid,));
    assert_eq!(ch.owner_of(nft), addr(BOB));
    assert_eq!(ch.my_nfts(addr(BOB)), vec![(nft, "ipfs://art".into())]);
    assert!(ch.my_nfts(addr(ALICE)).is_empty());
    assert_eq!(ch.escrow(gold, addr(BOB)), 500);
    // The seller really holds the tokens: a Token::transfer from the contract.
    assert_eq!(ch.tokens(gold, addr(ALICE)), 1_000);
    assert!(ch.listings(0, 10).0.is_empty());

    // Conservation: contract balance == escrowed + unclaimed; supply is unchanged.
    let held = ch.contract_tokens(gold);
    assert_eq!(held, 9_000);
    assert_eq!(
        held,
        ch.query::<u128>("escrowed", addr(0), &(gold,))
            + ch.query::<u128>("unclaimed", addr(0), &(gold,))
    );
    assert_eq!(held + ch.tokens(gold, addr(ALICE)), 10_000);

    // Leftover escrow can be withdrawn, but not more than there is.
    ch.fails(
        "withdraw",
        addr(BOB),
        &(gold, 501u128),
        "insufficient escrow",
    );
    ch.mutate::<()>("withdraw", addr(BOB), &(gold, 500u128));
    assert_eq!(ch.escrow(gold, addr(BOB)), 0);
    assert_eq!(ch.tokens(gold, addr(BOB)), 500);
    // The new owner can resell.
    let lid2: u64 = ch.mutate("list", addr(BOB), &(nft, gold, 7u128));
    assert_ne!(lid, lid2);
}

#[test]
fn market_failure_cases_leave_no_trace() {
    let mut ch = Chain::new();
    let gold = ch.currency(10_000);
    let nft = ch.mint(addr(ALICE), "ipfs://art");
    ch.fund(addr(BOB), gold, 400);
    ch.fund(addr(ALICE), gold, 5_000);

    // Non-owner list, zero price, unknown token.
    ch.fails("list", addr(BOB), &(nft, gold, 100u128), "not the owner");
    ch.fails(
        "list",
        addr(ALICE),
        &(nft, gold, 0u128),
        "price must be positive",
    );
    ch.fails(
        "list",
        addr(ALICE),
        &(nft, Token([9; 32]), 5u128),
        "unknown token",
    );
    ch.fails("list", addr(ALICE), &(77u64, gold, 5u128), "no such nft");

    let lid: u64 = ch.mutate("list", addr(ALICE), &(nft, gold, 1_000u128));
    ch.fails("list", addr(ALICE), &(nft, gold, 1u128), "already listed");
    ch.fails("transfer", addr(ALICE), &(nft, addr(BOB)), "nft is listed");

    // Buy own listing (even with ample escrow), insufficient escrow, unknown listing.
    ch.fails("buy", addr(ALICE), &(lid,), "cannot buy your own listing");
    ch.fails("buy", addr(BOB), &(lid,), "insufficient escrow");
    ch.fails("buy", addr(CAROL), &(lid,), "insufficient escrow");
    ch.fails("buy", addr(BOB), &(lid + 50,), "no such listing");

    // Cancel by a non-seller (the NFT owner is not enough; neither is the buyer).
    ch.fails("cancel", addr(BOB), &(lid,), "not the seller");
    ch.fails("cancel", addr(BOB), &(lid + 50,), "no such listing");

    // Deposits are bounded by the unclaimed surplus.
    ch.fails("deposit", addr(BOB), &(gold, 0u128), "zero amount");
    ch.fails(
        "deposit",
        addr(BOB),
        &(gold, 4_601u128),
        "not enough unclaimed tokens",
    );
    ch.fails(
        "withdraw",
        addr(CAROL),
        &(gold, 1u128),
        "insufficient escrow",
    );

    // Double buy: the second attempt finds no listing and nothing moves.
    ch.fund(addr(BOB), gold, 600);
    ch.fund(addr(CAROL), gold, 1_000);
    ch.mutate::<()>("buy", addr(BOB), &(lid,));
    ch.fails("buy", addr(CAROL), &(lid,), "no such listing");
    assert_eq!(ch.owner_of(nft), addr(BOB));
    assert_eq!(ch.escrow(gold, addr(CAROL)), 1_000);
    assert_eq!(ch.tokens(gold, addr(ALICE)), 1_000);
    ch.fails("cancel", addr(ALICE), &(lid,), "no such listing");
}

#[test]
fn cancel_then_relist_and_buy_cancelled_fails() {
    let mut ch = Chain::new();
    let gold = ch.currency(1_000);
    let nft = ch.mint(addr(ALICE), "u");
    ch.fund(addr(BOB), gold, 1_000);
    let lid: u64 = ch.mutate("list", addr(ALICE), &(nft, gold, 10u128));
    ch.mutate::<()>("cancel", addr(ALICE), &(lid,));
    assert!(ch.listings(0, 10).0.is_empty());
    ch.fails("buy", addr(BOB), &(lid,), "no such listing");
    // Transfers work again, and the NFT can be listed afresh.
    let lid2: u64 = ch.mutate("list", addr(ALICE), &(nft, gold, 20u128));
    assert_ne!(lid, lid2);
    ch.mutate::<()>("buy", addr(BOB), &(lid2,));
    assert_eq!(ch.tokens(gold, addr(ALICE)), 20);
    assert_eq!(ch.escrow(gold, addr(BOB)), 980);
}

#[test]
fn mint_and_list_is_atomic() {
    let mut ch = Chain::new();
    let gold = ch.currency(100);
    // The listing half is rejected (unknown token), so the mint half must not persist.
    ch.fails(
        "mint_and_list",
        addr(ALICE),
        &("ipfs://x".to_string(), Token([9; 32]), 5u128),
        "unknown token",
    );
    ch.fails(
        "mint_and_list",
        addr(ALICE),
        &("ipfs://x".to_string(), gold, 0u128),
        "price must be positive",
    );
    assert!(ch.my_nfts(addr(ALICE)).is_empty());
    let (id, lid): (u64, u64) = ch.mutate(
        "mint_and_list",
        addr(ALICE),
        &("ipfs://x".to_string(), gold, 5u128),
    );
    assert_eq!(id, 0, "the failed attempts consumed no ids");
    assert_eq!(ch.listings(0, 10).0[0].0, lid);
}

#[test]
fn listings_are_paged() {
    let mut ch = Chain::new();
    let gold = ch.currency(100);
    let mut lids = Vec::new();
    for i in 0..7u128 {
        let (_, lid): (u64, u64) = ch.mutate(
            "mint_and_list",
            addr(ALICE),
            &(format!("ipfs://{i}"), gold, i + 1),
        );
        lids.push(lid);
    }
    ch.mutate::<()>("cancel", addr(ALICE), &(lids[3],));

    let mut seen = Vec::new();
    let (mut cursor, mut pages) = (0u64, 0);
    loop {
        let (rows, next) = ch.listings(cursor, 3);
        assert!(rows.len() <= 3);
        seen.extend(rows.iter().map(|r| r.0));
        pages += 1;
        match next {
            Some(n) => cursor = n,
            None => break,
        }
    }
    let mut want = lids.clone();
    want.remove(3);
    assert_eq!(seen, want);
    assert_eq!(pages, 2);
    // limit 0 is treated as 1.
    assert_eq!(ch.listings(0, 0).0.len(), 1);
}
