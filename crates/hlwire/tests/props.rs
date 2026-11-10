//! Property tests: msgpack roundtrips, builder <-> decoder roundtrips,
//! sign/recover, request build/parse, fuzzed decoder input.

use hlwire::*;
use interfaces::*;
use proptest::prelude::*;
use rand::SeedableRng;
use serde_json::{Map, Value};

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<u64>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        (-1e12f64..1e12f64).prop_map(|f| serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)),
        ".{0,40}".prop_map(Value::String),
        "[a-z]{30,300}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 64, 20, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..20).prop_map(Value::Array),
            prop::collection::vec(("[a-zA-Z]{1,8}", inner), 0..20).prop_map(|kv| {
                let mut m = Map::new();
                for (k, v) in kv {
                    m.insert(k, v);
                }
                Value::Object(m)
            }),
        ]
    })
}

fn arb_px() -> impl Strategy<Value = Px> {
    (1u64..10_000_000_000_000).prop_map(Px)
}
fn arb_sz() -> impl Strategy<Value = Sz> {
    (0i64..10_000_000_000_000).prop_map(Sz)
}

fn arb_order() -> impl Strategy<Value = OrderRequest> {
    let kind = prop_oneof![
        prop_oneof![Just(Tif::Gtc), Just(Tif::Ioc), Just(Tif::Alo)]
            .prop_map(|tif| OrderKind::Limit { tif }),
        (arb_px(), any::<bool>(), any::<bool>()).prop_map(|(p, m, tp)| OrderKind::Trigger {
            trigger_px: p,
            is_market: m,
            tpsl: if tp { TpSl::Tp } else { TpSl::Sl }
        }),
    ];
    (
        0u32..200,
        any::<bool>(),
        arb_px(),
        arb_sz(),
        any::<bool>(),
        kind,
        proptest::option::of(any::<[u8; 16]>()),
    )
        .prop_map(
            |(asset, is_buy, limit_px, sz, reduce_only, kind, cloid)| OrderRequest {
                asset,
                is_buy,
                limit_px,
                sz,
                reduce_only,
                kind,
                cloid,
            },
        )
}

fn arb_oc() -> impl Strategy<Value = OidOrCloid> {
    prop_oneof![
        any::<u64>().prop_map(OidOrCloid::Oid),
        any::<[u8; 16]>().prop_map(OidOrCloid::Cloid)
    ]
}

fn arb_addr() -> impl Strategy<Value = Address> {
    any::<[u8; 20]>().prop_map(Address)
}

