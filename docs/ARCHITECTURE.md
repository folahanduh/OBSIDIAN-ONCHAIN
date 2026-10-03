# Tenebra — Architecture

Status: **v0 core libraries**. Implemented and tested: matching engine, quant
indicators + dynamic fees, privacy/compliance primitives, session-key auth, and a
Python backtester with bit-exact parity. Not yet built: sequencer node, risk
engine, WebSocket gateway, terminal UI, settlement contracts (see §9).

---

## 1. System overview

```
 ┌──────────── Terminal (TS/React) ────────────┐
 │ session key (WebCrypto Ed25519, non-extract) │
 │ sign action → seal to sequencer X25519 key   │
 └───────────────┬──────────────────────────────┘
                 │ WSS (sealed bytes only)
 ┌───────────────▼──────────────┐
 │ Gateway (stateless, N×)      │  rate limit, size check, fan-in; cannot read orders
 └───────────────┬──────────────┘
                 │ SPSC ring
 ┌───────────────▼─────────────────────────────────────────────────────┐
 │ Sequencer (single-threaded deterministic state machine)            │
 │  1. append ciphertext → OrderingLog, sign (height, head)  ◄─ commit │
 │  2. open(sealed) → ActionEnvelope::decode                ◄─ reveal  │
 │  3. Authorizer::authorize (session scope, replay window)            │
 │  4. risk check (balances/margin)                  [not yet built]   │
 │  5. OrderBook::place / cancel  → Events                             │
 │  6. FeeSchedule (σ from EwmaVol) → per-fill fees                    │
 │  7. encrypt_note(fill, owner's epoch viewing key) → EncryptedNote   │
 │  8. post-batch: TwapOracle.update(mid), EwmaVol.on_sample(mid)      │
 └───────┬───────────────────────────────┬─────────────────────────────┘
         │ batch: ordering head,         │ private feeds (own fills via
         │ state root, notes, public     │ session-authenticated WS)
         ▼ market data (L2/trades)       ▼
   DA layer / L1 settlement        Compliance portal (CEX): decrypts
                                    only disclosed epochs of one account
```

Crates:

| Crate | Role | Key types |
|---|---|---|
| `tenebra-types` | integer primitives, market spec | `Price`, `Qty`, `MarketSpec` |
| `tenebra-engine` | CLOB matching | `OrderBook`, `OrderRequest`, `Event` |
| `tenebra-quant` | VWAP/TWAP/σ/fees, `no_std` | `RollingVwap`, `TwapOracle`, `EwmaVol`, `FeeSchedule` |
| `tenebra-privacy` | viewing keys, notes, sealed orders, ordering log | `ViewingSeed`, `EncryptedNote`, `seal`/`open`, `OrderingLog` |
| `tenebra-auth` | master/session keys, replay protection | `Authorizer`, `SessionGrant`, `ActionEnvelope` |
| `tenebra-e2e` | full-pipeline tests | — |
| `backtest/` | Python mirror + backtester | `tenebra_bt` |

---

## 2. Determinism rules (consensus path)

1. **Integers only.** Prices in ticks (`u64`), sizes in lots (`u64`), notional in
   quote atoms (`u128`). Notional of any valid order `< 2^108` (validated in
   `MarketSpec::validate`), so `notional × fee_pips` cannot overflow `u128`.
2. **Explicit rounding.** Every division documents floor/ceil. Fees round
   *against* the payer (taker fee ceil, rebate floor); the venue never loses
   to rounding.
3. **No hash-map iteration** affects outputs. Hash maps are lookup-only; every
   ordered traversal goes through `BTreeMap` or intrusive lists.
4. **Overflow halts, never wraps.** Release profile sets `overflow-checks = true`;
   indicator code uses checked math and returns errors instead of wrong values.
5. **Bounded work per input.** `max_match_steps` caps resting orders a single
   taker can touch (fills + STP cancels). Every ring buffer is fixed-size.
6. **Cross-language parity.** `testdata/quant_golden.json` is produced by Rust and
   reproduced bit-for-bit by Python in CI.

---

## 3. Matching engine (`tenebra-engine`)

**Structure.** Per side, `BTreeMap<u64, Level>` where asks key on `price` and bids
on `!price`, so the best level on both sides is `first_entry()` and the cross test
is one comparison `level_key <= side_key(opposite, limit)`. Orders live in a slab
(`Vec<Node>` + free list) with two intrusive doubly-linked lists per node:
price-level FIFO and per-account. `HashMap<OrderId, u32>` indexes the slab.

| Op | Complexity |
|---|---|
| place (no cross) | O(log L) |
| match k makers | O(k + log L) |
| cancel by id | O(1) slab + O(log L) level lookup |
| cancel_all(account) | O(k_account · log L) |
| FOK pre-check | O(k) non-mutating dry run (mirrors `match_taker` step-for-step) |

