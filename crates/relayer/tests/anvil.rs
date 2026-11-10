//! Integration tests against a real Anvil + the Foundry-built contracts.
//! Each test spawns its own Anvil on a random port; skipped (with a message)
//! when `anvil`/`forge` are not on PATH.

mod common;

use common::*;
use interfaces::*;
use relayer::contract::{erc20_balance, update_validator_set_calldata, BridgeView};
use relayer::eip712::{domain_separator, secret_address, sign_digest, validator_set_update_digest, withdrawal_digest_raw};
use relayer::relay::{RelayOutcome, Relayer};
use relayer::txsign::TxSender;
use relayer::{recover_signer, withdrawal_digest, DepositWatcher, WatcherConfig};
use std::sync::Arc;
use std::time::Duration;

macro_rules! require_tools {
    () => {
        if !tools_available() {
            return;
        }
    };
}

fn addrs(keys: &[secp256k1::SecretKey]) -> Vec<Address> {
    keys.iter().map(secret_address).collect()
}

fn pw(id: u64, dest: Address, raw: i128) -> PendingWithdrawal {
    PendingWithdrawal {
        id,
        user: Address([0xee; 20]),
        destination: dest,
        amount: Usd::from_raw(raw),
        created_at: 1,
        signatures: vec![],
    }
}

fn sign_all(w: &mut PendingWithdrawal, keys: &[secp256k1::SecretKey], p: &BridgeParams) {
    let d = withdrawal_digest(w, p);
    for k in keys {
        w.signatures.push((secret_address(k), sign_digest(k, &d)));
    }
}

fn watcher_cfg(url: String) -> WatcherConfig {
    let mut c = WatcherConfig::new(url);
    c.confirmations = 1;
    c.resubmit_after = Duration::from_secs(3600);
    c
}

#[tokio::test]
async fn digest_matches_contract() {
    require_tools!();
    let anvil = Anvil::start().await;
    let rpc = anvil.rpc();
    let keys = validator_keys(4, 1);
    let d = deploy(&rpc, &addrs(&keys), &[1, 1, 1, 1], 1).await;
    let view = BridgeView::new(&rpc, d.bridge);
    let cases: [(u64, Address, u128); 5] = [
        (0, Address([1; 20]), 1),
        (7, Address::from_hex("0x70997970C51812dc3A010C7d01b50e0d17dc79C8").unwrap(), 499_000_000),
        (u64::MAX, Address([0xff; 20]), i64::MAX as u128),
        (123_456, Address([0x42; 20]), 5_000_000),
        (1 << 40, Address([0x80; 20]), 1 << 62),
    ];
    for (id, dest, amount) in cases {
        let on_chain = view.hash_withdrawal(id, &dest, amount).await.unwrap();
        let ours = withdrawal_digest_raw(id, &dest, amount, d.chain_id, &d.bridge);
        assert_eq!(on_chain, ours, "withdrawal digest mismatch for id {id}");
        let w = pw(id, dest, amount as i128);
        assert_eq!(withdrawal_digest(&w, &d.params()), ours);
    }
    // domain separator
    let out = rpc.call(&d.bridge, &relayer::abi::encode_call("domainSeparator()", &[]), None).await.unwrap();
    assert_eq!(&out[..32], &domain_separator(d.chain_id, &d.bridge));
    // validator set update digest
    let nv = addrs(&validator_keys(5, 2));
    let np = vec![1u128, 2, 3, 4, 5];
    assert_eq!(
        view.hash_validator_set_update(&nv, &np, 9).await.unwrap(),
        validator_set_update_digest(&nv, &np, 9, d.chain_id, &d.bridge)
    );
}

