"""Bit-exact mirrors of `crates/dq-quant` streaming indicators.

Any change here must be matched in Rust (and vice versa); the golden-vector
test fails otherwise.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from enum import Enum

from .fixedpoint import PIPS_DEN, SCALE, U128_MAX, mul_div_ceil, mul_div_floor

LAMBDA_DEN = 1 << 16
RETURN_CLAMP = SCALE
MAX_FEE_PIPS = 100_000


class QuantError(Exception):
    class Kind(Enum):
        Overflow = "Overflow"
        NonMonotonicTime = "NonMonotonicTime"
        InsufficientHistory = "InsufficientHistory"
        ZeroPrice = "ZeroPrice"
        InvalidConfig = "InvalidConfig"

    def __init__(self, kind: "QuantError.Kind"):
        super().__init__(kind.value)
        self.kind = kind


def _err(kind: str) -> QuantError:
    return QuantError(QuantError.Kind(kind))


class RollingVwap:
    """Rolling VWAP over the last `n_buckets` buckets of `bucket_ms` (current included)."""

    def __init__(self, bucket_ms: int, n_buckets: int):
        if bucket_ms <= 0 or n_buckets <= 0:
            raise _err("InvalidConfig")
        self.bucket_ms = bucket_ms
        self.n = n_buckets
        self.pv = [0] * n_buckets
        self.v = [0] * n_buckets
        self.head: int | None = None
        self.sum_pv = 0
        self.sum_v = 0

    def advance(self, now_ms: int) -> None:
        b = now_ms // self.bucket_ms
        h = self.head
        if h is None:
            self.head = b
            return
        if b < h:
            raise _err("NonMonotonicTime")
        if b == h:
            return
        start = b - self.n + 1 if b - h >= self.n else h + 1
        for i in range(start, b + 1):
            s = i % self.n
            self.sum_pv -= self.pv[s]
            self.sum_v -= self.v[s]
            self.pv[s] = 0
            self.v[s] = 0
        self.head = b

    def record(self, ts_ms: int, price: int, qty: int) -> None:
        if price == 0:
            raise _err("ZeroPrice")
        self.advance(ts_ms)
        pv = price * qty
        s = (ts_ms // self.bucket_ms) % self.n
        new = (self.pv[s] + pv, self.v[s] + qty, self.sum_pv + pv, self.sum_v + qty)
        if any(x > U128_MAX for x in new):
            raise _err("Overflow")
        self.pv[s], self.v[s], self.sum_pv, self.sum_v = new

    def vwap(self) -> int | None:
        """Ticks × 1e9, floor-rounded; None if no volume in window."""
        return mul_div_floor(self.sum_pv, SCALE, self.sum_v)

    def volume(self) -> int:
        return self.sum_v


@dataclass
class Observation:
    ts_ms: int
    cum: int
    price: int


class TwapOracle:
    """Cumulative-price TWAP oracle with ring-buffer history (capacity >= 2)."""

    def __init__(self, capacity: int):
        if capacity < 2:
            raise _err("InvalidConfig")
        self.cap = capacity
        self.ring: list[Observation] = [Observation(0, 0, 0) for _ in range(capacity)]
        self.len = 0
        self.head = 0

    def _at(self, logical: int) -> Observation:
        oldest = (self.head + self.cap + 1 - self.len) % self.cap
        return self.ring[(oldest + logical) % self.cap]

    def newest(self) -> Observation | None:
        return self.ring[self.head] if self.len else None

    def oldest(self) -> Observation | None:
        return self._at(0) if self.len else None

    def update(self, ts_ms: int, price: int) -> None:
        if price == 0:
            raise _err("ZeroPrice")
        last = self.newest()
        if last is None:
            self.ring[0] = Observation(ts_ms, 0, price)
            self.len, self.head = 1, 0
            return
        if ts_ms < last.ts_ms:
            raise _err("NonMonotonicTime")
        if ts_ms == last.ts_ms:
            last.price = price
            return
        cum = last.cum + last.price * (ts_ms - last.ts_ms)
        if cum > U128_MAX:
            raise _err("Overflow")
        self.head = (self.head + 1) % self.cap
        self.ring[self.head] = Observation(ts_ms, cum, price)
        self.len = min(self.len + 1, self.cap)

    def cumulative_at(self, t: int) -> int:
        newest = self.newest()
        if newest is None:
            raise _err("InsufficientHistory")
        if t >= newest.ts_ms:
            o = newest
        else:
            if t < self._at(0).ts_ms:
                raise _err("InsufficientHistory")
            lo, hi = 0, self.len - 1
            while hi - lo > 1:
                mid = lo + (hi - lo) // 2
                if self._at(mid).ts_ms <= t:
                    lo = mid
                else:
                    hi = mid
            o = self._at(lo)
        c = o.cum + o.price * (t - o.ts_ms)
        if c > U128_MAX:
            raise _err("Overflow")
        return c

    def twap(self, end_ms: int, window_ms: int) -> int:
        if window_ms == 0:
            raise _err("InvalidConfig")
        start = end_ms - window_ms
        if start < 0:
            raise _err("InsufficientHistory")
        c1 = self.cumulative_at(end_ms)
        c0 = self.cumulative_at(start)
        r = mul_div_floor(c1 - c0, SCALE, window_ms)
        if r is None:
            raise _err("Overflow")
        return r


class EwmaVol:
    """EWMA variance of per-sample simple returns; σ and var scaled by 1e9 / 1e18."""

    def __init__(self, lambda_num: int, initial_sigma: int):
        if not (0 < lambda_num < LAMBDA_DEN) or initial_sigma > SCALE:
            raise _err("InvalidConfig")
        self.lam = lambda_num
        self.var = initial_sigma * initial_sigma
        self.last: int | None = None
        self.samples = 0

    def on_sample(self, price: int) -> None:
        if price == 0:
            raise _err("ZeroPrice")
        if self.last is not None:
            r = mul_div_floor(abs(price - self.last), SCALE, self.last)
            if r is None:
                raise _err("Overflow")
            r = min(r, RETURN_CLAMP)
            self.var = (self.lam * self.var + (LAMBDA_DEN - self.lam) * r * r) // LAMBDA_DEN
        self.last = price
        self.samples += 1

    def sigma(self) -> int:
        return math.isqrt(self.var)

    def variance(self) -> int:
        return self.var


@dataclass(frozen=True)
class FeeSchedule:
    maker_pips: int
    base_taker_pips: int
    min_taker_pips: int
    max_taker_pips: int
    sigma_ref: int
    slope_pips: int

    def validate(self) -> None:
        ok = (
            self.min_taker_pips <= self.base_taker_pips <= self.max_taker_pips <= MAX_FEE_PIPS
            and self.sigma_ref > 0
            and abs(self.maker_pips) <= MAX_FEE_PIPS
            and (self.maker_pips >= 0 or -self.maker_pips <= self.min_taker_pips)
        )
        if not ok:
            raise _err("InvalidConfig")

    def taker_pips(self, sigma: int) -> int:
        excess = max(sigma - self.sigma_ref, 0)
        add = mul_div_floor(self.slope_pips, excess, self.sigma_ref)
        raw = self.base_taker_pips + (U128_MAX if add is None else add)
        return max(min(raw, self.max_taker_pips), self.min_taker_pips)

    def taker_fee(self, notional: int, sigma: int) -> int:
        f = mul_div_ceil(notional, self.taker_pips(sigma), PIPS_DEN)
        if f is None:
            raise _err("Overflow")
        return f

    def maker_fee(self, notional: int) -> int:
        """Signed: positive = fee (ceil), negative = rebate (floor)."""
        p = abs(self.maker_pips)
        if self.maker_pips >= 0:
            f = mul_div_ceil(notional, p, PIPS_DEN)
        else:
            f = mul_div_floor(notional, p, PIPS_DEN)
        if f is None:
            raise _err("Overflow")
        return f if self.maker_pips >= 0 else -f
