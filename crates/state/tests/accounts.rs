mod common;
use common::*;
use interfaces::Application;
use interfaces::*;
use state::errors as msg;

fn setup() -> H {
    H::new(&[(addr(MM), "1000000"), (addr(ALICE), "10000"), (addr(BOB), "10000")])
}

#[test]
fn nonce_rules_and_expiry() {
    let mut h = setup();
    let a = addr(ALICE);
    let n = h.time + 500;
    let o = h.block(vec![user_tx(a, &Action::Noop, n), user_tx(a, &Action::Noop, n)]);
    assert_eq!(o.receipts[0].result, Ok(ActionResponse::Default));
    assert_eq!(o.receipts[1].result, Err("Invalid nonce: duplicate nonce".into()));
    // too old / too far in the future
    let o = h.block(vec![
        user_tx(a, &Action::Noop, h.time - 3 * 24 * 3600 * 1000),
        user_tx(a, &Action::Noop, h.time + 2 * 24 * 3600 * 1000),
    ]);
    assert!(o.receipts[0].result.as_ref().unwrap_err().starts_with("Invalid nonce"));
    assert!(o.receipts[1].result.as_ref().unwrap_err().starts_with("Invalid nonce"));
    // 100 highest nonces: after 100 newer nonces, an older unused one is rejected.
    let base = h.time + 10_000;
    let txs: Vec<Tx> = (0..100).map(|i| user_tx(a, &Action::Noop, base + i)).collect();
    let o = h.block(txs);
    assert!(o.receipts.iter().all(|r| r.result.is_ok()));
    let o = h.block(vec![user_tx(a, &Action::Noop, base - 1), user_tx(a, &Action::Noop, base + 5000)]);
    assert!(o.receipts[0].result.as_ref().unwrap_err().contains("must be greater than"));
    assert!(o.receipts[1].result.is_ok());
    // Signer is a separate nonce space: Bob may reuse Alice's nonce.
    let o = h.block(vec![user_tx(addr(BOB), &Action::Noop, base + 5000)]);
    assert!(o.receipts[0].result.is_ok());
    // expiresAfter in the past
    let Tx::User(mut u) = user_tx(a, &Action::Noop, base + 6000) else { unreachable!() };
    u.expires_after = Some(h.time);
    let o = h.block(vec![Tx::User(u)]);
    assert_eq!(o.receipts[0].result, Err("Expired".into()));
    // the expired tx did not consume its nonce
    let o = h.block(vec![user_tx(a, &Action::Noop, base + 6000)]);
    assert!(o.receipts[0].result.is_ok());
}

#[test]
fn invalid_signature_has_no_effect() {
    let mut h = setup();
    let a = addr(ALICE);
    let n = h.time + 100;
    let Tx::User(mut u) =
        user_tx(a, &Action::Order { orders: vec![limit(1, true, "1100", "0.2", Tif::Gtc)], grouping: Grouping::Na }, n)
    else {
        unreachable!()
    };
    u.signature.v = 0;
    let o = h.block(vec![Tx::User(u)]);
    assert_eq!(o.receipts[0].result, Err(msg::INVALID_SIGNATURE.into()));
    assert_eq!(o.receipts[0].user, Address::ZERO);
    assert_eq!(o.order_actions, 0);
    assert!(h.chain.q().open_orders(&a).is_empty());
    assert!(h.chain.user_meta(&a).is_none());
    // garbage action bytes
    let Tx::User(mut u) = user_tx(a, &Action::Noop, n) else { unreachable!() };
    u.action_bytes = vec![0xff, 0xff];
    let o = h.block(vec![Tx::User(u)]);
    assert!(o.receipts[0].result.as_ref().unwrap_err().starts_with("Failed to decode action"));
}

#[test]
fn agents_named_and_unnamed() {
    let mut h = setup();
    let a = addr(ALICE);
    let (ag1, ag2, ag3) = (addr(501), addr(502), addr(503));
    let r = h.send_with_nonce(a, |n| Action::ApproveAgent { agent_address: ag1, agent_name: None, nonce: n });
    assert_eq!(r, Ok(ActionResponse::Default));
    assert_eq!(h.chain.q().principal_of(&ag1), a);
    // agent order acts on the principal's account
    let st = h.order1(ag1, limit(1, true, "1100", "0.2", Tif::Gtc));
    let oid = resting_oid(&st);
    assert_eq!(h.chain.q().order_by_oid(&a, oid).unwrap().oid, oid);
    assert_eq!(h.outcomes.last().unwrap().receipts[0].user, a);
    // named agent persists, new unnamed replaces the old unnamed
    h.send_with_nonce(a, |n| Action::ApproveAgent { agent_address: ag2, agent_name: Some("persist".into()), nonce: n })
        .unwrap();
    h.send_with_nonce(a, |n| Action::ApproveAgent { agent_address: ag3, agent_name: Some(String::new()), nonce: n })
        .unwrap();
    let q = h.chain.q();
    assert_eq!(q.principal_of(&ag1), ag1);
    assert_eq!(q.principal_of(&ag2), a);
    assert_eq!(q.principal_of(&ag3), a);
    // the replaced agent is now just an unknown address
    assert_eq!(h.send(ag1, Action::Cancel { cancels: vec![(1, oid)] }), Err(msg::user_does_not_exist(&ag1)));
    assert_eq!(h.cancel(ag2, 1, oid), SimpleStatus::Success);
    // someone else cannot take Alice's agent
    let r = h.send_with_nonce(addr(BOB), |n| Action::ApproveAgent { agent_address: ag2, agent_name: None, nonce: n });
    assert_eq!(r, Err(msg::AGENT_IN_USE.into()));
    // agents cannot move funds on behalf of the principal (user-signed actions use the signer)
    let r = h.send_with_nonce(ag2, |n| Action::UsdSend { destination: addr(BOB), amount: usd("1"), time: n });
    assert!(r.is_err());
    assert_eq!(h.chain.q().account(&a).withdrawable, usd("10000"));
}

