//! HTTPS actions on the chain (`docs/ORACLES.md`, section 3): request, escrow, votes, delivery to
//! the callback, and every way it can be abused. Security-sensitive: each refusal path is tested
//! (forged, replayed, misdirected and unauthorised votes; callbacks called by users; hosts outside
//! the allowlist; queries trying to start an action), plus determinism between the sequential and
//! the parallel executor.
//!
//! The validators are played by the test: it signs their votes with their consensus keys, exactly
//! as `btb-node` does after it fetched and transformed a response.

mod support;
use btb_chain::actions::{self, Pending};
use btb_chain::*;
use btb_codec::{Decode, Encode};
use btb_runtime::MemKv;
use btb_vm::FunctionKind;
use support::*;

const E: u64 = 1_000;
const EACH: u64 = 1_000_000_000_000;
const N: usize = 4;
const STAKE: u64 = 25_000;
/// Enough for the callback's gas limit plus every validator's answer.
const FEE: u64 = 6_000_000;

fn ckey(i: usize) -> SigningKey {
    SigningKey::from_bytes(&[100 + i as u8; 32])
}

fn ckey_bytes(i: usize) -> [u8; 32] {
    ckey(i).verifying_key().to_bytes()
}

struct Env {
    net: Net<MemKv>,
    ops: Vec<Wallet>,
    users: Vec<Wallet>,
    contract: Address,
}

fn env_with(ex: Executor) -> Env {
    let mut ops: Vec<Wallet> = (0..N).map(|i| Wallet::new(10 + i as u8)).collect();
    let mut users: Vec<Wallet> = (0..3).map(|i| Wallet::new(50 + i as u8)).collect();
    let mut all: Vec<&Wallet> = ops.iter().collect();
    all.extend(users.iter());
    let mut cfg = ChainConfig::new(CHAIN_ID);
    cfg.staking = StakingConfig {
        epoch_len: E,
        max_validators: 8,
        min_stake: 1_000,
        unbonding_blocks: E,
        slash_bps: 2_000,
        reporter_reward_bps: 1_000,
    };
    let g = genesis_with(&all, EACH, cfg);
    let vals: Vec<GenesisValidator> = (0..N)
        .map(|i| GenesisValidator {
            operator: ops[i].addr(),
            consensus_key: ckey_bytes(i),
            network_addr: Vec::new(),
            stake: STAKE,
        })
        .collect();
    let state = ChainState::from_genesis_with_validators(MemKv::new(), &g, &vals).unwrap();
    let mut net = Net { state, ex };
    // the contract: deployed by user 0, funded (it pays the deposit for what the callbacks store)
    let tx = users[0].deploy(elf("actions"), manifest("actions"));
    let contract = deployed(&net.run(vec![tx]).remove(0));
    let fund = users[0].transfer(contract, 100_000_000_000);
    ok(&net.run(vec![fund]).remove(0));
    ops.iter_mut().for_each(|o| o.chain_id = CHAIN_ID);
    Env {
        net,
        ops,
        users,
        contract,
    }
}

fn env() -> Env {
    env_with(Executor::sequential())
}

impl Env {
    fn run(&mut self, txs: Vec<Transaction>, votes: Vec<ActionVote>) -> Vec<Receipt> {
        let b = self.net.block_for(txs).with_actions(votes);
        let (r, root) = self.net.ex.execute_block(&mut self.net.state, &b).unwrap();
        assert_eq!(root, self.net.state.root().unwrap());
        assert_eq!(
            self.net.state.total_supply().unwrap(),
            u128::from(TOTAL_SUPPLY),
            "the supply is fixed"
        );
        r
    }

    fn run_one(&mut self, tx: Transaction) -> Receipt {
        self.run(vec![tx], vec![]).remove(0)
    }

    fn votes(&mut self, votes: Vec<ActionVote>) -> Vec<Receipt> {
        self.run(vec![], votes)
    }

    fn hosts(&mut self, hosts: &[&str]) -> Receipt {
        let c = self.contract;
        let tx = self.users[0].sign(TxKind::Action(ActionOp::SetHosts {
            contract: c,
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }));
        self.run_one(tx)
    }