#[tokio::test]
async fn relayer_submits_withdrawal_at_threshold_and_rejects_replay() {
    require_tools!();
    let anvil = Anvil::start().await;
    let rpc = anvil.rpc();
    let keys = validator_keys(4, 3);
    let d = deploy(&rpc, &addrs(&keys), &[1, 1, 1, 1], 1).await;
    let p = d.params();
    let dest = Address([0x77; 20]);
    let mut relayer = Relayer::new(&anvil.url(), d.bridge, dev_key(9));
    relayer.retry_after = Duration::ZERO;

    // Two valid signatures: below quorum (3 of 4).
    let mut w = pw(5, dest, 499_000_000);
    sign_all(&mut w, &keys[..2], &p);
    // plus a non-validator and a duplicate
    let outsider = validator_keys(1, 99);
    sign_all(&mut w, &outsider, &p);
    w.signatures.push(w.signatures[0]);
    let r = relayer.process(std::slice::from_ref(&w)).await.unwrap();
    assert_eq!(r, vec![(5, RelayOutcome::BelowThreshold { have: 2, need: 3 })]);

    // Third validator signs (out of order) -> submitted.
    sign_all(&mut w, &keys[3..4], &p);
    w.signatures.reverse();
    let r = relayer.process(std::slice::from_ref(&w)).await.unwrap();
    assert!(matches!(r[0].1, RelayOutcome::Submitted { .. }), "{r:?}");
    assert_eq!(erc20_balance(&rpc, &d.usdc, &dest).await.unwrap(), 499_000_000);
    assert!(BridgeView::new(&rpc, d.bridge).used_withdrawal(5).await.unwrap());

    // Replay: relayer sees it used; a raw resubmission reverts.
    let r = relayer.process(std::slice::from_ref(&w)).await.unwrap();
    assert_eq!(r[0].1, RelayOutcome::AlreadyUsed);
    let mut sorted: Vec<_> = w.signatures.clone();
    sorted.sort_by_key(|(a, _)| *a);
    sorted.dedup_by_key(|(a, _)| *a);
    let sigs: Vec<Sig65> = sorted
        .iter()
        .filter(|(a, _)| addrs(&keys).contains(a))
        .map(|(_, s)| *s)
        .collect();
    let data = relayer::contract::withdraw_calldata(5, &dest, 499_000_000, &sigs);
    let tx = TxSender::new(&rpc, dev_key(8)).await.unwrap();
    let err = tx.send_and_wait(Some(&d.bridge), data).await.unwrap_err();
    assert!(format!("{err:#}").contains("WithdrawalAlreadyUsed"), "{err:#}");
    assert_eq!(erc20_balance(&rpc, &d.usdc, &dest).await.unwrap(), 499_000_000);
}

#[tokio::test]
async fn rotation_via_rust_calldata_retires_old_signers() {
    require_tools!();
    let anvil = Anvil::start().await;
    let rpc = anvil.rpc();
    let old = validator_keys(4, 4);
    let d = deploy(&rpc, &addrs(&old), &[1, 1, 1, 1], 1).await;
    let new = validator_keys(3, 5);
    let (nv, np) = (addrs(&new), vec![10u128, 10, 10]);
    let digest = validator_set_update_digest(&nv, &np, 2, d.chain_id, &d.bridge);
    let sigs: Vec<Sig65> = old[..3].iter().map(|k| sign_digest(k, &digest)).collect();
    let tx = TxSender::new(&rpc, dev_key(8)).await.unwrap();
    tx.send_and_wait(Some(&d.bridge), update_validator_set_calldata(&nv, &np, 2, &sigs)).await.unwrap();
    let view = BridgeView::new(&rpc, d.bridge);
    assert_eq!(view.epoch().await.unwrap(), 2);
    assert_eq!(view.validators().await.unwrap(), (nv.clone(), np.clone()));
    assert_eq!(view.quorum_power().await.unwrap(), 21);
    // epoch replay
    let err = tx.send_and_wait(Some(&d.bridge), update_validator_set_calldata(&nv, &np, 2, &sigs)).await.unwrap_err();
    assert!(format!("{err:#}").contains("StaleEpoch"), "{err:#}");

    // Fully signed by the old set: the relayer filters them out.
    let mut relayer = Relayer::new(&anvil.url(), d.bridge, dev_key(9));
    let mut w = pw(1, Address([0x55; 20]), 1_000_000);
    sign_all(&mut w, &old, &d.params());
    let r = relayer.process(std::slice::from_ref(&w)).await.unwrap();
    assert_eq!(r[0].1, RelayOutcome::BelowThreshold { have: 0, need: 21 });
    sign_all(&mut w, &new[..2], &d.params()); // 20 < 21
    let r = relayer.process(std::slice::from_ref(&w)).await.unwrap();
    assert_eq!(r[0].1, RelayOutcome::BelowThreshold { have: 20, need: 21 });
    sign_all(&mut w, &new[2..], &d.params());
    let r = relayer.process(std::slice::from_ref(&w)).await.unwrap();
    assert!(matches!(r[0].1, RelayOutcome::Submitted { .. }), "{r:?}");
}