fn arb_action() -> impl Strategy<Value = Action> {
    let grouping = prop_oneof![
        Just(Grouping::Na),
        Just(Grouping::NormalTpsl),
        Just(Grouping::PositionTpsl)
    ];
    let amt = (0i64..i64::MAX / 2).prop_map(Usd);
    prop_oneof![
        (prop::collection::vec(arb_order(), 0..10), grouping)
            .prop_map(|(orders, grouping)| Action::Order { orders, grouping }),
        prop::collection::vec((any::<u32>(), any::<u64>()), 0..10)
            .prop_map(|cancels| Action::Cancel { cancels }),
        prop::collection::vec((any::<u32>(), any::<[u8; 16]>()), 0..10)
            .prop_map(|cancels| Action::CancelByCloid { cancels }),
        (arb_oc(), arb_order()).prop_map(|(oid, order)| Action::Modify { oid, order }),
        prop::collection::vec((arb_oc(), arb_order()), 0..5)
            .prop_map(|modifies| Action::BatchModify { modifies }),
        proptest::option::of(any::<u64>()).prop_map(|time| Action::ScheduleCancel { time }),
        (any::<u32>(), any::<bool>(), any::<u32>()).prop_map(|(asset, is_cross, leverage)| {
            Action::UpdateLeverage {
                asset,
                is_cross,
                leverage,
            }
        }),
        (any::<u32>(), any::<bool>(), any::<i64>()).prop_map(|(asset, is_buy, n)| {
            Action::UpdateIsolatedMargin {
                asset,
                is_buy,
                ntli: Usd(n),
            }
        }),
        (arb_addr(), amt.clone(), any::<u64>()).prop_map(|(destination, amount, time)| {
            Action::UsdSend {
                destination,
                amount,
                time,
            }
        }),
        (
            arb_addr(),
            "[A-Z]{3,5}:0x[0-9a-f]{32}",
            amt.clone(),
            any::<u64>()
        )
            .prop_map(|(destination, token, amount, time)| Action::SpotSend {
                destination,
                token,
                amount,
                time
            }),
        (amt.clone(), any::<bool>(), any::<u64>()).prop_map(|(amount, to_perp, nonce)| {
            Action::UsdClassTransfer {
                amount,
                to_perp,
                nonce,
            }
        }),
        (arb_addr(), amt, any::<u64>()).prop_map(|(destination, amount, time)| Action::Withdraw3 {
            destination,
            amount,
            time
        }),
        (
            arb_addr(),
            proptest::option::of("[a-z0-9]{1,16}"),
            any::<u64>()
        )
            .prop_map(|(agent_address, agent_name, nonce)| Action::ApproveAgent {
                agent_address,
                agent_name,
                nonce
            }),
        "[A-Z0-9]{1,20}".prop_map(|code| Action::SetReferrer { code }),
        Just(Action::Noop),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn msgpack_roundtrip(v in arb_json()) {
        let b = json_to_msgpack(&v).unwrap();
        let back = msgpack_to_json(&b).unwrap();
        prop_assert_eq!(json_to_msgpack(&back).unwrap(), b.clone());
        // through JSON text too (what the API does)
        let txt = serde_json::to_string(&v).unwrap();
        let reparsed: Value = serde_json::from_str(&txt).unwrap();
        prop_assert_eq!(json_to_msgpack(&reparsed).unwrap(), b);
    }

    #[test]
    fn action_builder_decoder_roundtrip(a in arb_action()) {
        let j = action_to_json(&a);
        let b = json_to_msgpack(&j).unwrap();
        prop_assert_eq!(decode_action(&b).unwrap(), a);
    }

    #[test]
    fn fast_decoder_matches_generic(a in arb_action(), idx in any::<usize>(), byte in any::<u8>(), cut in any::<usize>()) {
        let b = json_to_msgpack(&action_to_json(&a)).unwrap();
        let same = |b: &[u8]| -> Result<(), TestCaseError> {
            let f = decode_action(b);
            let g = decode_action_generic(b);
            match (&f, &g) {
                (Ok(x), Ok(y)) => prop_assert_eq!(x, y),
                (Err(_), Err(_)) => {}
                _ => prop_assert!(false, "fast={:?} generic={:?} bytes={}", f, g, hex::encode(b)),
            }
            Ok(())
        };
        same(&b)?;
        let mut m = b.clone();
        let i = idx % m.len();
        m[i] = byte;
        same(&m)?;
        let mut m2 = b.clone();
        m2[i] ^= 1 << (byte % 8);
        same(&m2)?;
        same(&b[..cut % (b.len() + 1)])?;
        let mut ext = b.clone();
        ext.push(byte);
        same(&ext)?;
    }

    #[test]
    fn fast_decoder_matches_generic_json(v in arb_json(), t in prop_oneof![Just("order"), Just("cancel"), Just("cancelByCloid"), Just("modify"), Just("batchModify")],
                                        k in prop_oneof![Just("orders"), Just("cancels"), Just("modifies"), Just("oid"), Just("order"), Just("grouping")],
                                        first in any::<bool>()) {
        // Random shapes under hot keys, with "type" first or last.
        let mut m = serde_json::Map::new();
        if first { m.insert("type".into(), Value::from(t)); }
        m.insert(k.into(), v);
        if !first { m.insert("type".into(), Value::from(t)); }
        let b = json_to_msgpack(&Value::Object(m)).unwrap();
        let f = decode_action(&b);
        let g = decode_action_generic(&b);
        prop_assert_eq!(f.is_ok(), g.is_ok(), "fast={:?} generic={:?}", f, g);
        if let (Ok(x), Ok(y)) = (f, g) { prop_assert_eq!(x, y); }
    }

    #[test]
    fn decoder_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let _ = decode_action(&bytes);
        let _ = msgpack_to_json(&bytes);
        let _ = action_type(&bytes);
    }

    #[test]
    fn decoder_never_panics_on_mutations(a in arb_action(), idx in any::<usize>(), byte in any::<u8>()) {
        let mut b = json_to_msgpack(&action_to_json(&a)).unwrap();
        if !b.is_empty() {
            let i = idx % b.len();
            b[i] = byte;
        }
        let _ = decode_action(&b);
        b.truncate(idx % (b.len() + 1));
        let _ = decode_action(&b);
    }

    #[test]
    fn sign_and_verify_l1(a in arb_action(), seed in any::<u64>(), nonce in any::<u64>(),
                          vault in proptest::option::of(arb_addr()), exp in proptest::option::of(any::<u64>()),
                          mainnet in any::<bool>()) {
        prop_assume!(!is_user_signed(&a));
        let w = Wallet::random(&mut rand_chacha::ChaCha20Rng::seed_from_u64(seed));
        let j = action_to_json(&a);
        let body = signed_l1_request(&w, j, nonce, vault, exp, mainnet);
        let txt = serde_json::to_string(&body).unwrap();
        let tx = parse_exchange_request(&serde_json::from_str(&txt).unwrap()).unwrap();
        let ver = HlVerifier { is_mainnet: mainnet };
        let act = ver.decode(&tx).unwrap();
        prop_assert_eq!(&act, &a);
        prop_assert_eq!(ver.recover_signer(&tx, &act).unwrap(), w.address());
        let other = HlVerifier { is_mainnet: !mainnet };
        prop_assert_ne!(other.recover_signer(&tx, &act).unwrap(), w.address());
    }

    #[test]
    fn sign_and_verify_user(a in arb_action(), seed in any::<u64>(), mainnet in any::<bool>()) {
        prop_assume!(is_user_signed(&a));
        let w = Wallet::random(&mut rand_chacha::ChaCha20Rng::seed_from_u64(seed));
        let body = signed_user_request(&w, action_to_json(&a), mainnet);
        let tx = parse_exchange_request(&body).unwrap();
        let ver = HlVerifier { is_mainnet: mainnet };
        let act = ver.decode(&tx).unwrap();
        prop_assert_eq!(&act, &a);
        prop_assert_eq!(ver.recover_signer(&tx, &act).unwrap(), w.address());
        let other = HlVerifier { is_mainnet: !mainnet };
        prop_assert!(other.recover_signer(&tx, &act).is_err());
    }

    #[test]
    fn decimal_wire_roundtrip(v in any::<i64>()) {
        let u = Usd(v);
        prop_assert_eq!(parse_usd(&u.to_wire()).unwrap(), u);
        let s = Sz(v.unsigned_abs() as i64 & i64::MAX);
        prop_assert_eq!(parse_sz(&s.to_wire()).unwrap(), s);
        let p = Px(v as u64);
        prop_assert_eq!(parse_px(&p.to_wire()).unwrap(), p);
    }

    #[test]
    fn fast_decimal_matches_interfaces(s in "[+-]?[0-9]{0,10}(\\.[0-9]{0,10})?", d in 0u32..10) {
        let ours = parse_wire_decimal(&s, d);
        let reference = interfaces::parse_decimal(&s, d);
        match (&ours, &reference) {
            (Ok(a), Ok(b)) => prop_assert_eq!(a, b),
            (Err(_), Err(_)) => {}
            _ => prop_assert!(false, "{s:?} d={d}: ours={ours:?} ref={reference:?}"),
        }
    }

    #[test]
    fn decimal_scientific(m in 1u64..1_000_000, e in -6i32..6) {
        // m * 10^e as "me<e>" equals the plain decimal
        let sci = format!("{m}e{e}");
        let raw = (m as i128) * 10i128.pow((6 + e) as u32);
        prop_assert_eq!(parse_wire_decimal(&sci, 6).unwrap(), raw);
    }

    #[test]
    fn signature_hex_without_leading_zeros(seed in any::<u64>(), d in any::<[u8; 32]>()) {
        let w = Wallet::random(&mut rand_chacha::ChaCha20Rng::seed_from_u64(seed));
        let sig = w.sign_digest(&d);
        let strip = |b: &[u8; 32]| {
            let h = hex::encode(b);
            let t = h.trim_start_matches('0');
            format!("0x{}", if t.is_empty() { "0" } else { t })
        };
        let j = serde_json::json!({"r": strip(&sig.r), "s": strip(&sig.s), "v": sig.v - 27});
        let p = parse_signature(&j).unwrap();
        prop_assert_eq!(p, sig);
        prop_assert_eq!(recover(&d, &p).unwrap(), w.address());
    }
}

