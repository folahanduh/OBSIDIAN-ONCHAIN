//! Token mechanics for the Tenebra router: fee discounts, staking with
//! warmup/cooldown, real-yield reward accounting and revenue splitting.
//!
//! This crate is the single source of truth for the maths. The on-chain
//! program (`solana/programs/tenebra-guard`) stores these structs in its
//! accounts and calls these functions; the Python mirror
//! (`backtest/tenebra_bt/tokenomics.py`) reproduces them bit-for-bit for
//! simulation. `no_std`, no allocation, integer-only, explicit rounding.
//!
//! Units: token amounts are raw base units (`u64`), rates are ppm
//! (1 ppm = 0.0001%, 1_000_000 = 100%) or bps (10_000 = 100%), time is unix
//! seconds (`i64`, as in Solana's `Clock`).

#![no_std]

pub const PPM: u64 = 1_000_000;
pub const BPS: u64 = 10_000;
/// `PPM` as the `u32` used for discount rates (100%).
pub const FULL_DISCOUNT_PPM: u32 = 1_000_000;
/// Fixed-point scale of the reward accumulator (rewards per staked unit).
pub const ACC_PRECISION: u128 = 1_000_000_000_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TokenomicsError {
    InvalidConfig,
    Overflow,
    InsufficientStake,
    NothingToWithdraw,
    CooldownActive,
    ZeroAmount,
}

pub type Result<T> = core::result::Result<T, TokenomicsError>;

// ------------------------------------------------------------------ helpers

/// Exact `floor(a·b/d)`; `None` on overflow or `d == 0`.
#[inline]
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    let (q, r) = (a / d, a % d);
    q.checked_mul(b)?.checked_add(r.checked_mul(b)? / d)
}

/// Exact `ceil(a·b/d)`.
#[inline]
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    let f = mul_div_floor(a, b, d)?;
    if (a % d).checked_mul(b)? % d == 0 {
        Some(f)
    } else {
        f.checked_add(1)
    }
}

/// `floor(√n)`. Own implementation (Newton) so the crate builds on the older
/// rustc shipped with Solana's SBF toolchain.
pub fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    // Initial guess 2^ceil(bits/2) ≥ √n, then Newton converges from above.
    let shift = (128 - n.leading_zeros()).div_ceil(2);
    let mut x = 1u128 << shift;
    loop {
        let y = (x + n / x) >> 1;
        if y >= x {
            return x;
        }
        x = y;
    }
}

fn to_u64(x: u128) -> Result<u64> {
    u64::try_from(x).map_err(|_| TokenomicsError::Overflow)
}

// ------------------------------------------------------------ fee discount

/// Square-root fee discount for stakers:
///
/// `discount(S) = min(cap, cap · √(S / S_cap))`
///
/// which is the blueprint's `min(cap, k·√S)` with `k = cap / √S_cap`, but
/// parameterised by the stake at which the cap is reached — easier to reason
/// about when setting it. Concave: each extra token buys less discount, so
/// whales are rewarded without draining revenue.
///
/// Computed exactly as `isqrt(⌊cap² · S / S_cap⌋)` (since
/// `⌊√⌊x⌋⌋ = ⌊√x⌋`), so the result is the true floor, never an approximation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DiscountCurve {
    pub cap_ppm: u32,
    pub stake_for_cap: u64,
}

impl DiscountCurve {
    pub fn validate(&self) -> Result<()> {
        if self.cap_ppm as u64 > PPM || self.stake_for_cap == 0 {
            return Err(TokenomicsError::InvalidConfig);
        }
        Ok(())
    }

    pub fn discount_ppm(&self, effective_stake: u64) -> u32 {
        if effective_stake >= self.stake_for_cap {
            return self.cap_ppm;
        }
        let cap = self.cap_ppm as u128;
        // cap² ≤ 10^12, stake < 2^64 ⇒ product < 2^104: cannot overflow.
        let x = mul_div_floor(
            cap * cap,
            effective_stake as u128,
            self.stake_for_cap as u128,
        )
        .unwrap_or(0);
        // isqrt(x) < cap ≤ 10^6, fits u32.
        u32::try_from(isqrt(x)).unwrap_or(self.cap_ppm)
    }
}

// --------------------------------------------------------------- fee model

/// Bounds the frontend may choose a markup within (e.g. 0.15%–0.80%).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MarkupBounds {
    pub min_ppm: u32,
    pub max_ppm: u32,
}

