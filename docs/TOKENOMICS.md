# Tenebra Router — Guard Program & Token Mechanics (Solana)

Status: **v1 core implemented and tested natively**; not yet deployed or audited.

| Piece | Where | Tests |
|---|---|---|
| Token maths (discounts, staking, rewards, splits) | `crates/tenebra-tokenomics` (`no_std`, integer-only) | 12 unit, 5 property, golden vectors |
| On-chain program (Anchor 0.31) | `solana/programs/tenebra-guard` | 7 unit (tx-layout rules) |
| Runtime tests (program + real SPL Token in a local runtime) | `solana/harness` | 13 integration |
| Python mirror + revenue simulator | `backtest/tenebra_bt/{tokenomics,revenue_sim}.py` | bit-exact parity with Rust |

---

## 1. The guarded swap

Every swap routed through Tenebra is one Solana transaction with this shape:

```
 setup ixs (create ATA, wrap SOL)          ← optional, before
 tenebra_guard::pre_swap                   ← records balances, takes input-side fee
 <one allowed router ix> (Jupiter V6)      ← the actual swap
 tenebra_guard::post_swap                  ← checks deltas, takes output-side fee
 cleanup ixs (unwrap SOL)                  ← optional, after
```

`pre_swap` reads the instructions sysvar and refuses to run unless the
transaction has exactly this shape. So the fee and the slippage check can't be
stripped off, and nothing unapproved can run inside the guarded window.

What the program enforces on every swap, independent of the router:

| Check | How | Error |
|---|---|---|
| Router is approved | program ID ∈ config allowlist (default: Jupiter V6 `JUP6Lkb…aV4`) | `RouterNotAllowed` |
| Shape is pre → 1 router ix → post | instructions-sysvar introspection; `ComputeBudget` ixs allowed anywhere | `BadLayout` |
| Not invoked via CPI | the current top-level ix must be our own `pre_swap`/`post_swap` | `NotTopLevel` |
| Slippage limit | output balance delta, net of fee, ≥ `min_out` | `SlippageExceeded` |
| No overspend | input balance delta ≤ `max_in` | `OverSpend` |
| Sanctions | signer not on the on-chain denylist | `Denylisted` |
| Fee charged, at the right rate | computed from the staker's discount curve, transferred to the fee vault in the same tx | — |
| Fee currency | fees only in configured fee mints (USDC, wSOL), on the chosen side of the swap | `BadFeeMint` |
| Markup bounds | markup ∈ [0.15%, 0.80%] (configurable, hard cap 10%) | `MarkupOutOfBounds` |

Checks are based on token balance deltas, not on parsing the router's
instruction data. They stay correct whatever route Jupiter picks, and keep
working across Jupiter upgrades.

**MEV protection.** The guard turns a sandwich into a failed transaction: if
the price is pushed past `min_out`, the whole transaction reverts and the user
loses only the network fee. Keeping the transaction out of searchers' view is a
separate off-chain job for the Tier 1 relay (not yet built). The relay adds
a read-only `jitodontfront…` account, which makes Jito's block engine reject
any bundle that places another transaction in front of it, and submits via
Jito with a tip. `min_out` comes from the depth-aware slippage model (Tier 2,
also off-chain).

## 2. Fees and the staking discount

```
fee  = ⌈ amount × markup × (1 − discount) ⌉          (one rounding, protocol-favourable)
discount(S) = min(cap, cap × √(S / S_cap))
```

This is your blueprint's `min(0.50, k·√S)` with `k = cap / √S_cap`. It's
written in terms of `S_cap` (the stake at which the cap is reached) because
that's easier to set. Defaults: cap 50% at 1,000,000 tokens, so 250,000 tokens
gives 25% off and 10,000 tokens gives 5% off. It's computed exactly as
`isqrt(⌊cap²·S/S_cap⌋)`, with no approximation.

The curve is concave, so each extra token buys less discount: large holders
are rewarded without draining revenue. Splitting stake across wallets lowers
the discount on each wallet, so it gains nothing.

**Tiers** (effective stake ≥ 10k / 100k / 1M tokens by default) are readable
on-chain, so the off-chain relay can gate Tier 1/2 privileges.
`zero_fee_tier` (default **off**) waives the markup for a tier. Note that the
blueprint's "Tier 3: zero-markup" contradicts its 50% discount cap. Turning
it on means your largest stakers pay nothing, which is the volume that matters
most to revenue. That's your call.

## 3. Staking

| Rule | Why |
|---|---|
| Deposits count for discounts/tiers only after `warmup` (default 1 day) | Stops flash-loan staking: borrow → stake → discounted swap → unstake in one tx |
| Deposits earn rewards immediately | No penalty for honest stakers |
| Unstake = request, then withdraw after `cooldown` (default 7 days); nothing is earned while cooling | Stops reward sniping around distributions |
| Denylisted addresses can't stake or claim | Compliance |

## 4. Revenue split and real yield