#[test]
fn parse_exchange_request_rejects() {
    let w = Wallet::random(&mut rand_chacha::ChaCha20Rng::seed_from_u64(3));
    let good = signed_l1_request(&w, noop_action(), 5, None, None, false);
    assert!(parse_exchange_request(&good).is_ok());
    let mut cases = vec![];
    let mut b = good.clone();
    b["nonce"] = Value::from(-1);
    cases.push(b);
    let mut b = good.clone();
    b["signature"]["v"] = Value::from(30);
    cases.push(b);
    let mut b = good.clone();
    b["signature"]["r"] = Value::from("0x".to_string() + &"1".repeat(65));
    cases.push(b);
    let mut b = good.clone();
    b["vaultAddress"] = Value::from("0x1234");
    cases.push(b);
    let mut b = good.clone();
    b["expiresAfter"] = Value::from("5");
    cases.push(b);
    let mut b = good.clone();
    b["action"] = Value::from(5);
    cases.push(b);
    let mut b = good.clone();
    b["action"] = serde_json::json!({"x": 1});
    cases.push(b);
    let mut b = good.clone();
    b.as_object_mut().unwrap().remove("signature");
    cases.push(b);
    cases.push(Value::from("x"));
    for c in cases {
        assert!(parse_exchange_request(&c).is_err(), "{c}");
    }
    // null vault / expires accepted (Python SDK posts nulls)
    let mut b = good.clone();
    b["vaultAddress"] = Value::Null;
    b["expiresAfter"] = Value::Null;
    assert_eq!(
        parse_exchange_request(&b).unwrap(),
        parse_exchange_request(&good).unwrap()
    );
}