**Semantics.**
- TIF: `Gtc`, `Ioc`, `Fok`, `PostOnly` (reject-on-cross). There is **no unbounded
  market order**: every order carries a limit; "market" = IOC with a protective
  limit, which caps slippage and closes the thin-book sweep vector.
- Fills execute at the maker's price; price-time priority.
- STP: `CancelTaker` / `CancelMaker` / `CancelBoth`. Self-matching is impossible.
- Rejections (`Rejected`) happen before an id is assigned and never mutate state.
- `cancel` returns `NotFound` for both unknown and foreign ids — no oracle for
  other accounts' order ids.
- Capacity: global `max_resting_orders`, per-account `max_open_orders_per_account`.
  Remainders that would exceed them are cancelled with an explicit reason.

**Verification.** 16 unit tests + a differential property test that checks the
exact event stream and resulting book against a naive O(n) reference
implementation (2 000 random sequences × ≤120 ops, every op validated with a full
structural invariant check), + a quantity-conservation property. Three injected
mutants (FOK boundary, STP mode, capacity off-by-one) are all caught.

**Latency** (`cargo run -p tenebra-engine --release --example bench`, single thread,
2 M mixed ops, ~280 k resting orders, shared cloud container):

| p50 | p90 | p99 | p99.9 | throughput |
|---|---|---|---|---|
| ~0.2 µs | ~0.6 µs | ~1.0 µs | ~4.7 µs | ~2.5 M ops/s |

`OrderBook::reserve_capacity()` must be called at startup: without it, id-index
rehashing produced a 13.8 ms tail spike in the same benchmark. Remaining ms-level
maxima are consistent with container preemption; production pins the sequencer to
an isolated core. Next optimisation if needed: replace the `BTreeMap` with a
tick-indexed array ladder around mid (O(1) level access, cache-friendly).

---

## 4. Quant layer (`tenebra-quant`, mirrored in `backtest/tenebra_bt`)

Fixed point: `S = 10^9`. Prices `p` in ticks, quantities `q` in lots.

**Exact mul-div** (no 256-bit intermediates): `a = q·d + r ⇒ ⌊ab/d⌋ = q·b + ⌊r·b/d⌋`,
returns `None` instead of a wrong answer on overflow.

**Rolling VWAP** over `n` buckets of width `B` ms (current bucket included):

$$\text{VWAP}_t = \left\lfloor \frac{S \cdot \sum_{i \in W_t} p_i q_i}{\sum_{i \in W_t} q_i} \right\rfloor,\quad W_t = \{ i : \lfloor t_i/B \rfloor \ge \lfloor t/B \rfloor - n + 1 \}$$

O(1) amortised update; integer sums, so eviction is exact (no float drift).

**TWAP oracle.** Observations `(tₖ, Cₖ, pₖ)` with `Cₖ = Cₖ₋₁ + pₖ₋₁ (tₖ − tₖ₋₁)` and
`pₖ` the price in effect *after* `tₖ`, so `C(t) = Cₖ + pₖ (t − tₖ)` is exact (no
interpolation error).

$$\text{TWAP}[t-w, t] = \left\lfloor \frac{S\,(C(t) - C(t-w))}{w} \right\rfloor$$

Feed it the post-batch mid, once per batch. A price overwritten within the same
timestamp accrues zero weight, so intra-block spikes cannot move it; moving the
TWAP by δ for window w requires holding the price displaced for ~δ·w — i.e.
capital at risk across batches.

**EWMA volatility** (RiskMetrics), sampled at fixed cadence, `D = 2^16`:

$$r_t = \min\!\left(\left\lfloor \frac{S\,|p_t - p_{t-1}|}{p_{t-1}} \right\rfloor, S\right),\qquad
\sigma^2_t = \left\lfloor \frac{\lambda\,\sigma^2_{t-1} + (D-\lambda)\,r_t^2}{D} \right\rfloor,\qquad
\sigma_t = \lfloor\sqrt{\sigma^2_t}\rfloor$$

The 100 % clamp bounds one bad print's influence and guarantees `σ² ≤ 10^18`
(no overflow possible). `λ = 61604/65536 ≈ 0.94`: half-life ≈ 11 samples.

**Dynamic taker fee** (pips, 1 pip = 10⁻⁶):

$$f_{\text{taker}}(\sigma) = \operatorname{clamp}\!\left(f_{\text{base}} + \left\lfloor \frac{k\,(\sigma - \sigma_{\text{ref}})^+}{\sigma_{\text{ref}}} \right\rfloor,\ f_{\min},\ f_{\max}\right)$$

