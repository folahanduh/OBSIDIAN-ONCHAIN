//! Streaming indicators vs naive full recomputation.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use dq_quant::*;
use proptest::prelude::*;

proptest! {
    #[test]
    fn mul_div_matches_wide_math(a in any::<u64>(), b in any::<u64>(), d in 1u64..) {
        let (a, b, d) = (a as u128, b as u128, d as u128);
        prop_assert_eq!(mul_div_floor(a, b, d), Some(a * b / d));
        prop_assert_eq!(mul_div_ceil(a, b, d), Some((a * b).div_ceil(d)));
    }

    #[test]
    fn vwap_matches_naive(
        bucket_ms in 1u64..500,
        n in 1usize..12,
        trades in prop::collection::vec((0u64..400, 1u64..10_000, 0u64..1_000), 1..200),
    ) {
        let mut v = RollingVwap::new(bucket_ms, n).unwrap();
        let mut ts = 0u64;
        let mut hist: Vec<(u64, u64, u64)> = vec![];
        for (dt, p, q) in trades {
            ts += dt;
            v.record(ts, p, q).unwrap();
            hist.push((ts, p, q));
            let b = ts / bucket_ms;
            let lo = b.saturating_sub(n as u64 - 1);
            let (pv, vol) = hist.iter()
                .filter(|(t, _, _)| t / bucket_ms >= lo)
                .fold((0u128, 0u128), |(a, c), &(_, p, q)| (a + p as u128 * q as u128, c + q as u128));
            prop_assert_eq!(v.volume(), vol);
            prop_assert_eq!(v.vwap(), (vol > 0).then(|| pv * SCALE / vol));
        }
    }

    #[test]
    fn twap_matches_naive_integral(
        cap in 2usize..16,
        ups in prop::collection::vec((0u64..50, 1u64..1_000), 1..60),
        q_end_back in 0u64..200,
        q_win in 1u64..300,
    ) {
        let mut o = TwapOracle::new(cap).unwrap();
        let mut ts = 0u64;
        let mut steps: Vec<(u64, u64)> = vec![]; // (ts, price in effect from ts)
        for (dt, p) in ups {
            ts += dt;
            o.update(ts, p).unwrap();
            match steps.last_mut() {
                Some(last) if last.0 == ts => last.1 = p,
                _ => steps.push((ts, p)),
            }
        }
        // Price at time t (naive): last step with ts <= t.
        let price_at = |t: u64| steps.iter().rev().find(|s| s.0 <= t).map(|s| s.1);
        let end = ts + 50 - q_end_back.min(ts + 50);
        let oldest = o.oldest().unwrap().ts_ms;
        match end.checked_sub(q_win) {
            Some(start) if start >= oldest => {
                let integral: u128 = (start..end).map(|t| price_at(t).unwrap() as u128).sum();
                prop_assert_eq!(o.twap(end, q_win).unwrap(), integral * SCALE / q_win as u128);
            }
            _ => prop_assert_eq!(o.twap(end, q_win), Err(QuantError::InsufficientHistory)),
        }
    }

    #[test]
    fn taker_fee_monotone_and_bounded(s1 in any::<u64>(), s2 in any::<u64>()) {
        let f = FeeSchedule {
            maker_pips: -50, base_taker_pips: 250, min_taker_pips: 200,
            max_taker_pips: 1_000, sigma_ref: 1_000_000, slope_pips: 300,
        };
        let (lo, hi) = (s1.min(s2), s1.max(s2));
        prop_assert!(f.taker_pips(lo) <= f.taker_pips(hi));
        prop_assert!((200..=1_000).contains(&f.taker_pips(hi)));
    }
}
