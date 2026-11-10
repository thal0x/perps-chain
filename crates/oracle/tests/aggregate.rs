use interfaces::*;
use oracle::*;
use proptest::prelude::*;

fn addr(i: u8) -> Address {
    let mut a = [0u8; 20];
    a[19] = i;
    Address(a)
}

fn px(v: u64) -> Px {
    Px(v * 100_000_000)
}

fn wm(v: &[(u64, u8, u64)]) -> Option<Px> {
    let mut e: Vec<_> = v.iter().map(|(p, a, s)| (px(*p), addr(*a), *s)).collect();
    weighted_median(&mut e)
}

#[test]
fn weighted_median_basics() {
    assert_eq!(wm(&[]), None);
    assert_eq!(wm(&[(100, 1, 0)]), None);
    assert_eq!(wm(&[(100, 1, 5)]), Some(px(100)));
    // equal stakes, odd count -> middle
    assert_eq!(wm(&[(300, 1, 1), (100, 2, 1), (200, 3, 1)]), Some(px(200)));
    // equal stakes, even count -> lower median
    assert_eq!(wm(&[(200, 1, 1), (100, 2, 1)]), Some(px(100)));
    assert_eq!(wm(&[(400, 1, 1), (100, 2, 1), (300, 3, 1), (200, 4, 1)]), Some(px(200)));
    // weights: heavy validator dominates
    assert_eq!(wm(&[(100, 1, 1), (200, 2, 1), (300, 3, 10)]), Some(px(300)));
    // exactly half at the low price -> low price (2*cum >= total)
    assert_eq!(wm(&[(100, 1, 5), (200, 2, 3), (300, 3, 2)]), Some(px(100)));
    // just under half at low price -> next
    assert_eq!(wm(&[(100, 1, 4), (200, 2, 4), (300, 3, 2)]), Some(px(200)));
    // zero-stake entries ignored
    assert_eq!(wm(&[(1, 9, 0), (100, 1, 1), (200, 2, 1), (300, 3, 1)]), Some(px(200)));
    // ties on px: result is that px regardless of address order
    assert_eq!(wm(&[(100, 5, 1), (100, 1, 1), (200, 2, 1)]), Some(px(100)));
}

#[test]
fn weighted_median_order_independent() {
    let base = [(105u64, 1u8, 3u64), (101, 2, 1), (103, 3, 2), (101, 4, 7), (110, 5, 1)];
    let want = wm(&base);
    let mut v = base.to_vec();
    for k in 0..v.len() {
        v.rotate_left(1);
        v.swap(0, k);
        assert_eq!(wm(&v), want);
    }
}

#[test]
fn aggregate_quorum_and_staleness() {
    let mut o = OracleState::new(10_000);
    assert_eq!(o.aggregate(0, 0, 0), None);
    o.submit(addr(1), 1, &[(0, px(100)), (1, px(10))], 1_000);
    o.submit(addr(2), 1, &[(0, px(102))], 2_000);
    o.submit(addr(3), 1, &[(0, px(101))], 3_000);
    // quorum 3 met
    assert_eq!(o.aggregate(0, 3_000, 3), Some(px(101)));
    assert_eq!(o.aggregate(0, 3_000, 4), None);
    // asset 1 only one submission
    assert_eq!(o.aggregate(1, 3_000, 1), Some(px(10)));
    assert_eq!(o.aggregate(1, 3_000, 2), None);
    assert_eq!(o.aggregate(7, 3_000, 0), None);
    // freshness boundary: age == window is fresh, window+1 is stale
    assert_eq!(o.aggregate(0, 11_000, 3), Some(px(101)));
    assert_eq!(o.aggregate(0, 11_001, 3), None);
    assert_eq!(o.aggregate(0, 11_001, 2), Some(px(101))); // validators 2,3 -> lower median of 101,102
    assert_eq!(o.fresh_stake(0, 11_001), 2);
    // resubmission replaces
    o.submit(addr(1), 1, &[(0, px(200))], 11_001);
    assert_eq!(o.aggregate(0, 11_001, 3), Some(px(102)));
    assert_eq!(o.submissions(0).len(), 3);
    // stale entirely
    assert_eq!(o.aggregate(0, 100_000, 1), None);
}

#[test]
fn duplicate_asset_in_one_submission_last_wins() {
    let mut o = OracleState::new(10);
    o.submit(addr(1), 1, &[(0, px(1)), (0, px(2))], 0);
    assert_eq!(o.aggregate(0, 0, 1), Some(px(2)));
}

