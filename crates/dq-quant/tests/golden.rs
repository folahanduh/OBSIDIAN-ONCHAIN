//! Cross-language golden vectors. The Python backtester replays the same
//! trades from `testdata/quant_golden.json` and must reproduce every
//! checkpoint bit-for-bit. Regenerate with `DQ_BLESS=1 cargo test -p dq-quant --test golden`.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use dq_quant::*;
use serde_json::{json, Value};
use std::path::PathBuf;

const BUCKET_MS: u64 = 1_000;
const N_BUCKETS: usize = 60;
const TWAP_CAP: usize = 4_096;
const TWAP_WINDOW_MS: u64 = 30_000;
const LAMBDA_NUM: u32 = 61_604; // ≈ 0.94
const INIT_SIGMA: u64 = 1_000_000; // 0.1% per sample
const SAMPLE_EVERY: usize = 10;
const CHECK_EVERY: usize = 50;
const N_TRADES: usize = 3_000;

fn fees() -> FeeSchedule {
    FeeSchedule {
        maker_pips: -50,
        base_taker_pips: 250,
        min_taker_pips: 200,
        max_taker_pips: 1_000,
        sigma_ref: 1_000_000,
        slope_pips: 300,
    }
}

/// xorshift64* — deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn trades() -> Vec<(u64, u64, u64)> {
    let mut rng = Rng(0xD15E_A5E0_0000_0001);
    let (mut ts, mut px) = (1_700_000_000_000u64, 100_000u64);
    (0..N_TRADES)
        .map(|i| {
            ts += rng.next() % 1_500;
            // Regime switch halfway through: 8x wider moves.
            let amp = if i < N_TRADES / 2 { 25 } else { 200 };
            let step = rng.next() % (2 * amp + 1);
            px = (px + step).saturating_sub(amp).max(1);
            (ts, px, 1 + rng.next() % 500)
        })
        .collect()
}

fn compute() -> Value {
    let f = fees();
    f.validate().unwrap();
    let trades = trades();
    let mut vwap = RollingVwap::new(BUCKET_MS, N_BUCKETS).unwrap();
    let mut twap = TwapOracle::new(TWAP_CAP).unwrap();
    let mut vol = EwmaVol::new(LAMBDA_NUM, INIT_SIGMA).unwrap();
    let mut checkpoints = vec![];

    for (i, &(ts, p, q)) in trades.iter().enumerate() {
        vwap.record(ts, p, q).unwrap();
        twap.update(ts, p).unwrap();
        if i % SAMPLE_EVERY == 0 {
            vol.on_sample(p).unwrap();
        }
        if i % CHECK_EVERY == CHECK_EVERY - 1 {
            let sigma = vol.sigma();
            let notional = p as u128 * q as u128;
            checkpoints.push(json!({
                "i": i,
                "vwap": vwap.vwap().map(|v| v.to_string()),
                "twap": match twap.twap(ts, TWAP_WINDOW_MS) {
                    Ok(v) => Value::String(v.to_string()),
                    Err(e) => Value::String(format!("{e:?}")),
                },
                "variance": vol.variance().to_string(),
                "sigma": sigma,
                "taker_pips": f.taker_pips(sigma),
                "taker_fee": f.taker_fee(notional, sigma).unwrap().to_string(),
                "maker_fee": f.maker_fee(notional).unwrap().to_string(),
            }));
        }
    }

    json!({
        "version": 1,
        "params": {
            "bucket_ms": BUCKET_MS, "n_buckets": N_BUCKETS,
            "twap_cap": TWAP_CAP, "twap_window_ms": TWAP_WINDOW_MS,
            "lambda_num": LAMBDA_NUM, "init_sigma": INIT_SIGMA,
            "sample_every": SAMPLE_EVERY, "check_every": CHECK_EVERY,
            "fees": {
                "maker_pips": f.maker_pips, "base_taker_pips": f.base_taker_pips,
                "min_taker_pips": f.min_taker_pips, "max_taker_pips": f.max_taker_pips,
                "sigma_ref": f.sigma_ref, "slope_pips": f.slope_pips,
            },
        },
        "trades": trades.iter().map(|&(t, p, q)| json!([t, p, q])).collect::<Vec<_>>(),
        "checkpoints": checkpoints,
    })
}

#[test]
fn golden_vectors() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/quant_golden.json");
    let got = compute();
    if std::env::var_os("DQ_BLESS").is_some() {
        std::fs::write(&path, serde_json::to_string(&got).unwrap() + "\n").unwrap();
        return;
    }
    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("missing golden file; run with DQ_BLESS=1"),
    )
    .unwrap();
    assert_eq!(got, want, "quant output drifted from golden vectors");
}
