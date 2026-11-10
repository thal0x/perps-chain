use interfaces::*;
use sim::agents::{Mode, PopParams, Population};
use sim::app::World;
use sim::gen::{generate, GenConfig};
use sim::log::*;
use sim::replay::{replay, InvariantEvery, ReplayOpts};
use sim::sign::{sign_all, SignCtx, Signer, Unsigned};

fn gen_bytes(cfg: &GenConfig, f: &sim::AppFactory) -> (Vec<u8>, Vec<(Height, Hash32)>, sim::gen::GenSummary) {
    let mut buf = Vec::new();
    let mut roots = Vec::new();
    let s = generate(cfg, f, &mut buf, |h, r| roots.push((h, r))).unwrap();
    (buf, roots, s)
}

#[test]
fn log_bytes_deterministic_null_app() {
    let cfg = GenConfig::small(11);
    let f = sim::null_factory();
    let (a, _, _) = gen_bytes(&cfg, &f);
    let (b, _, _) = gen_bytes(&cfg, &f);
    assert_eq!(a, b);
    let mut c2 = cfg.clone();
    c2.seed = 12;
    let (c, _, _) = gen_bytes(&c2, &f);
    assert_ne!(a, c);
}

#[test]
fn log_bytes_deterministic_across_thread_counts() {
    let cfg = GenConfig::small(5);
    let f = sim::state_factory();
    let p1 = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let p3 = rayon::ThreadPoolBuilder::new().num_threads(3).build().unwrap();
    let (a, ra, _) = p1.install(|| gen_bytes(&cfg, &f));
    let (b, rb, _) = p3.install(|| gen_bytes(&cfg, &f));
    assert_eq!(a, b);
    assert_eq!(ra, rb);
}

#[test]
fn gen_then_replay_twice_with_state_identical_roots() {
    let mut cfg = GenConfig::small(42);
    cfg.actions = 8_000;
    cfg.sim_ms = 120_000;
    let f = sim::state_factory();
    let (bytes, gen_roots, s) = gen_bytes(&cfg, &f);
    assert_eq!(s.footer.blocks, 120);
    assert!(s.footer.actions >= 8_000, "{:?}", s.footer);
    assert!(s.fills > 0, "no fills: {:?}", s.results);
    let mut outs = Vec::new();
    for _ in 0..2 {
        let mut roots_txt = Vec::new();
        let r = replay(&bytes[..], &f, &ReplayOpts { invariants: InvariantEvery::Every, progress_ms: 0, max_violations: 0 }, Some(&mut roots_txt)).unwrap();
        assert!(r.violations.is_empty(), "{:?}", r.violations);
        assert_eq!(r.blocks, 120);
        assert_eq!(r.invariant_checks, 120);
        assert_eq!(r.final_root, s.final_root);
        assert_eq!(r.roots_digest, s.roots_digest);
        outs.push(roots_txt);
    }
    assert_eq!(outs[0], outs[1]);
    let expect: String = gen_roots.iter().map(|(h, r)| format!("{h} {}\n", r.to_hex())).collect();
    assert_eq!(String::from_utf8(outs[0].clone()).unwrap(), expect);
}

fn header() -> LogHeader {
    let (g, _) = sim::genesis(1, 4, 3, &sim::default_markets());
    LogHeader {
        genesis_hash: sim::genesis::genesis_hash(&g),
        seed: 1,
        params: GenParams {
            seed: 1,
            actions: 0,
            blocks: 0,
            block_ms: 1000,
            validators: 4,
            accounts: 3,
            invalid_ppm: 0,
            skewed_validators: 0,
            markets: vec![],
        },
        genesis: g,
        generator: "test".into(),
    }
}

