//! Differential tests: the fast `/exchange` parser must agree exactly with
//! `hlwire::parse_exchange_request` (the authoritative path).
use proptest::prelude::*;
use serde_json::Value;

fn slow(text: &str) -> Option<interfaces::UserTx> {
    let v: Value = serde_json::from_str(text).ok()?;
    hlwire::parse_exchange_request(&v).ok()
}

fn check(text: &str) {
    let s = slow(text);
    if let Some(f) = api::wire::fast_parse(text.as_bytes()) {
        assert_eq!(Some(&f), s.as_ref(), "fast path disagrees on {text}");
    }
    // the public entry point (fast + fallback) always equals the slow path
    assert_eq!(api::wire::parse_exchange_bytes(text.as_bytes()).ok(), s, "{text}");
}

#[test]
fn sdk_vectors_take_fast_path_and_match() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../hlwire/testdata");
    let mut n = 0;
    for f in ["python_vectors.json", "ts_vectors.json"] {
        let Ok(t) = std::fs::read_to_string(format!("{dir}/{f}")) else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        for case in v.as_array().unwrap() {
            let body = case["body_raw"].as_str().unwrap();
            check(body);
            assert!(api::wire::fast_parse(body.as_bytes()).is_some(), "fast path rejected SDK body {}", case["name"]);
            n += 1;
        }
    }
    assert!(n >= 40, "only {n} vectors found");
}

#[test]
fn edge_cases() {
    let sig = r#""signature":{"r":"0x01","s":"0x02","v":27}"#;
    for action in [
        r#"{"type":"order","orders":[],"grouping":"na"}"#,
        r#"{"type":"x","a":{"b":{"c":[1,-1,-33,255,256,65536,4294967296,-129,-32769,-2147483649,18446744073709551615]}}}"#,
        r#"{"type":"x","f":1.5,"g":1e3,"h":-0.0,"i":1E-7}"#,
        r#"{"type":"x","s":"é\n\"q\"","t":"😀"}"#,
        r#"{"type":"x","dup":1,"dup":2}"#,
        r#"{"type":"x","big":18446744073709551616}"#,
        r#"{"type":"x","n":null,"t":true,"f":false}"#,
        r#"{"type":5}"#,
        r#"{"notype":1}"#,
        r#"[1,2]"#,
        r#"5"#,
        r#"{"type":"x","long":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
    ] {
        check(&format!(r#"{{"action":{action},"nonce":5,{sig}}}"#));
        check(&format!(r#"{{"action":{action},"nonce":5,{sig},"vaultAddress":null,"expiresAfter":null}}"#));
        check(&format!(r#"{{"nonce":5,{sig},"action":{action},"vaultAddress":"0x0000000000000000000000000000000000000001","expiresAfter":99}}"#));
    }
    let a = r#"{"type":"noop"}"#;
    for body in [
        format!(r#"{{"action":{a},"nonce":-1,{sig}}}"#),
        format!(r#"{{"action":{a},"nonce":1.5,{sig}}}"#),
        format!(r#"{{"action":{a},"nonce":1}}"#),
        format!(r#"{{"action":{a},"nonce":1,{sig},"vaultAddress":5}}"#),
        format!(r#"{{"action":{a},"nonce":1,{sig},"vaultAddress":"0x12"}}"#),
        format!(r#"{{"action":{a},"nonce":1,{sig},"extra":[1,2]}}"#),
        format!(r#"{{"action":{a},"nonce":1,"nonce":2,{sig}}}"#),
        format!(r#"{{"action":{a},"nonce":1,{sig}}} trailing"#),
        r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x01","s":"0x02","v":"x"}}"#.to_string(),
        "".to_string(),
        "nul".to_string(),
    ] {
        check(&body);
    }
}

fn json_leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|i| serde_json::json!(i)),
        any::<u64>().prop_map(|i| serde_json::json!(i)),
        (-1e12f64..1e12).prop_map(|f| serde_json::from_str::<Value>(&format!("{f:?}")).unwrap()),
        "[a-zA-Z0-9 _\\-\\.\"\\\\é😀]{0,40}".prop_map(Value::String),
    ]
}

fn json_value() -> impl Strategy<Value = Value> {
    json_leaf().prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..20).prop_map(Value::Array),
            prop::collection::vec(("[a-z]{1,6}", inner), 0..20).prop_map(|kv| Value::Object(kv.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn fast_equals_slow(fields in prop::collection::vec(("[a-z]{1,6}", json_value()), 0..10), nonce in any::<u64>(), ty in "[a-zA-Z]{1,12}") {
        let mut action = serde_json::Map::new();
        action.insert("type".into(), Value::String(ty));
        for (k, v) in fields {
            action.insert(k, v);
        }
        let body = serde_json::json!({"action": Value::Object(action), "nonce": nonce, "signature": {"r": "0x0a", "s": "0x0b", "v": 28}});
        let text = body.to_string();
        let f = api::wire::fast_parse(text.as_bytes());
        prop_assert!(f.is_some(), "fast path rejected {}", text);
        prop_assert_eq!(f, slow(&text));
        // also pretty-printed (whitespace) input
        let pretty = serde_json::to_string_pretty(&body).unwrap();
        prop_assert_eq!(api::wire::fast_parse(pretty.as_bytes()), slow(&pretty));
    }
}