    fn request_tx(
        &mut self,
        user: usize,
        function: &str,
        args: &impl Encode,
        fee: u64,
    ) -> Transaction {
        let c = self.contract;
        self.users[user].sign(TxKind::Action(ActionOp::Request {
            contract: c,
            function: function.into(),
            args: args.encode(),
            gas_limit: GAS,
            max_fee: fee,
        }))
    }

    /// Request an action and return its id.
    fn request(&mut self, function: &str, args: &impl Encode) -> Hash {
        let tx = self.request_tx(1, function, args, FEE);
        let r = self.run_one(tx);
        ok(&r);
        r.output.try_into().unwrap()
    }

    fn pending(&self) -> Vec<(Hash, Pending)> {
        actions::pending_list(self.net.state.store()).unwrap()
    }

    /// The contract's delivery log: `(status, data, args, request id)`.
    fn log(&mut self) -> Vec<(u8, Vec<u8>, Vec<u8>, Hash)> {
        let c = self.contract;
        let who = self.users[2].addr();
        let raw = Vec::<u8>::decode(&self.net.query(&c, "last", &(), &who)).unwrap();
        let mut out = Vec::new();
        let mut r = &raw[..];
        while !r.is_empty() {
            let status = r[0];
            let n = u32::from_le_bytes(r[1..5].try_into().unwrap()) as usize;
            let data = r[5..5 + n].to_vec();
            r = &r[5 + n..];
            let m = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
            let args = r[4..4 + m].to_vec();
            r = &r[4 + m..];
            let id: Hash = r[..32].try_into().unwrap();
            r = &r[32..];
            out.push((status, data, args, id));
        }
        out
    }
}

fn vote(i: usize, id: Hash, status: u8, data: &[u8]) -> ActionVote {
    ActionVote::sign(CHAIN_ID, &ckey(i), id, status, data.to_vec())
}

fn url() -> String {
    "http://127.0.0.1:8080/price".to_string()
}

fn callbacks(rs: &[Receipt]) -> Vec<&Receipt> {
    rs.iter()
        .filter(|r| matches!(r.origin, Origin::Action { .. }))
        .collect()
}

fn reverted(r: &Receipt) -> Revert {
    match &r.status {
        Status::Reverted(x) => x.clone(),
        other => panic!("expected a revert, got {other:?}"),
    }
}

// ------------------------------------------------------------------------------ happy path

