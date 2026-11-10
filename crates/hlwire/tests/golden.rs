//! Golden vectors produced by the official Python SDK and the TS SDK
//! (`testdata/gen_python_vectors.py`, `testdata/gen_ts_vectors.mjs`).

use hlwire::*;
use interfaces::*;
use serde_json::Value;

const USER_KEY: &str = "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
const AGENT_KEY: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

fn load(name: &str) -> Vec<Value> {
    let p = format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"));
    let s = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
    let v: Value = serde_json::from_str(&s).unwrap();
    v.as_array().unwrap().clone()
}

fn h32(s: &str) -> [u8; 32] {
    let mut o = [0u8; 32];
    hex::decode_to_slice(s.trim_start_matches("0x"), &mut o).unwrap();
    o
}

fn wallet_for(signer: &str) -> Wallet {
    for k in [USER_KEY, AGENT_KEY] {
        let w = Wallet::from_secret_hex(k).unwrap();
        if w.address().to_hex() == signer {
            return w;
        }
    }
    panic!("unknown signer {signer}")
}

/// Vectors whose action must be rejected by `decode_action` (still valid
/// signatures).
fn expect_decode_err(name: &str) -> bool {
    name.contains("suffix")
}

struct Stats {
    n: usize,
    l1: usize,
    user: usize,
}

