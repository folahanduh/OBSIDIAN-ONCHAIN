"""Rust ↔ Python parity on shared golden vectors and edge cases."""

from __future__ import annotations

import json
import random
from pathlib import Path

import pytest

from darkquant_bt import (
    SCALE,
    EwmaVol,
    FeeSchedule,
    QuantError,
    RollingVwap,
    TwapOracle,
    mul_div_ceil,
    mul_div_floor,
)

GOLDEN = Path(__file__).resolve().parents[2] / "testdata" / "quant_golden.json"


def test_golden_vectors_bit_exact():
    g = json.loads(GOLDEN.read_text())
    p = g["params"]
    fees = FeeSchedule(**p["fees"])
    fees.validate()
    vwap = RollingVwap(p["bucket_ms"], p["n_buckets"])
    twap = TwapOracle(p["twap_cap"])
    vol = EwmaVol(p["lambda_num"], p["init_sigma"])
    checks = iter(g["checkpoints"])
    n_checked = 0

    for i, (ts, px, q) in enumerate(g["trades"]):
        vwap.record(ts, px, q)
        twap.update(ts, px)
        if i % p["sample_every"] == 0:
            vol.on_sample(px)
        if i % p["check_every"] == p["check_every"] - 1:
            want = next(checks)
            assert want["i"] == i
            sigma = vol.sigma()
            v = vwap.vwap()
            try:
                tw = str(twap.twap(ts, p["twap_window_ms"]))
            except QuantError as e:
                tw = e.kind.value
            got = {
                "i": i,
                "vwap": None if v is None else str(v),
                "twap": tw,
                "variance": str(vol.variance()),
                "sigma": sigma,
                "taker_pips": fees.taker_pips(sigma),
                "taker_fee": str(fees.taker_fee(px * q, sigma)),
                "maker_fee": str(fees.maker_fee(px * q)),
            }
            assert got == want, f"checkpoint {i}"
            n_checked += 1
    assert n_checked == len(g["checkpoints"]) > 0


def test_mul_div_matches_rust_overflow_semantics():
    rng = random.Random(7)
    for _ in range(5_000):
        a, b, d = rng.getrandbits(64), rng.getrandbits(64), rng.getrandbits(64) or 1
        assert mul_div_floor(a, b, d) == a * b // d
        assert mul_div_ceil(a, b, d) == -(-a * b // d)
    u128_max = (1 << 128) - 1
    assert mul_div_floor(u128_max // 3, 3, 3) == u128_max // 3
    assert mul_div_floor(u128_max, 2, 1) is None
    assert mul_div_floor(1, 1, 0) is None


def test_twap_same_block_spike_has_no_weight():
    o = TwapOracle(8)
    o.update(0, 100)
    o.update(10, 1_000_000)
    o.update(10, 100)
    assert o.twap(20, 20) == 100 * SCALE


def test_vwap_rejects_time_travel():
    v = RollingVwap(1_000, 3)
    v.record(5_000, 1, 1)
    with pytest.raises(QuantError):
        v.record(3_999, 1, 1)


def test_rebate_above_min_taker_rejected():
    f = FeeSchedule(-201, 250, 200, 1_000, 1_000_000, 300)
    with pytest.raises(QuantError):
        f.validate()
