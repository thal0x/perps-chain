//! End-to-end with the production `HlChain`: real HL signing (hlwire
//! wallets, L1 phantom-agent and user-signed EIP-712), real engines.
use hlwire::*;
use interfaces::*;
use state::testkit::{limit, test_genesis, usd, GENESIS_TIME};
use state::{ChainConfig, HlChain};

struct E2e {
    chain: HlChain,
    time: u64,
    vals: Vec<Wallet>,
    vnonce: u64,
}

fn wallet(i: u8) -> Wallet {
    Wallet::from_bytes(&[i; 32]).unwrap()
}

impl E2e {
    fn new(funded: &[(&Wallet, &str)]) -> Self {
        let vals: Vec<Wallet> = (100..104u8).map(wallet).collect();
        let bal: Vec<(Address, Usd)> = funded.iter().map(|(w, s)| (w.address(), usd(s))).collect();
        let mut g = test_genesis(4, &bal);
        for (i, w) in vals.iter().enumerate() {
            g.validators.validators[i].address = w.address();
        }
        Self { chain: HlChain::new_hl(g, ChainConfig::strict()), time: GENESIS_TIME, vals, vnonce: 0 }
    }
    fn block(&mut self, txs: Vec<Tx>) -> BlockOutcome {
        self.time += 1000;
        let ctx = BlockContext {
            height: self.chain.query().height() + 1,
            timestamp_ms: self.time,
            proposer: self.vals[0].address(),
            block_id: Hash32::ZERO,
        };
        for t in &txs {
            // mempool admission first (fills the shared signature cache)
            let _ = self.chain.check_tx(t);
        }
        self.chain.execute_block(&ctx, &txs)
    }
    fn l1(&mut self, w: &Wallet, action: serde_json::Value) -> Tx {
        self.time += 1;
        let body = signed_l1_request(w, action, self.time, None, None, false);
        Tx::User(parse_exchange_request(&body).unwrap())
    }
    fn user_signed(&mut self, w: &Wallet, action: serde_json::Value) -> Tx {
        let body = signed_user_request(w, action, false);
        Tx::User(parse_exchange_request(&body).unwrap())
    }
    fn oracle(&mut self, asset: AssetId, px: Px) -> Vec<Tx> {
        let vals = self.vals.clone();
        vals.iter()
            .map(|w| {
                self.vnonce += 1;
                Tx::Validator(sign_validator_tx(w, 1337, self.vnonce, ValidatorAction::OraclePrices { prices: vec![(asset, px)] }))
            })
            .collect()
    }
}

use interfaces::Application;

#[test]
fn hl_signed_flows() {
    let (mm, alice, agent) = (wallet(1), wallet(2), wallet(3));
    let mut e = E2e::new(&[(&mm, "1000000"), (&alice, "10000")]);
    // check_tx returns the recovered signer
    let t = e.l1(&mm, noop_action());
    assert_eq!(e.chain.check_tx(&t), Ok(mm.address()));
    // MM rests asks; Alice approves an agent (user-signed) and the agent trades.
    let t1 = e.l1(&mm, order_action(&[limit(1, false, "3000", "1", Tif::Gtc), limit(1, false, "3001", "1", Tif::Gtc)], Grouping::Na));
    let n = e.time + 1;
    e.time = n;
    let t2 = e.user_signed(&alice, approve_agent_action(&agent.address(), None, n));
    let o = e.block(vec![t1, t2]);
    assert!(matches!(&o.receipts[0].result, Ok(ActionResponse::Order(st)) if st.iter().all(|s| matches!(s, OrderStatusResp::Resting { .. }))), "{:?}", o.receipts[0]);
    assert_eq!(o.receipts[1].result, Ok(ActionResponse::Default), "{:?}", o.receipts[1]);
    assert_eq!(e.chain.query().principal_of(&agent.address()), alice.address());
    let t = e.l1(&agent, order_action(&[limit(1, true, "3001", "1.5", Tif::Ioc)], Grouping::Na));
    let o = e.block(vec![t]);
    assert_eq!(o.receipts[0].user, alice.address());
    let Ok(ActionResponse::Order(st)) = &o.receipts[0].result else { panic!("{:?}", o.receipts[0]) };
    assert!(matches!(st[0], OrderStatusResp::Filled { total_sz, .. } if total_sz == Sz::parse("1.5").unwrap()), "{st:?}");
    assert_eq!(e.chain.clearinghouse().position(&alice.address(), 1), Sz::parse("1.5").unwrap());
    // usdSend + withdraw3 (user-signed)
    let n = e.time + 1;
    e.time = n;
    let t1 = e.user_signed(&mm, usd_send_action(&alice.address(), usd("100"), n));
    let t2 = e.user_signed(&mm, withdraw3_action(&mm.address(), usd("500"), n + 1));
    e.time = n + 1;
    let o = e.block(vec![t1, t2]);
    assert_eq!(o.receipts[0].result, Ok(ActionResponse::Default), "{:?}", o.receipts[0]);
    assert_eq!(o.receipts[1].result, Ok(ActionResponse::Default), "{:?}", o.receipts[1]);
    let pw = e.chain.query().pending_withdrawals();
    assert_eq!((pw[0].amount, pw[0].destination), (usd("499"), mm.address()));
    // validator oracle txs signed with real keys move the oracle price
    let txs = e.oracle(1, Px::parse("3100").unwrap());
    e.block(txs);
    assert_eq!(e.chain.oracle_px(1), Px::parse("3100").unwrap());
    // a tampered signature: error receipt, nothing happens
    let Tx::User(mut u) = e.l1(&mm, cancel_action(&[(1, 2)])) else { unreachable!() };
    u.signature.s[31] ^= 1;
    let before = e.chain.query().open_orders(&mm.address()).len();
    let o = e.block(vec![Tx::User(u)]);
    assert!(o.receipts[0].result.is_err() || o.receipts[0].user != mm.address());
    assert_eq!(e.chain.query().open_orders(&mm.address()).len(), before);
}

#[test]
fn hl_chain_replay_is_deterministic() {
    let (mm, alice) = (wallet(1), wallet(2));
    let mut e = E2e::new(&[(&mm, "1000000"), (&alice, "10000")]);
    let mut blocks = vec![];
    for i in 0..30u32 {
        let p = format!("{}", 3000 + (i % 7));
        let t1 = e.l1(&mm, order_action(&[limit(1, false, &p, "0.5", Tif::Gtc), limit(1, true, "2990", "0.5", Tif::Gtc)], Grouping::Na));
        let t2 = e.l1(&alice, order_action(&[limit(1, i % 2 == 0, if i % 2 == 0 { "3010" } else { "2980" }, "0.2", Tif::Ioc)], Grouping::Na));
        let mut txs = vec![t1, t2];
        if i % 5 == 0 {
            txs.extend(e.oracle(1, Px::parse(&p).unwrap()));
        }
        let ctx_time = e.time + 1000;
        let o = e.block(txs.clone());
        blocks.push((ctx_time, txs, o.state_root));
    }
    let mut e2 = E2e::new(&[(&mm, "1000000"), (&alice, "10000")]);
    for (t, txs, root) in blocks {
        e2.time = t - 1000;
        let o = e2.block(txs);
        assert_eq!(o.state_root, root);
    }
}