fn check_vectors(file: &str, sdk: &str) -> Stats {
    let vs = load(file);
    assert!(vs.len() > 30, "{file}: too few vectors");
    let mut st = Stats { n: 0, l1: 0, user: 0 };
    for v in &vs {
        let name = v["name"].as_str().unwrap();
        let ctx = format!("{sdk}/{name}");
        assert_eq!(v["sdk"], sdk);
        let is_mainnet = v["is_mainnet"].as_bool().unwrap();
        let signer = v["signer"].as_str().unwrap();
        // Parse the exact posted body text (key order preserved).
        let body: Value = serde_json::from_str(v["body_raw"].as_str().unwrap()).unwrap();
        let action = &body["action"];

        // 1. msgpack byte-identical to the SDK's encoder.
        let mp = json_to_msgpack(action).unwrap();
        assert_eq!(hex::encode(&mp), v["msgpack_hex"].as_str().unwrap(), "{ctx}: msgpack");
        // 2. msgpack -> json -> msgpack roundtrip.
        let back = msgpack_to_json(&mp).unwrap();
        assert_eq!(&back, action, "{ctx}: msgpack_to_json");
        assert_eq!(json_to_msgpack(&back).unwrap(), mp, "{ctx}: re-encode");
        assert_eq!(action_type(&mp).unwrap(), action["type"].as_str().unwrap());

        // 3. request parsing.
        let tx = parse_exchange_request(&body).unwrap_or_else(|e| panic!("{ctx}: parse: {e}"));
        assert_eq!(tx.action_bytes, mp);
        assert_eq!(tx.nonce, body["nonce"].as_u64().unwrap());
        assert_eq!(tx.vault_address.map(|a| a.to_hex()), body["vaultAddress"].as_str().map(|s| s.to_lowercase()));
        assert_eq!(tx.expires_after, body["expiresAfter"].as_u64());

        // 4. hashes / digests.
        let digest = h32(v["digest"].as_str().unwrap());
        let kind = v["kind"].as_str().unwrap();
        match kind {
            "l1" => {
                st.l1 += 1;
                let ah = l1_action_hash(&mp, tx.nonce, tx.vault_address, tx.expires_after);
                assert_eq!(ah, h32(v["action_hash"].as_str().unwrap()), "{ctx}: action hash");
                assert_eq!(l1_signing_digest(ah, is_mainnet), digest, "{ctx}: l1 digest");
                assert_ne!(l1_signing_digest(ah, !is_mainnet), digest);
            }
            "user" => {
                st.user += 1;
                assert_eq!(user_signed_digest(action).unwrap(), digest, "{ctx}: user digest");
            }
            k => panic!("{ctx}: kind {k}"),
        }

        // 5. recovery.
        assert_eq!(recover(&digest, &tx.signature).unwrap().to_hex(), signer, "{ctx}: recover");
        assert_eq!(recover_signer(&tx, is_mainnet).unwrap().to_hex(), signer, "{ctx}: recover_signer");
        // wrong chain must not recover the signer
        assert!(recover_signer(&tx, !is_mainnet).map(|a| a.to_hex() != signer).unwrap_or(true), "{ctx}: cross-chain");

        // 6. our signer reproduces the SDK signature (RFC 6979).
        let w = wallet_for(signer);
        assert_eq!(w.sign_digest(&digest), tx.signature, "{ctx}: sign_digest");
        if kind == "l1" {
            let sig = sign_l1_action(&w, action, tx.nonce, tx.vault_address, tx.expires_after, is_mainnet);
            assert_eq!(sig, tx.signature, "{ctx}: sign_l1_action");
        } else if action["signatureChainId"] == DEFAULT_SIGNATURE_CHAIN_ID {
            let mut a2 = action.clone();
            let sig = sign_user_signed_action(&w, &mut a2, is_mainnet);
            assert_eq!(sig, tx.signature, "{ctx}: sign_user_signed_action");
            assert_eq!(json_to_msgpack(&a2).unwrap(), mp, "{ctx}: sign_user_signed_action must not reorder");
        }

        // 7. build_exchange_request -> parse is identity on the tx.
        let rebuilt = build_exchange_request(action.clone(), tx.nonce, tx.signature, tx.vault_address, tx.expires_after);
        assert_eq!(parse_exchange_request(&rebuilt).unwrap(), tx, "{ctx}: build/parse");

        // 8. typed decode + verifier.
        let ver = HlVerifier { is_mainnet };
        match ver.decode(&tx) {
            Err(e) => assert!(expect_decode_err(name), "{ctx}: decode failed: {e}"),
            Ok(a) => {
                assert!(!expect_decode_err(name), "{ctx}: decode should fail");
                assert_eq!(is_user_signed(&a), kind == "user", "{ctx}");
                assert_eq!(ver.recover_signer(&tx, &a).unwrap().to_hex(), signer, "{ctx}: verifier");
                // typed roundtrip through the SDK-shaped builders
                let rebuilt = action_to_json(&a);
                assert_eq!(decode_action(&json_to_msgpack(&rebuilt).unwrap()).unwrap(), a, "{ctx}: builder roundtrip");
                // Python L1 actions: builders reproduce the exact bytes
                // (except the builder-fee field we drop, and "modify",
                // which Python never sends).
                // Uppercase cloids are lowercased by our builders.
                if sdk == "python" && kind == "l1" && action.get("builder").is_none() && !name.contains("upper") {
                    assert_eq!(json_to_msgpack(&rebuilt).unwrap(), mp, "{ctx}: builder bytes");
                }
                // Python user-signed: builder + sign reproduces key order and signature
                // when amounts are already in minimal form.
                if sdk == "python" && kind == "user" {
                    let mut b = rebuilt.clone();
                    let sig = sign_user_signed_action(&w, &mut b, is_mainnet);
                    let keys_b: Vec<_> = b.as_object().unwrap().keys().cloned().collect();
                    let keys_a: Vec<_> = action.as_object().unwrap().keys().cloned().collect();
                    assert_eq!(keys_b, keys_a, "{ctx}: user-signed key order");
                    if b == *action {
                        assert_eq!(sig, tx.signature, "{ctx}: rebuilt user-signed sig");
                    }
                }
            }
        }

        // 9. tampering breaks recovery.
        let mut bad = tx.clone();
        bad.nonce += 1;
        assert!(recover_signer(&bad, is_mainnet).map(|a| a.to_hex() != signer).unwrap_or(true), "{ctx}: tamper");
        st.n += 1;
    }
    st
}

#[test]
fn python_sdk_vectors() {
    let st = check_vectors("python_vectors.json", "python");
    assert!(st.l1 >= 30 && st.user >= 10, "l1={} user={}", st.l1, st.user);
    eprintln!("python vectors: {} ({} l1, {} user)", st.n, st.l1, st.user);
}

#[test]
fn ts_sdk_vectors() {
    let st = check_vectors("ts_vectors.json", "ts");
    assert!(st.l1 >= 30 && st.user >= 10, "l1={} user={}", st.l1, st.user);
    eprintln!("ts vectors: {} ({} l1, {} user)", st.n, st.l1, st.user);
}