#[test]
fn user_signed_digest_errors() {
    use serde_json::json;
    assert!(user_signed_digest(&json!({"type":"order"})).is_err());
    assert!(user_signed_digest(&json!({"type":"usdSend","destination":"0x00","amount":"1","time":1,"hyperliquidChain":"Testnet"})).is_err());
    assert!(user_signed_digest(&json!({"type":"usdSend","signatureChainId":"0x66eee","hyperliquidChain":"Testnet","amount":"1","time":1})).is_err());
    assert!(user_signed_digest(&json!({"type":"usdSend","signatureChainId":"0x66eee","hyperliquidChain":"Testnet","destination":"x","amount":"1","time":"1"})).is_err());
    assert!(user_signed_digest(&json!({"type":"usdClassTransfer","signatureChainId":"0x66eee","hyperliquidChain":"Testnet","amount":"1","toPerp":1,"nonce":1})).is_err());
    assert!(user_signed_digest(&json!({"type":"approveAgent","signatureChainId":"0x66eee","hyperliquidChain":"Testnet","agentAddress":"0x1","nonce":1})).is_err());
    assert!(user_signed_digest(&json!({"type":"usdSend","signatureChainId":"zz","hyperliquidChain":"Testnet","destination":"x","amount":"1","time":1})).is_err());
    // agentName: absent == null == ""
    let base = json!({"type":"approveAgent","signatureChainId":"0x66eee","hyperliquidChain":"Testnet","agentAddress":"0x5e9ee1089755c3435139848e47e6635505d5a13a","nonce":1});
    let d0 = user_signed_digest(&base).unwrap();
    let mut b1 = base.clone();
    b1["agentName"] = Value::Null;
    let mut b2 = base.clone();
    b2["agentName"] = Value::from("");
    let mut b3 = base.clone();
    b3["agentName"] = Value::from("x");
    assert_eq!(user_signed_digest(&b1).unwrap(), d0);
    assert_eq!(user_signed_digest(&b2).unwrap(), d0);
    assert_ne!(user_signed_digest(&b3).unwrap(), d0);
}

#[test]
fn order_wire_checked() {
    let o = OrderRequest {
        asset: 0,
        is_buy: true,
        limit_px: Px::parse("3000.5").unwrap(),
        sz: Sz::parse("0.00001").unwrap(),
        reduce_only: false,
        kind: OrderKind::Limit { tif: Tif::Gtc },
        cloid: None,
    };
    assert!(order_to_wire_json_checked(&o, 5).is_ok());
    assert!(order_to_wire_json_checked(&o, 4).is_err());
    let mut o2 = o.clone();
    o2.limit_px = Px::parse("3000.55").unwrap();
    assert!(order_to_wire_json_checked(&o2, 5).is_err());
    assert_eq!(
        serde_json::to_string(&order_to_wire_json(&o)).unwrap(),
        r#"{"a":0,"b":true,"p":"3000.5","s":"0.00001","r":false,"t":{"limit":{"tif":"Gtc"}}}"#
    );
}