impl MarkupBounds {
    pub fn validate(&self) -> Result<()> {
        if self.min_ppm > self.max_ppm || self.max_ppm as u64 > PPM / 10 {
            // Hard ceiling: 10% markup.
            return Err(TokenomicsError::InvalidConfig);
        }
        Ok(())
    }

    pub fn contains(&self, markup_ppm: u32) -> bool {
        (self.min_ppm..=self.max_ppm).contains(&markup_ppm)
    }
}

/// Fee charged on `amount`: `⌈amount · markup · (1 − discount)⌉`, single
/// rounding step, rounded up so the protocol never loses to rounding.
pub fn fee_amount(amount: u64, markup_ppm: u32, discount_ppm: u32) -> Result<u64> {
    let keep = PPM
        .checked_sub(discount_ppm as u64)
        .ok_or(TokenomicsError::InvalidConfig)?;
    let rate = markup_ppm as u128 * keep as u128; // ≤ 10^12
    let fee =
        mul_div_ceil(amount as u128, rate, (PPM * PPM) as u128).ok_or(TokenomicsError::Overflow)?;
    to_u64(fee)
}

/// Staking tiers gate off-chain privileges (private Jito routing, depth-aware
/// slippage, …) and, optionally, a fee waiver. Thresholds are on effective
/// (warmed-up) stake.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TierThresholds {
    pub tier1: u64,
    pub tier2: u64,
    pub tier3: u64,
}

impl TierThresholds {
    pub fn validate(&self) -> Result<()> {
        if self.tier1 == 0 || self.tier1 > self.tier2 || self.tier2 > self.tier3 {
            return Err(TokenomicsError::InvalidConfig);
        }
        Ok(())
    }

    pub fn tier(&self, effective_stake: u64) -> u8 {
        match effective_stake {
            s if s >= self.tier3 => 3,
            s if s >= self.tier2 => 2,
            s if s >= self.tier1 => 1,
            _ => 0,
        }
    }
}

/// Everything needed to price one swap's fee.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FeePolicy {
    pub markup: MarkupBounds,
    pub discount: DiscountCurve,
    pub tiers: TierThresholds,
    /// Tier at or above which the markup is waived entirely; 0 = never.
    pub zero_fee_tier: u8,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FeeQuote {
    pub tier: u8,
    pub discount_ppm: u32,
    pub fee: u64,
}

impl FeePolicy {
    pub fn validate(&self) -> Result<()> {
        self.markup.validate()?;
        self.discount.validate()?;
        self.tiers.validate()?;
        if self.zero_fee_tier > 3 {
            return Err(TokenomicsError::InvalidConfig);
        }
        Ok(())
    }

    pub fn quote(&self, amount: u64, markup_ppm: u32, effective_stake: u64) -> Result<FeeQuote> {
        if !self.markup.contains(markup_ppm) {
            return Err(TokenomicsError::InvalidConfig);
        }
        let tier = self.tiers.tier(effective_stake);
        if self.zero_fee_tier != 0 && tier >= self.zero_fee_tier {
            return Ok(FeeQuote {
                tier,
                discount_ppm: FULL_DISCOUNT_PPM,
                fee: 0,
            });
        }
        let discount_ppm = self.discount.discount_ppm(effective_stake);
        Ok(FeeQuote {
            tier,
            discount_ppm,
            fee: fee_amount(amount, markup_ppm, discount_ppm)?,
        })
    }
}

// ------------------------------------------------------------------ staking

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StakeTiming {
    /// Seconds a deposit must sit before it counts for discounts/tiers.
    /// Defeats flash-loan staking (borrow → stake → discounted swap → unstake).
    pub warmup_secs: i64,
    /// Seconds between requesting an unstake and withdrawing. Earns nothing
    /// meanwhile; defeats reward sniping around distributions.
    pub cooldown_secs: i64,
}

impl StakeTiming {
    pub fn validate(&self) -> Result<()> {
        if self.warmup_secs < 0 || self.cooldown_secs < 0 {
            return Err(TokenomicsError::InvalidConfig);
        }
        Ok(())
    }
}

/// One account's stake. `matured + pending` earns rewards immediately;
/// only `matured` (plus `pending` once its warmup elapsed) counts as
/// *effective* stake for discounts and tiers.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StakePosition {
    pub matured: u64,
    pub pending: u64,
    pub pending_since: i64,
    pub cooling: u64,
    pub cooldown_ends: i64,
}

impl StakePosition {
    /// Reward-bearing stake.
    pub fn staked(&self) -> u64 {
        self.matured.saturating_add(self.pending)
    }

