"""Tenebra backtester.

`fixedpoint` and `indicators` are bit-exact mirrors of `crates/tenebra-quant`
(verified against `testdata/quant_golden.json`). Strategies backtested here
see exactly the VWAP/TWAP/σ/fee values the on-chain engine will compute.
"""

from .fixedpoint import PIPS_DEN, SCALE, mul_div_ceil, mul_div_floor
from .indicators import EwmaVol, FeeSchedule, QuantError, RollingVwap, TwapOracle

__all__ = [
    "SCALE",
    "PIPS_DEN",
    "mul_div_floor",
    "mul_div_ceil",
    "RollingVwap",
    "TwapOracle",
    "EwmaVol",
    "FeeSchedule",
    "QuantError",
]