#[tokio::test]
async fn watcher_attests_deposit_once_after_confirmations() {
    require_tools!();
    let anvil = Anvil::start().await;
    let rpc = anvil.rpc();
    let keys = validator_keys(4, 6);
    let d = deploy(&rpc, &addrs(&keys), &[1, 1, 1, 1], 1).await;
    let host = Arc::new(FakeHost::new(keys[0], d.params()));
    let mut watcher = DepositWatcher::new(host.clone(), watcher_cfg(anvil.url()));

    let txh = user_deposit(&rpc, &d, 1, 1_000_000_000).await;
    // deposit is in the head block: 0 confirmations -> not yet final
    let s = watcher.poll_once().await.unwrap();
    assert!(s.deposits_attested.is_empty(), "{s:?}");
    assert!(host.take_actions().is_empty());

    rpc.anvil_mine(1).await.unwrap();
    let s = watcher.poll_once().await.unwrap();
    assert_eq!(s.deposits_attested, vec![0]);
    let acts = host.take_actions();
    assert_eq!(
        acts,
        vec![ValidatorAction::BridgeDeposit(DepositAttestation {
            deposit_id: 0,
            user: secret_address(&dev_key(1)),
            amount: Usd::from_raw(1_000_000_000),
            eth_tx_hash: txh,
        })]
    );
    // further polls (more blocks) do not resubmit
    for _ in 0..3 {
        rpc.anvil_mine(1).await.unwrap();
        watcher.poll_once().await.unwrap();
    }
    assert!(host.take_actions().is_empty());

    // L1 processes it; a new deposit is attested, the old one never again.
    host.processed.lock().insert(0);
    user_deposit(&rpc, &d, 2, 5_000_000).await;
    rpc.anvil_mine(1).await.unwrap();
    let s = watcher.poll_once().await.unwrap();
    assert_eq!(s.deposits_attested, vec![1]);
    let acts = host.take_actions();
    assert!(matches!(&acts[..], [ValidatorAction::BridgeDeposit(a)] if a.deposit_id == 1 && a.amount == Usd::from_raw(5_000_000)));

    // Node restart: a fresh watcher rescans from block 0 and only attests unprocessed deposits.
    let mut restarted = DepositWatcher::new(host.clone(), watcher_cfg(anvil.url()));
    let s = restarted.poll_once().await.unwrap();
    assert_eq!(s.deposits_attested, vec![1]);
    host.processed.lock().insert(1);
    let mut restarted2 = DepositWatcher::new(host.clone(), watcher_cfg(anvil.url()));
    host.take_actions();
    let s = restarted2.poll_once().await.unwrap();
    assert!(s.deposits_attested.is_empty());
    assert!(host.take_actions().is_empty());
}

#[tokio::test]
async fn watcher_resubmits_unprocessed_deposit_after_timeout() {
    require_tools!();
    let anvil = Anvil::start().await;
    let rpc = anvil.rpc();
    let keys = validator_keys(4, 7);
    let d = deploy(&rpc, &addrs(&keys), &[1, 1, 1, 1], 1).await;
    let host = Arc::new(FakeHost::new(keys[0], d.params()));
    let mut cfg = watcher_cfg(anvil.url());
    cfg.confirmations = 0;
    cfg.resubmit_after = Duration::from_millis(300);
    let mut watcher = DepositWatcher::new(host.clone(), cfg);
    user_deposit(&rpc, &d, 1, 10_000_000).await;
    assert_eq!(watcher.poll_once().await.unwrap().deposits_attested, vec![0]);
    assert!(watcher.poll_once().await.unwrap().deposits_attested.is_empty());
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(watcher.poll_once().await.unwrap().deposits_attested, vec![0]);
    host.processed.lock().insert(0);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(watcher.poll_once().await.unwrap().deposits_attested.is_empty());
}

#[tokio::test]
async fn watcher_signs_withdrawals_once_and_reports_finalization() {
    require_tools!();
    let anvil = Anvil::start().await;
    let rpc = anvil.rpc();
    let keys = validator_keys(4, 8);
    let d = deploy(&rpc, &addrs(&keys), &[1, 1, 1, 1], 1).await;
    let p = d.params();
    let host = Arc::new(FakeHost::new(keys[2], p.clone()));
    let mut watcher = DepositWatcher::new(host.clone(), watcher_cfg(anvil.url()));
    let dest = Address([0x66; 20]);

    let w = pw(11, dest, 499_000_000);
    host.pending.lock().push(w.clone());
    let s = watcher.poll_once().await.unwrap();
    assert_eq!(s.withdrawals_signed, vec![11]);
    let acts = host.take_actions();
    let [ValidatorAction::WithdrawalSignature { withdrawal_id: 11, signature }] = acts[..] else {
        panic!("unexpected actions {acts:?}")
    };
    assert_eq!(recover_signer(&withdrawal_digest(&w, &p), &signature), Some(host.address()));
    // not signed again while pending (and not again once the L1 has our signature)
    watcher.poll_once().await.unwrap();
    host.pending.lock()[0].signatures.push((host.address(), signature));
    watcher.poll_once().await.unwrap();
    assert!(host.take_actions().is_empty());

    // Non-validator node does nothing.
    let mut nv_host = FakeHost::new(keys[3], p.clone());
    nv_host.is_validator = false;
    let nv_host = Arc::new(nv_host);
    nv_host.pending.lock().push(w.clone());
    let mut nv_watcher = DepositWatcher::new(nv_host.clone(), watcher_cfg(anvil.url()));
    assert_eq!(nv_watcher.poll_once().await.unwrap(), Default::default());
    assert!(nv_host.take_actions().is_empty());

    // Other validators sign; the relayer submits; the watcher sees Withdrawn.
    let mut full = host.pending.lock()[0].clone();
    sign_all(&mut full, &[keys[0], keys[1]], &p);
    *host.pending.lock() = vec![full.clone()];
    let mut relayer = Relayer::new(&anvil.url(), d.bridge, dev_key(9));
    let r = relayer.process(std::slice::from_ref(&full)).await.unwrap();
    assert!(matches!(r[0].1, RelayOutcome::Submitted { .. }), "{r:?}");
    let s = watcher.poll_once().await.unwrap();
    assert!(s.withdrawals_finalized.is_empty(), "needs 1 confirmation");
    rpc.anvil_mine(1).await.unwrap();
    let s = watcher.poll_once().await.unwrap();
    assert_eq!(s.withdrawals_finalized, vec![11]);
    assert_eq!(host.take_actions(), vec![ValidatorAction::WithdrawalFinalized { withdrawal_id: 11 }]);
    rpc.anvil_mine(1).await.unwrap();
    watcher.poll_once().await.unwrap();
    assert!(host.take_actions().is_empty());
    // L1 garbage-collects it
    host.pending.lock().clear();
    watcher.poll_once().await.unwrap();
    assert!(host.take_actions().is_empty());
}

