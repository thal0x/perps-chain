use interfaces::*;
use oracle::feeds::*;
use oracle::OracleState;
use proptest::prelude::*;

fn spec(asset: AssetId, name: &str, sz_decimals: u32, initial: &str) -> MarketSpec {
    MarketSpec {
        asset,
        name: name.into(),
        sz_decimals,
        max_leverage: 20,
        only_isolated: false,
        initial_px: Px::parse(initial).unwrap(),
    }
}

fn markets() -> Vec<MarketSpec> {
    vec![spec(0, "BTC", 5, "65000"), spec(1, "ETH", 4, "3200.5"), spec(2, "DOGE", 0, "0.12345")]
}

const T0: u64 = 1_700_000_000_000;

#[test]
fn starts_at_initial_px() {
    let mut f = SyntheticFeed::new(&markets(), SynthParams { noise: Rate::ZERO, ..Default::default() }, T0, 7, 1);
    let p = f.prices(T0 + 999);
    assert_eq!(p, vec![(0, Px::parse("65000").unwrap()), (1, Px::parse("3200.5").unwrap()), (2, Px::parse("0.12345").unwrap())]);
    // before start: bucket 0 as well
    assert_eq!(f.prices(T0 - 5_000), p);
}

#[test]
fn reference_is_common_and_independent_of_call_pattern() {
    let ms = markets();
    let mut a = SyntheticFeed::new(&ms, SynthParams::default(), T0, 42, 1);
    let mut b = SyntheticFeed::new(&ms, SynthParams::default(), T0, 42, 2);
    // a is polled every second, b only occasionally and out of order
    let mut ref_a = vec![];
    for s in 0..3_600u64 {
        let now = T0 + s * 1000 + 17;
        a.prices(now);
        if s % 600 == 599 {
            ref_a.push((now, a.reference_px(0, now).unwrap(), a.reference_px(2, now).unwrap()));
        }
    }
    for (now, r0, r2) in ref_a.iter().rev() {
        assert_eq!(b.reference_px(0, *now), Some(*r0));
        assert_eq!(b.reference_px(2, *now), Some(*r2));
    }
    // a different common seed gives a different path
    let mut c = SyntheticFeed::new(&ms, SynthParams::default(), T0, 43, 1);
    let (now, r0, _) = ref_a[5];
    assert_ne!(c.reference_px(0, now), Some(r0));
    // the path actually moves
    assert_ne!(r0, ms[0].initial_px);
    assert_eq!(a.reference_px(9, now), None);
}

#[test]
fn same_validator_seed_identical_outputs() {
    let ms = markets();
    let mut a = SyntheticFeed::new(&ms, SynthParams::default(), T0, 5, 9);
    let mut b = SyntheticFeed::new(&ms, SynthParams::default(), T0, 5, 9);
    for s in (0..2000u64).step_by(37) {
        assert_eq!(a.prices(T0 + s * 1000), b.prices(T0 + s * 1000));
    }
}

#[test]
fn noise_is_small_and_validator_specific() {
    let ms = markets();
    let params = SynthParams { round_to_valid_px: false, ..Default::default() };
    let noise = params.noise.0 as i128;
    let mut feeds: Vec<_> = (0..4).map(|v| SyntheticFeed::new(&ms, params.clone(), T0, 11, 100 + v)).collect();
    let mut differ = false;
    for s in (0..5000u64).step_by(101) {
        let now = T0 + s * 1000;
        let outs: Vec<_> = feeds.iter_mut().map(|f| f.prices(now)).collect();
        let r = feeds[0].reference_px(0, now).unwrap().0 as i128;
        for o in &outs {
            let p = o[0].1 .0 as i128;
            // |p - r| <= r * noise + 1
            assert!((p - r).abs() * RATE_SCALE <= r * noise + RATE_SCALE, "noise too large");
        }
        if outs[0] != outs[1] {
            differ = true;
        }
    }
    assert!(differ);
}

#[test]
fn skew_fixed_and_growing() {
    let ms = markets();
    let params = SynthParams { noise: Rate::ZERO, round_to_valid_px: false, ..Default::default() };
    let five_pct = Rate::from_raw(50_000_000_000);
    let mut honest = SyntheticFeed::new(&ms, params.clone(), T0, 3, 1);
    let mut adv = SyntheticFeed::new(&ms, params.clone(), T0, 3, 2).with_skew(Skew::Fixed(five_pct));
    let now = T0 + 100_000;
    let h = honest.prices(now)[0].1 .0 as i128;
    let a = adv.prices(now)[0].1 .0 as i128;
    assert_eq!(a, h * 105 / 100);

    let g = Skew::Growing { per_sec: Rate::from_raw(1_000_000_000), max: five_pct, start_ms: T0 };
    assert_eq!(g.bias(T0), Rate::ZERO);
    assert_eq!(g.bias(T0 + 10_000), Rate::from_raw(10_000_000_000));
    assert_eq!(g.bias(T0 + 1_000_000), five_pct);
    let gneg = Skew::Growing { per_sec: Rate::from_raw(-1_000_000_000), max: five_pct, start_ms: T0 };
    assert_eq!(gneg.bias(T0 + 1_000_000), Rate::from_raw(-50_000_000_000));
    assert_eq!(Skew::None.bias(now), Rate::ZERO);
}

