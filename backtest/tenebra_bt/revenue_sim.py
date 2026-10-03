"""Revenue / yield simulator using the exact on-chain fee and split maths.

    python -m tenebra_bt.revenue_sim --daily-volume 50000000 --markup 0.30

All money is computed in USDC base units (6 decimals) with the same integer
functions the program uses, then printed in dollars. Outputs are projections
from the assumptions you pass in, not forecasts.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass

from .tokenomics import PPM, BurnCurve, DiscountCurve, SplitParams, fee_amount, split

USDC = 1_000_000  # base units per dollar
TOKEN = 1_000_000  # stake token assumed to have 6 decimals


@dataclass(frozen=True)
class Assumptions:
    daily_volume_usd: float
    markup_pct: float
    staker_volume_share: float  # fraction of volume traded by staked accounts
    avg_staker_stake: float  # tokens per staking account (sets their discount)
    total_staked: float  # tokens
    token_price_usd: float
    usage_intensity: float = 1.0  # today's revenue vs its moving average


CURVE = DiscountCurve(cap_ppm=500_000, stake_for_cap=1_000_000 * TOKEN)
SPLIT = SplitParams(treasury_bps=5_000, burn=BurnCurve(base_bps=5_000, min_bps=3_000, max_bps=9_000, slope_bps=2_000))


@dataclass(frozen=True)
class Result:
    gross_fees: float
    discount_given: float
    net_fees: float
    treasury: float
    buyback: float
    stakers: float
    staker_discount_pct: float
    burn_share_pct: float
    staker_apr_pct: float


def simulate(a: Assumptions) -> Result:
    volume = int(a.daily_volume_usd * USDC)
    markup_ppm = round(a.markup_pct / 100 * PPM)
    staker_vol = int(volume * a.staker_volume_share)
    other_vol = volume - staker_vol
    discount = CURVE.discount_ppm(int(a.avg_staker_stake * TOKEN))

    gross = fee_amount(volume, markup_ppm, 0)
    net = fee_amount(other_vol, markup_ppm, 0) + fee_amount(staker_vol, markup_ppm, discount)
    intensity = round(a.usage_intensity * PPM)
    parts = split(net, SPLIT, intensity)

    staked_value = a.total_staked * a.token_price_usd
    apr = (parts.stakers / USDC * 365 / staked_value * 100) if staked_value > 0 else 0.0
    return Result(
        gross_fees=gross / USDC,
        discount_given=(gross - net) / USDC,
        net_fees=net / USDC,
        treasury=parts.treasury / USDC,
        buyback=parts.buyback / USDC,
        stakers=parts.stakers / USDC,
        staker_discount_pct=discount / PPM * 100,
        burn_share_pct=SPLIT.burn.burn_bps(intensity) / 100,
        staker_apr_pct=apr,
    )


def _money(x: float) -> str:
    return f"${x:,.0f}"


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--daily-volume", type=float, default=50_000_000, help="USD routed per day")
    p.add_argument("--markup", type=float, default=0.30, help="markup in percent (0.15–0.80)")
    p.add_argument("--staker-volume-share", type=float, default=0.40, help="share of volume from stakers (0–1)")
    p.add_argument("--avg-staker-stake", type=float, default=250_000, help="tokens per staking account")
    p.add_argument("--total-staked", type=float, default=100_000_000, help="tokens staked in total")
    p.add_argument("--token-price", type=float, default=0.10, help="USD per token")
    p.add_argument("--intensity", type=float, default=1.0, help="today's revenue / moving average")
    args = p.parse_args(argv)

    base = Assumptions(
        args.daily_volume, args.markup, args.staker_volume_share, args.avg_staker_stake,
        args.total_staked, args.token_price, args.intensity,
    )
    r = simulate(base)
    print(f"Daily volume {_money(base.daily_volume_usd)} at {base.markup_pct:.2f}% markup")
    print(f"  stakers' discount           {r.staker_discount_pct:.1f}% on {base.staker_volume_share:.0%} of volume")
    print(f"  gross fees / day            {_money(r.gross_fees)}")
    print(f"  discounts given / day       {_money(r.discount_given)}")
    print(f"  net protocol revenue / day  {_money(r.net_fees)}")
    print(f"    treasury (50%)            {_money(r.treasury)}")
    print(f"    buyback & burn ({r.burn_share_pct:.0f}% of engine) {_money(r.buyback)}")
    print(f"    stakers (USDC)            {_money(r.stakers)}")
    print(f"  staker APR                  {r.staker_apr_pct:.1f}% (on {_money(base.total_staked * base.token_price_usd)} staked)")
    print()
    print("Sensitivity (same settings, different daily volume):")
    print(f"  {'volume/day':>14} {'revenue/day':>12} {'buyback/yr':>12} {'stakers/yr':>12} {'APR':>7}")
    for v in (1e6, 5e6, 10e6, 50e6, 100e6):
        s = simulate(Assumptions(v, *[getattr(base, f) for f in ("markup_pct", "staker_volume_share", "avg_staker_stake", "total_staked", "token_price_usd", "usage_intensity")]))
        print(f"  {_money(v):>14} {_money(s.net_fees):>12} {_money(s.buyback * 365):>12} {_money(s.stakers * 365):>12} {s.staker_apr_pct:>6.1f}%")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