fn rec(h: u64) -> BlockRecord {
    let txs = (0..(h % 5))
        .map(|i| {
            Tx::User(UserTx {
                action_bytes: vec![0x81, 0xa4, b't', b'y', b'p', b'e', 0xa4, b'n', b'o', b'o', b'p'],
                nonce: h * 10 + i,
                signature: Sig65 { r: [h as u8; 32], s: [i as u8; 32], v: 27 },
                vault_address: None,
                expires_after: None,
            })
        })
        .collect();
    BlockRecord { height: h, timestamp_ms: 1000 * h, proposer: Address([h as u8; 20]), txs }
}

#[test]
fn log_roundtrip_streaming() {
    let mut buf = Vec::new();
    let mut w = LogWriter::new(&mut buf, &header()).unwrap();
    for h in 1..=5000 {
        w.append(&rec(h)).unwrap();
    }
    let f = w.finish().unwrap();
    assert_eq!(f.blocks, 5000);
    let mut r = LogReader::new(&buf[..]).unwrap();
    assert_eq!(r.header(), &header());
    let mut n = 0;
    for b in r.by_ref() {
        n += 1;
        assert_eq!(b.unwrap(), rec(n));
    }
    assert_eq!(n, 5000);
    assert_eq!(r.footer(), Some(&f));
    // streaming: reading through a 1-byte-at-a-time reader works as well
    struct Slow<'a>(&'a [u8]);
    impl std::io::Read for Slow<'_> {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() || b.is_empty() {
                return Ok(0);
            }
            b[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }
    let r2 = LogReader::new(Slow(&buf)).unwrap();
    assert_eq!(r2.count(), 5000);
}

#[test]
fn log_rejects_truncation_and_gaps() {
    let mut buf = Vec::new();
    let mut w = LogWriter::new(&mut buf, &header()).unwrap();
    for h in 1..=100 {
        w.append(&rec(h)).unwrap();
    }
    assert!(w.append(&rec(102)).is_err());
    w.finish().unwrap();
    let cut = &buf[..buf.len() * 2 / 3];
    let res: Result<Vec<_>, _> = LogReader::new(cut).and_then(|r| r.collect());
    assert!(res.is_err());
    // no footer => error at the end
    let mut buf2 = Vec::new();
    {
        let mut w = LogWriter::new(&mut buf2, &header()).unwrap();
        w.append(&rec(1)).unwrap();
        drop(w);
    }
    let res: Result<Vec<_>, _> = LogReader::new(&buf2[..]).map(|r| r.collect()).unwrap_or_else(|e| Err(e));
    assert!(res.is_err());
}