    fn pending_matured(&self, now: i64, t: &StakeTiming) -> bool {
        self.pending > 0 && now >= self.pending_since.saturating_add(t.warmup_secs)
    }

    /// Stake counted for discounts and tiers at `now`.
    pub fn effective(&self, now: i64, t: &StakeTiming) -> u64 {
        if self.pending_matured(now, t) {
            self.staked()
        } else {
            self.matured
        }
    }

    pub fn mature(&mut self, now: i64, t: &StakeTiming) {
        if self.pending_matured(now, t) {
            self.matured += self.pending;
            self.pending = 0;
        }
    }

    /// Add stake. Any still-warming pending amount restarts its warmup with
    /// the new deposit (conservative: a top-up never shortens a wait).
    pub fn deposit(&mut self, amount: u64, now: i64, t: &StakeTiming) -> Result<()> {
        if amount == 0 {
            return Err(TokenomicsError::ZeroAmount);
        }
        self.mature(now, t);
        let pending = self
            .pending
            .checked_add(amount)
            .ok_or(TokenomicsError::Overflow)?;
        self.matured
            .checked_add(pending)
            .ok_or(TokenomicsError::Overflow)?;
        self.pending = pending;
        self.pending_since = now;
        Ok(())
    }

    /// Move `amount` into cooldown, taking from not-yet-matured stake first.
    /// Restarts the cooldown timer for everything already cooling.
    pub fn request_unstake(&mut self, amount: u64, now: i64, t: &StakeTiming) -> Result<()> {
        if amount == 0 {
            return Err(TokenomicsError::ZeroAmount);
        }
        self.mature(now, t);
        if amount > self.staked() {
            return Err(TokenomicsError::InsufficientStake);
        }
        let from_pending = amount.min(self.pending);
        self.pending -= from_pending;
        self.matured -= amount - from_pending;
        self.cooling = self
            .cooling
            .checked_add(amount)
            .ok_or(TokenomicsError::Overflow)?;
        self.cooldown_ends = now.saturating_add(t.cooldown_secs);
        Ok(())
    }

    pub fn withdraw(&mut self, now: i64) -> Result<u64> {
        if self.cooling == 0 {
            return Err(TokenomicsError::NothingToWithdraw);
        }
        if now < self.cooldown_ends {
            return Err(TokenomicsError::CooldownActive);
        }
        let out = self.cooling;
        self.cooling = 0;
        Ok(out)
    }
}

// --------------------------------------------------------- reward accounting

/// Per-reward-mint accumulator (MasterChef style). Rewards are paid in the
/// real fee currency (USDC/SOL), never by minting tokens.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RewardPool {
    /// Σ reward / total_staked, scaled by `ACC_PRECISION`.
    pub acc_per_share: u128,
    /// Rewards that arrived while nothing was staked; paid out with the next add.
    pub undistributed: u64,
}

/// A staker's checkpoint against one `RewardPool`.
///
/// Stores the accumulator value at the last settlement rather than the usual
/// MasterChef `reward_debt = ⌊acc·stake⌋`: flooring two running totals and
/// subtracting can credit one unit more than was distributed, whereas
/// `⌊(acc − snapshot)·stake⌋` can only round down. Summed over all stakers it
/// is therefore bounded by what the pool actually received.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RewardCheckpoint {
    pub acc_snapshot: u128,
    /// Settled but unclaimed rewards.
    pub owed: u64,
}

impl RewardPool {
    /// Distribute `amount` across `total_staked`. Rounds down: the pool can
    /// never promise more than it received (dust stays in the vault).
    pub fn add_rewards(&mut self, amount: u64, total_staked: u64) -> Result<()> {
        let amt = amount
            .checked_add(self.undistributed)
            .ok_or(TokenomicsError::Overflow)?;
        if total_staked == 0 {
            self.undistributed = amt;
            return Ok(());
        }
        let inc = mul_div_floor(amt as u128, ACC_PRECISION, total_staked as u128)
            .ok_or(TokenomicsError::Overflow)?;
        self.acc_per_share = self
            .acc_per_share
            .checked_add(inc)
            .ok_or(TokenomicsError::Overflow)?;
        self.undistributed = 0;
        Ok(())
    }

    /// Credit everything earned by `staked` since the last checkpoint. Must be
    /// called before any change to the position's stake (and when opening a
    /// position, with `staked = 0`, to start it at the current index).
    pub fn settle(&self, cp: &mut RewardCheckpoint, staked: u64) -> Result<()> {
        let delta = self.acc_per_share.saturating_sub(cp.acc_snapshot);
        let gain =
            mul_div_floor(delta, staked as u128, ACC_PRECISION).ok_or(TokenomicsError::Overflow)?;
        cp.owed = cp
            .owed
            .checked_add(to_u64(gain)?)
            .ok_or(TokenomicsError::Overflow)?;
        cp.acc_snapshot = self.acc_per_share;
        Ok(())
    }

