//! Integer-exact streaming indicators for on-chain execution.
//!
//! All state is fixed-size or bounded ring buffers; all arithmetic is integer
//! with explicit, documented rounding so that the Rust state machine, the
//! Python backtester (`backtest/darkquant_bt`) and any on-chain verifier agree
//! bit-for-bit. See `testdata/quant_golden.json` for the shared vectors.
//!
//! Fixed-point conventions:
//! - Prices in ticks (`u64`), quantities in lots (`u64`), time in ms.
//! - Fractional outputs (VWAP, TWAP, returns, σ) are scaled by [`SCALE`] = 1e9.
//! - Variance is scaled by `SCALE²` = 1e18.
//! - Fees in pips: 1 pip = 1e-6 of notional (0.01 bp).

#![no_std]
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

pub const SCALE: u128 = 1_000_000_000;
pub const PIPS_DEN: u128 = 1_000_000;
/// EWMA decay numerator denominator: λ = lambda_num / 2^16.
pub const LAMBDA_DEN: u128 = 1 << 16;
/// Per-sample |return| clamp (100%). Bounds the influence of a single bad
/// print and keeps `r²` ≤ 1e18 so the EWMA can never overflow.
pub const RETURN_CLAMP: u128 = SCALE;
/// Hard cap for any fee or rebate: 10% (keeps `notional * pips` < 2^128 for
/// notionals < 2^108, see `dq_types::MAX_ORDER_NOTIONAL_BITS`).
pub const MAX_FEE_PIPS: u32 = 100_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum QuantError {
    Overflow,
    NonMonotonicTime,
    InsufficientHistory,
    ZeroPrice,
    InvalidConfig,
}

pub type Result<T> = core::result::Result<T, QuantError>;

/// Exact `floor(a * b / d)`. Uses `a = q·d + r` ⇒ `a·b/d = q·b + r·b/d`, so no
/// 256-bit intermediate is needed while `r·b < 2^128`. Returns `None` on
/// division by zero or overflow (never a wrong answer).
#[inline]
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    let (q, r) = (a / d, a % d);
    q.checked_mul(b)?.checked_add(r.checked_mul(b)? / d)
}

/// Exact `ceil(a * b / d)`.
#[inline]
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    let f = mul_div_floor(a, b, d)?;
    // a·b mod d == (r·b) mod d, with r = a mod d.
    if (a % d).checked_mul(b)? % d == 0 {
        Some(f)
    } else {
        f.checked_add(1)
    }
}

// --------------------------------------------------------------------- VWAP

/// `id mod n` as a ring index; `n` is a buffer length so the result fits usize.
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn slot_index(id: u64, n: u64) -> usize {
    (id % n) as usize
}

#[derive(Copy, Clone, Debug, Default)]
struct Bucket {
    pv: u128,
    v: u128,
}

/// Rolling VWAP over the last `n_buckets` time buckets of `bucket_ms` each
/// (the current, partially-filled bucket included). O(1) amortised per
/// update, O(n) memory, exact integer sums (no drift on eviction).
#[derive(Clone, Debug)]
pub struct RollingVwap {
    bucket_ms: u64,
    buckets: Vec<Bucket>,
    head: Option<u64>,
    sum_pv: u128,
    sum_v: u128,
}

impl RollingVwap {
    pub fn new(bucket_ms: u64, n_buckets: usize) -> Result<Self> {
        if bucket_ms == 0 || n_buckets == 0 {
            return Err(QuantError::InvalidConfig);
        }
        Ok(RollingVwap {
            bucket_ms,
            buckets: vec![Bucket::default(); n_buckets],
            head: None,
            sum_pv: 0,
            sum_v: 0,
        })
    }

    pub fn window_ms(&self) -> u64 {
        self.bucket_ms.saturating_mul(self.buckets.len() as u64)
    }

