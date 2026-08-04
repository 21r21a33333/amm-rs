# amm-rs — AMM Library Design

**Status:** design approved section-by-section; pending full-spec review.
**Date:** 2026-08-04

**Goal:** A standalone, wei-exact, `no_std`-capable Rust library for quoting swaps across AMM protocols behind one open, object-safe trait — the "one-stop AMM library" any crypto dev can use and extend without forking.

**Architecture:** A pure-math core crate (`amm-core`) exposing a minimal object-safe `Pool` trait plus opt-in extension traits, with per-protocol quoters implemented as pure functions over pool state; an optional I/O crate (`amm-rpc`) that fetches on-chain state via `alloy`. Amounts and prices are typed value objects that carry their token identity. Execution (calldata) and LP-liquidity math are explicitly deferred to later phases.

**Tech stack:** Rust edition 2024, `alloy-primitives` (`U256`/`I256`/`ruint`) for the numeric substrate, `uniswap_v3_math` and `curve-math` reused for proven protocol math, `thiserror` for errors, `alloy` (provider/contract) behind the `amm-rpc` crate, `proptest` for property tests.

---

## 1. Goal & non-goals

### Goals
- One **open, object-safe** trait (`Pool`) that a third party can implement for their own AMM in their own crate — no fork required.
- **Wei-exact** quoting: every quoter reproduces the on-chain contract's output to the wei, proven by golden vectors.
- **Typed** amounts and prices that carry token identity, so token-mixups and wrong-direction quotes are `Result` errors, not silent wrong numbers.
- **`no_std + alloc`-capable** pure-math core, so it runs off-chain, in wasm, and in constrained/enclave targets.
- **Chain-agnostic core, EVM-first**: the type model does not hardcode EVM; only the adapters do.
- Rich but layered capability surface (exact-out, spot price, price impact, limits, slippage guards, multi-hop path quoting) without bloating the core trait.

### Non-goals (this phase)
- **Execution / calldata / transaction building** — deferred to a later `amm-execution` phase.
- **LP-liquidity math** (add/remove/mint/burn, impermanent loss) — deferred to a later `LiquidityMath` extension.
- **Routing / path-finding** — the library provides `quote_path` composition but not a route optimizer.
- **RFQ / oracle venues** (Hashflow, 0x RFQ, GMX GLP) — not constant-function AMMs; out of scope for the `Pool` contract.
- **Non-EVM adapters** — the core stays chain-agnostic-ready, but only EVM protocols are implemented.

---

## 2. Locked design decisions

| # | Decision | Choice | Rationale |
|---|---|---|---|
| 1 | Numeric model | Wei-exact, **U256-native** raw amounts | Matches the chain; makes differential tests meaningful |
| 2 | Execution depth | **Quoting core first**; execution a later phase | Ship a trustworthy quoting library first |
| 3 | Packaging | **New standalone repo** (`amm-rs`); arb-router becomes a consumer | Community library; single source of truth |
| 4 | Data / I/O | **Pure-math core + optional `amm-rpc`** (bring-your-own-data) | Keeps the core dependency-light and `no_std` |
| 5 | Chain scope | **Chain-agnostic core, EVM-first** | Opaque ids + U256 don't block a future non-EVM adapter |
| 6 | Architecture | **Minimal object-safe core + extension traits** | Open to third-party pools; `Box<dyn Pool>` works |
| 7 | Trait namespace | **`traits/`** | States its content (per naming standard) |
| 8 | Protocol-impl namespace | **`protocols/<family>/`** | States its content; avoids overloading "exchange" |
| 9 | Math vs I/O layout | **Split across crates by layer** | Keeps `amm-core` network-free and `no_std` |
| 10 | Error model | **`Result<_, QuoteError>`**, math-only; I/O errors separate | Diagnostics; reusable off-chain; sans-io core |
| 11 | Amount type | **Typed `AssetAmount { asset, raw }`** (field-based) | Token-identity safety; keeps trait object-safe |
| 12 | Price type | **Directional `Price { base, quote, ratio }`** | Unambiguous, invertible (Uniswap SDK convention) |
| 13 | LP-liquidity math | **Deferred** to a later phase | Keeps v1 swap-focused and shippable |
| 14 | v1 pool families | **Port-only**: Uni V2/V3/V4, Curve, Aerodrome/Solidly | Ship the architecture with already-trusted math |

### 2a. Post-research refinements (2026-08-04 revalidation)