#[test]
fn a_request_is_agreed_by_two_thirds_and_delivered_to_the_callback() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let user_before = e.net.bal(&e.users[1].addr());
    let escrow_before = e.net.bal(&actions::escrow_pool());
    let tx = e.request_tx(1, "quote", &url(), FEE);
    let req = e.run_one(tx);
    ok(&req);
    let id: Hash = req.output.clone().try_into().unwrap();

    // pending, escrowed, with the validated request stored
    let p = e.pending();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].0, id);
    assert_eq!(p[0].1.escrow, FEE);
    assert_eq!(p[0].1.args, url().encode());
    assert_eq!(p[0].1.http().unwrap().url, url());
    assert_eq!(p[0].1.http().unwrap().timeout_ms, 2000);
    assert_eq!(e.net.bal(&actions::escrow_pool()), escrow_before + FEE);
    assert_eq!(
        e.net.bal(&e.users[1].addr()),
        user_before - FEE - req.fee,
        "the user paid the gas and the escrow"
    );

    // two of four validators (50% of the stake) are not enough, in one block or across blocks
    assert!(callbacks(&e.votes(vec![vote(0, id, 0, b"42"), vote(1, id, 0, b"42")])).is_empty());
    assert!(callbacks(&e.votes(vec![])).is_empty());
    assert_eq!(e.pending().len(), 1);
    assert!(e.log().is_empty());

    // the third vote, in a later block, completes the two thirds: delivered in that same block
    let rs = e.votes(vec![vote(2, id, 0, b"42")]);
    let cb = callbacks(&rs);
    assert_eq!(cb.len(), 1);
    ok(cb[0]);
    match &cb[0].origin {
        Origin::Action {
            contract,
            function,
            request,
        } => {
            assert_eq!(*contract, e.contract);
            assert_eq!(function, "quote_callback");
            assert_eq!(*request, id);
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(e.log(), vec![(0, b"42".to_vec(), url().encode(), id)]);
    assert!(e.pending().is_empty());
    assert_eq!(actions::pending_count(e.net.state.store()).unwrap(), 0);

    // settlement: the three validators that answered earn RESPONSE_GAS each, the callback's gas
    // goes to the validator pool, everything else is refunded
    let paid = actions::RESPONSE_GAS;
    for i in 0..3 {
        assert_eq!(
            e.net.bal(&e.ops[i].addr()),
            EACH - STAKE + paid,
            "validator {i}"
        );
    }
    assert_eq!(
        e.net.bal(&e.ops[3].addr()),
        EACH - STAKE,
        "silent validators earn nothing"
    );
    assert_eq!(e.net.bal(&actions::escrow_pool()), escrow_before);
    assert_eq!(
        e.net.bal(&e.users[1].addr()),
        user_before - req.fee - cb[0].fee - 3 * paid
    );

    // a late fourth vote finds no request: ignored, no second delivery
    let rs = e.votes(vec![vote(3, id, 0, b"42")]);
    assert!(callbacks(&rs).is_empty());
    assert_eq!(e.log().len(), 1);
}

#[test]
fn a_post_request_carries_its_body_and_headers() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let id = e.request("post", &(url(), b"hello".to_vec()));
    let p = e.pending().remove(0).1;
    let h = p.http().unwrap();
    assert_eq!(h.method, 1);
    assert_eq!(h.body, b"hello");
    assert_eq!(
        h.headers,
        vec![("content-type".to_string(), "text/plain".to_string())]
    );
    let votes = (0..3).map(|i| vote(i, id, 0, b"ok")).collect();
    assert_eq!(callbacks(&e.votes(votes)).len(), 1);
}

#[test]
fn an_agreed_failure_is_delivered_as_a_failure() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let id = e.request("quote", &url());
    let votes = (0..3).map(|i| vote(i, id, 1, b"fetch failed")).collect();
    let rs = e.votes(votes);
    ok(callbacks(&rs)[0]);
    assert_eq!(e.log()[0].0, actions::STATUS_FAILED);
    assert_eq!(e.log()[0].1, b"fetch failed");
}

// ------------------------------------------------------------------------------ no agreement

#[test]
fn validators_that_disagree_get_an_error_callback_at_once() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let id = e.request("raw", &url());
    // 25% each, four different answers: nothing can reach two thirds any more after the third
    let rs = e.votes(vec![vote(0, id, 0, b"a"), vote(1, id, 0, b"b")]);
    assert!(
        callbacks(&rs).is_empty(),
        "two more could still agree with one of them"
    );
    let rs = e.votes(vec![vote(2, id, 0, b"c")]);
    let cb = callbacks(&rs);
    assert_eq!(
        cb.len(),
        1,
        "one validator left cannot make 2/3 of anything"
    );
    ok(cb[0]);
    let log = e.log();
    assert_eq!(log[0].0, actions::STATUS_DISAGREEMENT);
    assert!(log[0].1.is_empty());
    // the three that answered were paid for their work
    for i in 0..3 {
        assert_eq!(
            e.net.bal(&e.ops[i].addr()),
            EACH - STAKE + actions::RESPONSE_GAS
        );
    }
    assert!(e.pending().is_empty());
}

#[test]
fn a_two_two_split_is_a_disagreement_too() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let id = e.request("raw", &url());
    let rs = e.votes(vec![
        vote(0, id, 0, b"a"),
        vote(1, id, 0, b"a"),
        vote(2, id, 0, b"b"),
        vote(3, id, 0, b"b"),
    ]);
    ok(callbacks(&rs)[0]);
    assert_eq!(e.log()[0].0, actions::STATUS_DISAGREEMENT);
}

