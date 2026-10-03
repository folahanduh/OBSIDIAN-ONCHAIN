#![allow(clippy::cast_possible_truncation)]

use super::*;

const T: StakeTiming = StakeTiming {
    warmup_secs: 86_400,
    cooldown_secs: 7 * 86_400,
};

fn curve() -> DiscountCurve {
    DiscountCurve {
        cap_ppm: 500_000,
        stake_for_cap: 1_000_000_000_000, // 1M tokens at 6 decimals
    }
}

fn policy() -> FeePolicy {
    FeePolicy {
        markup: MarkupBounds {
            min_ppm: 1_500,
            max_ppm: 8_000,
        },
        discount: curve(),
        tiers: TierThresholds {
            tier1: 10_000_000_000,
            tier2: 100_000_000_000,
            tier3: 1_000_000_000_000,
        },
        zero_fee_tier: 0,
    }
}

#[test]
fn isqrt_matches_std() {
    for n in (0u128..10_000).chain([u64::MAX as u128, u128::MAX, (1 << 64) - 1, 1 << 100]) {
        assert_eq!(isqrt(n), n.isqrt(), "n={n}");
    }
}

#[test]
fn discount_curve_shape() {
    let c = curve();
    c.validate().unwrap();
    assert_eq!(c.discount_ppm(0), 0);
    assert_eq!(c.discount_ppm(c.stake_for_cap), 500_000);
    assert_eq!(c.discount_ppm(u64::MAX), 500_000);
    // A quarter of the cap stake gives half the cap discount (√¼ = ½).
    assert_eq!(c.discount_ppm(c.stake_for_cap / 4), 250_000);
    // 1% of the cap stake gives 10% of the cap discount.
    assert_eq!(c.discount_ppm(c.stake_for_cap / 100), 50_000);
}

#[test]
fn fee_rounds_up_once() {
    // 0.30% of 1_000_001 = 3000.003 → 3001
    assert_eq!(fee_amount(1_000_001, 3_000, 0).unwrap(), 3_001);
    // 50% discount: 0.15% of 1_000_000 = 1500 exactly
    assert_eq!(fee_amount(1_000_000, 3_000, 500_000).unwrap(), 1_500);
    assert_eq!(fee_amount(0, 3_000, 0).unwrap(), 0);
    assert_eq!(
        fee_amount(u64::MAX, 8_000, 0).unwrap(),
        (u64::MAX as u128 * 8_000).div_ceil(1_000_000) as u64
    );
    assert_eq!(
        fee_amount(1, 3_000, 1_000_001),
        Err(TokenomicsError::InvalidConfig)
    );
}

#[test]
fn quote_applies_tier_and_bounds() {
    let p = policy();
    p.validate().unwrap();
    let q = p.quote(2_000_000_000_000, 3_000, 0).unwrap(); // $2M at 6 dp, no stake
    assert_eq!((q.tier, q.discount_ppm, q.fee), (0, 0, 6_000_000_000)); // $6,000
    let q = p
        .quote(2_000_000_000_000, 3_000, 1_000_000_000_000)
        .unwrap();
    assert_eq!((q.tier, q.discount_ppm, q.fee), (3, 500_000, 3_000_000_000));
    assert_eq!(p.quote(1, 1_499, 0), Err(TokenomicsError::InvalidConfig));
    assert_eq!(p.quote(1, 8_001, 0), Err(TokenomicsError::InvalidConfig));

    let waived = FeePolicy {
        zero_fee_tier: 3,
        ..p
    };
    assert_eq!(
        waived.quote(1_000, 3_000, 1_000_000_000_000).unwrap().fee,
        0
    );
    assert_eq!(waived.quote(1_000, 3_000, 100_000_000_000).unwrap().tier, 2);
}

#[test]
fn flash_stake_gets_no_discount() {
    let mut pos = StakePosition::default();
    pos.deposit(1_000, 100, &T).unwrap();
    assert_eq!(pos.staked(), 1_000, "earns rewards immediately");
    assert_eq!(pos.effective(100, &T), 0, "no discount in the same block");
    assert_eq!(pos.effective(100 + T.warmup_secs - 1, &T), 0);
    assert_eq!(pos.effective(100 + T.warmup_secs, &T), 1_000);
}

#[test]
fn top_up_restarts_only_pending_warmup() {
    let mut pos = StakePosition::default();
    pos.deposit(1_000, 0, &T).unwrap();
    pos.deposit(500, T.warmup_secs, &T).unwrap(); // first deposit matures here
    assert_eq!((pos.matured, pos.pending), (1_000, 500));
    assert_eq!(pos.effective(T.warmup_secs, &T), 1_000);
    // A second top-up before the 500 matures restarts its clock.
    pos.deposit(1, T.warmup_secs + 10, &T).unwrap();
    assert_eq!(pos.effective(2 * T.warmup_secs + 5, &T), 1_000);
    assert_eq!(pos.effective(2 * T.warmup_secs + 10, &T), 1_501);
}