/// Every generated order that is not deliberately invalid uses HL-valid
/// prices and sizes; every signature recovers to the expected address.
fn check_population(mode: Mode, feedback: bool) {
    let (g, keys) = sim::genesis(9, 4, 120, &sim::default_markets());
    let mut pop = Population::new(&g, &keys, PopParams { seed: 9, mode, invalid_ppm: 50_000, skewed_validators: 1, validators: true });
    let world = World::new(g.markets.len(), 120, feedback);
    let cx = SignCtx { keys: &keys, chain_id: g.chain_id, is_mainnet: false, want_body: true };
    let (mut orders, mut invalid, mut user_signed, mut validator) = (0, 0, 0, 0);
    for h in 1..=40u64 {
        let ts = g.genesis_time_ms + h * 1000;
        let us = pop.step(h, ts, 600, &world, &keys);
        let signed = sign_all(&cx, &us);
        assert_eq!(signed.len(), us.len());
        for (u, s) in us.iter().zip(&signed) {
            match (u, &s.tx) {
                (Unsigned::User { signer, action, invalid: bad, corruption, .. }, Tx::User(tx)) => {
                    // the posted body parses back to the same tx
                    let body: serde_json::Value = serde_json::from_slice(s.body.as_ref().unwrap()).unwrap();
                    assert_eq!(&hlwire::parse_exchange_request(&body).unwrap(), tx);
                    if *bad {
                        invalid += 1;
                    }
                    if *corruption == sim::sign::Corruption::None {
                        let want = match signer {
                            Signer::Account(i) => keys.accounts[*i].address(),
                            Signer::Agent(i) => keys.agents[*i].address(),
                            Signer::Stranger(k) => keys.stranger(*k).address(),
                        };
                        if !*bad || !hlwire::is_user_signed(action) {
                            assert_eq!(hlwire::recover_signer(tx, false).unwrap(), want);
                        }
                    }
                    if hlwire::is_user_signed(action) {
                        user_signed += 1;
                    }
                    assert_eq!(&hlwire::decode_action(&tx.action_bytes).unwrap(), action);
                    if *bad {
                        continue;
                    }
                    let mut check = |o: &OrderRequest| {
                        let m = &g.markets[o.asset as usize];
                        assert!(m.is_valid_px(o.limit_px), "{} px {}", m.name, o.limit_px);
                        if let OrderKind::Trigger { trigger_px, .. } = o.kind {
                            assert!(m.is_valid_px(trigger_px), "{} trigger {}", m.name, trigger_px);
                            if o.sz.0 == 0 {
                                return;
                            }
                        }
                        assert!(m.is_valid_sz(o.sz), "{} sz {}", m.name, o.sz);
                        orders += 1;
                    };
                    match action {
                        Action::Order { orders, .. } => orders.iter().for_each(&mut check),
                        Action::Modify { order, .. } => check(order),
                        Action::BatchModify { modifies } => modifies.iter().for_each(|(_, o)| check(o)),
                        _ => {}
                    }
                    assert!(action.order_action_count() <= 50);
                }
                (Unsigned::Validator { validator: v, .. }, Tx::Validator(tx)) => {
                    validator += 1;
                    assert_eq!(hlwire::recover_validator_tx(tx, g.chain_id).unwrap(), keys.validators[*v].address());
                }
                _ => panic!("kind mismatch"),
            }
        }
    }
    assert!(orders > 1000, "{orders}");
    assert!(invalid > 0);
    assert!(validator >= 160);
    if mode == Mode::Mixed {
        assert!(user_signed > 0);
    }
}

#[test]
fn population_valid_prices_sizes_and_signatures_mixed() {
    check_population(Mode::Mixed, false);
}

#[test]
fn population_valid_prices_sizes_and_signatures_bench() {
    check_population(Mode::Bench, false);
}

#[test]
fn population_deterministic() {
    let (g, keys) = sim::genesis(3, 4, 80, &sim::default_markets());
    let run = || {
        let mut pop = Population::new(&g, &keys, PopParams { seed: 3, mode: Mode::Mixed, invalid_ppm: 10_000, skewed_validators: 1, validators: true });
        let world = World::new(g.markets.len(), 80, false);
        (1..=30u64).flat_map(|h| pop.step(h, g.genesis_time_ms + h * 1000, 300, &world, &keys)).map(|u| format!("{u:?}")).collect::<Vec<_>>()
    };
    assert_eq!(run(), run());
}

#[test]
fn bench_batches_are_full() {
    let (g, keys) = sim::genesis(4, 1, 50, &sim::default_markets());
    let mut keys = keys;
    keys.kinds = (0..50).map(|i| if i < 40 { sim::agents::AgentKind::MarketMaker } else { sim::agents::AgentKind::Taker }).collect();
    let mut pop = Population::new(&g, &keys, PopParams { seed: 4, mode: Mode::Bench, invalid_ppm: 0, skewed_validators: 0, validators: false });
    let world = World::new(g.markets.len(), 50, false);
    let us: Vec<_> = (1..=10u64).flat_map(|h| pop.step(h, g.genesis_time_ms + h * 50, 5_000, &world, &keys)).collect();
    let total: u64 = us.iter().map(|u| u.order_actions()).sum();
    let batched: u64 = us.iter().map(|u| u.order_actions()).filter(|n| *n == 50).sum();
    assert!(total >= 50_000);
    assert!(batched * 100 / total >= 80, "{batched}/{total}");
}
