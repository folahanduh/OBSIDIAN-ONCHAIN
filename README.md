# Tenebra

Low-latency, privacy-preserving on-chain trading core: deterministic CLOB,
integer-exact quant indicators with volatility-adaptive fees, Zcash-style scoped
viewing keys for exchange compliance, and sealed (front-running-resistant) order flow.

Two products share this repo:

- **Tenebra Router (Solana)**: an on-chain guard around Jupiter swaps (approved
  router, enforced slippage limit and fee, sanctions denylist) plus staking with
  real-yield revenue sharing and volume-driven buyback.
  See **[docs/TOKENOMICS.md](docs/TOKENOMICS.md)**.
- **Tenebra core**: the private, low-latency order-book engine.
  See **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

## Layout

```
crates/
  tenebra-types      integer primitives (ticks, lots, market spec)
  tenebra-engine     price-time CLOB: STP, IOC/FOK/PostOnly, O(1) cancel, bounded matching
  tenebra-quant      rolling VWAP, TWAP oracle, EWMA σ, dynamic fees (no_std, integer-only)
  tenebra-privacy    epoch viewing keys, encrypted fill notes, sealed orders, ordering log
  tenebra-auth       master/session keys, scoped grants, replay window
  tenebra-e2e        end-to-end pipeline test
  tenebra-tokenomics fee discount, staking, real-yield and revenue-split maths (no_std)
solana/
  programs/tenebra-guard   Anchor program: swap guard, staking, distribution
  harness/                 runs the program in a local Solana runtime (own lockfile)
backtest/            Python (tenebra_bt): backtester, tokenomics mirror, revenue simulator
testdata/            Rust-generated golden vectors shared with Python
```

## Build & test

```bash
cargo test --workspace --release                        # unit, property, differential, e2e
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p tenebra-engine --release --example bench   # matching latency

cd backtest && pip install -e ".[dev]" && python -m pytest -q
python -m tenebra_bt [trades.csv]                       # run reference strategy
python -m tenebra_bt.revenue_sim --daily-volume 10000000 # fee/yield projection

cd solana && cargo test -p tenebra-guard --lib          # guard layout rules
cd solana/harness && cargo test                         # program + SPL Token, local runtime
```

Regenerate golden vectors after an intentional maths change:
`TENEBRA_BLESS=1 cargo test -p tenebra-quant -p tenebra-tokenomics --test golden`
(then re-run the Python tests).