> **Superseded 2026-08-04:** `no_std` was subsequently **dropped** — `amm-core` is plain `std` (see the plan's *Phase-2 update*). Every `no_std`/`--no-default-features` reference in this spec is obsolete. `Ratio` uses `num_rational::BigRational`; `amm-rpc` uses alloy's native multicall (no hand-rolled provider/multicall/retry).

Applied after re-auditing the plan against the SDK/ecosystem research and alloy/serde/tokio idioms:

- **R1 — one canonical core error.** `QuoteError` is the single `amm-core` math error (covers quote + price + ratio); `amm-rpc::RpcError` wraps it via `#[from]`. No scattered per-op errors.
- **R2 — structured `AssetId`.** `AssetId { chain: ChainId(u64), token: B256 }` (`Copy`, `Hash`, `Ord`), `FromStr`/`Display` for `"chainid:0xaddr"`. Cheap in hot loops; chain-agnostic (B256 holds any chain's address). Decimals stay in `TokenMeta`.
- **R3 — typed `Bps` + `Slippage` value object.** No bare `u32` bps anywhere; `Slippage::from_bps` with directional `min_amount_out`/`max_amount_in` (Balancer pattern).
- **R4 — `AssetAmount` value object.** `new`/`from_decimal` constructors, checked same-asset `checked_add`/`checked_sub`, `Copy`.
- **R5 — builders + encoding conversions.** Builders for many-field V3/V4 pools; `From`/`TryFrom` between `Price` and `sqrtPriceX96`/`tick` (the lingua-franca differentiator).
- **R6 — dropped `Pair`.** Direction carried by `AssetAmount` + `to`.
- **R7 — reuse decision.** Reuse `uniswap_v3_math` + `curve-math` for swap math; `uniswap-sdk-core-rust` evaluated and rejected for primitives (generic `CurrencyAmount<T>` breaks object-safety; `BigInt` backing fights `no_std` fixed-width). `Ratio` built on `ruint`.
- **R8 — object-safe async.** `StateSource` uses `async_trait` to stay `dyn`-safe, avoiding amms-rs's raw-AFIT non-object-safety.

---

## 3. Naming conventions (the standard this repo follows)

1. **A directory name is a namespace that states its exact contents.** No vague buckets (`math`, `utils`, `helpers`).
2. **Layers:** `primitives/` (domain value types) → `traits/` (the library's trait definitions) → `protocols/<family>/` (per-protocol `Pool` impls) → `slippage/`, `path/` (stateless helpers).
3. **Role-in-filename** within a protocol family: the pure quoter is named for its variant (`v2.rs`, `v3.rs`, `v4.rs`, `pool.rs`, `stable.rs`); the I/O fetch side lives in `amm-rpc` under the mirror path.
4. **Shared code lives at the level where it is shared, and the parent `mod.rs` documents it.**

---

## 4. Workspace & crate layout

```
amm-rs/                        # workspace · MIT OR Apache-2.0 · edition 2024
├── amm-core/                  # no_std + alloc · ZERO network deps · the heart
│  └── src/
│     ├── primitives/
│     │  ├── asset.rs          # AssetId, ChainId, AssetAmount, Pair, TokenMeta
│     │  ├── ratio.rs          # Ratio (exact rational)
│     │  ├── price.rs          # Price { base, quote, ratio }
│     │  └── pool.rs           # PoolId, ExchangeId, PoolKey, PoolKind
│     ├── traits/
│     │  ├── pool.rs           # object-safe core Pool trait
│     │  ├── exact_out.rs      # ExactOut
│     │  ├── pricing.rs        # Pricing (spot_price, price_impact_bps)
│     │  ├── introspect.rs     # Introspect (fee_bps, reserve, kind)
│     │  └── limits.rs         # Limits (max_amount_in, quote_with_limit)
│     ├── protocols/
│     │  ├── uniswap/          # v2.rs · v3.rs · v4.rs
│     │  ├── curve/            # pool.rs
│     │  └── aerodrome/        # volatile.rs · stable.rs · slipstream.rs
│     ├── slippage/            # min_amount_out · max_amount_in · compound_tolerance_bps
│     ├── path/                # Hop, quote_path
│     └── error.rs             # QuoteError (math-only)
└── amm-rpc/                   # feature-gated crate · std + async · alloy + multicall
   └── src/
      ├── provider.rs          # alloy provider wiring
      ├── multicall.rs
      ├── retry.rs
      ├── source.rs            # StateSource trait + RpcError
      └── protocols/           # per-protocol discover/refresh (mirror of amm-core/protocols)
         ├── uniswap/          # v2.rs · v3.rs · v4.rs
         ├── curve/
         └── aerodrome/
```

Notes:
- Family quoters are pure math → they live in `amm-core/protocols`. Only chain-reading/decoding lives in `amm-rpc`.
- Shared fetch helpers (address parsing, `call`, error-wrapping) live at the `amm-rpc` level they're shared, documented in its `mod.rs`.
- The old `Amount ↔ U256` converter disappears: `AssetAmount.raw` **is** `U256`.

---

## 5. Primitives

```rust
// primitives/asset.rs
pub struct ChainId(pub u64);                              // e.g. 1 = ethereum; Copy
pub struct AssetId { pub chain: ChainId, pub token: B256 } // Copy identity; B256 holds any chain's
                                                          // address (EVM left-padded, Solana 32-byte)
//   FromStr/Display give the human "chainid:0xaddr" form. Decimals live in TokenMeta, NOT identity,
//   so an AssetId can be built without first fetching metadata.
pub struct AssetAmount { pub asset: AssetId, pub raw: U256 }  // Copy value object; carries its token
impl AssetAmount {
    pub fn new(asset: AssetId, raw: U256) -> Self;
    pub fn from_decimal(asset: AssetId, decimals: u8, s: &str) -> Result<Self, QuoteError>;
    pub fn checked_add(self, other: Self) -> Result<Self, QuoteError>; // Err(AssetMismatch) if assets differ
    pub fn checked_sub(self, other: Self) -> Result<Self, QuoteError>;
}
pub struct TokenMeta { pub decimals: u8, pub symbol: String }
// (No `Pair` type — direction is carried by AssetAmount + `to: AssetId`.)

// primitives/ratio.rs — exact rational, the price lingua franca
pub struct Ratio { /* reduced big-integer num/den; U512 scratch for cross-multiply */ }
impl Ratio {
    pub fn new(num: U256, den: U256) -> Option<Ratio>;    // None if den == 0
    pub fn invert(self) -> Option<Ratio>;
    pub fn cmp(&self, other: &Ratio) -> core::cmp::Ordering; // cross-multiply, no division
}
// Protocol adapters may construct via wider inputs (e.g. sqrtPriceX96² / Q192, which
// exceeds U256 before reduction); the internal width (reduced U256 vs U512 throughout)
// is finalized in the implementation plan (see §15).

pub enum Rounding { Down, Up, HalfUp }   // used only at the decimal-rendering boundary
pub struct Bps(pub u16);                 // typed basis points; replaces every bare u32 bps param

// primitives/price.rs — directional, invertible exchange rate
pub struct Price { pub base: AssetId, pub quote: AssetId, ratio: Ratio }
impl Price {
    pub fn invert(self) -> Price;                                   // swaps base/quote + ratio
    pub fn quote(&self, input: &AssetAmount) -> Result<AssetAmount, QuoteError>; // input.asset must == base
    pub fn compose(self, next: Price) -> Result<Price, QuoteError>; // self.quote must == next.base
    pub fn to_significant(&self, digits: u8, meta_base: &TokenMeta, meta_quote: &TokenMeta,
                          rounding: Rounding) -> String;            // only lossy edge; explicit rounding
}

// primitives/pool.rs
pub struct PoolId(String);
pub struct ExchangeId(String);
pub struct PoolKey { pub exchange: ExchangeId, pub chain: ChainId,
                     pub address: String, pub assets: Vec<AssetId>, pub fee_bps: Option<u32> }
#[non_exhaustive]
pub enum PoolKind { UniswapV2, UniswapV3, UniswapV4, CurveStable, CurveCrypto,
                    AerodromeVolatile, AerodromeStable, Slipstream } // tag for display/introspection only
```

`raw` stays wei-exact `U256`. Decimal rendering happens only in `to_significant`/`to_fixed`, which take an explicit `Rounding` and the relevant `TokenMeta` (decimals live on metadata, not on the hot-path amount).

---

## 6. Numeric & representation model

Dual representation — the research conclusion is "do not pick one":

- **Prices → exact rational (`Ratio`).** A spot/marginal price is inherently fractional; representing it as a reduced `U256` numerator/denominator (with `U512` scratch for cross-multiply) is lossless, invertible, and composes across hops. This is the library's *lingua franca* for prices.
- **Swap quoting → protocol-faithful fixed-point.** `quote()` must reproduce the contract's output including its rounding (floor/ceil in the protocol's favor). Each protocol adapter does its math in the protocol's native fixed-point (e.g. Uniswap V3 Q64.96 `sqrtPriceX96` + tick math) and returns a wei-exact `U256`. We **reuse `uniswap_v3_math`** for V3/V4 tick/swap math and **`curve-math`** for Curve rather than reimplementing.
- **Rounding discipline:** slippage guards round against the trader (`min_amount_out` down, `max_amount_in` up). Display rounding is always explicit (`Rounding` argument); there is no silent `Display` that rounds.

---

## 7. Trait surface

```rust
// traits/pool.rs — object-safe core (no async, no generics → Box<dyn Pool> works)
pub trait Pool: Send + Sync {
    fn id(&self) -> PoolId;
    fn assets(&self) -> &[AssetId];
    /// Exact-in, fee+impact-inclusive, protocol-faithful (matches the contract to the wei).
    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError>;
}

// traits/exact_out.rs
pub trait ExactOut: Pool {
    /// Input required to receive exactly `amount_out`. Closed-form (V2) or bounded bisection (V3/Curve).
    fn quote_exact_out(&self, amount_out: &AssetAmount, from: &AssetId) -> Result<AssetAmount, QuoteError>;
}

// traits/pricing.rs
pub trait Pricing: Pool {
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError>;
    /// Default impl: compare `quote()` output to `spot_price().quote()`.
    fn price_impact_bps(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<u32, QuoteError> { /* default */ }
}

// traits/introspect.rs
pub trait Introspect: Pool {
    fn fee_bps(&self, source: &AssetId, destination: &AssetId) -> Option<u32>;
    fn reserve(&self, asset: &AssetId) -> Option<AssetAmount>;   // where defined (V2/Curve)
    fn kind(&self) -> PoolKind;
}

// traits/limits.rs
pub struct LimitedQuote { pub amount_in: AssetAmount, pub amount_out: AssetAmount, pub limited: bool }
pub trait Limits: Pool {
    fn max_amount_in(&self, from: &AssetId, to: &AssetId) -> Option<AssetAmount>;
    fn quote_with_limit(&self, amount_in: &AssetAmount, to: &AssetId, limit: Price)
        -> Result<LimitedQuote, QuoteError>;
}
```

Direction is carried by the typed `AssetAmount` (input token) plus the `to: &AssetId` (output token), so no separate `Pair` type is needed. The core trait is deliberately tiny and object-safe; capabilities are opt-in so a simple constant-product pool need not implement tick-based limits.

---

## 8. Error model

```rust
// amm-core/error.rs — MATH ONLY. No transport/IO concerns here.
#[non_exhaustive]
pub enum QuoteError {
    AssetMismatch { expected: AssetId, got: AssetId },
    AssetNotInPool { source: AssetId, destination: AssetId },
    InsufficientLiquidity,
    PriceLimitCrossed,
    Overflow,
    Unsupported,          // e.g. exact-out not supported by this pool
}

// amm-rpc/source.rs — I/O ONLY.
#[non_exhaustive]
pub enum RpcError { Transport(String), Decode(String), NotFound(String), Internal(String) }
```

Separating math from I/O keeps the pure simulation core testable and reusable without a provider — a gap in existing crates (amms-rs conflates the two).

---

## 9. Stateless helper modules

```rust
// slippage/ — a Slippage value object (Balancer `Slippage` pattern), typed in Bps
pub struct Slippage(Bps);
impl Slippage {
    pub fn from_bps(bps: Bps) -> Self;
    pub fn min_amount_out(&self, quoted_out: &AssetAmount) -> AssetAmount; // rounds DOWN
    pub fn max_amount_in (&self, quoted_in:  &AssetAmount) -> AssetAmount; // rounds UP
    pub fn compound(self, hops: usize) -> Slippage;                        // 1-(1-t)^n across hops
}

// path/
pub struct Hop<'a> { pub pool: &'a dyn Pool, pub to: AssetId }
pub fn quote_path(start: &AssetAmount, hops: &[Hop<'_>]) -> Result<AssetAmount, QuoteError>;
```

`quote_path` composes forward quotes across a heterogeneous `&[dyn Pool]`, threading the running `AssetAmount` and returning the first hop's `QuoteError` on failure.

---

## 10. The `amm-rpc` fetch layer

```rust
// amm-rpc/source.rs (feature-gated · std + async)
#[async_trait]
pub trait StateSource {
    async fn discover(&self, chain: &ChainId, assets: &[AssetId]) -> Result<Vec<PoolKey>, RpcError>;
    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError>;
}
```

This is arb-router's `Exchange` trait re-homed to the I/O crate with a clearer name and I/O-only error. **Bring-your-own-data** consumers skip it entirely and construct pools directly, e.g. `protocols::uniswap::V3Pool::new(state…)` from a subgraph, DB, or test fixture. `amm-rpc` ships an `alloy`-backed default `StateSource` with batched multicall refresh and retry.

---

## 11. Pool-family roadmap

**v1 — proven set, re-homed** (already wei-exact and differential-tested in arb-router):
- Uniswap **V2**, **V3**, **V4** (standard hooks)
- **Curve** — StableSwap + CryptoSwap variants (`curve-math`)
- **Aerodrome / Solidly** — volatile, stable, Slipstream

**v2 — high-value distinct math** (proves open extensibility on non-Uniswap shapes):
- **Balancer** V2/V3 (Weighted + ComposableStable) — first non-Uniswap invariant; the "add a protocol" reference
- **Algebra** (concentrated + dynamic fee — QuickSwap/Camelot)
- **Trader Joe / LFJ Liquidity Book** (discrete bins)

**v3+ — long tail:** Maverick, Kyber (Elastic/Classic), DODO (PMM), Bancor, other Solidly forks.

**Documented limits:**
- Uniswap V4 pools whose **hooks alter the swap curve / dynamic fee** cannot be quoted off-chain from state alone; we quote standard/known hooks and document the boundary.
- **RFQ / oracle venues** (Hashflow, 0x RFQ, GMX GLP) don't satisfy "quote from state"; a possible future *separate* trait, not part of `Pool`.

---

## 12. Correctness & testing strategy

- **Golden vectors:** each quoter checked wei-exact against the contract's own `getAmountOut` / `get_dy` / `QuoterV2` at pinned mainnet blocks (arb-router's `live.rs` differential approach, promoted to the standard and published in the README as the credibility artifact).
- **Property tests (`proptest`):** `invert(invert(p)) == p`; quote round-trip bounds; monotonicity; no value creation; `min_amount_out ≤ quote ≤` spot-implied.
- **`no_std` CI:** build `amm-core` with `--no-default-features` to keep the portability promise honest.

---

## 13. Packaging, features, portability

- **License:** `MIT OR Apache-2.0` (Rust-ecosystem standard).
- **Edition:** 2024. **MSRV:** pinned (≥ the edition-2024 floor); enforced with a `cargo-semver-checks` CI gate.
- **Features (`amm-core`):** `default = ["std"]`; additive per-protocol flags `uniswap-v2` / `uniswap-v3` / `uniswap-v4` / `curve` / `aerodrome`; `serde`. Purely additive — enabling one never breaks another. A user compiling only `uniswap-v3` pulls zero Curve/Aerodrome code.
- **`amm-rpc`** is a separate opt-in crate (pulls `alloy` provider + tokio).
- **Interop:** depend on `alloy-primitives` for `U256`/`I256`; provide `From`/`TryFrom` between `Price`/`Ratio` and protocol-native encodings (`sqrtPriceX96`, tick, Curve rates) — being the lingua franca between encodings is a differentiator.
- **Docs:** `#![deny(missing_docs)]`, runnable doctests, an `examples/` dir mirroring real tasks (quote a V3 swap, add a custom pool type, sync from RPC).

---

## 14. Out of scope / future phases

- **`amm-execution` (later phase):** per-protocol swap calldata + approvals; meshes into an executor/router.
- **`LiquidityMath` extension (later):** add/remove liquidity, LP mint/burn, position value, impermanent-loss helpers.
- **RFQ/oracle venues:** possible separate trait.
- **Non-EVM adapters:** core stays chain-agnostic-ready; adapters are EVM-only for now.

---

## 15. Open items to refine during planning

- **Fixed-point module home/name:** V3/V4 tick math is reused from `uniswap_v3_math`; if any *shared* fixed-point helpers emerge, they get a precisely-named module (candidate `fixed_point/`) rather than living loose. To confirm when detailing protocols.
- **`Ratio` representation:** RESOLVED — `Ratio` wraps `num_rational::BigRational` (arbitrary precision), which exactly represents a Uniswap V3 price (`sqrtPriceX96²/2¹⁹²` ~2³²¹, beyond `U256`) with no width-juggling, and — verified — still compiles `no_std + alloc`. Supersedes the earlier hand-rolled `U512` plan. Only `spot_price` builds the `Ratio`; `quote()` stays on fixed-point tick math.
- **`PoolKind` variant list:** finalize the exact set when the v1 protocols are implemented (`#[non_exhaustive]` protects semver either way).
- **Pool-families research:** a background survey may refine v2/v3 priorities and per-family difficulty; does not affect v1.
