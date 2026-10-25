//! End to end: build `examples/launcher` (native user tokens + faucet) for riscv64 and run it
//! in `btb-vm` over `RuntimeHost`/`MemKv` (transactional) and `MemHost` (parity).

use btb_codec::{Address, Decode, Encode};
use btb_runtime::{layout, KvStore, MemKv, RuntimeHost, Tables};
use btb_sdk::token::Token;
use btb_vm::decode::Trap;
use btb_vm::testing::MemHost;
use btb_vm::token::derive_id;
use btb_vm::{CallError, Contract, FunctionKind};
use std::collections::BTreeMap;
use std::sync::OnceLock;

mod common;
use common::{abort_msg, GAS, MEM};

const CONTRACT: [u8; 32] = [8; 32];
const LAUNCHER: Address = Address([1; 32]);

fn addr(n: u8) -> Address {
    Address([0x40 + n; 32])
}

fn elf() -> &'static [u8] {
    static ELF: OnceLock<Vec<u8>> = OnceLock::new();
    ELF.get_or_init(|| common::build_example("launcher"))
}

type Holders = (Vec<(Address, u128)>, Option<Address>);

/// A chain with the launcher deployed at `CONTRACT`; every call is its own transaction.
struct Chain {
    c: Contract,
    kv: MemKv,
}

impl Chain {
    fn new() -> Self {
        let mut kv = MemKv::new();
        // The contract pays a storage deposit per byte, so it needs native BTB.
        kv.put(&layout::balance_key(&CONTRACT), &1_000_000u64.to_le_bytes())
            .unwrap();
        Chain {
            c: Contract::from_elf(elf()).expect("valid contract"),
            kv,
        }
    }

    fn run(
        &mut self,
        kind: FunctionKind,
        name: &str,
        who: Address,
        input: &impl Encode,
    ) -> Result<(Vec<u8>, u64), CallError> {
        let tables = Tables::new();
        let mut h = RuntimeHost::new(&self.kv, &tables, CONTRACT, who.0, 1);
        let out = self.c.call(kind, name, &input.encode(), &mut h, GAS, MEM)?;
        let batch = h.finish().expect("no faults");
        self.kv.commit(batch).unwrap();
        Ok((out.output, out.gas_used))
    }

    fn launch(&mut self, supply: u128, faucet: u128, mintable: bool) -> Token {
        let input = (
            "Gold".to_string(),
            "GLD".to_string(),
            6u8,
            supply,
            faucet,
            mintable,
        );
        let (out, _) = self
            .run(FunctionKind::Mutation, "launch", LAUNCHER, &input)
            .unwrap();
        Token::decode(&out).unwrap()
    }

    fn claim(&mut self, t: Token, who: Address) -> Result<u128, CallError> {
        let (out, _) = self.run(FunctionKind::Mutation, "claim", who, &(t,))?;
        Ok(u128::decode(&out).unwrap())
    }

    fn balance(&mut self, t: Token, who: Address) -> u128 {
        let (out, _) = self
            .run(FunctionKind::Query, "balance_of", LAUNCHER, &(t, who))
            .unwrap();
        u128::decode(&out).unwrap()
    }

    fn supply(&mut self, t: Token) -> u128 {
        let (out, _) = self
            .run(FunctionKind::Query, "info", LAUNCHER, &(t,))
            .unwrap();
        <(String, String, u8, u128)>::decode(&out).unwrap().3
    }

    fn holders_page(&mut self, t: Token, cursor: Option<Address>, limit: u32) -> Holders {
        let (out, _) = self
            .run(
                FunctionKind::Query,
                "holders",
                LAUNCHER,
                &(t, cursor, limit),
            )
            .unwrap();
        Holders::decode(&out).unwrap()
    }

    fn all_holders(&mut self, t: Token, limit: u32) -> Vec<(Address, u128)> {
        let (mut all, mut cursor) = (Vec::new(), None);
        loop {
            let (page, next) = self.holders_page(t, cursor, limit);
            all.extend(page);
            match next {
                Some(n) => cursor = Some(n),
                None => return all,
            }
        }
    }

    /// Total native BTB: every balance plus every storage deposit.
    fn native_total(&self) -> u128 {
        self.kv
            .entries()
            .filter(|(k, _)| k[0] == layout::TAG_BALANCE || k[0] == layout::TAG_DEPOSIT)
            .map(|(_, v)| u64::from_le_bytes(v[..8].try_into().unwrap()) as u128)
            .sum()
    }
}