```
                fee vault (USDC / wSOL)
                       │  distribute()  — permissionless, once per epoch (default 1 day)
       ┌───────────────┴───────────────┐
  treasury 50%                    token engine 50%
                         ┌─────────────┴─────────────┐
                 buyback vault  burn(I)        staker reward vault  1 − burn(I)
```

```
burn(I) = clamp(base + slope × (I − 1)⁺, min, max)     defaults: base 50%, slope 20%, range 30–90%
I       = this epoch's revenue ÷ moving average of past epochs
```

So the burn share rises exactly when usage spikes, as the blueprint asks. It
is driven by on-chain revenue rather than price volatility because revenue
needs no price oracle and can only be inflated by paying real fees. (A
Pyth-based price-volatility input can be added later.)

Staker rewards are paid in the fee currency (USDC/wSOL) from real revenue.
No tokens are ever minted. The reward accounting can never owe more than the
pool received. Every rounding step goes in the pool's favour, which a property
test checks over thousands of random stake/unstake/reward sequences. That test
caught and fixed a 1-unit over-payment in the textbook "MasterChef" method.

Every split adds up exactly: treasury + buyback + stakers = fees, with no lost
units.

**Projection** (`python -m tenebra_bt.revenue_sim`; assumptions are inputs, not forecasts):

| Volume/day | Net revenue/day | Buyback/yr | Stakers/yr | APR on $10M staked |
|---|---|---|---|---|
| $1M | $2,700 | $246k | $246k | 2.5% |
| $10M | $27,000 | $2.46M | $2.46M | 24.6% |
| $50M | $135,000 | $12.3M | $12.3M | 123% |

(0.30% markup, 40% of volume from stakers at a 25% discount, normal usage.)

## 5. Compliance

- **Denylist**: up to 256 sorted addresses, binary-searched on every swap, edited
  only by `compliance_authority` (a separate key from `admin`).
- **What it does not do**: it can't screen the history of addresses it has
  never seen, or the pools Jupiter routes through. That's off-chain analytics
  (Chainalysis/TRM) in the relay, before the transaction is built. The
  on-chain list is the last line of defence, not the whole screening process.
- **Audit trail**: every swap emits a `SwapGuarded` event (user, mints, amounts,
  fee, tier, discount); every distribution emits `Distributed`.

## 6. Deliberately not built (and why)

| Blueprint item | Status | Reason |
|---|---|---|
| Buyback execution (swap → burn) | v2 | Needs a program-signed Jupiter call with an oracle-bounded minimum price. Until then the buyback share accumulates visibly in the buyback vault, rather than giving a hot key discretionary access to the funds |
| Token as collateral to borrow "flash liquidity" | **Not recommended** | Lending against your own token is the classic death spiral: Mango Markets (Oct 2022, ~$114M) was drained by pumping the collateral token and borrowing against it. And flash loans need no collateral at all, since they are repaid in the same transaction. If wanted: offer flash loans from protocol liquidity for a fee, gated by tier |
| "1,000 Sub-Wallet Shield" | Not in repo | Splitting flow across many wallets conflicts with pillar B: institutions' compliance teams must be able to link sub-wallets to one owner. If built, it should sit under the viewing-key disclosure model |
| Tier 1 private relay, Tier 2 depth-aware slippage | Next | Off-chain services; the program already exposes tiers and enforces `min_out` |

## 7. Open decisions (yours)

1. **Token name/ticker.** The blueprint says `$OBSIDIAN`, but Obsidian Finance
   already trades as OBS. The code is name-agnostic: the stake mint is just a
   config value.
2. **Legal review before launch.** Paying holders a share of protocol revenue
   (the staker reward vault) is the feature most likely to make a token a
   security under US law (Howey: expected profits from others' efforts). Burn
   and distribution differ in treatment by jurisdiction. Get securities counsel
   before mainnet; this note is not legal advice.
3. `zero_fee_tier` on or off (§2).
4. Parameter values: defaults are in `solana/harness/tests/guard.rs::economics()`.

## 8. Build, test, deploy

```bash
cargo test -p tenebra-tokenomics --release            # maths (core workspace)
cd solana && cargo test -p tenebra-guard --lib        # tx-layout rules
cd solana/harness && cargo test                       # program in a local Solana runtime
cd backtest && python -m pytest -q                    # Python parity
python -m tenebra_bt.revenue_sim --daily-volume 10000000 --markup 0.25
```

Deploying (not done here):
- Install Solana CLI ≥ 2.2 and Anchor 0.31.1, then run `anchor keys sync`. It
  replaces the placeholder program ID `HueCcTw…h3KC` with your deploy key.
  Then `anchor build`.
- `solana/Cargo.lock` pins `blake3 = 1.5.5`, and `.cargo/config.toml` makes
  Cargo pick dependency versions that build with Solana's compiler (rustc 1.84).
  Keep both when updating dependencies.
- The runtime tests run the program natively, not as compiled on-chain
  bytecode. Before mainnet: run the same tests against the `anchor build`
  output (LiteSVM), measure compute units, and get an independent audit.
