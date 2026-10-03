"""Bit-exact mirror of `crates/tenebra-tokenomics`.

Same integer maths, same rounding, same error conditions, verified against
`testdata/tokenomics_golden.json`. Use it to simulate fee revenue, staker
yield and buyback volume before changing on-chain parameters.
"""

from __future__ import annotations

from dataclasses import dataclass
from math import isqrt

from .fixedpoint import U128_MAX, mul_div_ceil, mul_div_floor

PPM = 1_000_000
BPS = 10_000
ACC_PRECISION = 1_000_000_000_000
U64_MAX = (1 << 64) - 1


class TokenomicsError(Exception):
    pass


def _u64(x: int) -> int:
    if not 0 <= x <= U64_MAX:
        raise TokenomicsError("Overflow")
    return x


# ------------------------------------------------------------- fee discount


@dataclass(frozen=True)
class DiscountCurve:
    """discount(S) = min(cap, cap·√(S / stake_for_cap)), exact floor."""

    cap_ppm: int
    stake_for_cap: int

    def validate(self) -> None:
        if self.cap_ppm > PPM or self.stake_for_cap == 0:
            raise TokenomicsError("InvalidConfig")

    def discount_ppm(self, effective_stake: int) -> int:
        if effective_stake >= self.stake_for_cap:
            return self.cap_ppm
        x = mul_div_floor(self.cap_ppm * self.cap_ppm, effective_stake, self.stake_for_cap) or 0
        return isqrt(x)


def fee_amount(amount: int, markup_ppm: int, discount_ppm: int) -> int:
    """⌈amount · markup · (1 − discount)⌉ with a single rounding step."""
    if discount_ppm > PPM:
        raise TokenomicsError("InvalidConfig")
    rate = markup_ppm * (PPM - discount_ppm)
    fee = mul_div_ceil(amount, rate, PPM * PPM)
    if fee is None:
        raise TokenomicsError("Overflow")
    return _u64(fee)


@dataclass(frozen=True)
class TierThresholds:
    tier1: int
    tier2: int
    tier3: int

    def tier(self, effective_stake: int) -> int:
        if effective_stake >= self.tier3:
            return 3
        if effective_stake >= self.tier2:
            return 2
        if effective_stake >= self.tier1:
            return 1
        return 0


# ------------------------------------------------------------------ staking


@dataclass(frozen=True)
class StakeTiming:
    warmup_secs: int
    cooldown_secs: int


@dataclass
class StakePosition:
    matured: int = 0
    pending: int = 0
    pending_since: int = 0
    cooling: int = 0
    cooldown_ends: int = 0

    def staked(self) -> int:
        return min(self.matured + self.pending, U64_MAX)

    def _pending_matured(self, now: int, t: StakeTiming) -> bool:
        return self.pending > 0 and now >= self.pending_since + t.warmup_secs

    def effective(self, now: int, t: StakeTiming) -> int:
        return self.staked() if self._pending_matured(now, t) else self.matured

    def mature(self, now: int, t: StakeTiming) -> None:
        if self._pending_matured(now, t):
            self.matured += self.pending
            self.pending = 0

    def deposit(self, amount: int, now: int, t: StakeTiming) -> None:
        if amount == 0:
            raise TokenomicsError("ZeroAmount")
        self.mature(now, t)
        pending = _u64(self.pending + amount)
        _u64(self.matured + pending)
        self.pending = pending
        self.pending_since = now

    def request_unstake(self, amount: int, now: int, t: StakeTiming) -> None:
        if amount == 0:
            raise TokenomicsError("ZeroAmount")
        self.mature(now, t)
        if amount > self.staked():
            raise TokenomicsError("InsufficientStake")
        from_pending = min(amount, self.pending)
        self.pending -= from_pending
        self.matured -= amount - from_pending
        self.cooling = _u64(self.cooling + amount)
        self.cooldown_ends = now + t.cooldown_secs

    def withdraw(self, now: int) -> int:
        if self.cooling == 0:
            raise TokenomicsError("NothingToWithdraw")
        if now < self.cooldown_ends:
            raise TokenomicsError("CooldownActive")
        out, self.cooling = self.cooling, 0
        return out


@dataclass
class RewardCheckpoint:
    acc_snapshot: int = 0
    owed: int = 0


@dataclass
class RewardPool:
    acc_per_share: int = 0
    undistributed: int = 0

    def add_rewards(self, amount: int, total_staked: int) -> None:
        amt = _u64(amount + self.undistributed)
        if total_staked == 0:
            self.undistributed = amt
            return
        inc = mul_div_floor(amt, ACC_PRECISION, total_staked)
        if inc is None or self.acc_per_share + inc > U128_MAX:
            raise TokenomicsError("Overflow")
        self.acc_per_share += inc
        self.undistributed = 0

    def settle(self, cp: RewardCheckpoint, staked: int) -> None:
        delta = max(self.acc_per_share - cp.acc_snapshot, 0)
        gain = mul_div_floor(delta, staked, ACC_PRECISION)
        if gain is None:
            raise TokenomicsError("Overflow")
        cp.owed = _u64(cp.owed + _u64(gain))
        cp.acc_snapshot = self.acc_per_share

    def claim(self, cp: RewardCheckpoint, staked: int) -> int:
        self.settle(cp, staked)
        out, cp.owed = cp.owed, 0
        return out


# ------------------------------------------------------------ revenue split


@dataclass(frozen=True)
class BurnCurve:
    base_bps: int
    min_bps: int
    max_bps: int
    slope_bps: int

    def burn_bps(self, intensity_ppm: int) -> int:
        excess = max(intensity_ppm - PPM, 0)
        add = mul_div_floor(self.slope_bps, excess, PPM)
        raw = self.base_bps + (U128_MAX if add is None else add)
        return max(min(raw, self.max_bps), self.min_bps)


@dataclass(frozen=True)
class SplitParams:
    treasury_bps: int
    burn: BurnCurve


@dataclass(frozen=True)
class Split:
    treasury: int
    buyback: int
    stakers: int


def _bps_share(amount: int, bps: int) -> int:
    return amount * min(bps, BPS) // BPS


def split(amount: int, p: SplitParams, intensity_ppm: int) -> Split:
    treasury = _bps_share(amount, p.treasury_bps)
    engine = amount - treasury
    buyback = _bps_share(engine, p.burn.burn_bps(intensity_ppm))
    return Split(treasury, buyback, engine - buyback)


@dataclass
class RevenueTracker:
    ewma_scaled: int = 0
    epochs: int = 0

    def observe(self, revenue: int, alpha_ppm: int) -> int:
        r = revenue * PPM
        if self.epochs == 0 or self.ewma_scaled == 0:
            intensity = PPM
        else:
            intensity = mul_div_floor(r, PPM, self.ewma_scaled)
            if intensity is None:
                raise TokenomicsError("Overflow")
        a = min(alpha_ppm, PPM)
        self.ewma_scaled = r if self.epochs == 0 else (a * r + (PPM - a) * self.ewma_scaled) // PPM
        self.epochs += 1
        return min(intensity, U64_MAX)
