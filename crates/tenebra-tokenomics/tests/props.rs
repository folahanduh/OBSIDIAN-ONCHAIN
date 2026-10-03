//! Economic invariants under random sequences.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use proptest::prelude::*;
use tenebra_tokenomics::*;

const T: StakeTiming = StakeTiming {
    warmup_secs: 100,
    cooldown_secs: 1_000,
};

#[derive(Clone, Debug)]
enum Op {
    Stake(usize, u64),
    Unstake(usize, u64),
    Reward(u64),
    Advance(i64),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..4, 1u64..1_000_000_000).prop_map(|(u, a)| Op::Stake(u, a)),
        (0usize..4, 1u64..1_000_000_000).prop_map(|(u, a)| Op::Unstake(u, a)),
        (0u64..10_000_000_000).prop_map(Op::Reward),
        (0i64..500).prop_map(Op::Advance),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    /// The pool never owes more than it received, and rounding loses less
    /// than one base unit per distribution plus one per settlement.
    #[test]
    fn rewards_never_over_distribute(ops in prop::collection::vec(arb_op(), 1..80)) {
        let mut pool = RewardPool::default();
        let mut pos = [StakePosition::default(); 4];
        let mut cps = [RewardCheckpoint::default(); 4];
        let (mut now, mut total, mut paid_in, mut distributions, mut settles) = (0i64, 0u64, 0u128, 0u128, 0u128);

        for op in ops {
            match op {
                Op::Stake(u, a) => {
                    pool.settle(&mut cps[u], pos[u].staked()).unwrap();
                    settles += 1;
                    pos[u].deposit(a, now, &T).unwrap();
                    total += a;
                }
                Op::Unstake(u, a) => {
                    let a = a.min(pos[u].staked());
                    if a == 0 { continue; }
                    pool.settle(&mut cps[u], pos[u].staked()).unwrap();
                    settles += 1;
                    pos[u].request_unstake(a, now, &T).unwrap();
                    total -= a;
                }
                Op::Reward(r) => {
                    pool.add_rewards(r, total).unwrap();
                    paid_in += r as u128;
                    if total > 0 { distributions += 1; }
                }
                Op::Advance(dt) => now += dt,
            }
            prop_assert_eq!(total, pos.iter().map(|p| p.staked()).sum::<u64>());
        }
        let mut owed = 0u128;
        for u in 0..4 {
            pool.settle(&mut cps[u], pos[u].staked()).unwrap();
            settles += 1;
            owed += cps[u].owed as u128;
        }
        let carried = pool.undistributed as u128;
        prop_assert!(owed + carried <= paid_in, "owed {} + carried {} > paid {}", owed, carried, paid_in);
        prop_assert!(paid_in - owed - carried <= distributions + settles, "too much dust");
    }

    #[test]
    fn effective_never_exceeds_staked(ops in prop::collection::vec(arb_op(), 1..80)) {
        let mut p = StakePosition::default();
        let mut now = 0i64;
        for op in ops {
            match op {
                Op::Stake(_, a) => { p.deposit(a, now, &T).unwrap(); }
                Op::Unstake(_, a) => { let _ = p.request_unstake(a, now, &T); }
                Op::Advance(dt) => now += dt,
                Op::Reward(_) => { let _ = p.withdraw(now); }
            }
            prop_assert!(p.effective(now, &T) <= p.staked());
            prop_assert!(p.matured <= p.effective(now, &T));
        }
    }

    #[test]
    fn discount_monotone_concave_capped(cap in 0u32..=1_000_000, scap in 1u64.., a in any::<u64>(), b in any::<u64>()) {
        let c = DiscountCurve { cap_ppm: cap, stake_for_cap: scap };
        let (lo, hi) = (a.min(b), a.max(b));
        prop_assert!(c.discount_ppm(lo) <= c.discount_ppm(hi));
        prop_assert!(c.discount_ppm(hi) <= cap);
        // Exact floor of cap·√(s/S): d² ≤ cap²·s/S < (d+1)².
        let s = lo.min(scap - 1) as u128;
        let d = c.discount_ppm(s as u64) as u128;
        let lhs = (cap as u128).pow(2) * s;
        prop_assert!(d * d * scap as u128 <= lhs);
        prop_assert!((d + 1) * (d + 1) * scap as u128 > lhs);
    }

    #[test]
    fn split_conserves(amount in any::<u64>(), t in 0u16..=10_000, base in 0u16..=10_000, i in any::<u64>()) {
        let p = SplitParams { treasury_bps: t, burn: BurnCurve { base_bps: base, min_bps: 0, max_bps: 10_000, slope_bps: 1_234 } };
        let s = split(amount, &p, i);
        prop_assert_eq!(s.treasury as u128 + s.buyback as u128 + s.stakers as u128, amount as u128);
    }

    #[test]
    fn fee_bounded_by_markup(amount in any::<u64>(), m in 0u32..=100_000, d in 0u32..=1_000_000) {
        let f = fee_amount(amount, m, d).unwrap() as u128;
        let exact_upper = (amount as u128 * m as u128).div_ceil(1_000_000);
        prop_assert!(f <= exact_upper);
        prop_assert!(f * 1_000_000 * 1_000_000 >= amount as u128 * m as u128 * (1_000_000 - d as u128));
    }
}