#[test]
fn create_derives_deterministic_id_and_info() {
    let mut ch = Chain::new();
    let t0 = ch.launch(1_000, 10, false);
    assert_eq!(t0.0, derive_id(&CONTRACT, 0));
    let t1 = ch.launch(5, 1, true);
    assert_eq!(t1.0, derive_id(&CONTRACT, 1));
    assert_ne!(t0, t1);

    let (out, _) = ch
        .run(FunctionKind::Query, "info", LAUNCHER, &(t0,))
        .unwrap();
    assert_eq!(
        <(String, String, u8, u128)>::decode(&out).unwrap(),
        ("Gold".into(), "GLD".into(), 6, 1_000)
    );
    // The whole initial supply sits with the creating contract.
    assert_eq!(ch.balance(t0, Address(CONTRACT)), 1_000);
    assert_eq!(ch.supply(t1), 5);

    // Invalid parameters abort.
    let bad = ("".to_string(), "GLD".to_string(), 6u8, 1u128, 1u128, false);
    let e = ch
        .run(FunctionKind::Mutation, "launch", LAUNCHER, &bad)
        .unwrap_err();
    assert_eq!(abort_msg(e), "invalid token parameters");
}

#[test]
fn faucet_transfers_and_second_claim_aborts_atomically() {
    let mut ch = Chain::new();
    let t = ch.launch(1_000, 10, false);
    assert_eq!(ch.claim(t, addr(1)).unwrap(), 10);
    assert_eq!(ch.balance(t, addr(1)), 10);
    assert_eq!(ch.balance(t, Address(CONTRACT)), 990);
    assert_eq!(ch.supply(t), 1_000);

    let before = ch.kv.clone();
    let e = ch.claim(t, addr(1)).unwrap_err();
    assert_eq!(abort_msg(e), "already claimed");
    assert_eq!(ch.kv, before);
}

#[test]
fn insufficient_balance_aborts_and_discards_everything() {
    let mut ch = Chain::new();
    let t = ch.launch(100, 60, false);
    ch.claim(t, addr(1)).unwrap();
    let before = ch.kv.clone();
    // 40 left, the faucet wants 60: the claim marker written before the failing transfer must
    // not survive.
    let e = ch.claim(t, addr(2)).unwrap_err();
    assert_eq!(abort_msg(e), "insufficient token balance");
    assert_eq!(ch.kv, before);
    assert_eq!(ch.balance(t, addr(2)), 0);
    assert_eq!(ch.balance(t, Address(CONTRACT)), 40);
}

#[test]
fn holders_page_by_address_and_drop_at_zero() {
    let mut ch = Chain::new();
    let t = ch.launch(1_000, 1, false);
    // Claim in a scrambled order so insertion order differs from address order.
    for n in [5u8, 2, 7, 1, 6, 3, 4] {
        ch.claim(t, addr(n)).unwrap();
    }
    // 7 claimers + the contract (still holds 993) = 8 holders.
    let (p1, c1) = ch.holders_page(t, None, 3);
    assert_eq!(p1.len(), 3);
    assert_eq!(c1, Some(p1[2].0));
    let (p2, c2) = ch.holders_page(t, c1, 3);
    let (p3, c3) = ch.holders_page(t, c2, 3);
    assert_eq!((p2.len(), p3.len(), c3), (3, 2, None));

    let paged: Vec<_> = [p1, p2, p3].concat();
    let full = ch.all_holders(t, 256);
    assert_eq!(paged, full);
    assert_eq!(full.len(), 8);
    assert!(
        full.windows(2).all(|w| w[0].0 < w[1].0),
        "ascending by address"
    );
    assert_eq!(full.iter().map(|h| h.1).sum::<u128>(), 1_000);

    // Boundary: a page exactly the size of the set has no cursor; one smaller does.
    assert_eq!(ch.holders_page(t, None, 8).1, None);
    assert!(ch.holders_page(t, None, 7).1.is_some());

    // A holder whose balance reaches zero disappears: drain the contract of a small token.
    let t2 = ch.launch(2, 2, false);
    ch.claim(t2, addr(1)).unwrap();
    let hs = ch.all_holders(t2, 10);
    assert_eq!(hs, vec![(addr(1), 2)]);
    // Holders of one token never leak into another.
    assert!(ch.all_holders(t, 256).iter().all(|h| h.0 != addr(9)));
}

