"""Reference strategies. Integer comparisons only, so the same logic can be
ported to an on-chain keeper/vault without re-deriving thresholds."""

from __future__ import annotations

from dataclasses import dataclass

from .backtester import MarketState, Order
from .fixedpoint import SCALE


@dataclass
class VwapReversion:
    """Fade deviations from rolling VWAP, scaled by current volatility.

    Enter long when   last < VWAP · (1 − k·σ),  short when  last > VWAP · (1 + k·σ).
    Flatten when price crosses back through VWAP. Stand aside when the dynamic
    taker fee exceeds `max_taker_pips` (high-σ regimes are where the fee curve
    is designed to tax takers — don't fight it).
    """

    k_sigma: int = 3  # band width in σ units
    clip: int = 20  # lots per order
    max_position: int = 100
    max_taker_pips: int = 400
    slippage_ticks: int = 5  # protective limit distance

    def on_trade(self, s: MarketState) -> list[Order]:
        if s.vwap is None or s.taker_pips > self.max_taker_pips:
            return []
        px = s.last * SCALE  # ticks × 1e9, same scale as vwap
        band = s.vwap * self.k_sigma * s.sigma // SCALE  # vwap · k·σ
        lo, hi = s.vwap - band, s.vwap + band
        buy = Order(1, self.clip, s.last + self.slippage_ticks)
        sell = Order(-1, self.clip, max(1, s.last - self.slippage_ticks))

        if px < lo and s.position + self.clip <= self.max_position:
            return [buy]
        if px > hi and s.position - self.clip >= -self.max_position:
            return [sell]
        if s.position > 0 and px >= s.vwap:
            return [Order(-1, s.position, max(1, s.last - self.slippage_ticks))]
        if s.position < 0 and px <= s.vwap:
            return [Order(1, -s.position, s.last + self.slippage_ticks)]
        return []