#[test]
fn unstake_cooldown() {
    let mut pos = StakePosition::default();
    pos.deposit(1_000, 0, &T).unwrap();
    pos.deposit(300, T.warmup_secs, &T).unwrap();
    pos.request_unstake(400, T.warmup_secs + 1, &T).unwrap();
    // Pending (300) is consumed first, then 100 from matured.
    assert_eq!((pos.matured, pos.pending, pos.cooling), (900, 0, 400));
    assert_eq!(
        pos.withdraw(T.warmup_secs + 2),
        Err(TokenomicsError::CooldownActive)
    );
    assert_eq!(pos.withdraw(T.warmup_secs + 1 + T.cooldown_secs), Ok(400));
    assert_eq!(
        pos.withdraw(i64::MAX),
        Err(TokenomicsError::NothingToWithdraw)
    );
    assert_eq!(
        pos.request_unstake(901, 0, &T),
        Err(TokenomicsError::InsufficientStake)
    );
    assert_eq!(pos.deposit(0, 0, &T), Err(TokenomicsError::ZeroAmount));
}

#[test]
fn rewards_pro_rata_and_late_joiner_gets_nothing_old() {
    let mut pool = RewardPool::default();
    let (mut a, mut b) = (RewardCheckpoint::default(), RewardCheckpoint::default());
    let (sa, sb) = (300u64, 100u64);
    pool.settle(&mut a, 0).unwrap();
    pool.add_rewards(900, sa).unwrap(); // only A staked
    pool.settle(&mut b, 0).unwrap(); // B joins after
    pool.add_rewards(400, sa + sb).unwrap();
    pool.settle(&mut a, sa).unwrap();
    pool.settle(&mut b, sb).unwrap();
    assert_eq!(a.owed, 900 + 300);
    assert_eq!(b.owed, 100);
    // Non-divisible amounts round down: the vault keeps the dust.
    let mut c = RewardCheckpoint::default();
    pool.settle(&mut c, 0).unwrap();
    pool.add_rewards(1, 3).unwrap();
    pool.settle(&mut c, 3).unwrap();
    assert_eq!(c.owed, 0);
}

#[test]
fn rewards_with_no_stakers_are_carried() {
    let mut pool = RewardPool::default();
    pool.add_rewards(500, 0).unwrap();
    assert_eq!(pool.undistributed, 500);
    let mut cp = RewardCheckpoint::default();
    pool.settle(&mut cp, 0).unwrap();
    pool.add_rewards(0, 10).unwrap();
    pool.settle(&mut cp, 10).unwrap();
    assert_eq!(cp.owed, 500);
}

#[test]
fn split_sums_exactly() {
    let p = SplitParams {
        treasury_bps: 5_000,
        burn: BurnCurve {
            base_bps: 5_000,
            min_bps: 3_000,
            max_bps: 9_000,
            slope_bps: 2_000,
        },
    };
    p.validate().unwrap();
    let s = split(150_000_000_001, &p, 1_000_000);
    assert_eq!(s.treasury + s.buyback + s.stakers, 150_000_000_001);
    assert_eq!(s.treasury, 75_000_000_000);
    assert_eq!(s.buyback, 37_500_000_000);
    // Usage at 2.5× average: burn = 50% + 20%·1.5 = 80% of the engine half.
    let s = split(1_000_000, &p, 2_500_000);
    assert_eq!(
        (s.treasury, s.buyback, s.stakers),
        (500_000, 400_000, 100_000)
    );
    // Capped at 90%.
    assert_eq!(p.burn.burn_bps(u64::MAX), 9_000);
    // Quiet periods never drop below base (the curve only adds).
    assert_eq!(p.burn.burn_bps(0), 5_000);
}

#[test]
fn revenue_intensity() {
    let mut r = RevenueTracker::default();
    assert_eq!(r.observe(1_000, 200_000).unwrap(), 1_000_000);
    assert_eq!(r.observe(1_000, 200_000).unwrap(), 1_000_000);
    assert_eq!(r.observe(3_000, 200_000).unwrap(), 3_000_000);
    // EWMA now 0.2·3000 + 0.8·1000 = 1400 ⇒ 1400 revenue is intensity 1.0.
    assert_eq!(r.observe(1_400, 200_000).unwrap(), 1_000_000);
}

#[test]
fn config_validation() {
    assert!(DiscountCurve {
        cap_ppm: 1_000_001,
        stake_for_cap: 1
    }
    .validate()
    .is_err());
    assert!(DiscountCurve {
        cap_ppm: 1,
        stake_for_cap: 0
    }
    .validate()
    .is_err());
    assert!(MarkupBounds {
        min_ppm: 9,
        max_ppm: 8
    }
    .validate()
    .is_err());
    assert!(MarkupBounds {
        min_ppm: 0,
        max_ppm: 100_001
    }
    .validate()
    .is_err());
    assert!(TierThresholds {
        tier1: 0,
        tier2: 1,
        tier3: 2
    }
    .validate()
    .is_err());
    assert!(StakeTiming {
        warmup_secs: -1,
        cooldown_secs: 0
    }
    .validate()
    .is_err());
    assert!(BurnCurve {
        base_bps: 1,
        min_bps: 2,
        max_bps: 3,
        slope_bps: 0
    }
    .validate()
    .is_err());
}
