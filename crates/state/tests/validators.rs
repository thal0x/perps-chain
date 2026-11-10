mod common;
use common::*;
use interfaces::*;
use secp256k1::SecretKey;
use state::crypto;

fn att(id: u64, user: Address, amount: &str) -> DepositAttestation {
    DepositAttestation { deposit_id: id, user, amount: usd(amount), eth_tx_hash: Hash32([id as u8; 32]) }
}

#[test]
fn oracle_prices_drive_mark() {
    let mut h = H::new(&[(addr(MM), "1000000")]);
    assert_eq!(h.chain.oracle_px(0), px("60000"));
    // below quorum (2 of 4): unchanged
    let v: Vec<Address> = (0..4).map(|i| addr(1000 + i)).collect();
    h.block(vec![
        validator_tx(v[0], 1, ValidatorAction::OraclePrices { prices: vec![(0, px("61000"))] }),
        validator_tx(v[1], 1, ValidatorAction::OraclePrices { prices: vec![(0, px("61000"))] }),
    ]);
    assert_eq!(h.chain.oracle_px(0), px("60000"));
    // a non-validator and a replayed nonce are ignored
    h.block(vec![
        validator_tx(addr(55), 1, ValidatorAction::OraclePrices { prices: vec![(0, px("1"))] }),
        validator_tx(v[0], 1, ValidatorAction::OraclePrices { prices: vec![(0, px("1"))] }),
        validator_tx(v[2], 1, ValidatorAction::OraclePrices { prices: vec![(0, px("62000"))] }),
    ]);
    assert_eq!(h.chain.oracle_px(0), px("61000"));
    assert_eq!(h.chain.mark_px(0), px("61000"));
    // mark = median(oracle, bid, ask)
    h.order(addr(MM), vec![limit(0, true, "60500", "0.1", Tif::Gtc), limit(0, false, "60800", "0.1", Tif::Gtc)]);
    assert_eq!(h.chain.mark_px(0), px("60800"));
    assert_eq!(h.chain.q().market_ctx(0).mark_px, px("60800"));
    assert_eq!(h.chain.q().mid(0), Some(px("60650")));
    // stale submissions stop counting: price stays
    for _ in 0..12 {
        h.block(vec![]);
    }
    assert_eq!(h.chain.oracle_px(0), px("61000"));
}

#[test]
fn deposit_quorum_credits_exactly_once() {
    let mut h = H::new(&[]);
    let u = addr(ALICE);
    let v: Vec<Address> = (0..4).map(|i| addr(1000 + i)).collect();
    let dep = |vi: usize, n: u64, a: DepositAttestation| validator_tx(v[vi], n, ValidatorAction::BridgeDeposit(a));
    h.block(vec![dep(0, 1, att(7, u, "1000")), dep(1, 1, att(7, u, "1000"))]);
    assert!(!h.chain.q().processed_deposit(7));
    assert_eq!(h.chain.q().account(&u).withdrawable, Usd::ZERO);
    // conflicting content from validator 2 does not count toward quorum
    h.block(vec![dep(2, 1, att(7, u, "9999"))]);
    assert!(!h.chain.q().processed_deposit(7));
    // duplicate attestation by validator 0 ignored
    h.block(vec![dep(0, 2, att(7, u, "1000"))]);
    assert!(!h.chain.q().processed_deposit(7));
    h.block(vec![dep(3, 1, att(7, u, "1000"))]);
    assert!(h.chain.q().processed_deposit(7));
    assert_eq!(h.chain.q().account(&u).withdrawable, usd("1000"));
    assert_eq!(h.chain.flows().deposits, usd("1000"));
    assert!(h.last_events().iter().any(|e| matches!(e, Event::Ledger { delta: LedgerDelta::Deposit { .. }, .. })));
    // late attestations do nothing
    h.block(vec![dep(2, 2, att(7, u, "1000")), dep(1, 2, att(7, u, "1000"))]);
    assert_eq!(h.chain.q().account(&u).withdrawable, usd("1000"));
    assert_eq!(h.chain.flows().deposits, usd("1000"));
}

#[test]
fn withdrawal_flow_with_real_validator_signatures() {
    let keys: Vec<SecretKey> = (1..=4u8).map(|i| SecretKey::from_byte_array(&[i; 32]).unwrap()).collect();
    let vaddr: Vec<Address> = keys.iter().map(crypto::address_of_secret).collect();
    let mut g = test_genesis(4, &[(addr(ALICE), usd("1000"))]);
    for (i, a) in vaddr.iter().enumerate() {
        g.validators.validators[i].address = *a;
    }
    let mut h = H::from_genesis(g);
    let u = addr(ALICE);
    let dest = addr(4242);
    let r = h.send_with_nonce(u, |n| Action::Withdraw3 { destination: dest, amount: usd("1"), time: n });
    assert!(r.is_err(), "amount must exceed the 1 USDC fee");
    let r = h.send_with_nonce(u, |n| Action::Withdraw3 { destination: dest, amount: usd("500"), time: n });
    assert_eq!(r, Ok(ActionResponse::Default));
    assert_eq!(h.chain.q().account(&u).withdrawable, usd("500"));
    assert_eq!(h.chain.flows().withdrawals, usd("500"));
    let pw = h.chain.q().pending_withdrawals();
    assert_eq!(pw.len(), 1);
    assert_eq!((pw[0].user, pw[0].destination, pw[0].amount), (u, dest, usd("499")));
    let id = pw[0].id;
    let digest = state::placeholder_withdrawal_digest(&pw[0], &h.chain.genesis_ref().bridge);
    let sig = |i: usize| crypto::sign_digest(&keys[i], &digest);
    // wrong signer's signature is rejected; good ones stored, sorted, deduped
    h.block(vec![
        validator_tx(vaddr[0], 1, ValidatorAction::WithdrawalSignature { withdrawal_id: id, signature: sig(1) }),
        validator_tx(vaddr[2], 1, ValidatorAction::WithdrawalSignature { withdrawal_id: id, signature: sig(2) }),
        validator_tx(vaddr[1], 1, ValidatorAction::WithdrawalSignature { withdrawal_id: id, signature: sig(1) }),
        validator_tx(vaddr[1], 2, ValidatorAction::WithdrawalSignature { withdrawal_id: id, signature: sig(1) }),
    ]);
    let pw = h.chain.q().pending_withdrawals();
    let signers: Vec<Address> = pw[0].signatures.iter().map(|s| s.0).collect();
    let mut expect = vec![vaddr[1], vaddr[2]];
    expect.sort();
    assert_eq!(signers, expect);
    for (a, s) in &pw[0].signatures {
        assert_eq!(crypto::ecrecover(&digest, s), Some(*a));
    }
    // finalization needs quorum attestations
    let fin = |i: usize, n: u64| validator_tx(vaddr[i], n, ValidatorAction::WithdrawalFinalized { withdrawal_id: id });
    h.block(vec![fin(0, 10), fin(1, 10)]);
    assert_eq!(h.chain.q().pending_withdrawals().len(), 1);
    h.block(vec![fin(2, 10)]);
    assert!(h.chain.q().pending_withdrawals().is_empty());
}
