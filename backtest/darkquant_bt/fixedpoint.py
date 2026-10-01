"""Integer fixed-point helpers mirroring `dq_quant::{mul_div_floor, mul_div_ceil}`.

Python ints are unbounded, so u128 overflow is emulated explicitly: wherever
Rust returns `None`, these return `None`. That keeps edge-case behaviour (not
just happy-path values) identical across languages.
"""

from __future__ import annotations

SCALE = 1_000_000_000
PIPS_DEN = 1_000_000
U128_MAX = (1 << 128) - 1


def _u128(x: int) -> int | None:
    return x if 0 <= x <= U128_MAX else None


def mul_div_floor(a: int, b: int, d: int) -> int | None:
    """Exact floor(a*b/d) with Rust's overflow semantics (a = q*d + r)."""
    if d == 0:
        return None
    q, r = divmod(a, d)
    qb = _u128(q * b)
    rb = _u128(r * b)
    if qb is None or rb is None:
        return None
    return _u128(qb + rb // d)


def mul_div_ceil(a: int, b: int, d: int) -> int | None:
    f = mul_div_floor(a, b, d)
    if f is None:
        return None
    rb = _u128((a % d) * b)
    if rb is None:
        return None
    return f if rb % d == 0 else _u128(f + 1)