#[test]
fn transfers_and_ledger() {
    let mut g = test_genesis(4, &[(addr(ALICE), usd("10000"))]);
    g.spot_balances = vec![(addr(ALICE), 1, usd("50")), (addr(ALICE), 0, usd("100"))];
    let mut h = H::from_genesis(g);
    let (a, b) = (addr(ALICE), addr(BOB));
    let r = h.send_with_nonce(a, |n| Action::UsdSend { destination: b, amount: usd("1000"), time: n });
    assert_eq!(r, Ok(ActionResponse::Default));
    assert_eq!(h.chain.q().account(&b).withdrawable, usd("1000"));
    let ledgers = h.last_events().iter().filter(|e| matches!(e, Event::Ledger { .. })).count();
    assert_eq!(ledgers, 2);
    // time field must equal the nonce
    let n = h.next_nonce();
    let o = h.block(vec![user_tx(a, &Action::UsdSend { destination: b, amount: usd("1"), time: n + 1 }, n)]);
    assert_eq!(o.receipts[0].result, Err(msg::NONCE_MISMATCH.into()));
    // too much
    let r = h.send_with_nonce(a, |n| Action::UsdSend { destination: b, amount: usd("100000"), time: n });
    assert!(r.is_err());
    // spotSend PURR by "NAME:0xTOKENID"
    let tok = format!("PURR:0x{}", hex::encode([0x11u8; 16]));
    let r = h.send_with_nonce(a, |n| Action::SpotSend { destination: b, token: tok.clone(), amount: usd("20"), time: n });
    assert_eq!(r, Ok(ActionResponse::Default));
    assert_eq!(h.chain.q().account(&b).spot_balances, vec![(1, usd("20"))]);
    let r = h.send_with_nonce(a, |n| Action::SpotSend {
        destination: b,
        token: "PURR:0xdeadbeef".into(),
        amount: usd("1"),
        time: n,
    });
    assert_eq!(r, Err(msg::INVALID_TOKEN.into()));
    // usdClassTransfer both ways
    let r = h.send_with_nonce(a, |n| Action::UsdClassTransfer { amount: usd("100"), to_perp: true, nonce: n });
    assert_eq!(r, Ok(ActionResponse::Default));
    assert_eq!(h.chain.q().account(&a).withdrawable, usd("9100"));
    let r = h.send_with_nonce(a, |n| Action::UsdClassTransfer { amount: usd("50"), to_perp: false, nonce: n });
    assert_eq!(r, Ok(ActionResponse::Default));
    assert_eq!(h.chain.q().account(&a).spot_balances, vec![(0, usd("50")), (1, usd("30"))]);
    // leverage
    assert_eq!(h.send(a, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 51 }), Err(msg::INVALID_LEVERAGE.into()));
    assert_eq!(h.send(a, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 10 }), Ok(ActionResponse::Default));
    assert_eq!(h.chain.clearinghouse().leverage_of(&a, 0), Leverage::Cross(10));
    // referrer recorded once
    assert_eq!(h.send(a, Action::SetReferrer { code: "ABC".into() }), Ok(ActionResponse::Default));
    assert_eq!(h.send(a, Action::SetReferrer { code: "ABC".into() }), Err(msg::REFERRER_ALREADY_SET.into()));
    assert_eq!(h.chain.user_meta(&a).unwrap().referrer.as_deref(), Some("ABC"));
}

#[test]
fn duplicate_cloid_rejected() {
    let mut h = setup();
    let a = addr(ALICE);
    let c = Some([3u8; 16]);
    let st = h.order(
        a,
        vec![
            OrderRequest { cloid: c, ..limit(1, true, "1100", "0.2", Tif::Gtc) },
            OrderRequest { cloid: c, ..limit(0, true, "50000", "0.01", Tif::Gtc) },
        ],
    );
    resting_oid(&st[0]);
    assert_eq!(err_of(&st[1]), msg::DUPLICATE_CLOID);
    // other users may use the same cloid
    resting_oid(&h.order1(addr(BOB), OrderRequest { cloid: c, ..limit(1, true, "1100", "0.2", Tif::Gtc) }));
}

#[test]
fn check_tx_admission() {
    let h = setup();
    let a = addr(ALICE);
    let app: &dyn Application = &h.chain;
    let good = user_tx(a, &Action::Noop, h.time + 10);
    assert_eq!(app.check_tx(&good), Ok(()));
    assert!(h.chain.sig_cache().len() >= 1);
    let Tx::User(mut u) = good.clone() else { unreachable!() };
    u.signature.v = 1;
    assert_eq!(app.check_tx(&Tx::User(u)), Err(AdmissionError::BadSignature));
    let v = validator_tx(addr(1000), 1, ValidatorAction::OraclePrices { prices: vec![] });
    assert_eq!(app.check_tx(&v), Ok(()));
    let v = validator_tx(addr(9), 1, ValidatorAction::OraclePrices { prices: vec![] });
    assert_eq!(app.check_tx(&v), Err(AdmissionError::UnknownSigner(addr(9))));
}

#[test]
fn signature_cache_is_used_by_execution() {
    let mut h = setup();
    let tx = user_tx(addr(ALICE), &Action::Noop, h.time + 10);
    h.chain.check_tx(&tx).unwrap();
    let before = h.chain.sig_cache().len();
    let o = h.block(vec![tx]);
    assert!(o.receipts[0].result.is_ok());
    assert_eq!(h.chain.sig_cache().len(), before - 1);
}