#[test]
fn unauthorized_and_authorized_mint_and_burn() {
    let mut ch = Chain::new();
    let fixed = ch.launch(100, 1, false);
    let before = ch.kv.clone();
    // A token without a mint authority can never change supply.
    let e = ch
        .run(
            FunctionKind::Mutation,
            "mint_more",
            LAUNCHER,
            &(fixed, addr(1), 5u128),
        )
        .unwrap_err();
    assert_eq!(abort_msg(e), "not the mint authority");
    let e = ch
        .run(
            FunctionKind::Mutation,
            "burn_faucet",
            LAUNCHER,
            &(fixed, 5u128),
        )
        .unwrap_err();
    assert_eq!(abort_msg(e), "not the mint authority");
    assert_eq!(ch.kv, before);

    let m = ch.launch(100, 1, true);
    // The contract is the authority, but the contract only lets the launcher drive it.
    let e = ch
        .run(
            FunctionKind::Mutation,
            "mint_more",
            addr(1),
            &(m, addr(1), 5u128),
        )
        .unwrap_err();
    assert_eq!(abort_msg(e), "only the launcher");

    ch.run(
        FunctionKind::Mutation,
        "mint_more",
        LAUNCHER,
        &(m, addr(1), 25u128),
    )
    .unwrap();
    assert_eq!((ch.supply(m), ch.balance(m, addr(1))), (125, 25));
    ch.run(
        FunctionKind::Mutation,
        "burn_faucet",
        LAUNCHER,
        &(m, 40u128),
    )
    .unwrap();
    assert_eq!((ch.supply(m), ch.balance(m, Address(CONTRACT))), (85, 60));

    // Checked math: overflow and over-burn abort and change nothing.
    let before = ch.kv.clone();
    let e = ch
        .run(
            FunctionKind::Mutation,
            "mint_more",
            LAUNCHER,
            &(m, addr(2), u128::MAX),
        )
        .unwrap_err();
    assert_eq!(abort_msg(e), "token overflow");
    let e = ch
        .run(
            FunctionKind::Mutation,
            "burn_faucet",
            LAUNCHER,
            &(m, 61u128),
        )
        .unwrap_err();
    assert_eq!(abort_msg(e), "insufficient token balance");
    assert_eq!(ch.kv, before);
}

#[test]
fn queries_cannot_transfer() {
    let mut ch = Chain::new();
    let t = ch.launch(100, 10, false);
    let before = ch.kv.clone();
    let e = ch
        .run(FunctionKind::Query, "claim_in_query", addr(1), &(t,))
        .unwrap_err();
    assert!(matches!(
        e,
        CallError::Trap {
            trap: Trap::WriteInQuery,
            ..
        }
    ));
    assert_eq!(ch.kv, before);
    assert_eq!(ch.balance(t, addr(1)), 0);
}

#[test]
fn native_btb_is_untouched_by_token_operations() {
    let mut ch = Chain::new();
    // Give users some native BTB so a leak would be visible.
    for n in 1..=3 {
        ch.kv
            .put(&layout::balance_key(&addr(n).0), &500u64.to_le_bytes())
            .unwrap();
    }
    let native = |ch: &Chain, a: Address| ch.kv.get(&layout::balance_key(&a.0)).unwrap();
    let total = ch.native_total();
    let users: Vec<_> = (1..=3).map(|n| native(&ch, addr(n))).collect();

    let t = ch.launch(1_000_000, 100, true);
    for n in 1..=3 {
        ch.claim(t, addr(n)).unwrap();
    }
    ch.run(
        FunctionKind::Mutation,
        "mint_more",
        LAUNCHER,
        &(t, addr(1), 77u128),
    )
    .unwrap();
    ch.run(FunctionKind::Mutation, "burn_faucet", LAUNCHER, &(t, 5u128))
        .unwrap();

    assert_eq!(
        users,
        (1..=3).map(|n| native(&ch, addr(n))).collect::<Vec<_>>()
    );
    assert_eq!(native(&ch, LAUNCHER), None);
    // The only native movement is the contract paying storage deposits to itself: balances plus
    // deposits are conserved, so nothing was minted.
    assert_eq!(ch.native_total(), total);
    // Token state lives under its own tags, not in native balances.
    assert!(ch.kv.entries().any(|(k, _)| k[0] == layout::TAG_TOKEN));
    assert!(ch
        .kv
        .entries()
        .any(|(k, _)| k[0] == layout::TAG_TOKEN_BALANCE));
}