    /// Roll the window forward to `now_ms`, evicting expired buckets.
    pub fn advance(&mut self, now_ms: u64) -> Result<()> {
        let b = now_ms / self.bucket_ms;
        let n = self.buckets.len() as u64;
        let start = match self.head {
            None => {
                self.head = Some(b);
                return Ok(());
            }
            Some(h) if b < h => return Err(QuantError::NonMonotonicTime),
            Some(h) if b == h => return Ok(()),
            Some(h) if b - h >= n => b - n + 1,
            Some(h) => h + 1,
        };
        for id in start..=b {
            let slot = &mut self.buckets[slot_index(id, n)];
            self.sum_pv -= slot.pv;
            self.sum_v -= slot.v;
            *slot = Bucket::default();
        }
        self.head = Some(b);
        Ok(())
    }

    pub fn record(&mut self, ts_ms: u64, price: u64, qty: u64) -> Result<()> {
        if price == 0 {
            return Err(QuantError::ZeroPrice);
        }
        self.advance(ts_ms)?;
        let pv = (price as u128) * (qty as u128);
        let n = self.buckets.len() as u64;
        let slot = &mut self.buckets[slot_index(ts_ms / self.bucket_ms, n)];
        // Check all four additions before committing any of them.
        let new = (
            slot.pv.checked_add(pv),
            slot.v.checked_add(qty as u128),
            self.sum_pv.checked_add(pv),
            self.sum_v.checked_add(qty as u128),
        );
        let (Some(a), Some(b), Some(c), Some(d)) = new else {
            return Err(QuantError::Overflow);
        };
        (slot.pv, slot.v, self.sum_pv, self.sum_v) = (a, b, c, d);
        Ok(())
    }

    /// VWAP in ticks × 1e9 as of the last `advance`/`record`, floor-rounded.
    pub fn vwap(&self) -> Option<u128> {
        mul_div_floor(self.sum_pv, SCALE, self.sum_v)
    }

    pub fn volume(&self) -> u128 {
        self.sum_v
    }
}

// --------------------------------------------------------------------- TWAP

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub ts_ms: u64,
    /// Σ price·dt (tick·ms) up to `ts_ms`.
    pub cum: u128,
    /// Price in effect from `ts_ms` until the next observation.
    pub price: u64,
}

/// Cumulative-price TWAP oracle (Uniswap-v2/v3 style accumulator in a ring
/// buffer). Because each observation stores the price in effect *after* it,
/// the cumulative is exactly piecewise-linear and interpolation is exact.
///
/// Manipulation resistance: a price only accrues weight for the time it is in
/// effect, so a print that is reverted within the same timestamp (block) has
/// zero weight. Feed it the post-batch mid, not individual trades.
#[derive(Clone, Debug)]
pub struct TwapOracle {
    ring: Vec<Observation>,
    len: usize,
    /// Index of newest observation.
    head: usize,
}

