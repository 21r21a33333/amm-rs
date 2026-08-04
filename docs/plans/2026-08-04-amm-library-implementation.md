# amm-rs Library Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the v1 `amm-rs` workspace — a wei-exact, `no_std`-capable Rust AMM quoting library behind an open, object-safe `Pool` trait with typed `AssetAmount`/`Price`, populated with Uniswap V2/V3/V4, Curve, and Aerodrome ported from arb-router's differential-tested adapters.

**Architecture:** Two crates. `amm-core` (`no_std + alloc`, zero network deps) holds primitives, traits, per-protocol pure quoters, and stateless slippage/path helpers. `amm-rpc` (std + async, feature-gated) fetches on-chain state via `alloy`. Amounts and prices are typed value objects carrying token identity; quoting is protocol-faithful fixed-point (reusing `uniswap_v3_math` + `curve-math`); prices are exact rationals.

**Tech Stack:** Rust edition 2024, `alloy-primitives` (`U256`/`I256`), `uniswap_v3_math`, `curve-math`, `thiserror`, `alloy` (in `amm-rpc`), `proptest`.

**Reference:** design spec at `amm-rs/docs/specs/2026-08-04-amm-library-design.md`. Source to port from: `../arb-router/src/` (working tree at `/Users/diwakarmatsaa/Desktop/catalog/arb-router`).

**Refinements applied (2026-08-04 revalidation — see spec §2a):** (R1) single canonical `amm_core::QuoteError`; `amm-rpc::RpcError` wraps it via `#[from]`. (R2) `AssetId { chain: ChainId(u64), token: B256 }` — `Copy`, `FromStr`/`Display` for `"chainid:0xaddr"`; NOT a String. (R3) typed `Bps(u16)` + a `Slippage` value object (no bare `u32` bps). (R4) `AssetAmount` is a `Copy` value object with `new`/`from_decimal`/`checked_add`/`checked_sub`. (R5) builders for many-field V3/V4 pools + `From`/`TryFrom` `Price ↔ sqrtPriceX96`/`tick`. (R6) no `Pair` type. (R7) reuse `uniswap_v3_math`+`curve-math`; `Ratio` on `ruint`. (R8) `StateSource` uses `async_trait` (object-safe).

## Global Constraints

- License: `MIT OR Apache-2.0`. Edition 2024. MSRV pinned to the edition-2024 floor.
- `amm-core` MUST compile under `--no-default-features` (`no_std + alloc`); CI enforces this.
- Trait boundary is wei-exact `U256` raw amounts wrapped in `AssetAmount`; prices are exact-rational `Ratio`; NO `f64` anywhere in public signatures.
- Core `Pool` trait MUST stay object-safe (no generics, no `async fn` on it) so `Box<dyn Pool>` works.
- All fallible math returns `Result<_, QuoteError>`; `QuoteError` is math-only and MUST NOT reference I/O. I/O errors are `RpcError` in `amm-rpc`.
- Every quoter is validated wei-exact against the contract's own view function (golden vectors) — ported from arb-router `src/adapters/exchanges/live.rs`.
- Naming: dirs are namespaces that state their contents; `traits/`, `protocols/<family>/`; quoter files named by variant (`v2.rs`, `v3.rs`, `pool.rs`, `stable.rs`); no `math`/`utils` buckets.
- Reuse `uniswap_v3_math` for V3/V4 tick math and `curve-math` for Curve; do NOT reimplement.
- Commits: conventional-commit messages; commit only at each task's final step.

---

### Task 0: Scaffold the workspace

**Files:**
- Create: `Cargo.toml` (workspace root), `amm-core/Cargo.toml`, `amm-rpc/Cargo.toml`
- Create: `amm-core/src/lib.rs`, `amm-rpc/src/lib.rs`
- Create: `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`, `.gitignore`, `rust-toolchain.toml`
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: the crate graph `amm-rpc → amm-core`; feature flags `std` (default), `uniswap-v2/v3/v4`, `curve`, `aerodrome`, `serde` on `amm-core`.

- [ ] **Step 1: `git init` and create the workspace root `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = ["amm-core", "amm-rpc"]

[workspace.package]
edition = "2024"
license = "MIT OR Apache-2.0"
rust-version = "1.85"

[workspace.dependencies]
alloy-primitives = { version = "1", default-features = false }
thiserror = "2"
```

- [ ] **Step 2: `amm-core/Cargo.toml` — `no_std`-capable, additive features**

```toml
[package]
name = "amm-core"
version = "0.1.0"
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
alloy-primitives = { workspace = true }
thiserror = { workspace = true, optional = true }
serde = { version = "1", features = ["derive"], optional = true, default-features = false }
uniswap_v3_math = { version = "0.6", default-features = false, optional = true }
curve-math = { git = "https://github.com/sunce86/curve-math", features = ["swap"], optional = true }

[features]
default = ["std"]
std = ["alloy-primitives/std", "dep:thiserror"]
serde = ["dep:serde", "alloy-primitives/serde"]
uniswap-v2 = []
uniswap-v3 = ["dep:uniswap_v3_math"]
uniswap-v4 = ["dep:uniswap_v3_math"]
curve = ["dep:curve-math"]
aerodrome = ["dep:uniswap_v3_math"]

[dev-dependencies]
proptest = "1"
```

