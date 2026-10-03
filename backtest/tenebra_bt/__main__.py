"""`python -m tenebra_bt [trades.csv]` — run the reference strategy.

CSV columns: ts_ms,price_ticks,qty_lots (header optional). With no argument,
replays the trades from testdata/quant_golden.json.
"""

from __future__ import annotations

import csv
import json
import sys
from pathlib import Path

from .backtester import Trade, run
from .strategies import VwapReversion

GOLDEN = Path(__file__).resolve().parents[2] / "testdata" / "quant_golden.json"


def load_csv(path: str) -> list[Trade]:
    out = []
    with open(path, newline="") as f:
        for row in csv.reader(f):
            if not row or not row[0].strip().isdigit():
                continue
            out.append(Trade(int(row[0]), int(row[1]), int(row[2])))
    return out


def main(argv: list[str]) -> int:
    if len(argv) > 1:
        trades = load_csv(argv[1])
    else:
        trades = [Trade(*t) for t in json.loads(GOLDEN.read_text())["trades"]]
    res = run(trades, VwapReversion())
    for k, v in res.summary().items():
        print(f"{k:>15}  {v}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
