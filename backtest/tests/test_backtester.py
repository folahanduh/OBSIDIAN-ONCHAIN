from __future__ import annotations

import json
from pathlib import Path

from darkquant_bt.backtester import Config, MarketState, Order, Trade, run
from darkquant_bt.strategies import VwapReversion

GOLDEN = Path(__file__).resolve().parents[2] / "testdata" / "quant_golden.json"


def golden_trades() -> list[Trade]:
    return [Trade(*t) for t in json.loads(GOLDEN.read_text())["trades"]]


class BuyOnce:
    def __init__(self, at_ts: int, limit: int):
        self.at_ts, self.limit, self.done = at_ts, limit, False

    def on_trade(self, s: MarketState) -> list[Order]:
        if not self.done and s.ts_ms >= self.at_ts:
            self.done = True
            return [Order(1, 10, self.limit)]
        return []


def test_no_lookahead_and_latency():
    trades = [Trade(0, 100, 100), Trade(30, 101, 100), Trade(50, 150, 100), Trade(51, 102, 100)]
    res = run(trades, BuyOnce(0, 10_000), Config(latency_ms=50, half_spread_ticks=1))
    # Arrives at t=50; the t=50 print is not strictly after arrival → fills at t=51.
    assert [(f.ts_ms, f.price) for f in res.fills] == [(51, 103)]


def test_protective_limit_cancels():
    trades = [Trade(0, 100, 100), Trade(100, 120, 100)]
    res = run(trades, BuyOnce(0, 110), Config(latency_ms=10))
    assert res.fills == [] and res.cancelled == 1


def test_participation_cap_and_fee_accounting():
    trades = [Trade(0, 100, 100), Trade(100, 100, 8)]
    cfg = Config(latency_ms=10, half_spread_ticks=0, participation=0.25)
    res = run(trades, BuyOnce(0, 200), cfg)
    (f,) = res.fills
    assert f.qty == 2 and res.cancelled == 1  # 25% of 8; remainder IOC-cancelled
    notional = 100 * 2 * cfg.tick_size
    assert f.fee == cfg.fees.taker_fee(notional, cfg.vol_init_sigma)
    assert res.cash == -(notional + f.fee)
    assert res.fees_paid == f.fee > 0


def test_reference_strategy_is_deterministic():
    a = run(golden_trades(), VwapReversion()).summary()
    b = run(golden_trades(), VwapReversion()).summary()
    assert a == b
    assert a["fills"] > 0
    assert abs(a["final_position"]) <= VwapReversion().max_position
