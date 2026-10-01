# DarkQuant (OBSIDIAN-ONCHAIN)

Low-latency, privacy-preserving on-chain trading core: deterministic CLOB,
integer-exact quant indicators with volatility-adaptive fees, Zcash-style scoped
viewing keys for exchange compliance, and sealed (front-running-resistant) order flow.

Design, formulas and threat model: **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

## Layout

```
crates/
  dq-types     integer primitives (ticks, lots, market spec)
  dq-engine    price-time CLOB: STP, IOC/FOK/PostOnly, O(1) cancel, bounded matching
  dq-quant     rolling VWAP, TWAP oracle, EWMA σ, dynamic fees (no_std, integer-only)
  dq-privacy   epoch viewing keys, encrypted fill notes, sealed orders, ordering log
  dq-auth      master/session keys, scoped grants, replay window
  dq-e2e       end-to-end pipeline test
backtest/      Python backtester; bit-exact mirror of dq-quant
testdata/      Rust-generated golden vectors shared with Python
```

## Build & test

```bash
cargo test --workspace --release                       # unit, property, differential, e2e
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p dq-engine --release --example bench       # matching latency

cd backtest && pip install -e ".[dev]" && python -m pytest -q
python -m darkquant_bt [trades.csv]                     # run reference strategy
```

Regenerate golden vectors after an intentional quant change:
`DQ_BLESS=1 cargo test -p dq-quant --test golden` (then re-run the Python tests).