#[test]
fn a_request_nobody_answers_times_out_and_refunds_the_escrow() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let user_before = e.net.bal(&e.users[1].addr());
    let tx = e.request_tx(1, "quote", &url(), FEE);
    let req = e.run_one(tx);
    ok(&req);
    let created = e.pending()[0].1.created;
    let deadline = created + actions::DEADLINE_BLOCKS;
    assert_eq!(e.pending()[0].1.deadline, deadline);
    let mut delivered = None;
    while e.net.state.head().unwrap().height < deadline {
        let rs = e.votes(vec![]);
        if !callbacks(&rs).is_empty() {
            delivered = Some(e.net.state.head().unwrap().height);
        }
    }
    assert_eq!(
        delivered,
        Some(deadline),
        "delivered in the deadline block, not before"
    );
    let log = e.log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0, actions::STATUS_TIMEOUT);
    // nobody answered: nobody is paid; the user lost the gas of the request and of the callback
    let after = e.net.bal(&e.users[1].addr());
    assert!(after < user_before && user_before - after < req.fee + 1_000_000);
    assert_eq!(e.net.bal(&actions::escrow_pool()), 0);
    for i in 0..N {
        assert_eq!(e.net.bal(&e.ops[i].addr()), EACH - STAKE);
    }
}

#[test]
fn a_late_quorum_does_not_beat_the_deadline_but_an_early_one_does() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let id = e.request("quote", &url());
    // one answer, then the deadline: the callback gets a timeout, the one answer was paid
    e.votes(vec![vote(0, id, 0, b"7")]);
    let deadline = e.pending()[0].1.deadline;
    while e.net.state.head().unwrap().height + 1 < deadline {
        assert!(callbacks(&e.votes(vec![])).is_empty());
    }
    // votes that complete the quorum in the deadline block still win over the timeout
    let rs = e.votes(vec![vote(1, id, 0, b"7"), vote(2, id, 0, b"7")]);
    assert_eq!(callbacks(&rs).len(), 1);
    assert_eq!(e.log()[0].0, actions::STATUS_OK);
}

// ------------------------------------------------------------------------------ forged votes

#[test]
fn forged_replayed_and_misdirected_votes_are_ignored() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let id = e.request("quote", &url());
    let other = e.request("quote", &url());

    // a stranger with a valid signature, 3 of them: nothing
    let stranger = |s: u8| {
        ActionVote::sign(
            CHAIN_ID,
            &SigningKey::from_bytes(&[s; 32]),
            id,
            0,
            b"666".to_vec(),
        )
    };
    // another chain's signature
    let wrong_chain = ActionVote::sign(CHAIN_ID + 1, &ckey(0), id, 0, b"666".to_vec());
    // a flipped bit in the data, signature kept
    let mut tampered = vote(1, id, 0, b"42");
    tampered.data = b"43".to_vec();
    // a vote for `other` moved to `id`
    let mut moved = vote(2, other, 0, b"42");
    moved.request = id;
    // a status the validators cannot sign
    let bad_status = ActionVote::sign(CHAIN_ID, &ckey(3), id, actions::STATUS_TIMEOUT, vec![]);
    // an oversized result
    let big = ActionVote::sign(
        CHAIN_ID,
        &ckey(3),
        id,
        0,
        vec![1; actions::MAX_RESULT_BYTES + 1],
    );
    // a request that does not exist
    let ghost = vote(0, [9; 32], 0, b"x");
    let junk = vec![
        stranger(1),
        stranger(2),
        stranger(3),
        wrong_chain,
        tampered,
        moved,
        bad_status,
        big,
        ghost,
    ];
    let rs = e.votes(junk);
    assert!(callbacks(&rs).is_empty());
    assert_eq!(e.pending().len(), 2);
    assert!(actions::votes_of(e.net.state.store(), &id)
        .unwrap()
        .is_empty());

    // the first statement of a validator stands: voting twice (even differently) counts once
    e.votes(vec![
        vote(0, id, 0, b"42"),
        vote(0, id, 0, b"43"),
        vote(0, id, 0, b"42"),
    ]);
    let v = actions::votes_of(e.net.state.store(), &id).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].1, actions::outcome_hash(0, b"42"));
    // so three copies of one validator's vote are not a quorum
    let rs = e.votes(vec![
        vote(0, id, 0, b"42"),
        vote(0, id, 0, b"42"),
        vote(0, id, 0, b"42"),
    ]);
    assert!(callbacks(&rs).is_empty());

    // the real quorum
    let rs = e.votes(vec![vote(1, id, 0, b"42"), vote(2, id, 0, b"42")]);
    assert_eq!(callbacks(&rs).len(), 1);
    assert_eq!(e.log().len(), 1);
    assert_eq!(e.log()[0].1, b"42");

    // replaying every vote ever made (to the finished request and to the other one) does
    // nothing: no second callback, and the old votes do not count for the other request
    let replay = vec![
        vote(0, id, 0, b"42"),
        vote(1, id, 0, b"42"),
        vote(2, id, 0, b"42"),
    ];
    let rs = e.votes(replay);
    assert!(callbacks(&rs).is_empty());
    assert_eq!(e.log().len(), 1);
    assert_eq!(e.pending().len(), 1);
    assert!(actions::votes_of(e.net.state.store(), &other)
        .unwrap()
        .is_empty());
}