fn find(file: &str, name: &str) -> Value {
    let v = load(file).into_iter().find(|v| v["name"] == name).unwrap_or_else(|| panic!("{name}"));
    serde_json::from_str(v["body_raw"].as_str().unwrap()).unwrap()
}

fn decode_body(body: &Value) -> Action {
    decode_action(&json_to_msgpack(&body["action"]).unwrap()).unwrap()
}

#[test]
fn typed_spot_checks() {
    let user = Wallet::from_secret_hex(USER_KEY).unwrap().address();
    let dest = parse_address("0x5e9ee1089755c3435139848e47e6635505d5a13a").unwrap();
    for f in ["python_vectors.json", "ts_vectors.json"] {
        match decode_body(&find(f, "order_trigger_sl_market")) {
            Action::Order { orders, grouping } => {
                assert_eq!(grouping, Grouping::Na);
                let o = &orders[0];
                assert_eq!((o.asset, o.is_buy, o.reduce_only), (0, false, true));
                assert_eq!(o.limit_px, Px::parse("29000").unwrap());
                assert_eq!(o.sz, Sz::parse("0.5").unwrap());
                assert_eq!(o.kind, OrderKind::Trigger { trigger_px: Px::parse("29500").unwrap(), is_market: true, tpsl: TpSl::Sl });
            }
            a => panic!("{a:?}"),
        }
        match decode_body(&find(f, "bulk_orders_normal_tpsl")) {
            Action::Order { orders, grouping } => {
                assert_eq!(grouping, Grouping::NormalTpsl);
                assert_eq!(orders.len(), 3);
            }
            a => panic!("{a:?}"),
        }
        match decode_body(&find(f, "bulk_orders_position_tpsl")) {
            Action::Order { orders, grouping } => {
                assert_eq!(grouping, Grouping::PositionTpsl);
                assert_eq!(orders[0].sz, Sz(0));
            }
            a => panic!("{a:?}"),
        }
        match decode_body(&find(f, "bulk_orders_50")) {
            Action::Order { orders, .. } => {
                assert_eq!(orders.len(), 50);
                assert_eq!(orders[49].cloid, Some(cloid_from_u128(1049)));
                assert_eq!(orders[0].kind, OrderKind::Limit { tif: Tif::Alo });
            }
            a => panic!("{a:?}"),
        }
        assert!(matches!(decode_body(&find(f, "order_cloid_upper")), Action::Order { orders, .. } if orders[0].cloid == Some(cloid_from_u128(0xabcdef))));
        assert_eq!(decode_body(&find(f, "update_isolated_margin_remove")), match f {
            "python_vectors.json" => Action::UpdateIsolatedMargin { asset: 1, is_buy: true, ntli: Usd(-3_000_250_000) },
            _ => Action::UpdateIsolatedMargin { asset: 1, is_buy: false, ntli: Usd(-3_000_250_000) },
        });
        assert_eq!(decode_body(&find(f, "schedule_cancel_none")), Action::ScheduleCancel { time: None });
        assert_eq!(decode_body(&find(f, "schedule_cancel_time")), Action::ScheduleCancel { time: Some(1_700_000_100_000) });
        assert_eq!(decode_body(&find(f, "update_leverage_cross")), Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 20 });
        assert_eq!(decode_body(&find(f, "set_referrer")), Action::SetReferrer { code: "ASDFASDF".into() });
        assert_eq!(decode_body(&find(f, "noop")), Action::Noop);
        match decode_body(&find(f, "usd_send")) {
            Action::UsdSend { destination, amount, time } => {
                assert_eq!(destination, dest);
                assert_eq!(amount, Usd(12_500_000));
                assert!(time > 1_700_000_000_000);
            }
            a => panic!("{a:?}"),
        }
        match decode_body(&find(f, "spot_send")) {
            Action::SpotSend { token, amount, .. } => {
                assert_eq!(token, "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2");
                assert_eq!(amount, Usd(1_250_000));
            }
            a => panic!("{a:?}"),
        }
        assert!(matches!(decode_body(&find(f, "withdraw3")), Action::Withdraw3 { amount: Usd(2_000_000), .. }));
        assert!(matches!(decode_body(&find(f, "usd_class_transfer_to_perp")), Action::UsdClassTransfer { amount: Usd(10_000_000), to_perp: true, .. }));
        assert!(matches!(decode_body(&find(f, "approve_agent_named")), Action::ApproveAgent { agent_name: Some(n), .. } if n == "bot1"));
        assert!(matches!(decode_body(&find(f, "approve_agent_unnamed")), Action::ApproveAgent { agent_name: None, .. }));
        match decode_body(&find(f, "batch_modify")) {
            Action::BatchModify { modifies } => {
                assert_eq!(modifies[0].0, OidOrCloid::Oid(1));
                // TS SDK quirk: `oid: v.union([UnsignedInteger, Cloid])` coerces a
                // small cloid hex string to a number (valibot toNumber).
                if f == "ts_vectors.json" {
                    assert_eq!(modifies[1].0, OidOrCloid::Oid(2));
                } else {
                    assert_eq!(modifies[1].0, OidOrCloid::Cloid(cloid_from_u128(2)));
                }
                assert_eq!(modifies[1].1.cloid, Some(cloid_from_u128(2)));
                assert_eq!(modifies[1].1.limit_px, Px::parse("3000.25").unwrap());
            }
            a => panic!("{a:?}"),
        }
        // agent signs for the user: recovered signer is the agent, not the user
        let b = find(f, "agent_order");
        let tx = parse_exchange_request(&b).unwrap();
        let agent = Wallet::from_secret_hex(AGENT_KEY).unwrap().address();
        assert_eq!(recover_signer(&tx, false).unwrap(), agent);
        assert_ne!(agent, user);
    }
    // TS-only shapes
    let ts = "ts_vectors.json";
    assert!(matches!(decode_body(&find(ts, "order_frontend_market")), Action::Order { orders, .. } if orders[0].kind == OrderKind::Limit { tif: Tif::Ioc }));
    assert!(matches!(decode_body(&find(ts, "order_grouping_priority")), Action::Order { grouping: Grouping::Na, .. }));
    assert_eq!(decode_body(&find(ts, "cancel_big_oid_fast")), Action::Cancel { cancels: vec![(0, 1), (2, (1u64 << 40) + 7)] });
    assert!(matches!(decode_body(&find(ts, "modify_by_cloid_allow")), Action::Modify { oid: OidOrCloid::Oid(5), .. }));
    assert_eq!(
        decode_body(&find(ts, "modify_by_big_cloid")),
        Action::Modify {
            oid: OidOrCloid::Cloid(parse_cloid("0xd4c3b2a1f0e9d8c7b6a5948372615049").unwrap()),
            order: OrderRequest {
                asset: 0,
                is_buy: true,
                limit_px: Px::parse("31000").unwrap(),
                sz: Sz::parse("0.03").unwrap(),
                reduce_only: false,
                kind: OrderKind::Limit { tif: Tif::Gtc },
                cloid: None
            }
        }
    );
    assert!(matches!(decode_body(&find(ts, "modify_by_oid")), Action::Modify { oid: OidOrCloid::Oid(77), .. }));
    assert!(matches!(decode_body(&find(ts, "usd_send_chain_66eee")), Action::UsdSend { amount: Usd(1), .. }));
}

