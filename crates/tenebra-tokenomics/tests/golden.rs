//! Golden vectors shared with `backtest/tenebra_bt/tokenomics.py`.
//! Regenerate: `TENEBRA_BLESS=1 cargo test -p tenebra-tokenomics --test golden`.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use serde_json::{json, Value};
use std::path::PathBuf;
use tenebra_tokenomics::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

const TIMING: StakeTiming = StakeTiming {
    warmup_secs: 86_400,
    cooldown_secs: 604_800,
};

fn curve() -> DiscountCurve {
    DiscountCurve {
        cap_ppm: 500_000,
        stake_for_cap: 1_000_000_000_000,
    }
}

fn split_params() -> SplitParams {
    SplitParams {
        treasury_bps: 5_000,
        burn: BurnCurve {
            base_bps: 5_000,
            min_bps: 3_000,
            max_bps: 9_000,
            slope_bps: 2_000,
        },
    }
}

fn compute() -> Value {
    let mut rng = Rng(0x7E4E_B4A0_0000_0001);
    let c = curve();

    let discounts: Vec<Value> = (0..200)
        .map(|i| {
            let s = match i % 4 {
                0 => rng.next() % 1_000,
                1 => rng.next() % 10_000_000_000,
                2 => rng.next() % 2_000_000_000_000,
                _ => rng.next(),
            };
            json!([s.to_string(), c.discount_ppm(s)])
        })
        .collect();

    let fees: Vec<Value> = (0..200)
        .map(|_| {
            let amount = rng.next() >> (rng.next() % 64);
            let markup = (rng.next() % 100_001) as u32;
            let discount = (rng.next() % 1_000_001) as u32;
            json!([
                amount.to_string(),
                markup,
                discount,
                fee_amount(amount, markup, discount).unwrap().to_string()
            ])
        })
        .collect();

    // Staking + rewards: 4 accounts, random ops; record state after each op.
    let mut pool = RewardPool::default();
    let mut pos = [StakePosition::default(); 4];
    let mut cps = [RewardCheckpoint::default(); 4];
    let (mut now, mut total) = (1_700_000_000i64, 0u64);
    let mut ops = vec![];
    for _ in 0..300 {
        let u = (rng.next() % 4) as usize;
        let op = match rng.next() % 5 {
            0 | 1 => {
                let a = 1 + rng.next() % 1_000_000_000_000;
                pool.settle(&mut cps[u], pos[u].staked()).unwrap();
                pos[u].deposit(a, now, &TIMING).unwrap();
                total += a;
                json!(["stake", u, a.to_string()])
            }
            2 => {
                let a = (rng.next() % (pos[u].staked() + 1)).max(1);
                pool.settle(&mut cps[u], pos[u].staked()).unwrap();
                let ok = pos[u].request_unstake(a, now, &TIMING).is_ok();
                if ok {
                    total -= a;
                }
                json!(["unstake", u, a.to_string()])
            }
            3 => {
                let r = rng.next() % 10_000_000_000;
                pool.add_rewards(r, total).unwrap();
                json!(["reward", r.to_string()])
            }
            _ => {
                let dt = (rng.next() % 200_000) as i64;
                now += dt;
                json!(["advance", dt])
            }
        };
        let snapshot: Vec<Value> = (0..4)
            .map(|i| {
                let mut cp = cps[i];
                pool.settle(&mut cp, pos[i].staked()).unwrap();
                json!([
                    pos[i].matured.to_string(),
                    pos[i].pending.to_string(),
                    pos[i].cooling.to_string(),
                    pos[i].effective(now, &TIMING).to_string(),
                    cp.owed.to_string()
                ])
            })
            .collect();
        ops.push(json!({"op": op, "acc": pool.acc_per_share.to_string(), "undistributed": pool.undistributed.to_string(), "users": snapshot}));
    }

    // Revenue epochs → intensity → split.
    let sp = split_params();
    let mut tracker = RevenueTracker::default();
    let epochs: Vec<Value> = (0..120)
        .map(|i| {
            let base = 150_000_000_000u64; // $150k/day at 6dp
            let rev = if i % 17 == 5 {
                base * (2 + rng.next() % 4)
            } else {
                base / 2 + rng.next() % base
            };
            let intensity = tracker.observe(rev, 200_000).unwrap();
            let s = split(rev, &sp, intensity);
            json!([
                rev.to_string(),
                intensity.to_string(),
                sp.burn.burn_bps(intensity),
                s.treasury.to_string(),
                s.buyback.to_string(),
                s.stakers.to_string()
            ])
        })
        .collect();

    json!({
        "version": 1,
        "curve": {"cap_ppm": c.cap_ppm, "stake_for_cap": c.stake_for_cap.to_string()},
        "timing": {"warmup_secs": TIMING.warmup_secs, "cooldown_secs": TIMING.cooldown_secs},
        "split": {"treasury_bps": sp.treasury_bps, "base_bps": sp.burn.base_bps, "min_bps": sp.burn.min_bps, "max_bps": sp.burn.max_bps, "slope_bps": sp.burn.slope_bps, "alpha_ppm": 200_000},
        "start_ts": 1_700_000_000i64,
        "discounts": discounts,
        "fees": fees,
        "staking": ops,
        "epochs": epochs,
    })
}

#[test]
fn golden_vectors() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tokenomics_golden.json");
    let got = compute();
    if std::env::var_os("TENEBRA_BLESS").is_some() {
        std::fs::write(&path, serde_json::to_string(&got).unwrap() + "\n").unwrap();
        return;
    }
    let want: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("run with TENEBRA_BLESS=1"))
            .unwrap();
    assert_eq!(got, want, "tokenomics output drifted from golden vectors");
}