#[test]
fn a_validator_cannot_vote_after_leaving_the_set_and_non_validators_never_count() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let id = e.request("quote", &url());
    // an account key that is not a validator's consensus key, even the operator's own key
    let operator_vote = ActionVote::sign(CHAIN_ID, &e.ops[0].key, id, 0, b"42".to_vec());
    let rs = e.votes(vec![operator_vote]);
    assert!(callbacks(&rs).is_empty());
    assert!(actions::votes_of(e.net.state.store(), &id)
        .unwrap()
        .is_empty());
}

#[test]
fn a_block_full_of_junk_votes_still_executes() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let _ = e.request("quote", &url());
    let junk: Vec<ActionVote> = (0..actions::MAX_VOTES_PER_BLOCK)
        .map(|i| ActionVote {
            request: [i as u8; 32],
            status: 0,
            data: vec![i as u8; 8],
            key: ckey_bytes(i % N),
            sig: [i as u8; 64],
        })
        .collect();
    assert!(callbacks(&e.votes(junk)).is_empty());
    assert_eq!(e.pending().len(), 1);
}

// ------------------------------------------------------------------------------ authority

#[test]
fn users_cannot_call_a_callback_or_feed_a_contract_a_made_up_result() {
    let mut e = env();
    let c = e.contract;
    let fake = ([7u8; 32], Vec::<u8>::new(), 0u8, b"1000000".to_vec());
    let tx = e.users[1].call(c, "quote_callback", &fake);
    let r = e.run_one(tx);
    assert_eq!(
        reverted(&r),
        Revert::Action(ActionError::CallbackIsSystemOnly)
    );
    assert!(e.log().is_empty());
    // the callback's own check is the second lock: a plain mutation using it aborts for a user
    let tx = e.users[1].call(c, "guarded", &());
    let r = e.run_one(tx);
    assert!(
        matches!(reverted(&r), Revert::Abort(m) if m.contains("callback")),
        "{r:?}"
    );
    // a contract's owner has no special right either
    let tx = e.users[0].call(c, "raw_callback", &fake);
    assert_eq!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::CallbackIsSystemOnly)
    );
}

#[test]
fn queries_and_calls_cannot_start_an_action() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let c = e.contract;
    let who = e.users[2].addr();
    // an action is not a query
    let err = e
        .net
        .ex
        .query(&mut e.net.state, &c, "quote", &url().encode(), &who);
    assert_eq!(err, Err(Revert::NoSuchFunction));
    // a query that asks for a report (state change) traps instead of queueing one
    let err = e
        .net
        .ex
        .query(&mut e.net.state, &c, "cheat_report", &().encode(), &who);
    assert!(matches!(err, Err(Revert::Trap(_))), "{err:?}");
    // a Call transaction must be a mutation: queries and actions are refused before they run
    for kind in [FunctionKind::Query, FunctionKind::Action] {
        let tx = e.users[1].call_with(c, kind, "quote", &url(), GAS);
        let r = e.run_one(tx);
        assert_eq!(r.status, Status::Invalid(Invalid::KindNotAllowed(kind)));
        e.users[1].nonce -= 1; // an invalid transaction does not use its nonce
    }
    // a mutation call of an action's name finds no mutation
    let tx = e.users[1].call(c, "quote", &url());
    assert_eq!(reverted(&e.run_one(tx)), Revert::NoSuchFunction);
    assert!(e.pending().is_empty());
}