impl TwapOracle {
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity < 2 {
            return Err(QuantError::InvalidConfig);
        }
        Ok(TwapOracle {
            ring: vec![
                Observation {
                    ts_ms: 0,
                    cum: 0,
                    price: 0
                };
                capacity
            ],
            len: 0,
            head: 0,
        })
    }

    fn at(&self, logical: usize) -> &Observation {
        let cap = self.ring.len();
        let oldest = (self.head + cap + 1 - self.len) % cap;
        &self.ring[(oldest + logical) % cap]
    }

    pub fn newest(&self) -> Option<&Observation> {
        (self.len > 0).then(|| &self.ring[self.head])
    }

    pub fn oldest(&self) -> Option<&Observation> {
        (self.len > 0).then(|| self.at(0))
    }

    pub fn update(&mut self, ts_ms: u64, price: u64) -> Result<()> {
        if price == 0 {
            return Err(QuantError::ZeroPrice);
        }
        let Some(last) = self.newest().copied() else {
            self.ring[0] = Observation {
                ts_ms,
                cum: 0,
                price,
            };
            self.len = 1;
            self.head = 0;
            return Ok(());
        };
        if ts_ms < last.ts_ms {
            return Err(QuantError::NonMonotonicTime);
        }
        if ts_ms == last.ts_ms {
            // Zero-duration: overwrite, the replaced price accrued no weight.
            self.ring[self.head].price = price;
            return Ok(());
        }
        let cum = (last.price as u128)
            .checked_mul((ts_ms - last.ts_ms) as u128)
            .and_then(|x| x.checked_add(last.cum))
            .ok_or(QuantError::Overflow)?;
        let cap = self.ring.len();
        self.head = (self.head + 1) % cap;
        self.ring[self.head] = Observation { ts_ms, cum, price };
        self.len = (self.len + 1).min(cap);
        Ok(())
    }

    /// Cumulative at time `t` (oldest.ts ≤ t; t beyond newest extrapolates
    /// with the current price).
    pub fn cumulative_at(&self, t: u64) -> Result<u128> {
        let newest = self.newest().ok_or(QuantError::InsufficientHistory)?;
        let o = if t >= newest.ts_ms {
            newest
        } else {
            if t < self.at(0).ts_ms {
                return Err(QuantError::InsufficientHistory);
            }
            // Largest logical i with ts ≤ t. Invariant: at(lo).ts ≤ t < at(hi).ts.
            let (mut lo, mut hi) = (0usize, self.len - 1);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if self.at(mid).ts_ms <= t {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            self.at(lo)
        };
        (o.price as u128)
            .checked_mul((t - o.ts_ms) as u128)
            .and_then(|x| x.checked_add(o.cum))
            .ok_or(QuantError::Overflow)
    }

    /// TWAP over `[end - window, end]` in ticks × 1e9, floor-rounded. `end`
    /// may lie in the past (historical query) or beyond the newest observation.
    pub fn twap(&self, end_ms: u64, window_ms: u64) -> Result<u128> {
        if window_ms == 0 {
            return Err(QuantError::InvalidConfig);
        }
        let start = end_ms
            .checked_sub(window_ms)
            .ok_or(QuantError::InsufficientHistory)?;
        let c1 = self.cumulative_at(end_ms)?;
        let c0 = self.cumulative_at(start)?;
        mul_div_floor(c1 - c0, SCALE, window_ms as u128).ok_or(QuantError::Overflow)
    }
}

// --------------------------------------------------------------- volatility

/// EWMA (RiskMetrics) variance of per-sample simple returns:
/// `σ²ₜ = ⌊(λ·σ²ₜ₋₁ + (D−λ)·rₜ²) / D⌋`, `D = 2^16`, `|r|` clamped to 100%.
///
/// Must be sampled at a fixed cadence (e.g. once per block from the post-batch
/// mid) so that λ has a fixed time meaning: half-life = ln(2)/−ln(λ/D) samples.
#[derive(Clone, Debug)]
pub struct EwmaVol {
    lambda_num: u32,
    var: u128,
    last: Option<u64>,
    samples: u64,
}

impl EwmaVol {
    /// `initial_sigma` is per-sample σ × 1e9 (seed until warm).
    pub fn new(lambda_num: u32, initial_sigma: u64) -> Result<Self> {
        if lambda_num == 0 || lambda_num as u128 >= LAMBDA_DEN || initial_sigma as u128 > SCALE {
            return Err(QuantError::InvalidConfig);
        }
        let s = initial_sigma as u128;
        Ok(EwmaVol {
            lambda_num,
            var: s * s,
            last: None,
            samples: 0,
        })
    }

    pub fn on_sample(&mut self, price: u64) -> Result<()> {
        if price == 0 {
            return Err(QuantError::ZeroPrice);
        }
        if let Some(p0) = self.last {
            let diff = price.abs_diff(p0) as u128;
            let r = mul_div_floor(diff, SCALE, p0 as u128)
                .ok_or(QuantError::Overflow)?
                .min(RETURN_CLAMP);
            let lam = self.lambda_num as u128;
            // var ≤ 1e18, r² ≤ 1e18 ⇒ both products < 2^77. Cannot overflow.
            self.var = (lam * self.var + (LAMBDA_DEN - lam) * r * r) / LAMBDA_DEN;
        }
        self.last = Some(price);
        self.samples += 1;
        Ok(())
    }

    /// Per-sample σ × 1e9, floor(√var).
    pub fn sigma(&self) -> u64 {
        // var ≤ 1e18 ⇒ √var ≤ 1e9 < 2^30.
        u64::try_from(self.var.isqrt()).unwrap_or(u64::MAX)
    }