#[test]
fn outputs_are_valid_hl_prices() {
    let ms = markets();
    let mut f = SyntheticFeed::new(&ms, SynthParams::default(), T0, 77, 1);
    for s in (0..86_400u64).step_by(97) {
        for (a, p) in f.prices(T0 + s * 1000) {
            assert!(ms[a as usize].is_valid_px(p), "invalid px {} for {}", p.to_wire(), ms[a as usize].name);
        }
    }
}

#[test]
fn week_long_walk_stays_sane() {
    // ~1 week of 1 s buckets: reference stays within a broad band around
    // the initial price thanks to mean reversion and clamps.
    let ms = markets();
    let mut f = SyntheticFeed::new(&ms, SynthParams::default(), T0, 1234, 1);
    let mut lo = u64::MAX;
    let mut hi = 0;
    for h in 0..168u64 {
        let p = f.reference_px(0, T0 + h * 3_600_000).unwrap().0;
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let init = ms[0].initial_px.0;
    assert!(lo > init / 10 && hi < init * 10, "lo {lo} hi {hi}");
    assert!(lo < hi);
}

#[test]
fn feeds_drive_oracle_with_adversary() {
    // 4 equal validators, one skewed +50%: aggregate stays within honest range.
    let ms = markets();
    let mut feeds: Vec<_> = (0..4)
        .map(|v| {
            let f = SyntheticFeed::new(&ms, SynthParams::default(), T0, 99, v);
            if v == 3 {
                f.with_skew(Skew::Fixed(Rate::from_raw(500_000_000_000)))
            } else {
                f
            }
        })
        .collect();
    let mut o = OracleState::new(10_000);
    let addrs: Vec<Address> = (0..4u8).map(|i| Address([i; 20])).collect();
    for s in 0..300u64 {
        let now = T0 + s * 1000;
        let mut honest: Vec<Vec<(AssetId, Px)>> = vec![];
        for (v, f) in feeds.iter_mut().enumerate() {
            let p = f.prices(now);
            interfaces::OracleAggregator::submit(&mut o, addrs[v], 1, &p, now);
            if v != 3 {
                honest.push(p);
            }
        }
        for a in 0..3u32 {
            let m = interfaces::OracleAggregator::aggregate(&o, a, now, 3).unwrap();
            let hp: Vec<Px> = honest.iter().map(|p| p[a as usize].1).collect();
            assert!(*hp.iter().min().unwrap() <= m && m <= *hp.iter().max().unwrap());
        }
    }
}

#[test]
fn fixed_feed_shared_updates() {
    let mut f = FixedFeed::from_markets(&markets());
    let h = f.clone();
    assert_eq!(f.prices(0).len(), 3);
    h.set(1, Px::parse("3000").unwrap());
    h.remove(2);
    assert_eq!(f.prices(123), vec![(0, Px::parse("65000").unwrap()), (1, Px::parse("3000").unwrap())]);
}

#[test]
fn file_feed() {
    let dir = std::env::temp_dir().join(format!("oracle_file_feed_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("px.txt");
    let mut f = FilePriceFeed::new(&path);
    assert!(f.prices(0).is_empty()); // missing file
    std::fs::write(&path, "# comment\n1 3200.5\n0 65000 # btc\n\n").unwrap();
    assert_eq!(f.prices(0), vec![(0, Px::parse("65000").unwrap()), (1, Px::parse("3200.5").unwrap())]);
    std::fs::write(&path, "garbage line here\n").unwrap();
    assert_eq!(f.prices(0).len(), 2, "keeps last good prices");
    assert!(FilePriceFeed::parse("0 0").is_err());
    assert!(FilePriceFeed::parse("x 1").is_err());
    assert!(FilePriceFeed::parse("0 1 2").is_err());
    assert_eq!(FilePriceFeed::parse("0 1\n0 2").unwrap(), vec![(0, Px::parse("2").unwrap())]);
    std::fs::remove_dir_all(&dir).ok();
}

proptest! {
    #[test]
    fn round_down_valid(raw in 1u64..u64::MAX / 2, szd in 0u32..6) {
        let s = spec(0, "X", szd, "1");
        let p = round_down_to_valid_px(Px(raw), &s);
        prop_assert!(s.is_valid_px(p), "{} -> {}", Px(raw).to_wire(), p.to_wire());
        // rounding down, unless the input was below the smallest tick
        let tick = 10u64.pow(8 - s.max_px_decimals());
        if raw >= tick { prop_assert!(p.0 <= raw); } else { prop_assert_eq!(p.0, tick); }
        // already valid prices are unchanged
        prop_assert_eq!(round_down_to_valid_px(p, &s), p);
    }
}