Note: on `no_std`, error types use `core::fmt` impls instead of `thiserror` (gate `thiserror` behind `std`; provide manual `Display`/`Error`-free variants under `no_std`). The implementer resolves this by deriving `Display` manually in `no_std` and using `thiserror` only under `std`.

- [ ] **Step 3: `amm-core/src/lib.rs` — `no_std` attribute + module tree**

```rust
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

pub mod primitives;
pub mod traits;
pub mod protocols;
pub mod slippage;
pub mod path;
pub mod error;
```

- [ ] **Step 4: `amm-rpc/Cargo.toml` and `src/lib.rs` skeleton**

```toml
[package]
name = "amm-rpc"
version = "0.1.0"
edition.workspace = true
license.workspace = true

[dependencies]
amm-core = { path = "../amm-core", features = ["std"] }
alloy = { version = "1", features = ["provider-http", "contract"] }
async-trait = "0.1"
thiserror = { workspace = true }
tokio = { version = "1", features = ["rt", "macros"] }
```

```rust
// amm-rpc/src/lib.rs
pub mod source;
pub mod provider;
pub mod multicall;
pub mod retry;
pub mod protocols;
```

- [ ] **Step 5: License files, README stub, `.gitignore` (`/target`), `rust-toolchain.toml` (channel = "1.85")**

- [ ] **Step 6: CI workflow — build, test, and a dedicated `no_std` job**

```yaml
# .github/workflows/ci.yml (key jobs)
# - cargo test --workspace
# - cargo build -p amm-core --no-default-features   # no_std gate
# - cargo clippy --workspace -- -D warnings
# - cargo fmt --check
```

- [ ] **Step 7: Verify the empty workspace compiles**

Run: `cargo build --workspace` and `cargo build -p amm-core --no-default-features`
Expected: both succeed (empty modules).

- [ ] **Step 8: Commit**

```bash
git add -A && git commit -m "chore: scaffold amm-rs workspace (amm-core no_std + amm-rpc)"
```

---

### Task 1: `primitives/asset.rs` — ChainId, AssetId, AssetAmount, TokenMeta

**Files:**
- Create: `amm-core/src/primitives/mod.rs` (declares `asset`, `ratio`, `price`, `pool`)
- Create: `amm-core/src/primitives/asset.rs`