#[test]
fn prune_remove_and_encoding() {
    let mut o = OracleState::new(1_000);
    o.submit(addr(1), 10, &[(0, px(100)), (1, px(5))], 0);
    o.submit(addr(2), 20, &[(0, px(101))], 900);
    let bytes = o.encode();
    let d = OracleState::decode(&bytes).unwrap();
    assert_eq!(d, o);
    assert_eq!(d.encode(), bytes);
    // same logical state via different insertion order
    let mut o2 = OracleState::new(1_000);
    o2.submit(addr(2), 20, &[(0, px(101))], 900);
    o2.submit(addr(1), 10, &[(1, px(5)), (0, px(100))], 0);
    assert_eq!(o2.encode(), bytes);

    let before = o.aggregate(0, 1_500, 1);
    o.prune(1_500);
    assert_eq!(o.aggregate(0, 1_500, 1), before);
    assert_eq!(o.submissions(0).len(), 1);
    assert!(o.submissions(1).is_empty());
    o.remove_validator(&addr(2));
    assert!(o.submissions(0).is_empty());
    assert_eq!(o.encode(), OracleState::new(1_000).encode());

    assert!(OracleState::decode(&bytes[..bytes.len() - 1]).is_err());
    let mut v = bytes.clone();
    v[0] = 2;
    assert_eq!(OracleState::decode(&v), Err(OracleDecodeError::BadVersion(2)));
}

#[test]
fn from_genesis_uses_param() {
    let g_fresh = ChainParams::default().oracle_freshness_ms;
    let o = OracleState::new(g_fresh);
    assert_eq!(o.freshness_ms(), 10_000);
}

/// Reference: expand stakes into unit "votes", sort, take the element at
/// index ceil(T/2)-1 (lower median).
fn brute_median(v: &[(u64, u8, u64)]) -> Option<Px> {
    let mut votes: Vec<u64> = vec![];
    for (p, _, s) in v {
        for _ in 0..*s {
            votes.push(*p);
        }
    }
    if votes.is_empty() {
        return None;
    }
    votes.sort();
    let t = votes.len();
    Some(px(votes[t.div_ceil(2) - 1]))
}

proptest! {
    #[test]
    fn weighted_median_matches_brute_force(v in prop::collection::vec((1u64..50, any::<u8>(), 0u64..20), 0..20)) {
        prop_assert_eq!(wm(&v), brute_median(&v));
    }

    /// Skew resistance: adversaries with < 1/3 of total stake, any prices,
    /// honest validators possibly partly stale; whenever the aggregate exists
    /// (fresh >= quorum) it lies within [min, max] of fresh honest prices.
    #[test]
    fn skew_resistance(
        honest in prop::collection::vec((1_000u64..1_100, 1u64..100, any::<bool>()), 1..10),
        adv_prices in prop::collection::vec(prop_oneof![Just(1u64), Just(1_000_000u64), 1u64..5_000], 0..6),
        adv_share in 0u64..1000,
    ) {
        let honest_stake: u64 = honest.iter().map(|h| h.1).sum();
        // adversary total A with A < (H + A)/3  <=>  2A < H
        let max_adv = (honest_stake - 1) / 2;
        let adv_total = max_adv * adv_share / 1000;
        let total = honest_stake + adv_total;
        let quorum = total * 2 / 3 + 1;
        let mut o = OracleState::new(10_000);
        let now = 100_000u64;
        let mut fresh_honest = vec![];
        for (i, (p, s, stale)) in honest.iter().enumerate() {
            let t = if *stale { now - 10_001 } else { now - (i as u64) * 100 };
            o.submit(addr(i as u8), *s, &[(0, px(*p))], t);
            if !*stale { fresh_honest.push(px(*p)); }
        }
        let n_adv = adv_prices.len() as u64;
        let mut given = 0;
        for (j, p) in adv_prices.iter().enumerate() {
            let s = if j as u64 == n_adv - 1 { adv_total - given } else { adv_total / n_adv };
            given += s;
            o.submit(addr(200 + j as u8), s, &[(0, px(*p))], now);
        }
        prop_assert!(given <= adv_total);
        match o.aggregate(0, now, quorum) {
            None => {
                let fresh: u128 = o.fresh_stake(0, now);
                prop_assert!(fresh < quorum as u128);
            }
            Some(m) => {
                let lo = *fresh_honest.iter().min().unwrap();
                let hi = *fresh_honest.iter().max().unwrap();
                prop_assert!(lo <= m && m <= hi, "median {:?} outside honest [{:?}, {:?}]", m, lo, hi);
            }
        }
    }

    /// Aggregation does not depend on submission order.
    #[test]
    fn aggregate_order_independent(subs in prop::collection::vec((0u8..10, 1u64..200, 1u64..10, 0u64..20_000), 1..30)) {
        let mut a = OracleState::new(10_000);
        for (v, p, s, t) in &subs { a.submit(addr(*v), *s, &[(0, px(*p))], *t); }
        // replay in a different order but keep "latest per validator" the same:
        // the last submission per validator in `subs` order is what counts.
        let mut last = std::collections::BTreeMap::new();
        for (v, p, s, t) in &subs { last.insert(*v, (*p, *s, *t)); }
        let mut b = OracleState::new(10_000);
        for (v, (p, s, t)) in last.iter().rev() { b.submit(addr(*v), *s, &[(0, px(*p))], *t); }
        prop_assert_eq!(a.encode(), b.encode());
        for now in [0u64, 10_000, 20_000, 30_000] {
            prop_assert_eq!(a.aggregate(0, now, 1), b.aggregate(0, now, 1));
        }
    }
}