Rationale: adverse selection against makers rises with σ; charging takers more in
those regimes funds maker rebates and dampens toxic flow. Invariant enforced in
`validate()`: maker rebate ≤ `f_min`, so every fill is venue-non-negative and
colluding accounts cannot farm rebates. Golden vectors include a regime switch
where σ rises roughly 7× and the taker fee moves 250 → 887 → 1000 (cap) pips.

---

## 5. Privacy & compliance (`tenebra-privacy`)

### 5.1 Key hierarchy

```
Master key (Ed25519, cold)      — signs grants, withdrawals, revocations, VK registration
Session keys (Ed25519, hot)     — sign trading actions only (tenebra-auth)
ViewingSeed (32 B)              — never leaves the user
  └─ ivk_e = BLAKE3-derive("…epoch viewing key v1", seed ‖ account ‖ e)  (X25519)
```

Three unrelated key classes: a viewing key can never sign, a session key can never
withdraw (§6), and nothing derives spend authority from view authority.

**Why hardened derivation.** Additive (BIP32-style) derivation would let the
sequencer compute future epoch public keys, but `child_sk + master_pk` reveals
`master_sk` — a one-day disclosure would expose all history. We pay instead with
pre-registration: users register epoch public keys in batches (≤ 64 per
master-signed `RegisterViewingKeys`); the sequencer rejects orders from accounts
without a registered key for the current epoch (to be enforced in the node).
Keys are validated against small-order points at registration (`is_valid`).

### 5.2 Fill notes

Per fill, per counterparty:

```
note = (version, account, market, side, price, qty, fee, fill_seq, ts, rseed[32])   94 bytes
cm   = BLAKE3-derive("…note commitment v1", note)            hiding via rseed
k    = BLAKE3-derive("…note encryption key v1", X25519(esk, ivk_pk) ‖ epk ‖ ivk_pk)
ct   = ChaCha20-Poly1305(k, nonce = 0, aad = epoch ‖ cm)       one-time key ⇒ fixed nonce safe
on-chain: (epoch, epk, cm, ct[94], tag[16])                     fixed size, no length leak
```

Decryption recomputes `cm` and compares in constant time, which makes the scheme
key-committing: an auditor cannot be shown a plaintext different from the one
committed on-chain. `encrypt_note` refuses keys of the wrong account or epoch
(enforced at the sender, so disclosure scoping holds by construction).

### 5.3 Disclosure flow (Zcash-style viewing keys, scoped)

1. CEX requests audit for account A over dates `[d₁, d₂]`.
2. User builds `ComplianceDisclosure::from_seed_range(seed, A, e₁, e₂)` and seals it
   to the CEX's X25519 key with `SealPurpose::Disclosure` (separate KDF context
   from order sealing — no cross-protocol confusion).
3. CEX `scan()`s published notes; it learns exactly A's fills in `[e₁, e₂]` —
   nothing about other accounts or other epochs.

Optional policy (governance decision, not implemented): mandatory escrow, i.e.
additionally encrypt each note to a regulator/escrow key with threshold custody.

### 5.4 Trust model — what is private from whom (v0)

| Observer | Order contents pre-execution | Fills | Balances |
|---|---|---|---|
| Public / MEV searchers | hidden (sealed) | hidden (notes) | hidden (state root only) |
| Gateway operator | hidden (sealed) | hidden | hidden |
| Sequencer operator | **visible after commit** | visible | visible |
| CEX with disclosure | — | disclosed epochs of one account | derivable for those epochs |

v0 removes front-running by *the public and by reordering*, not operator
visibility. Hardening path, in order: (1) sequencer key in a TEE with remote
attestation; (2) threshold decryption committee (DKG over X25519/BLS; decrypt
batch only after the ordering head is signed by ≥ t members); (3) ZK validity
proofs of the state transition so the operator holds data but cannot misstate it.

---

## 6. MEV protection & sequencing

- Clients `seal(Order, signed_action, sequencer_pk)`; payload padded to 256-byte
  blocks (order types are indistinguishable by size).
- The sequencer appends each ciphertext to the `OrderingLog` hash chain
  `headₙ = H(headₙ₋₁ ‖ n ‖ H(ciphertext))`, signs `(n, headₙ)` and returns a receipt
  **before** decrypting. Reordering on content requires two signed receipts for
  the same height — publicly provable equivocation (slashable).
- Execution follows committed order. Anyone holding the published ciphertexts
  can recompute the head (`OrderingLog::replay`).

Residual risks: censorship/delay of specific ciphertexts (mitigation: inclusion
lists, forced inclusion via L1), metadata (IP/timing; mitigation: gateway mixing),
and latency races between ciphertext arrival times (mitigation: frequent batch
auctions per 50–100 ms window, uniform-price clearing — candidate v1 feature).