#[test]
fn the_first_phase_runs_read_only() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let tx = e.request_tx(1, "leak", &url(), FEE);
    let r = e.run_one(tx);
    assert!(
        matches!(reverted(&r), Revert::Trap(t) if t.contains("WriteInQuery")),
        "{r:?}"
    );
    assert!(e.pending().is_empty());
    assert_eq!(
        e.net.bal(&actions::escrow_pool()),
        0,
        "no escrow taken on a failed request"
    );
}

// ------------------------------------------------------------------------------ allowlist and limits

#[test]
fn only_allowlisted_hosts_can_be_called_and_only_the_deployer_sets_the_list() {
    let mut e = env();
    // no list yet: nothing may be called
    let tx = e.request_tx(1, "quote", &url(), FEE);
    assert!(matches!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::HostNotAllowed(_))
    ));
    // someone else cannot set it
    let c = e.contract;
    let tx = e.users[1].sign(TxKind::Action(ActionOp::SetHosts {
        contract: c,
        hosts: vec!["127.0.0.1:8080".into()],
    }));
    assert_eq!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::NotDeployer)
    );
    // a list that is itself bad
    for hosts in [
        vec!["*.com".to_string()],
        vec!["a b".to_string()],
        vec!["x".repeat(200)],
    ] {
        let tx = e.users[0].sign(TxKind::Action(ActionOp::SetHosts { contract: c, hosts }));
        let r = e.run_one(tx);
        assert_eq!(r.status, Status::Invalid(Invalid::TooLarge), "{r:?}");
        e.users[0].nonce -= 1;
    }
    ok(&e.hosts(&["127.0.0.1:8080"]));
    // another port, another host, another scheme-looking trick, user info: refused
    for u in [
        "http://127.0.0.1:9090/price",
        "http://localhost:8080/price",
        "http://127.0.0.1.evil.com:8080/",
        "http://127.0.0.1:8080@evil.com/",
        "http://2130706433:8080/",
        "ftp://127.0.0.1:8080/",
    ] {
        let tx = e.request_tx(1, "quote", &u.to_string(), FEE);
        let r = e.run_one(tx);
        assert!(
            matches!(
                reverted(&r),
                Revert::Action(ActionError::HostNotAllowed(_) | ActionError::BadRequest(_))
            ),
            "{u}: {r:?}"
        );
    }
    // the allowed one works; the first phase itself may return a URL the chain must still check
    let id = e.request("quote", &url());
    assert_eq!(e.pending()[0].0, id);
    let tx = e.request_tx(1, "sneaky", &url(), FEE);
    assert!(matches!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::BadRequest(_))
    ));
    // removing the list closes the door again (pending requests keep the list they were made with)
    ok(&e.hosts(&[]));
    assert_eq!(e.pending()[0].1.hosts, vec!["127.0.0.1:8080".to_string()]);
    let tx = e.request_tx(1, "quote", &url(), FEE);
    assert!(matches!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::HostNotAllowed(_))
    ));
}

