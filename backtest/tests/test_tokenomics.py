"""Rust ↔ Python parity for tenebra-tokenomics."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from tenebra_bt.tokenomics import (
    BurnCurve,
    DiscountCurve,
    RevenueTracker,
    RewardCheckpoint,
    RewardPool,
    SplitParams,
    StakePosition,
    StakeTiming,
    TokenomicsError,
    fee_amount,
    split,
)

GOLDEN = Path(__file__).resolve().parents[2] / "testdata" / "tokenomics_golden.json"


@pytest.fixture(scope="module")
def g():
    return json.loads(GOLDEN.read_text())


def test_discounts(g):
    c = DiscountCurve(g["curve"]["cap_ppm"], int(g["curve"]["stake_for_cap"]))
    for stake, want in g["discounts"]:
        assert c.discount_ppm(int(stake)) == want


def test_fees(g):
    for amount, markup, discount, want in g["fees"]:
        assert fee_amount(int(amount), markup, discount) == int(want)


def test_staking_and_rewards_replay(g):
    t = StakeTiming(g["timing"]["warmup_secs"], g["timing"]["cooldown_secs"])
    pool = RewardPool()
    pos = [StakePosition() for _ in range(4)]
    cps = [RewardCheckpoint() for _ in range(4)]
    now, total = g["start_ts"], 0
    for step in g["staking"]:
        op = step["op"]
        if op[0] == "stake":
            u, a = op[1], int(op[2])
            pool.settle(cps[u], pos[u].staked())
            pos[u].deposit(a, now, t)
            total += a
        elif op[0] == "unstake":
            u, a = op[1], int(op[2])
            pool.settle(cps[u], pos[u].staked())
            try:
                pos[u].request_unstake(a, now, t)
                total -= a
            except TokenomicsError:
                pass
        elif op[0] == "reward":
            pool.add_rewards(int(op[1]), total)
        else:
            now += op[1]
        assert str(pool.acc_per_share) == step["acc"]
        assert str(pool.undistributed) == step["undistributed"]
        for i, want in enumerate(step["users"]):
            cp = RewardCheckpoint(cps[i].acc_snapshot, cps[i].owed)
            pool.settle(cp, pos[i].staked())
            got = [str(pos[i].matured), str(pos[i].pending), str(pos[i].cooling), str(pos[i].effective(now, t)), str(cp.owed)]
            assert got == want


def test_revenue_epochs(g):
    s = g["split"]
    p = SplitParams(s["treasury_bps"], BurnCurve(s["base_bps"], s["min_bps"], s["max_bps"], s["slope_bps"]))
    tr = RevenueTracker()
    for rev, intensity, burn, tre, buy, stk in g["epochs"]:
        i = tr.observe(int(rev), s["alpha_ppm"])
        assert str(i) == intensity
        assert p.burn.burn_bps(i) == burn
        out = split(int(rev), p, i)
        assert [str(out.treasury), str(out.buyback), str(out.stakers)] == [tre, buy, stk]


def test_revenue_sim_matches_blueprint_headline():
    from tenebra_bt.revenue_sim import Assumptions, simulate

    r = simulate(Assumptions(50_000_000, 0.30, 0.0, 0, 100_000_000, 0.10))
    assert r.gross_fees == r.net_fees == 150_000  # $50M × 0.30%
    assert r.treasury == 75_000 and r.buyback + r.stakers == 75_000
