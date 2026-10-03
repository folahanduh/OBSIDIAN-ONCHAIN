"""Event-driven, latency-aware backtester over trade prints.

Execution model (deliberately conservative — taker only):
- A strategy sees trade `k` and may submit IOC orders. Orders become live at
  `ts_k + latency_ms` and can only fill against the first print with
  `ts > arrival` (strictly after — no same-tick lookahead).
- Fill price = that print ± `half_spread_ticks` (pay the spread), and the
  order is cancelled if this exceeds its protective limit (mirrors the
  engine's no-unbounded-market-orders rule).
- Fill size is capped at `participation` × the print's size.
- Fees use the on-chain σ-adaptive taker fee, computed with the integer
  `FeeSchedule` and the EWMA σ as of the fill (identical to the chain).

All money is integer quote atoms; floats appear only in the summary stats.
"""

from __future__ import annotations

import math
from collections import deque
from dataclasses import dataclass, field
from typing import Iterable, Protocol

from .indicators import EwmaVol, FeeSchedule, QuantError, RollingVwap, TwapOracle


@dataclass(frozen=True)
class Trade:
    ts_ms: int
    price: int  # ticks
    qty: int  # lots


@dataclass(frozen=True)
class Order:
    side: int  # +1 buy, -1 sell
    qty: int
    limit: int  # protective limit in ticks (always required)


@dataclass
class Fill:
    ts_ms: int
    side: int
    price: int
    qty: int
    fee: int


@dataclass
class MarketState:
    """Indicator snapshot handed to strategies (all integer, chain-identical)."""

    ts_ms: int
    last: int
    vwap: int | None  # ticks × 1e9
    twap: int | None  # ticks × 1e9
    sigma: int  # per-sample σ × 1e9
    taker_pips: int
    position: int


class Strategy(Protocol):
    def on_trade(self, state: MarketState) -> list[Order]: ...


@dataclass
class Config:
    tick_size: int = 10  # quote atoms per lot per tick
    latency_ms: int = 50
    half_spread_ticks: int = 1
    participation: float = 0.25
    vwap_bucket_ms: int = 1_000
    vwap_buckets: int = 60
    twap_cap: int = 4_096
    twap_window_ms: int = 30_000
    vol_lambda_num: int = 61_604
    vol_init_sigma: int = 1_000_000
    vol_sample_ms: int = 1_000
    fees: FeeSchedule = field(
        default_factory=lambda: FeeSchedule(
            maker_pips=-50,
            base_taker_pips=250,
            min_taker_pips=200,
            max_taker_pips=1_000,
            sigma_ref=1_000_000,
            slope_pips=300,
        )
    )


@dataclass
class Result:
    fills: list[Fill]
    equity: list[tuple[int, int]]  # (ts_ms, equity in quote atoms)
    cash: int
    position: int
    fees_paid: int
    cancelled: int

    @property
    def pnl(self) -> int:
        return self.equity[-1][1] if self.equity else 0

    def max_drawdown(self) -> int:
        peak, mdd = 0, 0
        for _, e in self.equity:
            peak = max(peak, e)
            mdd = max(mdd, peak - e)
        return mdd

    def sharpe(self, per_period_ms: int = 60_000) -> float:
        """Non-annualised Sharpe of equity changes sampled every `per_period_ms`."""
        if len(self.equity) < 3:
            return 0.0
        samples, nxt = [], self.equity[0][0]
        for ts, e in self.equity:
            if ts >= nxt:
                samples.append(e)
                nxt = ts + per_period_ms
        rets = [b - a for a, b in zip(samples, samples[1:])]
        if len(rets) < 2:
            return 0.0
        mu = sum(rets) / len(rets)
        sd = math.sqrt(sum((r - mu) ** 2 for r in rets) / (len(rets) - 1))
        return mu / sd if sd > 0 else 0.0

    def summary(self) -> dict[str, float | int]:
        return {
            "fills": len(self.fills),
            "cancelled": self.cancelled,
            "pnl": self.pnl,
            "fees_paid": self.fees_paid,
            "max_drawdown": self.max_drawdown(),
            "sharpe_1m": round(self.sharpe(), 4),
            "final_position": self.position,
        }


def run(trades: Iterable[Trade], strategy: Strategy, cfg: Config | None = None) -> Result:
    cfg = cfg or Config()
    cfg.fees.validate()
    vwap = RollingVwap(cfg.vwap_bucket_ms, cfg.vwap_buckets)
    twap = TwapOracle(cfg.twap_cap)
    vol = EwmaVol(cfg.vol_lambda_num, cfg.vol_init_sigma)
    next_sample: int | None = None

    pending: deque[tuple[int, Order]] = deque()  # (arrival_ts, order)
    fills: list[Fill] = []
    equity: list[tuple[int, int]] = []
    cash = position = fees_paid = cancelled = 0

    for t in trades:
        # 1) Execute orders that arrived strictly before this print.
        while pending and pending[0][0] < t.ts_ms:
            _, o = pending.popleft()
            px = t.price + o.side * cfg.half_spread_ticks
            cap = max(1, int(t.qty * cfg.participation))
            if (o.side > 0 and px > o.limit) or (o.side < 0 and px < o.limit) or px <= 0:
                cancelled += 1
                continue
            q = min(o.qty, cap)
            notional = px * q * cfg.tick_size
            fee = cfg.fees.taker_fee(notional, vol.sigma())
            cash -= o.side * notional + fee
            position += o.side * q
            fees_paid += fee
            fills.append(Fill(t.ts_ms, o.side, px, q, fee))
            if q < o.qty:
                cancelled += 1  # IOC remainder

        # 2) Update chain-identical indicators with this print.
        vwap.record(t.ts_ms, t.price, t.qty)
        twap.update(t.ts_ms, t.price)
        if next_sample is None or t.ts_ms >= next_sample:
            vol.on_sample(t.price)
            next_sample = (t.ts_ms // cfg.vol_sample_ms + 1) * cfg.vol_sample_ms

        equity.append((t.ts_ms, cash + position * t.price * cfg.tick_size))

        # 3) Strategy decision on the post-print state.
        try:
            tw: int | None = twap.twap(t.ts_ms, cfg.twap_window_ms)
        except QuantError:
            tw = None
        sigma = vol.sigma()
        state = MarketState(
            ts_ms=t.ts_ms,
            last=t.price,
            vwap=vwap.vwap(),
            twap=tw,
            sigma=sigma,
            taker_pips=cfg.fees.taker_pips(sigma),
            position=position,
        )
        for o in strategy.on_trade(state):
            if o.qty > 0 and o.side in (1, -1):
                pending.append((t.ts_ms + cfg.latency_ms, o))

    cancelled += len(pending)
    return Result(fills, equity, cash, position, fees_paid, cancelled)
