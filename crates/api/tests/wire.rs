//! `/exchange` body parsing goes through hlwire; check the api wrapper keeps
//! hlwire's results and HL's error text for undeserializable bodies.
use serde_json::Value;

fn slow(text: &str) -> Result<interfaces::UserTx, String> {
    let v: Value = serde_json::from_str(text).map_err(|_| api::wire::DESERIALIZE_ERR.to_string())?;
    hlwire::parse_exchange_request(&v)
}

#[test]
fn sdk_vectors_match_value_path() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../hlwire/testdata");
    let mut n = 0;
    for f in ["python_vectors.json", "ts_vectors.json"] {
        let Ok(t) = std::fs::read_to_string(format!("{dir}/{f}")) else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        for case in v.as_array().unwrap() {
            let body = case["body_raw"].as_str().unwrap();
            assert_eq!(api::wire::parse_exchange_bytes(body.as_bytes()), slow(body), "{}", case["name"]);
            n += 1;
        }
    }
    assert!(n >= 40, "only {n} vectors found");
}

#[test]
fn errors() {
    for body in ["", "nul", "{nope", "[1,2]", r#"{"action":{"type":"noop"},"nonce":1}"#, r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x01","s":"0x02","v":27}} x"#] {
        let r = api::wire::parse_exchange_bytes(body.as_bytes());
        assert!(r.is_err(), "{body}");
        assert_eq!(r.is_err(), slow(body).is_err());
    }
    assert_eq!(api::wire::parse_exchange_bytes(b"{nope").unwrap_err(), api::wire::DESERIALIZE_ERR);
    assert_eq!(api::wire::parse_exchange_bytes(b"").unwrap_err(), api::wire::DESERIALIZE_ERR);
    let ok = r#"{"action":{"type":"noop"},"nonce":1,"signature":{"r":"0x01","s":"0x02","v":27},"vaultAddress":null,"expiresAfter":null}"#;
    assert_eq!(api::wire::parse_exchange_bytes(ok.as_bytes()), slow(ok));
}