#[test]
fn requests_are_refused_for_missing_actions_callbacks_escrow_and_a_full_queue() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let tx = e.request_tx(1, "nothing_here", &url(), FEE);
    assert_eq!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::NoSuchAction)
    );
    // a mutation's name is not an action
    let tx = e.request_tx(1, "publish", &url(), FEE);
    assert_eq!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::NoSuchAction)
    );
    // no `<action>_callback` mutation to deliver to
    let tx = e.request_tx(1, "orphan", &url(), FEE);
    assert_eq!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::NoCallback)
    );
    let bad_contract = e.users[1].sign(TxKind::Action(ActionOp::Request {
        contract: [1; 32],
        function: "quote".into(),
        args: url().encode(),
        gas_limit: GAS,
        max_fee: FEE,
    }));
    assert_eq!(
        reverted(&e.run_one(bad_contract)),
        Revert::Action(ActionError::NoSuchContract)
    );
    // the escrow must pay the callback's gas limit and every validator's answer
    let needed = (ChainConfig::new(CHAIN_ID).cron_gas_limit + actions::RESPONSE_GAS * N as u64)
        * ChainConfig::new(CHAIN_ID).gas_price;
    let tx = e.request_tx(1, "quote", &url(), needed - 1);
    assert_eq!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::EscrowTooLow {
            needed,
            max_fee: needed - 1
        })
    );
    let tx = e.request_tx(1, "quote", &url(), needed);
    ok(&e.run_one(tx));
    assert_eq!(e.pending().len(), 1);
    // arguments that do not decode abort the first phase
    let tx = e.request_tx(1, "quote", &1u8, FEE);
    assert!(matches!(reverted(&e.run_one(tx)), Revert::Abort(_)));
    // a user who cannot afford the escrow is not valid at all
    let poor = Wallet::new(77);
    let mut poor = poor;
    let tx = poor.sign(TxKind::Action(ActionOp::Request {
        contract: e.contract,
        function: "quote".into(),
        args: url().encode(),
        gas_limit: GAS,
        max_fee: FEE,
    }));
    assert!(matches!(
        e.run_one(tx).status,
        Status::Invalid(Invalid::InsufficientBalance { .. })
    ));
}

#[test]
fn one_account_cannot_hold_the_queue_and_the_queue_is_bounded() {
    let mut e = env();
    ok(&e.hosts(&["127.0.0.1:8080"]));
    // one account: at most MAX_PENDING_PER_REQUESTER at a time
    for _ in 0..actions::MAX_PENDING_PER_REQUESTER {
        let tx = e.request_tx(1, "quote", &url(), FEE);
        ok(&e.run_one(tx));
    }
    let tx = e.request_tx(1, "quote", &url(), FEE);
    assert_eq!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::TooManyPending)
    );
    // another account is not affected
    let tx = e.request_tx(2, "quote", &url(), FEE);
    ok(&e.run_one(tx));
    // once a request is settled its slot is free again
    let id = e.pending()[0].0;
    let rs = e.votes((0..3).map(|i| vote(i, id, 0, b"1")).collect());
    assert_eq!(callbacks(&rs).len(), 1);
    let tx = e.request_tx(1, "quote", &url(), FEE);
    ok(&e.run_one(tx));

    // many accounts fill the whole queue; then nobody can add one
    let mut wallets: Vec<Wallet> = (0..20).map(|i| Wallet::new(100 + i)).collect();
    let fund: Vec<Transaction> = wallets
        .iter()
        .map(|w| e.users[0].transfer(w.addr(), 300_000_000))
        .collect();
    for r in e.run(fund, vec![]) {
        ok(&r);
    }
    let c = e.contract;
    // 20 accounts x 16 requests in one block: the queue takes 256 and refuses the rest
    // (the requests must be in one block: every request ends after DEADLINE_BLOCKS)
    let mut txs = Vec::new();
    for w in wallets.iter_mut() {
        for _ in 0..actions::MAX_PENDING_PER_REQUESTER {
            txs.push(w.sign(TxKind::Action(ActionOp::Request {
                contract: c,
                function: "quote".into(),
                args: url().encode(),
                gas_limit: 1_000_000, // 320 of them must fit the block gas limit
                max_fee: FEE,
            })));
        }
    }
    let before = e.pending().len();
    let rs = e.run(txs, vec![]);
    let refused = rs
        .iter()
        .filter(|r| r.status == Status::Reverted(Revert::Action(ActionError::TooManyPending)))
        .count();
    assert_eq!(
        refused,
        20 * 16 - (actions::MAX_PENDING as usize - before),
        "the rest is refused"
    );
    assert_eq!(e.pending().len() as u64, actions::MAX_PENDING);
    let last = &mut wallets[19];
    let tx = last.sign(TxKind::Action(ActionOp::Request {
        contract: c,
        function: "quote".into(),
        args: url().encode(),
        gas_limit: GAS,
        max_fee: FEE,
    }));
    assert_eq!(
        reverted(&e.run_one(tx)),
        Revert::Action(ActionError::TooManyPending)
    );
}