    pub fn variance(&self) -> u128 {
        self.var
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }
}

// --------------------------------------------------------------------- fees

/// Volatility-adaptive taker fee with a fixed maker fee/rebate:
///
/// `taker(σ) = clamp(base + ⌊slope · (σ − σ_ref)⁺ / σ_ref⌋, min, max)`
///
/// i.e. the taker fee rises by `slope` pips for every 100% that σ exceeds the
/// reference level. Rationale: in high-σ regimes, takers are more likely
/// informed (adverse selection on makers rises); charging them more funds
/// maker rebates/LP compensation and dampens toxic flow.
///
/// Safety invariant (checked in `validate`): `rebate ≤ min_taker`, so the venue
/// is never net-negative on a fill and colluding accounts cannot farm rebates
/// by trading against each other.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FeeSchedule {
    /// Positive = fee, negative = rebate.
    pub maker_pips: i32,
    pub base_taker_pips: u32,
    pub min_taker_pips: u32,
    pub max_taker_pips: u32,
    /// Per-sample σ × 1e9 at which the base fee applies.
    pub sigma_ref: u64,
    pub slope_pips: u32,
}

impl FeeSchedule {
    pub fn validate(&self) -> Result<()> {
        let ok = self.min_taker_pips <= self.base_taker_pips
            && self.base_taker_pips <= self.max_taker_pips
            && self.max_taker_pips <= MAX_FEE_PIPS
            && self.sigma_ref > 0
            && self.maker_pips.unsigned_abs() <= MAX_FEE_PIPS
            && (self.maker_pips >= 0 || self.maker_pips.unsigned_abs() <= self.min_taker_pips);
        if ok {
            Ok(())
        } else {
            Err(QuantError::InvalidConfig)
        }
    }

    pub fn taker_pips(&self, sigma: u64) -> u32 {
        let excess = sigma.saturating_sub(self.sigma_ref) as u128;
        let add = mul_div_floor(self.slope_pips as u128, excess, self.sigma_ref as u128)
            .unwrap_or(u128::MAX);
        let raw = (self.base_taker_pips as u128).saturating_add(add);
        let capped = u32::try_from(raw).map_or(self.max_taker_pips, |r| r.min(self.max_taker_pips));
        capped.max(self.min_taker_pips)
    }

    /// Taker fee in quote atoms, rounded **up** (venue never loses to rounding).
    pub fn taker_fee(&self, notional: u128, sigma: u64) -> Result<u128> {
        mul_div_ceil(notional, self.taker_pips(sigma) as u128, PIPS_DEN).ok_or(QuantError::Overflow)
    }