#[tokio::test]
async fn watcher_survives_anvil_restart() {
    require_tools!();
    let mut anvil = Anvil::start().await;
    let keys = validator_keys(4, 9);
    let d = deploy(&anvil.rpc(), &addrs(&keys), &[1, 1, 1, 1], 1).await;
    let host = Arc::new(FakeHost::new(keys[1], d.params()));
    let mut cfg = watcher_cfg(anvil.url());
    cfg.confirmations = 0;
    let mut watcher = DepositWatcher::new(host.clone(), cfg);
    user_deposit(&anvil.rpc(), &d, 1, 100_000_000).await;
    assert_eq!(watcher.poll_once().await.unwrap().deposits_attested, vec![0]);
    host.processed.lock().insert(0);
    host.take_actions();

    // Anvil goes away: polls fail but do not panic.
    anvil.restart().await;
    // Fresh chain; redeploy (same deployer nonce => same addresses).
    let d2 = deploy(&anvil.rpc(), &addrs(&keys), &[1, 1, 1, 1], 1).await;
    assert_eq!(d2.bridge, d.bridge);
    // different amount => different block contents => different hashes
    user_deposit(&anvil.rpc(), &d2, 1, 50_000_000).await; // id 0 again (already processed on L1)
    user_deposit(&anvil.rpc(), &d2, 2, 7_000_000).await; // id 1
    let s = watcher.poll_once().await.unwrap();
    assert!(s.chain_reset, "{s:?}");
    assert_eq!(s.deposits_attested, vec![1]);
    let acts = host.take_actions();
    assert!(matches!(&acts[..], [ValidatorAction::BridgeDeposit(a)] if a.deposit_id == 1));
}

#[tokio::test]
async fn watcher_tolerates_rpc_down() {
    let keys = validator_keys(1, 10);
    let p = BridgeParams { eth_chain_id: 31337, contract: Address([1; 20]), min_deposit: Usd::from_raw(1) };
    let host = Arc::new(FakeHost::new(keys[0], p.clone()));
    let w = pw(3, Address([2; 20]), 10);
    host.pending.lock().push(w);
    // nothing listens on port 1
    let mut watcher = DepositWatcher::new(host.clone(), watcher_cfg("http://127.0.0.1:1".into()));
    assert!(watcher.poll_once().await.is_err());
    // withdrawal signing does not depend on the EVM RPC
    assert!(matches!(&host.take_actions()[..], [ValidatorAction::WithdrawalSignature { withdrawal_id: 3, .. }]));
}

#[tokio::test]
async fn api_client_fetches_pending_withdrawals() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut w = pw(4, Address([3; 20]), 499_000_000);
    w.signatures.push((Address([9; 20]), Sig65 { r: [1; 32], s: [2; 32], v: 27 }));
    let body = serde_json::json!([relayer::api::to_wire(&w)]).to_string();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let mut req = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
            let s = String::from_utf8_lossy(&req);
            if s.contains("pendingWithdrawals") || n == 0 {
                break;
            }
        }
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&req).to_string()
    });
    let api = relayer::api::ApiClient::new(format!("http://127.0.0.1:{port}"));
    let got = api.pending_withdrawals().await.unwrap();
    assert_eq!(got, vec![w]);
    let req = server.await.unwrap();
    assert!(req.starts_with("POST /info"), "{req}");
    assert!(req.contains(r#"{"type":"pendingWithdrawals"}"#), "{req}");
}