// ------------------------------------------------------------------------------ wire formats

#[test]
fn action_transactions_and_blocks_round_trip() {
    let mut e = env();
    let c = e.contract;
    for op in [
        ActionOp::Request {
            contract: c,
            function: "quote".into(),
            args: vec![1, 2, 3],
            gas_limit: GAS,
            max_fee: FEE,
        },
        ActionOp::SetHosts {
            contract: c,
            hosts: vec!["a.example".into(), "*.b.example".into()],
        },
    ] {
        let tx = e.users[0].sign(TxKind::Action(op.clone()));
        let back = Transaction::decode(&tx.encode()).unwrap();
        assert_eq!(back, tx);
        assert_eq!(back.body.kind, TxKind::Action(op));
        assert!(back.verify_signature());
        // an unknown sub-tag is refused
        let mut bytes = tx.encode();
        let sub = bytes.iter().position(|b| *b == 13).unwrap() + 1; // tag then sub-tag
        bytes[sub] = 9;
        assert!(Transaction::decode(&bytes).is_err());
    }
    let id = [5u8; 32];
    let votes = vec![vote(0, id, 0, b"1"), vote(1, id, 1, b"")];
    let b = e.net.block_for(vec![]).with_actions(votes.clone());
    let back = Block::decode(&b.encode()).unwrap();
    assert_eq!(back, b);
    assert_eq!(back.actions, votes);
    // no votes: encoded exactly as a block without the section
    let plain = e.net.block_for(vec![]);
    assert!(Block::decode(&plain.encode()).unwrap().actions.is_empty());
    // truncated or padded vote sections are refused
    let mut bytes = b.encode();
    bytes.push(0);
    assert!(Block::decode(&bytes).is_err());
    bytes.truncate(bytes.len() - 3);
    assert!(Block::decode(&bytes).is_err());
}

// ------------------------------------------------------------------------------ determinism

fn scenario(ex: Executor) -> (Hash, Vec<Vec<(Origin, Status)>>) {
    let mut e = env_with(ex);
    ok(&e.hosts(&["127.0.0.1:8080"]));
    let mut history = Vec::new();
    let mut log = |rs: Vec<Receipt>| {
        history.push(
            rs.into_iter()
                .map(|r| (r.origin, r.status))
                .collect::<Vec<_>>(),
        )
    };
    let mut ids = Vec::new();
    // several requests in one block (the parallel executor sees a batch of conflicting txs)
    let txs: Vec<Transaction> = (0..6)
        .map(|i| {
            e.request_tx(
                1 + i % 2,
                if i % 3 == 0 { "raw" } else { "quote" },
                &url(),
                FEE,
            )
        })
        .collect();
    let rs = e.run(txs, vec![]);
    for r in &rs {
        ok(r);
        ids.push(<Hash>::try_from(r.output.clone()).unwrap());
    }
    log(rs);
    let mut votes = Vec::new();
    for (n, id) in ids.iter().enumerate() {
        for i in 0..3 {
            // the raw ones disagree, the quote ones agree
            let data: &[u8] = if n % 3 == 0 {
                [b"a", b"b", b"c"][i]
            } else {
                b"42"
            };
            votes.push(vote(i, *id, 0, data));
        }
    }
    log(e.votes(votes));
    log(e.votes(vec![]));
    (e.net.state.root().unwrap(), history)
}

#[test]
fn sequential_and_parallel_execution_agree() {
    let (root_seq, hist_seq) = scenario(Executor::sequential());
    let (root_par, hist_par) = scenario(Executor::parallel(4));
    assert_eq!(hist_seq, hist_par);
    assert_eq!(root_seq, root_par);
    // 2 raw requests disagree, 4 quote requests agree
    let callbacks_ok = hist_seq
        .iter()
        .flatten()
        .filter(|(o, s)| matches!(o, Origin::Action { .. }) && *s == Status::Success)
        .count();
    assert_eq!(callbacks_ok, 6);
}