// ---------------------------------------------------------------- fast body

fn slow_body(b: &[u8]) -> Result<UserTx, String> {
    let v: Value = serde_json::from_slice(b).map_err(|e| format!("invalid JSON: {e}"))?;
    parse_exchange_request(&v)
}

fn check_body(b: &[u8]) -> Result<(), TestCaseError> {
    let slow = slow_body(b);
    if let Some(f) = parse_exchange_request_fast(b) {
        prop_assert_eq!(Ok(f), slow.clone(), "fast path accepted {}", String::from_utf8_lossy(b));
    }
    prop_assert_eq!(parse_exchange_request_bytes(b), slow);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn fast_body_matches_reference(a in arb_action(), seed in any::<u64>(), nonce in any::<u64>(),
                                   vault in proptest::option::of(arb_addr()), exp in proptest::option::of(any::<u64>()),
                                   pretty in any::<bool>(), idx in any::<usize>(), byte in any::<u8>()) {
        let w = Wallet::random(&mut rand_chacha::ChaCha20Rng::seed_from_u64(seed));
        let body = if is_user_signed(&a) {
            signed_user_request(&w, action_to_json(&a), false)
        } else {
            signed_l1_request(&w, action_to_json(&a), nonce, vault, exp, false)
        };
        let txt = if pretty { serde_json::to_string_pretty(&body).unwrap() } else { serde_json::to_string(&body).unwrap() };
        let b = txt.as_bytes();
        prop_assert!(parse_exchange_request_fast(b).is_some(), "fast path not taken: {}", txt);
        check_body(b)?;
        let mut m = b.to_vec();
        let i = idx % m.len();
        m[i] = byte;
        check_body(&m)?;
        let mut m2 = b.to_vec();
        m2.remove(i);
        check_body(&m2)?;
        let mut m3 = b.to_vec();
        m3.insert(i, byte);
        check_body(&m3)?;
    }

    #[test]
    fn fast_body_random_json(v in arb_json(), extra in arb_json()) {
        let mut m = serde_json::Map::new();
        m.insert("action".into(), serde_json::json!({"type": "x", "v": v}));
        m.insert("nonce".into(), Value::from(5));
        m.insert("signature".into(), serde_json::json!({"r": "0x1", "s": "0x2", "v": 27, "x": extra.clone()}));
        m.insert("extra".into(), extra);
        let txt = serde_json::to_string(&Value::Object(m)).unwrap();
        check_body(txt.as_bytes())?;
    }
}

#[test]
fn fast_body_edge_cases() {
    let cases: &[&str] = &[
        r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#" {"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}} "#,
        r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27},}"#,
        r#"{"action":{"type":"noop"},"nonce":1,"nonce":2,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop","type":"x"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop","x":"\ud800"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop","x":"😀"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop","x":1.5},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop","x":-0},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop","x":18446744073709551616},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop","x":01},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop"},"nonce":1.0,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":0},"vaultAddress":null,"expiresAfter":null}"#,
        r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27},"vaultAddress":"0x1719884eb866cb12b2287399b15f7db5e7d775ea","expiresAfter":7}"#,
        r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":29}}"#,
        r#"{"action":{"type":5},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}} x"#,
        "{\"action\":{\"type\":\"no\nop\"},\"nonce\":1,\"signature\":{\"r\":\"0x1\",\"s\":\"0x2\",\"v\":27}}",
        r#"{"action":{"type":"noop","a":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17]},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        r#"{"action":{"type":"noop","x":"é\"\\\/\b\f\n\r\t","y":"é"},"nonce":1,"signature":{"r":"0x1","s":"0x2","v":27}}"#,
        "{}",
        "[]",
        "",
    ];
    for c in cases {
        let b = c.as_bytes();
        let slow = slow_body(b);
        if let Some(f) = parse_exchange_request_fast(b) {
            assert_eq!(Ok(f), slow, "{c}");
        }
        assert_eq!(parse_exchange_request_bytes(b), slow, "{c}");
    }
    // the common shapes take the fast path
    for i in [0, 1, 9, 13, 14, 19, 20] {
        assert!(parse_exchange_request_fast(cases[i].as_bytes()).is_some(), "{}", cases[i]);
    }
}