/// Tiny deterministic generator (no external crates).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn supply_equals_sum_of_balances_across_random_ops() {
    for seed in [1u64, 0xDEAD_BEEF, 42] {
        let mut rng = Rng(seed);
        let mut ch = Chain::new();
        let t = ch.launch(500, 7, true);
        let mut model: BTreeMap<Address, u128> = BTreeMap::new();
        model.insert(Address(CONTRACT), 500);
        let mut model_supply = 500u128;
        let mut claimed = Vec::new();
        for _ in 0..60 {
            let who = addr((rng.next() % 12) as u8);
            match rng.next() % 4 {
                0 | 1 => {
                    let r = ch.claim(t, who);
                    let have = model[&Address(CONTRACT)];
                    if claimed.contains(&who) || have < 7 {
                        assert!(r.is_err());
                    } else {
                        r.unwrap();
                        claimed.push(who);
                        *model.get_mut(&Address(CONTRACT)).unwrap() -= 7;
                        *model.entry(who).or_default() += 7;
                    }
                }
                2 => {
                    let amt = (rng.next() % 40) as u128;
                    ch.run(
                        FunctionKind::Mutation,
                        "mint_more",
                        LAUNCHER,
                        &(t, who, amt),
                    )
                    .unwrap();
                    *model.entry(who).or_default() += amt;
                    model_supply += amt;
                }
                _ => {
                    let amt = (rng.next() % 40) as u128;
                    let r = ch.run(FunctionKind::Mutation, "burn_faucet", LAUNCHER, &(t, amt));
                    let have = model[&Address(CONTRACT)];
                    if amt > have {
                        assert!(r.is_err());
                    } else {
                        r.unwrap();
                        *model.get_mut(&Address(CONTRACT)).unwrap() -= amt;
                        model_supply -= amt;
                    }
                }
            }
            model.retain(|_, v| *v > 0);
            // Invariants after every operation, failed or not.
            let holders = ch.all_holders(t, 4);
            assert_eq!(holders.iter().map(|h| h.1).sum::<u128>(), ch.supply(t));
            assert_eq!(ch.supply(t), model_supply);
            assert_eq!(
                holders,
                model.iter().map(|(a, b)| (*a, *b)).collect::<Vec<_>>()
            );
        }
    }
}

/// Same script on `MemHost` and on `RuntimeHost`: identical outputs and identical gas.
#[test]
fn memhost_and_runtime_host_agree_on_results_and_gas() {
    let c = Contract::from_elf(elf()).unwrap();
    let mut mem = MemHost {
        contract: CONTRACT,
        ..Default::default()
    };
    let mut ch = Chain::new();
    let launch = (
        "Gold".to_string(),
        "GLD".to_string(),
        6u8,
        1_000u128,
        10u128,
        true,
    );
    let script: Vec<(FunctionKind, &str, Address, Vec<u8>)> = {
        let t = Token(derive_id(&CONTRACT, 0));
        vec![
            (FunctionKind::Mutation, "launch", LAUNCHER, launch.encode()),
            (FunctionKind::Mutation, "claim", addr(1), (t,).encode()),
            (FunctionKind::Mutation, "claim", addr(2), (t,).encode()),
            (
                FunctionKind::Mutation,
                "mint_more",
                LAUNCHER,
                (t, addr(3), 9u128).encode(),
            ),
            (
                FunctionKind::Query,
                "holders",
                LAUNCHER,
                (t, None::<Address>, 10u32).encode(),
            ),
            (FunctionKind::Query, "info", LAUNCHER, (t,).encode()),
        ]
    };
    for (kind, name, who, input) in script {
        mem.caller = who.0;
        let a = c.call(kind, name, &input, &mut mem, GAS, MEM).unwrap();
        let tables = Tables::new();
        let mut h = RuntimeHost::new(&ch.kv, &tables, CONTRACT, who.0, 1);
        let b = c.call(kind, name, &input, &mut h, GAS, MEM).unwrap();
        let batch = h.finish().unwrap();
        ch.kv.commit(batch).unwrap();
        assert_eq!((a.output, a.gas_used), (b.output, b.gas_used), "{name}");
    }
}