#[test]
fn user_signed_policy_checks() {
    let b = find("python_vectors.json", "usd_send");
    let tx = parse_exchange_request(&b).unwrap();
    let signer = recover_signer(&tx, false).unwrap();
    // expiresAfter not allowed on user-signed actions
    let mut t2 = tx.clone();
    t2.expires_after = Some(1);
    assert!(recover_signer(&t2, false).is_err());
    // vault is not part of the user-signed payload
    let mut t3 = tx.clone();
    t3.vault_address = Some(Address([9; 20]));
    assert_eq!(recover_signer(&t3, false).unwrap(), signer);
    // signature v as 0/1
    let mut b2 = b.clone();
    let v = b2["signature"]["v"].as_u64().unwrap();
    b2["signature"]["v"] = Value::from(v - 27);
    assert_eq!(recover_signer(&parse_exchange_request(&b2).unwrap(), false).unwrap(), signer);
    // r/s without leading zeros (python vector usd_class_transfer_vault_suffix has a 63-digit s)
    let b4 = find("python_vectors.json", "usd_class_transfer_vault_suffix");
    assert_eq!(b4["signature"]["s"].as_str().unwrap().len(), 65);
    let tx4 = parse_exchange_request(&b4).unwrap();
    assert_eq!(recover_signer(&tx4, false).unwrap(), signer);
}