    /// Maker fee in quote atoms: positive fee rounds up, rebate rounds **down**.
    /// Returned as signed (negative = rebate paid to maker).
    pub fn maker_fee(&self, notional: u128) -> Result<i128> {
        let p = self.maker_pips.unsigned_abs() as u128;
        if self.maker_pips >= 0 {
            let f = mul_div_ceil(notional, p, PIPS_DEN).ok_or(QuantError::Overflow)?;
            i128::try_from(f).map_err(|_| QuantError::Overflow)
        } else {
            let f = mul_div_floor(notional, p, PIPS_DEN).ok_or(QuantError::Overflow)?;
            i128::try_from(f)
                .map(|x| -x)
                .map_err(|_| QuantError::Overflow)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_is_exact() {
        assert_eq!(mul_div_floor(7, 3, 2), Some(10));
        assert_eq!(mul_div_ceil(7, 3, 2), Some(11));
        assert_eq!(mul_div_ceil(8, 3, 2), Some(12));
        assert_eq!(mul_div_floor(1, 1, 0), None);
        // Large values where a*b would overflow but result fits.
        let a = u128::MAX / 3;
        assert_eq!(mul_div_floor(a, 3, 3), Some(a));
        assert_eq!(mul_div_floor(u128::MAX, 2, 1), None);
    }

    #[test]
    fn vwap_window_eviction() {
        let mut v = RollingVwap::new(1_000, 3).unwrap();
        v.record(0, 100, 1).unwrap();
        v.record(1_500, 200, 1).unwrap();
        assert_eq!(v.vwap(), Some(150 * SCALE));
        v.record(2_100, 400, 2).unwrap();
        assert_eq!(v.vwap(), Some(275 * SCALE)); // (100+200+800)/4
        v.advance(3_000).unwrap(); // bucket 0 expires
        assert_eq!(v.vwap(), Some(SCALE * 1000 / 3)); // (200+800)/3
        v.advance(10_000).unwrap();
        assert_eq!(v.vwap(), None);
        assert_eq!(v.volume(), 0);
        assert_eq!(v.record(9_000, 1, 1), Err(QuantError::NonMonotonicTime));
    }

    #[test]
    fn twap_exact_and_interpolated() {
        let mut o = TwapOracle::new(8).unwrap();
        o.update(0, 100).unwrap();
        o.update(10, 200).unwrap();
        o.update(30, 50).unwrap();
        // [0,30]: 100·10 + 200·20 = 5000 / 30
        assert_eq!(o.twap(30, 30).unwrap(), 5000 * SCALE / 30);
        // [5,25]: 100·5 + 200·15 = 3500 / 20 = 175
        assert_eq!(o.twap(25, 20).unwrap(), 175 * SCALE);
        // Extrapolation beyond newest: [30,40] at 50.
        assert_eq!(o.twap(40, 10).unwrap(), 50 * SCALE);
        assert_eq!(o.twap(30, 31), Err(QuantError::InsufficientHistory));
    }

    #[test]
    fn twap_same_timestamp_manipulation_has_no_weight() {
        let mut o = TwapOracle::new(8).unwrap();
        o.update(0, 100).unwrap();
        o.update(10, 1_000_000).unwrap(); // spike...
        o.update(10, 100).unwrap(); // ...reverted in the same block
        assert_eq!(o.twap(20, 20).unwrap(), 100 * SCALE);
    }

    #[test]
    fn twap_ring_wraps() {
        let mut o = TwapOracle::new(4).unwrap();
        for i in 0..10u64 {
            o.update(i * 10, 100 + i).unwrap();
        }
        assert_eq!(o.oldest().unwrap().ts_ms, 60);
        // [60,90]: 106,107,108 for 10ms each
        assert_eq!(o.twap(90, 30).unwrap(), 107 * SCALE);
        assert_eq!(o.twap(90, 31), Err(QuantError::InsufficientHistory));
    }

    #[test]
    fn ewma_converges_and_clamps() {
        let mut v = EwmaVol::new(61_604, 0).unwrap(); // λ≈0.94
                                                      // Alternating ±1% moves ⇒ σ → ~1% = 1e7.
        let mut p = 100_000u64;
        for i in 0..2_000 {
            p = if i % 2 == 0 { p + p / 100 } else { p - p / 101 };
            v.on_sample(p).unwrap();
        }
        let s = v.sigma();
        assert!((9_800_000..=10_100_000).contains(&s), "sigma {s}");
        // A 1000x print is clamped to r = 100%.
        v.on_sample(p * 1000).unwrap();
        assert!(v.variance() <= SCALE * SCALE);
    }

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

    #[test]
    fn fee_curve() {
        let f = fees();
        f.validate().unwrap();
        assert_eq!(f.taker_pips(0), 250);
        assert_eq!(f.taker_pips(1_000_000), 250);
        assert_eq!(f.taker_pips(2_000_000), 550);
        assert_eq!(f.taker_pips(1_500_000), 400);
        assert_eq!(f.taker_pips(u64::MAX), 1_000);
        assert_eq!(f.taker_fee(1_000_001, 0).unwrap(), 251); // ceil(250.00025)
        assert_eq!(f.maker_fee(1_000_001).unwrap(), -50); // floor(50.00005)
    }

    #[test]
    fn rebate_cannot_exceed_min_taker() {
        let f = FeeSchedule {
            maker_pips: -201,
            ..fees()
        };
        assert_eq!(f.validate(), Err(QuantError::InvalidConfig));
    }
}