**Interfaces:**
- Produces: `ChainId(u64)` (`Copy`); `AssetId { chain: ChainId, token: B256 }` (`Copy, Hash, Ord`; `FromStr`/`Display` for `"chainid:0xaddr"`); `AssetAmount { asset: AssetId, raw: U256 }` (`Copy`; `new(asset, raw)`, `from_decimal(asset, decimals, &str)`, `checked_add`/`checked_sub -> Result<Self, QuoteError>`); `TokenMeta { decimals: u8, symbol: String }`. No `Pair`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn asset_id_roundtrips_via_string() {
    let s = "1:0x000000000000000000000000000000000000abcd";
    let a: AssetId = s.parse().unwrap();
    assert_eq!(a.chain, ChainId(1));
    assert_eq!(a.to_string(), s);
}
#[test]
fn asset_id_rejects_malformed() {
    assert!("ethereum".parse::<AssetId>().is_err());
}
#[test]
fn asset_amount_checked_add_rejects_mismatch() {
    let x = AssetAmount::new(asset(1, 0xaa), U256::from(1u64));
    let y = AssetAmount::new(asset(1, 0xbb), U256::from(1u64));
    assert!(matches!(x.checked_add(y), Err(QuoteError::AssetMismatch { .. })));
}
// `asset(chain, byte)` = test helper building an AssetId with `byte` in the low address slot.
```

- [ ] **Step 2: Run tests, verify they fail** — Run: `cargo test -p amm-core primitives::asset` → FAIL (types undefined).

- [ ] **Step 3: Implement** the refined structured form (do NOT port arb-router's `String` AssetId):

```rust
use alloc::string::String;
use alloy_primitives::{B256, U256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChainId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetId { pub chain: ChainId, pub token: B256 }
// FromStr: parse "<u64>:0x<hex>" → left-pad the address hex into B256; Err on bad form.
// Display: "{chain}:0x{token:hex}" (canonical, lower-case).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetAmount { pub asset: AssetId, pub raw: U256 }
impl AssetAmount {
    pub fn new(asset: AssetId, raw: U256) -> Self { Self { asset, raw } }
    pub fn from_decimal(asset: AssetId, decimals: u8, s: &str) -> Result<Self, QuoteError>; // s * 10^decimals
    pub fn checked_add(self, o: Self) -> Result<Self, QuoteError>; // Err(AssetMismatch) if asset != o.asset
    pub fn checked_sub(self, o: Self) -> Result<Self, QuoteError>;
}

pub struct TokenMeta { pub decimals: u8, pub symbol: String }
```

- [ ] **Step 4: Run tests, verify they pass** — Run: `cargo test -p amm-core primitives::asset` → PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(core): typed AssetAmount + AssetId/ChainId/Pair/TokenMeta primitives"`

---

### Task 2: `primitives/ratio.rs` — exact-rational Ratio + Rounding

**Files:**
- Create: `amm-core/src/primitives/ratio.rs`

**Interfaces:**
- Consumes: `alloy_primitives::U256`.
- Produces: `Ratio` (reduced rational; `new(U256,U256)->Option<Ratio>`, `invert(self)->Option<Ratio>`, `cmp(&self,&Ratio)->Ordering` via cross-multiply using `U512` scratch, `mul(self, Ratio)->Option<Ratio>`), `enum Rounding { Down, Up, HalfUp }`, `pub(crate) fn apply(&self, x: U256, r: Rounding) -> Option<U256>` (multiply a raw amount by the ratio with explicit rounding), and `Bps(pub u16)` (typed basis points, `Copy`).

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn ratio_reduces_on_construction() {
    let r = Ratio::new(U256::from(6u64), U256::from(4u64)).unwrap(); // 3/2
    assert_eq!(r, Ratio::new(U256::from(3u64), U256::from(2u64)).unwrap());
}
#[test]
fn ratio_zero_denominator_is_none() {
    assert!(Ratio::new(U256::from(1u64), U256::ZERO).is_none());
}
#[test]
fn ratio_invert_roundtrips() {
    let r = Ratio::new(U256::from(3u64), U256::from(2u64)).unwrap();
    assert_eq!(r.invert().unwrap().invert().unwrap(), r);
}
#[test]
fn ratio_cmp_uses_cross_multiply() {
    let a = Ratio::new(U256::from(1u64), U256::from(3u64)).unwrap();
    let b = Ratio::new(U256::from(1u64), U256::from(2u64)).unwrap();
    assert!(a.cmp(&b) == core::cmp::Ordering::Less);
}
```

- [ ] **Step 2: Run tests, verify they fail.**

- [ ] **Step 3: Implement** — store reduced `num: U256`, `den: U256`; reduce via binary GCD; use `alloy_primitives::U512` for cross-multiply in `cmp`/`mul` to avoid overflow; `apply` computes `x * num / den` with the requested rounding (`Down` = floor, `Up` = ceil, `HalfUp`), widening to `U512` for the intermediate product.

- [ ] **Step 4: Run tests, verify they pass.**

- [ ] **Step 5: Commit** — `git commit -m "feat(core): exact-rational Ratio + Rounding"`

---

### Task 3: `primitives/price.rs` — directional Price

**Files:**
- Create: `amm-core/src/primitives/price.rs`

**Interfaces:**
- Consumes: `AssetId`, `AssetAmount`, `Ratio`, `Rounding`, `TokenMeta`, `QuoteError` (from Task 5 — if Task 5 not yet done, define a local `PriceError` and fold into `QuoteError` in Task 5; to avoid churn, DO Task 5 before Task 3).
- Produces: `Price { base: AssetId, quote: AssetId, ratio: Ratio }` with `invert(self)->Price`, `quote(&self, &AssetAmount)->Result<AssetAmount, QuoteError>`, `compose(self, Price)->Result<Price, QuoteError>`, `to_significant(&self, u8, &TokenMeta, &TokenMeta, Rounding)->String`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn price_quote_applies_to_base_amount() {
    // base=A quote=B, ratio 3/1 → 10 A quotes to 30 B
    let (a, b) = (AssetId::new("c:a").unwrap(), AssetId::new("c:b").unwrap());
    let p = Price::new(a.clone(), b.clone(), Ratio::new(U256::from(3u64), U256::from(1u64)).unwrap());
    let out = p.quote(&AssetAmount { asset: a, raw: U256::from(10u64) }).unwrap();
    assert_eq!(out.asset, b);
    assert_eq!(out.raw, U256::from(30u64));
}
#[test]
fn price_quote_rejects_wrong_base() {
    let (a, b) = (AssetId::new("c:a").unwrap(), AssetId::new("c:b").unwrap());
    let p = Price::new(a, b.clone(), Ratio::new(U256::from(3u64), U256::from(1u64)).unwrap());
    let wrong = AssetAmount { asset: b, raw: U256::from(1u64) };
    assert!(matches!(p.quote(&wrong), Err(QuoteError::AssetMismatch { .. })));
}
#[test]
fn price_invert_swaps_base_quote() {
    let (a, b) = (AssetId::new("c:a").unwrap(), AssetId::new("c:b").unwrap());
    let p = Price::new(a.clone(), b.clone(), Ratio::new(U256::from(3u64), U256::from(1u64)).unwrap());
    let inv = p.invert();
    assert_eq!(inv.base, b); assert_eq!(inv.quote, a);
}
```

- [ ] **Step 2: Run tests, verify they fail.**

- [ ] **Step 3: Implement** — `quote` checks `input.asset == self.base` (else `QuoteError::AssetMismatch`), applies `ratio` with `Rounding::Down`, tags the result `self.quote`. `compose` checks `self.quote == next.base` (else `AssetMismatch`), multiplies ratios. `to_significant` applies the decimal scalar `10^(base.decimals - quote.decimals)` before rendering with the given `Rounding`.

- [ ] **Step 4: Run tests, verify they pass.**

- [ ] **Step 5: Commit** — `git commit -m "feat(core): directional invertible Price"`

---

### Task 4: `primitives/pool.rs` — PoolId, ExchangeId, PoolKey, PoolKind

**Files:**
- Create: `amm-core/src/primitives/pool.rs`

**Interfaces:**
- Produces: `PoolId`, `ExchangeId` (opaque string newtypes), `PoolKey { exchange, chain, address: String, assets: Vec<AssetId>, fee_bps: Option<u32> }`, `#[non_exhaustive] enum PoolKind { UniswapV2, UniswapV3, UniswapV4, CurveStable, CurveCrypto, AerodromeVolatile, AerodromeStable, Slipstream }`.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn pool_id_roundtrips() {
    let p = PoolId::new("ethereum:univ3:0xabc");
    assert_eq!(p.as_str(), "ethereum:univ3:0xabc");
}
```

- [ ] **Step 2: Run test, verify it fails.**
- [ ] **Step 3: Implement** — port `../arb-router/src/primitives/pool.rs` (`PoolId`, `ExchangeId`, `PoolKey`) verbatim into `alloc`-friendly form; add the `#[non_exhaustive] PoolKind` enum.
- [ ] **Step 4: Run test, verify it passes.**
- [ ] **Step 5: Commit** — `git commit -m "feat(core): PoolId/ExchangeId/PoolKey/PoolKind primitives"`

---

### Task 5: `error.rs` — QuoteError (math-only)

**Files:**
- Create: `amm-core/src/error.rs`

**Interfaces:**
- Produces: `#[non_exhaustive] enum QuoteError { AssetMismatch { expected: AssetId, got: AssetId }, PairNotInPool { source: AssetId, destination: AssetId }, InsufficientLiquidity, PriceLimitCrossed, Overflow, Unsupported }`. Under `std` derive via `thiserror`; under `no_std` impl `core::fmt::Display` manually.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn quote_error_is_constructible_and_matchable() {
    let e = QuoteError::InsufficientLiquidity;
    assert!(matches!(e, QuoteError::InsufficientLiquidity));
}
```

- [ ] **Step 2: Run test, verify it fails.**
- [ ] **Step 3: Implement** the enum. `#[cfg_attr(feature = "std", derive(thiserror::Error))]` with `#[error(...)]` messages; a `#[cfg(not(feature = "std"))]` manual `Display` impl.
- [ ] **Step 4: Run test, verify it passes.** Also run `cargo build -p amm-core --no-default-features` to confirm the `no_std` error path compiles.
- [ ] **Step 5: Commit** — `git commit -m "feat(core): math-only QuoteError (std + no_std)"`

> Ordering note: implement Task 5 **before** Task 3 (Price needs `QuoteError`).

---

### Task 6: `traits/pool.rs` — object-safe core Pool trait

**Files:**
- Create: `amm-core/src/traits/mod.rs`, `amm-core/src/traits/pool.rs`

**Interfaces:**
- Consumes: `PoolId`, `AssetId`, `AssetAmount`, `QuoteError`.
- Produces: `trait Pool: Send + Sync { fn id(&self)->PoolId; fn assets(&self)->&[AssetId]; fn quote(&self, amount_in:&AssetAmount, to:&AssetId)->Result<AssetAmount,QuoteError>; }`.

- [ ] **Step 1: Write failing test** (object-safety + a fake pool)

```rust
struct FakePool { id: PoolId, assets: Vec<AssetId> }
impl Pool for FakePool {
    fn id(&self) -> PoolId { self.id.clone() }
    fn assets(&self) -> &[AssetId] { &self.assets }
    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
        Ok(AssetAmount { asset: to.clone(), raw: amount_in.raw * U256::from(2u64) })
    }
}
#[test]
fn pool_is_object_safe() {
    let p: Box<dyn Pool> = Box::new(FakePool { /* .. */ });
    let _ = p.id();
}
```

- [ ] **Step 2: Run test, verify it fails.**
- [ ] **Step 3: Implement** the trait exactly as in Interfaces. No generics, no `async`.
- [ ] **Step 4: Run test, verify it passes** (compiling `Box<dyn Pool>` proves object-safety).
- [ ] **Step 5: Commit** — `git commit -m "feat(core): object-safe Pool trait"`

---

### Task 7: extension traits — ExactOut, Pricing, Introspect, Limits

**Files:**
- Create: `amm-core/src/traits/exact_out.rs`, `pricing.rs`, `introspect.rs`, `limits.rs`

**Interfaces:**
- Consumes: `Pool`, `AssetId`, `AssetAmount`, `Price`, `PoolKind`, `QuoteError`.
- Produces the four traits exactly as in spec §7, plus `struct LimitedQuote { amount_in: AssetAmount, amount_out: AssetAmount, limited: bool }`. `Pricing::price_impact_bps` has a default impl computing `(spot_implied_out - actual_out) / spot_implied_out` in bps via `Ratio`.

- [ ] **Step 1: Write failing tests** — extend `FakePool` to impl `Pricing` (return a fixed `Price`) and assert `price_impact_bps` default returns `0` when quote == spot-implied.
- [ ] **Step 2: Run tests, verify they fail.**
- [ ] **Step 3: Implement** the four traits + `LimitedQuote` + the `price_impact_bps` default.
- [ ] **Step 4: Run tests, verify they pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(core): ExactOut/Pricing/Introspect/Limits extension traits"`

---

### Task 8: `slippage/` — tolerance guards

**Files:**
- Create: `amm-core/src/slippage/mod.rs`

**Interfaces:**
- Consumes: `AssetAmount`, `Bps`, `Ratio::apply`.
- Produces: `struct Slippage(Bps)` with `from_bps(Bps) -> Self`, `min_amount_out(&self, &AssetAmount) -> AssetAmount` (rounds down), `max_amount_in(&self, &AssetAmount) -> AssetAmount` (rounds up), `compound(self, hops: usize) -> Slippage`, `bps(&self) -> Bps`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn min_out_reduces_and_rounds_down() {
    let a = AssetAmount::new(asset(1, 0xaa), U256::from(10_000u64));
    let out = Slippage::from_bps(Bps(100)).min_amount_out(&a); // 1% → 9900
    assert_eq!(out.raw, U256::from(9_900u64));
}
#[test]
fn compound_two_legs_50bps_floors_to_99() {
    // 1 - (1 - 0.005)^2 = 0.009975 → 99.75 bps → floored to 99
    assert_eq!(Slippage::from_bps(Bps(50)).compound(2).bps(), Bps(99));
}
```

- [ ] **Step 2: Run tests, verify they fail.**
- [ ] **Step 3: Implement** — `Slippage(Bps)`; `min_amount_out = raw * (10_000 - bps) / 10_000` floored and `max_amount_in = raw * (10_000 + bps) / 10_000` ceiled (both via `Ratio::apply`); `compound(hops) = 10_000 - Π(10_000 - bps)/10_000` returned as integer `Bps` (port arb-router / MuadibRouter `compound_slippage_bps`, exact); `bps()` getter.
- [ ] **Step 4: Run tests, verify they pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(core): slippage guards (min_out/max_in/compound)"`

---

### Task 9: `path/` — multi-hop composition

**Files:**
- Create: `amm-core/src/path/mod.rs`

**Interfaces:**
- Consumes: `Pool`, `AssetAmount`, `AssetId`, `QuoteError`.
- Produces: `struct Hop<'a> { pub pool: &'a dyn Pool, pub to: AssetId }`, `fn quote_path(start: &AssetAmount, hops: &[Hop<'_>]) -> Result<AssetAmount, QuoteError>`.

- [ ] **Step 1: Write failing test** — two `FakePool`s (×2 each) composed; assert `quote_path(10, [hop1, hop2]) == 40`.
- [ ] **Step 2: Run test, verify it fails.**
- [ ] **Step 3: Implement** — fold `quote` across hops, threading the running `AssetAmount` (each hop's output asset is the next hop's input); propagate the first `QuoteError`.
- [ ] **Step 4: Run test, verify it passes.**
- [ ] **Step 5: Commit** — `git commit -m "feat(core): quote_path multi-hop composition"`

---

### Task 10: `protocols/uniswap/v2.rs` — port V2 quoter

**Files:**
- Create: `amm-core/src/protocols/mod.rs`, `amm-core/src/protocols/uniswap/mod.rs`, `amm-core/src/protocols/uniswap/v2.rs`

**Interfaces:**
- Consumes: `Pool` (+ `Introspect`, `Pricing`, `ExactOut`), `AssetAmount`, `AssetId`, `QuoteError`, `U256`.
- Produces: `struct UniswapV2Pool { id, assets: [AssetId;2], reserves: [U256;2], fee_bps: u32 }` with `new(...)` ctor and impls of `Pool` + `Introspect` + `Pricing` + `ExactOut`.

- [ ] **Step 1: Port the golden-vector test** from `../arb-router/src/adapters/exchanges/live.rs` (the V2 `getAmountsOut` differential case) as a `#[cfg(feature = "std")]` integration test, adapting to `UniswapV2Pool::new(...).quote(&AssetAmount, &to)` and asserting wei-exact equality to the pinned on-chain value.
- [ ] **Step 2: Write a pure unit test** for the constant-product formula: reserves `(1_000_000, 1_000_000)`, fee 30 bps, input 1000 → assert exact output from `out = in*(10000-fee)*rOut / (rIn*10000 + in*(10000-fee))`.
- [ ] **Step 3: Run tests, verify they fail.**
- [ ] **Step 4: Implement** — port the constant-product math from `../arb-router/src/adapters/exchanges/uniswap/v2.rs`, preserving the exact fee-inclusive formula; map direction from `(amount_in.asset, to)` to the reserve pair; `quote` returns `Err(PairNotInPool)` if the assets aren't the pool's two, `Err(InsufficientLiquidity)` on zero reserves. Implement `spot_price` = reserves ratio as `Ratio`; `quote_exact_out` = closed-form inverse; `reserve`/`fee_bps`/`kind`=`UniswapV2`.
- [ ] **Step 5: Run tests, verify they pass** (unit + golden vector).
- [ ] **Step 6: Commit** — `git commit -m "feat(core): Uniswap V2 quoter (ported, wei-exact)"`

---

### Task 11: `protocols/uniswap/v3.rs` — port V3 quoter

**Files:** Create `amm-core/src/protocols/uniswap/v3.rs`

**Interfaces:**
- Produces: `struct UniswapV3Pool { id, assets:[AssetId;2], sqrt_price_x96: U256, liquidity: u128, tick: i32, fee_pips: u32, tick_data: TickData }` implementing `Pool` + `Pricing` + `ExactOut` + `Limits`.

- [ ] **Step 1: Port the V3 golden-vector test** (QuoterV2 differential case) from `live.rs`.
- [ ] **Step 2: Run it, verify it fails.**
- [ ] **Step 3: Implement** — port `../arb-router/src/adapters/exchanges/uniswap/v3.rs`, calling `uniswap_v3_math::swap_math::compute_swap_step` (reuse, do not reimplement); `spot_price` from `sqrtPriceX96²/Q192` as a `Ratio` (use `U512`); `quote_with_limit` honoring `sqrtPriceLimit` and reporting `limited`; `quote_exact_out` via the signed-`I256` path in `compute_swap_step`; `kind()=UniswapV3`.
- [ ] **Step 4: Run tests, verify they pass** (within the wei tolerance the arb-router suite uses; prefer exact).
- [ ] **Step 5: Commit** — `git commit -m "feat(core): Uniswap V3 quoter (ported, tick math reused)"`

---

### Task 12: `protocols/uniswap/v4.rs` — port V4 quoter

**Files:** Create `amm-core/src/protocols/uniswap/v4.rs`

**Interfaces:**
- Produces: `struct UniswapV4Pool { .. , hooks: String }` implementing `Pool` + `Pricing` + `ExactOut` + `Limits`; standard/no-hook and static-fee pools only.

- [ ] **Step 1: Port the V4 golden-vector test** from `live.rs` (compounded LP+protocol fee case fixed in arb-router).
- [ ] **Step 2: Run it, verify it fails.**
- [ ] **Step 3: Implement** — port `../arb-router/src/adapters/exchanges/uniswap/v4.rs`, preserving the per-direction LP+protocol fee compounding fix; if `hooks` indicates a curve-altering/dynamic-fee hook, return `Err(QuoteError::Unsupported)` and document it; `kind()=UniswapV4`.
- [ ] **Step 4: Run tests, verify they pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(core): Uniswap V4 quoter (ported, standard hooks)"`

---

### Task 13: `protocols/curve/pool.rs` — port Curve quoter

**Files:** Create `amm-core/src/protocols/curve/mod.rs`, `amm-core/src/protocols/curve/pool.rs`

**Interfaces:**
- Produces: `struct CurvePool { id, assets: Vec<AssetId>, /* state consumed by curve-math */ }` implementing `Pool` + `Introspect` + `Pricing`; covers StableSwap + CryptoSwap variants via `curve-math`.

- [ ] **Step 1: Port a Curve golden-vector test** (`get_dy` differential case) from `live.rs`, incl. the `future_A()` amp fix noted in arb-router.
- [ ] **Step 2: Run it, verify it fails.**
- [ ] **Step 3: Implement** — port `../arb-router/src/adapters/exchanges/curve/pool.rs`, delegating to `curve-math` `get_dy`; map `(amount_in.asset, to)` to coin indices `i,j`; preserve the `future_A()` read; `kind()` = `CurveStable`/`CurveCrypto` per variant.
- [ ] **Step 4: Run tests, verify they pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(core): Curve quoter (ported, curve-math get_dy)"`

---

### Task 14: `protocols/aerodrome/` — port Solidly quoters

**Files:** Create `amm-core/src/protocols/aerodrome/mod.rs`, `volatile.rs`, `stable.rs`, `slipstream.rs`

**Interfaces:**
- Produces: `AerodromeVolatilePool`, `AerodromeStablePool`, `AerodromeSlipstreamPool`, each implementing `Pool` (+ `Pricing`; volatile/stable also `Introspect`+`ExactOut`; slipstream also `Limits`).

- [ ] **Step 1: Port the Aerodrome golden-vector tests** from `live.rs` (v2 volatile `getAmountOut`, Solidly stable, Slipstream).
- [ ] **Step 2: Run them, verify they fail.**
- [ ] **Step 3: Implement** — port `../arb-router/src/adapters/exchanges/aerodrome/{v2_exchange.rs quoter parts, stable.rs, slipstream_exchange.rs}` math into the three files; volatile = V2-style; stable = Solidly `x³y+y³x` invariant (preserve arb-router's exact routine); slipstream reuses `uniswap_v3_math`; set `kind()` accordingly.
- [ ] **Step 4: Run tests, verify they pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(core): Aerodrome volatile/stable/slipstream quoters (ported)"`

---

### Task 15: `amm-rpc` — StateSource trait, RpcError, provider/multicall/retry

**Files:** Create `amm-rpc/src/source.rs`, `provider.rs`, `multicall.rs`, `retry.rs`

**Interfaces:**
- Consumes: `amm_core::{primitives::*, traits::Pool}`.
- Produces: `#[non_exhaustive] enum RpcError { Transport(String), Decode(String), NotFound(String), Internal(String) }`; `#[async_trait] trait StateSource { async fn discover(&self, chain:&ChainId, assets:&[AssetId]) -> Result<Vec<PoolKey>, RpcError>; async fn refresh(&self, keys:&[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError>; }`; an alloy-backed provider with batched multicall + retry (ported from `../arb-router/src/adapters/rpc/{provider,multicall,retry}.rs`).

- [ ] **Step 1: Write a failing test** using a mock provider (wiremock-style or a hand fake) that `refresh` decodes a known V2 pool state into a `Box<dyn Pool>` whose `quote` matches Task 10's unit expectation.
- [ ] **Step 2: Run it, verify it fails.**
- [ ] **Step 3: Implement** `RpcError`, `StateSource`, and port the `provider`/`multicall`/`retry` machinery + the shared fetch helpers (`call`, address parsing) from arb-router `adapters/rpc` and `adapters/exchanges/mod.rs`.
- [ ] **Step 4: Run tests, verify they pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(rpc): StateSource + alloy provider/multicall/retry"`

---

### Task 16: `amm-rpc/protocols/` — per-protocol discover/refresh

**Files:** Create `amm-rpc/src/protocols/{uniswap/{v2,v3,v4}.rs, curve.rs, aerodrome.rs}`

**Interfaces:**
- Consumes: `StateSource`, the `amm-core` pool structs, the provider.
- Produces: per-protocol `discover`/`refresh` implementations that decode on-chain state into the corresponding `amm-core` pool structs. Ported from `../arb-router/src/adapters/exchanges/{uniswap/*_exchange.rs, curve/exchange.rs, aerodrome/*_exchange.rs}`.

- [ ] **Step 1: Port one representative refresh integration test** per family from arb-router (guarded to run against a live/pinned RPC or a recorded fixture).
- [ ] **Step 2: Run them, verify they fail.**
- [ ] **Step 3: Implement** by porting each `*_exchange.rs` discover/refresh, constructing the new `amm-core` pool structs (Tasks 10–14) instead of the old ones.
- [ ] **Step 4: Run tests, verify they pass.**
- [ ] **Step 5: Commit** — `git commit -m "feat(rpc): per-protocol discover/refresh (ported)"`

---

### Task 17: Docs, examples, `no_std` gate, README

**Files:** Modify `README.md`; create `amm-core/examples/quote_v3.rs`, `amm-core/examples/custom_pool.rs`; add `#![deny(missing_docs)]` to both crates.

**Interfaces:** none new.

- [ ] **Step 1: Add `#![deny(missing_docs)]`** to `amm-core/src/lib.rs` and `amm-rpc/src/lib.rs`; add doc comments to every public item (fix the resulting errors).
- [ ] **Step 2: Write `examples/quote_v3.rs`** (construct a `UniswapV3Pool` from literal state, quote a swap, print `to_significant`) and `examples/custom_pool.rs` (implement `Pool` for a trivial constant-sum pool — proving open extensibility without touching the crate).
- [ ] **Step 3: Run** `cargo test --workspace`, `cargo build -p amm-core --no-default-features`, `cargo run -p amm-core --example quote_v3`, `cargo doc --workspace`, `cargo clippy --workspace -- -D warnings`. All pass.
- [ ] **Step 4: Write README** — quickstart, the differentiator table, the golden-vector credibility note, feature-flag docs, and a "how to add a pool type" section pointing at `examples/custom_pool.rs`.
- [ ] **Step 5: Commit** — `git commit -m "docs: README, runnable examples, deny(missing_docs), no_std gate"`

---

## Self-Review

**Spec coverage:** primitives (T1–T4) ✅; numeric dual-representation — `Ratio` T2, protocol fixed-point T11/T14 ✅; traits T6–T7 ✅; error model T5 ✅; slippage/path T8–T9 ✅; `amm-rpc`/StateSource T15–T16 ✅; pool-family v1 set (Uni V2/V3/V4, Curve, Aerodrome) T10–T14 ✅; correctness (golden vectors per protocol task; `no_std` CI T0/T17; proptest — add invert/round-trip proptests inside T2/T3) ✅; packaging/features/license T0 ✅; docs/examples T17 ✅. Deferred items (execution, LP math, Balancer+) correctly absent.

**Placeholder scan:** protocol tasks reference exact arb-router source paths to port (not "implement later"); the one deliberate open item (`Ratio` internal width) is resolved in T2 (reduced `U256` + `U512` scratch).

**Type consistency:** `AssetAmount { asset, raw }`, `quote(&AssetAmount, &AssetId) -> Result<_, QuoteError>`, `Price { base, quote, ratio }`, `QuoteError` variants — used identically across T1–T16. Ordering fix noted: **T5 (QuoteError) before T3 (Price)**.

---

## Task-review corrections (2026-08-04 — SDK & Rust-idiom audit)

Applied after auditing every task against the SDK research (Uniswap/Balancer/Curve, amms-rs) and semantic Rust principles. **These supersede the task bodies where they conflict.**

- **C1 (T0, T5) — thiserror 2 is `no_std`.** thiserror 2.0 impls `core::error::Error` (Rust ≥1.81), so derive it unconditionally with `default-features = false`; DELETE the `#[cfg(not(feature="std"))]` manual `Display`. `thiserror` is a normal (non-optional) dep of `amm-core`, not gated behind `std`. *(Cargo files already fixed.)*
- **C2 (T2, T11) — `Ratio` is `U512`-backed [correctness].** A Uniswap V3 price is `sqrtPriceX96²/2¹⁹²`; `sqrtPriceX96²` reaches ~2³²¹, exceeding `U256`. Store `Ratio { num: U512, den: U512 }` (reduced); constructors `new(U256,U256)`, `from_parts(U512,U512)`, and `from_q192_sqrt(sqrt_price_x96: U256)` for V3. `cmp`/`checked_mul`/`apply` use `U1024` scratch (`ruint::Uint<1024,16>`); `apply -> Option<U256>` returns `None` if the result exceeds `U256`. `quote()` stays on `uniswap_v3_math` fixed-point (fast); only `spot_price` builds the wide `Ratio`.
- **C3 (T2) — `Ratio` implements `Ord`/`PartialOrd`, not a bare `cmp` method**; rename `mul` → `checked_mul(self, Ratio) -> Option<Ratio>` (no panicking `Mul`).
- **C4 (T6) — `Pool::id(&self) -> &PoolId`** (borrow, don't clone a `String` per call).
- **C5 (T7) — typed `Bps` in returns:** `Pricing::price_impact(&self, amount_in, to) -> Result<Bps, QuoteError>` (renamed from `price_impact_bps`); `Introspect::fee_bps(&self, ...) -> Option<Bps>`.
- **C6 (T5) — rename `PairNotInPool` → `AssetNotInPool { source, destination }`** (`Pair` was dropped in R6).
- **C7 (T1, T5) — dedicated `ParseError`:** `impl FromStr for AssetId { type Err = ParseError }`; `AssetAmount::from_decimal(...) -> Result<Self, ParseError>`. `QuoteError` stays math-only.
- **C8 (T1) — `try_add`/`try_sub` (Result), not `checked_add`/`checked_sub`** (`checked_*` connotes `Option`-on-overflow; our failure is `QuoteError::AssetMismatch`).
- **C9 (T3) — `Price::new` rejects a zero/degenerate ratio and `base == quote`**, so `invert()` is total.
- **C10 (T3) — `Price` impls `PartialOrd`** (returns `None` when `base`/`quote` differ) for ranking.
- **C11 (T11, T12) — builders + encoding conversions:** `UniswapV3Pool`/`UniswapV4Pool` get plain builders (7+ fields); add `Price` ⇄ `sqrtPriceX96`/`tick` `From`/`TryFrom`.
- **C12 (T0) — `[workspace.lints]` + `resolver`.** Centralized lints added; keep `resolver = "2"` (valid under edition 2024; bump to `"3"` only if MSRV-aware resolution is wanted). *(lints already added.)*
- **C13 (all) — derive discipline:** `Rounding`, `PoolKind`, `LimitedQuote`, `Bps` derive `Clone, Copy, Debug, PartialEq, Eq`; pool structs derive `Clone, Debug`; `#[non_exhaustive]` on `QuoteError`, `RpcError`, `ParseError`, `PoolKind`.
- **C14 (T4) — optional:** align `PoolKey.address → B256`; reconsider `PoolId`/`ExchangeId` cheapness later. Kept `String` for v1 (lookup/display key, not hot-path). Non-blocking.
- **C15 (T8) — `Slippage::from_percent`** constructor for Balancer parity (alongside `from_bps`).
- **C16 (T15) — `RpcError` has `#[from] QuoteError`** so a core-math failure during decode propagates cleanly.