    /// Take everything owed (for a claim instruction).
    pub fn claim(&self, cp: &mut RewardCheckpoint, staked: u64) -> Result<u64> {
        self.settle(cp, staked)?;
        let out = cp.owed;
        cp.owed = 0;
        Ok(out)
    }
}

// ----------------------------------------------------------- revenue split

/// Burn share of the token-engine half, as a function of usage intensity
/// (this epoch's fee revenue relative to its moving average):
///
/// `burn(I) = clamp(base + slope · (I − 1)⁺, min, max)`
///
/// so buyback-and-burn rises exactly when usage spikes. Intensity is measured
/// from on-chain revenue, which needs no price oracle and costs real fees to
/// inflate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BurnCurve {
    pub base_bps: u16,
    pub min_bps: u16,
    pub max_bps: u16,
    /// Extra burn bps per 100% of intensity above the average.
    pub slope_bps: u16,
}

impl BurnCurve {
    pub fn validate(&self) -> Result<()> {
        if self.min_bps > self.base_bps || self.base_bps > self.max_bps || self.max_bps as u64 > BPS
        {
            return Err(TokenomicsError::InvalidConfig);
        }
        Ok(())
    }

    pub fn burn_bps(&self, intensity_ppm: u64) -> u16 {
        let excess = intensity_ppm.saturating_sub(PPM) as u128;
        let add = mul_div_floor(self.slope_bps as u128, excess, PPM as u128).unwrap_or(u128::MAX);
        let raw = (self.base_bps as u128).saturating_add(add);
        let capped = u16::try_from(raw).map_or(self.max_bps, |r| r.min(self.max_bps));
        capped.max(self.min_bps)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SplitParams {
    /// Treasury share of all fees (blueprint: 50%).
    pub treasury_bps: u16,
    pub burn: BurnCurve,
}

impl SplitParams {
    pub fn validate(&self) -> Result<()> {
        if self.treasury_bps as u64 > BPS {
            return Err(TokenomicsError::InvalidConfig);
        }
        self.burn.validate()
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Split {
    pub treasury: u64,
    pub buyback: u64,
    pub stakers: u64,
}

/// `⌊amount · bps / 10_000⌋`, with `bps` clamped to 100% so the result never
/// exceeds `amount` (and therefore always fits in `u64`).
#[allow(clippy::cast_possible_truncation)]
fn bps_share(amount: u64, bps: u16) -> u64 {
    (amount as u128 * (bps as u128).min(BPS as u128) / BPS as u128) as u64
}

/// Split `amount` into treasury / buyback-and-burn / stakers. Floors each
/// share and gives the remainder to stakers, so the parts always sum exactly
/// to `amount`.
pub fn split(amount: u64, p: &SplitParams, intensity_ppm: u64) -> Split {
    let treasury = bps_share(amount, p.treasury_bps);
    let engine = amount - treasury;
    let buyback = bps_share(engine, p.burn.burn_bps(intensity_ppm));
    Split {
        treasury,
        buyback,
        stakers: engine - buyback,
    }
}

/// EWMA of per-epoch revenue for one fee mint; yields usage intensity.
/// `ewma` is scaled by `PPM` to keep fractional precision.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RevenueTracker {
    pub ewma_scaled: u128,
    pub epochs: u64,
}

impl RevenueTracker {
    /// Record one epoch's revenue and return its intensity (ppm of average).
    /// The first epoch has intensity 1.0 by definition. `alpha_ppm` is the
    /// weight of the new observation (e.g. 200_000 ≈ 9-epoch half-life).
    pub fn observe(&mut self, revenue: u64, alpha_ppm: u32) -> Result<u64> {
        let r = revenue as u128 * PPM as u128;
        let intensity = if self.epochs == 0 || self.ewma_scaled == 0 {
            PPM as u128
        } else {
            mul_div_floor(r, PPM as u128, self.ewma_scaled).ok_or(TokenomicsError::Overflow)?
        };
        let a = (alpha_ppm as u128).min(PPM as u128);
        self.ewma_scaled = if self.epochs == 0 {
            r
        } else {
            (a * r + (PPM as u128 - a) * self.ewma_scaled) / PPM as u128
        };
        self.epochs += 1;
        Ok(u64::try_from(intensity).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests;