---

## 7. Session-key authorization (`tenebra-auth`)

`SessionGrant` (master-signed): `{chain_id, account, session_pk, markets[≤32],
max_order_notional, can_place, can_cancel, valid_from, expires_at (TTL ≤ 7 d),
grant_nonce}`.

Boundaries:
- **Type-level**: `SessionAction ∈ {Place, Cancel, CancelAll}`. There is no
  withdraw/transfer variant; those are `MasterAction`s verified against the master key.
- **Scope**: market allow-list, per-order notional cap (checked math; overflow ⇒
  denied), place/cancel flags, validity window.
- **Owner injection**: the engine's `OrderRequest.owner` comes from the
  authenticated account, never from client bytes.
- **Signatures**: `verify_strict` (rejects malleable S and small-order points);
  weak public keys rejected at registration; domain-separated payloads per
  message type; `chain_id` in every payload.
- **Replay**: actions bound to `grant_nonce`; grant nonces strictly increase per
  account, so re-issuing a grant for the same session key cannot resurrect old
  envelopes. Per-session 64-wide sliding window (RFC 4303) tolerates out-of-order
  arrival across parallel connections. Nonces are consumed only after a valid
  signature (no nonce-burning DoS), and consumed even if the action is then denied.
- **Revocation**: `RevokeSessions{min_grant_nonce}` drops all older sessions.
- **Canonical wire format**: fixed little-endian layout, strict decode (exact
  length, valid tags, no trailing bytes).

---

## 8. Threat model

| Threat | Mitigation | Code | Residual |
|---|---|---|---|
| Front-running / sandwiching | sealed orders + commit-before-decrypt | `seal`, `OrderingLog` | operator visibility (§5.4) |
| Thin-book sweep / fat finger | mandatory limit price; notional cap per session | engine, `Permissions` | — |
| Sweep-the-book compute DoS | `max_match_steps` | `match_taker` | — |
| Memory DoS | global + per-account resting caps; bounded rings | `BookConfig` | — |
| Wash trading / rebate farming | STP always on; rebate ≤ min taker fee | `StpMode`, `FeeSchedule::validate` | multi-account collusion still pays ≥ 0 net fee |
| Order-id probing | uniform `NotFound` on foreign cancel | `cancel` | — |
| TWAP manipulation | time-weighted, zero weight for intra-timestamp prints | `TwapOracle` | sustained multi-batch manipulation (costly) |
| σ manipulation to move fees | per-sample return clamp; fixed-cadence sampling of mid | `EwmaVol` | |
| Session key theft | no withdraw capability; scoped; TTL; revocation | `tenebra-auth` | trading losses within cap until revoked |
| Signature replay (cross-chain/type/grant) | chain_id, domain tags, grant binding, replay window | `tenebra-auth` | — |
| Viewing key over-disclosure | per-epoch hardened keys; disclosure bounded to range | `keys.rs` | disclosed epochs are permanently readable by recipient |
| Ciphertext tampering / plaintext substitution | AEAD + commitment recheck (key-committing) | `note.rs` | — |
| Low-order X25519 keys | `was_contributory` on every DH; `is_valid` at registration | `kdf`, `keys.rs` | — |
| Commitment brute-force (low-entropy fills) | 32-byte `rseed` in every note | `FillNote` | — |
| Length side channel | fixed-size notes; 256-byte padded envelopes | `note.rs`, `seal.rs` | batch size/timing |
| Integer overflow | spec-level notional bound; checked math; overflow-checks in release | all | — |

---

## 9. Roadmap

1. **`tenebra-node` sequencer** — batch loop wiring the crates (as in `tenebra-e2e`), spot
   balance ledger with hold-on-rest, epoch-key presence check, state root
   (sparse Merkle over balances + note commitments), signed batch headers.
2. **Risk engine** — pre-trade balance/margin checks, then perps: mark price from
   `TwapOracle`, funding, liquidation engine.
3. **Gateway** — tokio + `tokio-tungstenite`; binary frames carrying sealed
   envelopes; per-IP/per-account token buckets; SPSC handoff to the sequencer.
4. **Terminal** — React/TS; WebCrypto Ed25519 session keys; X25519 sealing in
   WASM (compile `tenebra-privacy` + `tenebra-auth` encoders to `wasm32`, one source of truth).
5. **Settlement** — DA posting of (ordering head, state root, notes); L1/L2
   bridge contract with withdrawal proofs; forced-inclusion path.
6. **Hyperliquid** — integrate as an external liquidity/hedging venue via its API
   (builder codes for fee share) rather than as the core engine: its public order
   flow is incompatible with the privacy model above.
7. **Threshold decryption committee** and **ZK validity proofs** (§5.4).
