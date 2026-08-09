# amm-rs — Execution & Wallet Design

**Status:** Phase-1 design settled and hardened by a four-stream external review (2026-08-09, §2c) benchmarking against Uniswap SDK / alloy 2.3 / 0x / Balancer / Rust API Guidelines; Phase-2 (wallet) deferred pending external research. Plan to be regenerated to match §2c/E17–E20.
**Date:** 2026-08-07
**Supersedes:** the `amm-execution` phase deferred in [2026-08-04-amm-library-design.md](2026-08-04-amm-library-design.md) (non-goal "Execution / calldata").

**Goal:** Extend `amm-rs` from a quoting library into one that also **builds and (later) executes** swaps — slippage-guarded, exchange-specific-aware, EVM-first — without compromising the pure, wei-exact quoting core.

**Three phases:**
- **Phase 1 — `amm-client` (this spec's focus):** turn a quote into an unsigned, slippage-bounded swap transaction. Pure encoding (ABI/calldata), no signer, no runtime. Merges the existing read-only `amm-rpc` layer with new calldata builders — both are the AMM-specific, alloy-touching concern. Designed idiomatic from the start (§3).
- **Phase 2 — Idiomatic API review & refactor (§4):** a dedicated pass that audits the *whole* codebase (existing `amm-core`/`amm-rpc` + the new Phase-1 surface) for non-idiomatic patterns, benchmarks the public API against alloy / Uniswap SDK / other Rust libraries, produces a **suggestions doc for approval**, then applies the accepted refactors. Split into audit→approve→refactor so nothing is changed without a green light.
- **Phase 3 — `wallet` (deferred):** a generic, AMM-agnostic multi-wallet / multi-chain execution engine that signs, submits, sequences nonces, bumps fees, and tracks confirmations for *any* unsigned tx. Own spec, after external wallet-infra research completes.

---

## 1. Goal & non-goals

### Goals (Phase 1)
- Turn a `Pool` quote + `ExecutionOptions` into an unsigned `UnsignedTx { chain, to, data, value }`.
- Apply **slippage → min-amount-out / max-amount-in** inside the builder, so the on-chain bound can never be fumbled by the caller.
- Support **exact-in and exact-out** builds (symmetric with the existing quote surface).
- Support the DEX-specific encoding each protocol demands (V4 → Universal Router, Curve → direct pool, Solidly stable/volatile flag, etc.).
- Model **approvals** (ERC-20 / Permit2 / assume-approved) without executing them.
- Support **native-ETH** swaps (wrap/unwrap via the router), not WETH-only.
- Stay **pure**: no signer, no `tokio` runtime, no network in the build path — as unit-testable as the quoters.

### Non-goals (Phase 1)
- Signing, submission, nonce/gas management, confirmation tracking — **Phase 2 (`wallet`)**.
- **Multi-hop route building** — single-hop `build_swap` only; route composition stays the caller's job (arb-router already does this). Follow-up.
- Route *optimization* / path-finding — out of scope, as in the quoting spec.
- CEX order APIs — arb-router's concern, architecturally unrelated (REST/WebSocket vs tx-building).
- Non-EVM encoders — the boundary stays chain-family-agnostic, but only EVM is implemented.
- **Fee-on-transfer / rebasing tokens** (E20) — standard-ERC-20 assumption; no FoT metadata or guard. Un-swappable on V3/V4/Curve by contract design regardless; V2/Aerodrome supporting-variant encoding is a later add.
- **Gas estimation & simulation** — need an RPC; the build stays pure. Gas is the wallet layer's job; revert-reason decoding lives in the test layer.
- **Referrer / integrator fees, Permit2 typed-data construction** — designed-for-later (a typed hole), not built in v1.

---

## 2. Locked decisions

| # | Decision | Choice | Rationale |
|---|---|---|---|
| E1 | Scope of "all exchanges" | **On-chain EVM DEXes**; CEX excluded (arb-router's concern) | Tx-building vs order-API are unrelated architectures |
| E2 | Lifecycle depth | **Full lifecycle** the eventual target: multi-wallet, multi-chain, concurrent | Most demanding tier; shapes the Phase-2 nonce/gas/pool design |
| E3 | Crate topology | **Merge `amm-rpc` + calldata builders → `amm-client`**; keep `amm-core` pure | Reading state and encoding a swap are two halves of the same per-protocol on-chain concern |
| E4 | `wallet` crate | **Separate, generic, AMM-agnostic**; new workspace member; extractable later | Knows only chains/wallets/nonces/gas/lifecycle — never Pools or quotes |
| E5 | Phasing | **Phase 1 `amm-client`** (build), **Phase 2 `wallet`** (execute) | Separates the well-understood build work from the still-researched wallet infra |
| E6 | Build vs quote split | **`quoted_out` passed in**, not recomputed; builder applies slippage | Balancer's `query()`→`buildCall()` pattern; enables re-quote/re-build retry |
| E7 | Encode dispatch | **`Executable` trait in `amm-client`**, impl'd for each `amm-core` pool via the orphan rule; reached by **trait-upcasting `&dyn Pool → &dyn Any`** (Rust 1.86) + `PoolKind` downcast | `Executable` needs `alloy-sol-types` → can't live in `amm-core`; upcasting needs zero implementor boilerplate (vs a hand-written `as_any`) and keeps the pool set open (vs `enum_dispatch`) |
| E13 | MSRV | Bump workspace `rust-version` **1.85 → 1.86** | Trait upcasting (E7) stabilized in 1.86; `Pool: Any` forces `Pool: 'static` (fine — pools are owned structs) |
| E8 | Approvals | **Modeled, not executed**: `Permit2`-first with an `AssumeApproved` escape hatch | Matches Uniswap/0x/Balancer; sending the approve tx is Phase 2's job |
| E9 | Native ETH | **Both in + out**, modeled as a `Currency::{Native,Token}` sum type (not a `NativeMode` flag) | Intent lives in the type → can't disagree with the asset; Uniswap-SDK-proven; lets a caller pick ETH *or* WETH output |
| E14 | Options ergonomics | **Slippage required (no `Default`)**; every other option safe-defaulted; fluent dependency-free builder | A silent slippage default is an MEV footgun; safe-by-construction |
| E15 | API idioms | `#[non_exhaustive]` public enums/structs; enums over bare `bool`/`Option` (`Recipient`, `Deadline`, `ApprovalMode`); newtypes for domain values | Future-proof semver; no boolean blindness; make illegal states unrepresentable |
| E16 | Idiomatic sweep | **Dedicated review-and-refactor phase** across the whole codebase (§4) | Apply E14/E15 principles uniformly to existing quoting code, benchmarked against alloy/Uniswap-SDK/other Rust libs |
| E17 | Quote→build bridge | A minimal **`Route { hops, fee_tiers, trade_type }`** value between quote and build (not a bare scalar `quoted_out`) | Matches Uniswap `Trade` / Balancer `query()→buildCall`; future-proofs multi-hop as an additive layer, not a rewrite |
| E18 | Build output | `build_swap` returns a rich **`PreparedSwap { tx, min_received, approval, price_impact }`**, not a bare `UnsignedTx` | Echoes values already computed; hands the caller the approval **spender** (the #1 real revert cause) and guard values without re-deriving (0x/Balancer pattern) |
| E19 | Clock/identity | **Pure clockless build** — `build_swap` requires an absolute `Deadline::AtTimestamp` + explicit `Recipient::To`, typed-errors the rest; a thin **`resolve(opts, now, sender)`** edge helper turns `ttl`/sender into absolutes | Keeps the builder pure/testable (v3-sdk design); no silent `U256::MAX` deadline sentinel (an MEV footgun) |
| E20 | Fee-on-transfer | **Out of scope for v1** — standard ERC-20 assumption, no FoT metadata or guard | Offline detection is impossible; V3/V4/Curve contracts reject FoT by design anyway; V2/Aerodrome supporting-variant encoding is a clean later add |
| E21 | Approval modeling | `PreparedSwap.approval` is **`Option`** (`None` for native input); `ApprovalRequirement` carries **`reset_first`** for USDT/KNC-class tokens (`require(allowance==0 \|\| amount==0)`) | `min_allowance == 0` as a "nothing to approve" sentinel is the illegal-state E15 forbids; the zero-first reset is a real revert class every SDK handles |
| E22 | Exact-out guard | `PreparedSwap.max_spent: Option<AssetAmount>` surfaces the exact-out input ceiling (`max_amount_in`) directly | On exact-out the slippage guard is the *input* max; echoing it (not just burying it in calldata) is symmetric with `min_received` for exact-in |

### 2d. Review round 2 (2026-08-09) — fixes verified against alloy 2.3 source

A second adversarial pass (post-regeneration) verified 8/9 alloy calls against the installed source and folded these fixes into the plan:
- **`Deadline::None` removed** — it silently encoded `U256::MAX` (unbounded deadline = the MEV footgun E19 forbids). `ExecutionOptions::new` defaults `deadline` to `FromNow(default_ttl)`, so an *unresolved* build **errors** (`UnresolvedDeadline`) rather than shipping "never expires". `AtBlock` on a timestamp-bound router → `UnresolvedDeadline`, not `UnsupportedProtocol`.
- **`ForkExec::submit` returns the receipt** (`ExecOutput { status, gas_used, effective_gas_price }`), not `bool` — needed for the native-out proof.
- **Native-out wei-exact** uses a **zero-gas-price** submission (`max_fee = priority = 0`) so the ETH balance delta equals the quote exactly (gas doesn't perturb it).
- **`req.with_from(from)`** (not `req.from(...)`, which is a field); `resolve()` uses `saturating_add`; `fund_erc20` writes the **full-word** slot value (not a truncated low byte); `Cargo.toml` gains alloy `network`/`rpc-types`/`serde` + a declared `serde` feature (else `--all-features` no-ops the derives); exact-out `min_received` is built as `AssetAmount::new(output, raw)`; `build_swap_exact_out` guards `route.trade_type == ExactOut`.

### 2c. Review refinements (2026-08-09, benchmarked vs Uniswap SDK / alloy 2.3 / 0x / Balancer / Rust API Guidelines)

Applied after a four-stream external review of the Phase-1 design:
- **Dispatch:** `as_executable` drops the `PoolKind` pre-gate and simply tries `downcast_ref::<Concrete>()` per known type — `TypeId` is the real safety check, and `kind()` lives on `Introspect` not `Pool`, so gating on it was fragile.
- **`Executable` is sealed** (C-SEALED): the pool set stays open, but the encoder trait is closed so no downstream impl bypasses `as_executable`.
- **`BuildError`** gains `UnresolvedRecipient` / `UnresolvedDeadline`, a **structured** `MissingChainConfig { chain, what: MissingAddr }` (not a `&'static str`), and `#[source]`/`#[from]` chains (mirroring `alloy::contract::Error`). The dead `SlippageUnderflow` marker is dropped.
- **Common traits:** `Currency`/`CurrencyAmount`/`UnsignedTx` derive `Hash` + feature-gated `Serialize`/`Deserialize` (`UnsignedTx` crosses into the Redis-backed wallet); `Currency` gets `Display`. Matches `AssetId`'s trait set (C-COMMON-TRAITS).
- **`From<&UnsignedTx> for TransactionRequest`** bridge — the one place to reuse alloy's type instead of reinventing the last mile.
- **`Pool: Any ⇒ 'static`** is documented on the trait as a conscious constraint (blocks a future borrowing pool).
- **V2 rejects a `Some(price_limit)`** rather than silently dropping it; the single-hop-only `sqrtPriceLimit` invariant is enforced.
- **Builder:** hand-rolled `new(slippage).with_*()` confirmed idiomatic — **reject** `bon`/`typed-builder`/typestate (over-engineering for one required field).
| E10 | Exact-out | **In scope** for Phase 1 | `quote_exact_out` already exists; the build is symmetric |
| E11 | Multi-hop | **Deferred** to a follow-up | Single-hop suffices; arb-router composes routes itself |
| E12 | Rename timing | Land execution **inside `amm-rpc` first**; do the `amm-rpc → amm-client` rename as a final isolated commit | Keeps execution diffs reviewable, not buried under a rename |
| W1 | Wallet persistence | **Redis-backed** (garden-relayer's atomic Lua-script state machine) | Durable, multi-instance-safe from day one |
| W2 | Wallet signer trait | **Deferred** pending OpenZeppelin-Relayer / Privy code-level research | Worth seeing OZ's pluggable-backend design before locking |
| W3 | Wallet concurrency | **Wallet pool** (N hot wallets/chain as lanes) + **one owning actor per wallet** for nonce safety | Convergent answer from Fireblocks (Wallet Pools) and MEV bots (wallet-per-lane) |

---

## 3. Phase 1 architecture — `amm-client`

### 3.1 Crate topology

```
amm-rs/ (workspace)
├── amm-core/     # unchanged — pure quoting math + Pool/extension traits. alloy-primitives + std::any only.
├── amm-client/   # amm-rpc (read state) + NEW calldata builders (write). alloy + alloy-sol-types. AMM-specific.
└── wallet/       # PHASE 3 — generic multi-wallet execution engine. alloy signer/provider + tokio + redis. AMM-agnostic.
```

Dependency direction is one-way: `wallet` never depends on `amm-*`; `amm-client` never depends on `wallet`. They compose only in caller code, which is what keeps `wallet` cleanly extractable later.

### 3.2 The `Executable` trait and dispatch

`amm-core` gains exactly one addition — an `Any` supertrait on `Pool`, so a `&dyn Pool` can be upcast to `&dyn Any` (trait upcasting, stable since Rust 1.86). No new methods, no per-implementor boilerplate:

```rust
// amm-core::traits::pool
pub trait Pool: core::any::Any + Send + Sync { /* unchanged */ }
```

This is the standard modern idiom (the pre-1.86 alternative was a hand-written `as_any(&self) -> &dyn Any`, as in `bevy_reflect`'s `Reflect: Any`; upcasting removes that boilerplate — see the [Rust 1.86 release notes](https://blog.rust-lang.org/2025/04/03/Rust-1.86.0/)). It keeps the pool set **open** to third-party implementors, unlike a closed `enum_dispatch`. `Any: 'static` requires `Pool: 'static`, which holds — every pool is an owned state struct.

`amm-client` owns the encoding trait and implements it for each `amm-core` pool struct (permitted by the orphan rule — local trait, foreign type):

```rust
// amm-client
pub struct UnsignedTx {
    pub chain: ChainId,
    pub to: Address,      // router / pool / PoolManager to call
    pub data: Bytes,      // ABI-encoded calldata
    pub value: U256,      // native value (nonzero only when the input Currency is Native)
}

/// What one side of a swap pays in or receives. `Native` carries the wrap/unwrap
/// intent IN THE TYPE — there is no separate NativeMode flag, so intent can never
/// disagree with the asset. Mirrors the Uniswap SDK's `Currency` model. Keeping
/// `Native` and `Token(weth)` distinct lets a caller choose ETH *or* WETH output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]        // + Display; + serde behind the feature
#[non_exhaustive]
pub enum Currency {
    Native,            // the chain's native coin (ETH) — wrapped/unwrapped automatically
    Token(AssetId),    // an ERC-20, addressed directly
}

/// An amount of a Currency (mirrors amm-core's AssetAmount). Derives Hash + serde.
pub struct CurrencyAmount { pub currency: Currency, pub raw: U256 }

/// The quote→build bridge: the route the swap takes plus its direction. For
/// single-hop v1 `hops` is one pool; it generalizes to multi-hop additively.
#[non_exhaustive]
pub struct Route {
    pub hops: Vec<AssetId>,        // token path (>= 2 for single-hop: [in, out])
    pub fee_tiers: Vec<u32>,       // per-hop fee (V3/V4); empty for V2/Curve
    pub trade_type: TradeType,     // ExactIn | ExactOut
}

/// What the build step produces: the unsigned tx plus everything the caller
/// needs to sign safely without re-deriving it (0x/Balancer pattern).
#[non_exhaustive]
pub struct PreparedSwap {
    pub tx: UnsignedTx,
    pub min_received: AssetAmount,          // exact-in: slippage floor; exact-out: the target
    pub max_spent: Option<AssetAmount>,     // exact-out: the slippage input ceiling (E22)
    pub approval: Option<ApprovalRequirement>, // None when nothing to approve (native input) (E21)
    pub price_impact: Option<Bps>,          // echoed when computable
}

/// The allowance the swap needs before it can succeed — the #1 real revert cause.
#[non_exhaustive]
pub struct ApprovalRequirement {
    pub spender: Address,          // router / Permit2 to approve
    pub token: AssetId,            // the sell token
    pub min_allowance: U256,       // >= the amount the swap pulls
    pub reset_first: bool,         // USDT/KNC-class tokens require allowance→0 before a new non-zero set (E21)
}

/// Safe-by-construction build options. Slippage is REQUIRED (no `Default` invents
/// one — a silent slippage default is an MEV footgun); every other field has a
/// safe default. Constructed fluently via `ExecutionOptions::new(slippage).with_*`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ExecutionOptions {
    pub recipient: Recipient,         // Sender (default) | To(Address) — enum, not Option<Address>
    pub slippage: Slippage,           // required; reuse amm-core's value object
    pub deadline: Deadline,           // FromNow(Duration, default) | AtTimestamp(u64) | AtBlock(u64) — no "None" (E19)
    pub price_limit: Option<Price>,   // genuinely optional → Option; V3/V4/Slipstream sqrtPriceLimit
    pub approval: ApprovalMode,       // AssumeApproved (default) | Erc20 | Permit2 { signature }
}

/// Sealed (C-SEALED): the pool set is open, but only in-crate types encode, so
/// dispatch stays single-source-of-truth. `build_swap` requires an absolute
/// deadline + explicit recipient (E19) — resolve `ttl`/sender at the edge first.
pub trait Executable: private::Sealed {
    fn build_swap(
        &self,
        ctx: &ChainConfig,             // weth + native sentinel + per-protocol routers
        amount_in: CurrencyAmount,
        to: Currency,
        route: &Route,                 // hops + fee tiers + trade_type (E17)
        quoted_out: &AssetAmount,      // from Pool::quote(); slippage floor applied inside
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError>;

    fn build_swap_exact_out(
        &self,
        ctx: &ChainConfig,
        amount_out: CurrencyAmount,
        from: Currency,
        route: &Route,
        quoted_in: &AssetAmount,       // from ExactOut::quote_exact_out()
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError>;
}

impl From<&UnsignedTx> for alloy::rpc::types::TransactionRequest { /* to/value/input/chain_id */ }
```

The builder resolves `Currency::Native → ctx.weth` for the actual pool swap and derives wrap/unwrap from which side is `Native`:
```
ETH → USDC:  in Native,        to Token(usdc)  ⇒ wrap, set value, swap WETH→USDC
USDC → ETH:  in Token(usdc),   to Native       ⇒ swap USDC→WETH, unwrap to recipient
WETH → USDC: in Token(weth),   to Token(usdc)  ⇒ plain ERC-20 swap, no wrap/unwrap
```

`amm-client` provides the bridge from a fetched `Box<dyn Pool>` to its encoder:

```rust
// upcast to Any, then try each known concrete type. No PoolKind gate: downcast_ref's
// TypeId check IS the safety, and kind() lives on Introspect (not Pool), so gating on
// it was fragile. Order is cheap (one TypeId compare each) and short-circuits.
pub fn as_executable(pool: &dyn Pool) -> Option<&dyn Executable> {
    let any: &dyn core::any::Any = pool;   // stable trait upcast (Rust ≥ 1.86)
    if let Some(p) = any.downcast_ref::<UniswapV2Pool>() { return Some(p); }
    if let Some(p) = any.downcast_ref::<UniswapV3Pool>() { return Some(p); }
    if let Some(p) = any.downcast_ref::<UniswapV4Pool>() { return Some(p); }
    if let Some(p) = any.downcast_ref::<CurvePool>()     { return Some(p); }
    // … Aerodrome / Slipstream …
    None
}
```

Hot-path cost: a few `TypeId` (128-bit) compares, then the normal vtable call for the encode — negligible next to the ABI encoding itself.

This keeps the `dyn Pool` quoting API (which arb-router consumes) **untouched**, adds execution as a pure superset, and reuses each pool's stored identity — no parallel identity structs.

### 3.3 Per-protocol encoding

| Protocol | Target | Encoding | Status |
|---|---|---|---|
| Uniswap V2 | Router `swapExactTokensForTokens` / `…ForExactTokens` | token path + slippage bound + deadline | ✅ Feasible with current struct (tokens + `fee_bps`; router from config) |
| Uniswap V3 | Router `exactInputSingle` / `exactOutputSingle` | tokenIn/out, `fee_pips` (already the uint24), optional `sqrtPriceLimit` | ✅ Feasible; `Price → sqrtPriceX96` conversion exists |
| Uniswap V4 | Universal Router `V4_SWAP` command | full `PoolKey` (currency0/1, **static key fee**, tickSpacing, hooks) + actions | ✅ **Now feasible** — identity fields retained (§3.4) |
| Curve | Direct pool `exchange(i, j, dx, min_dy)` / `exchange_underlying` | pool address, coin indices, per-variant ABI | ⚠️ **All 12** variants targeted, **test-matrix-first**, popular-first order (§3.8) |
| Aerodrome (vol/stable) | Solidly router | tokens, **stable flag** (= which struct), factory | ✅ Feasible |
| Aerodrome Slipstream | V3-style router | as V3 | ✅ Feasible |

### 3.4 V4 identity retention — DONE (2026-08-07)

The only protocol needing an `amm-core` struct change. A V4 swap must reconstruct the `PoolKey = {currency0, currency1, fee, tickSpacing, hooks}` that hashes to the pool's `pool_id`; the quoting struct had discarded three of those. **Implemented and tested:**

- `UniswapV4Pool` gained `key_fee: u32` (the *static* pool-key fee — distinct from the live per-direction quoting fee), `tick_spacing: i32`, `hooks_address: Address`, with accessors. `hooks_address` is kept **separate** from the `hooks: Hooks` quoting classification (a `Hooks::None` pool can still carry a non-zero, price-neutral hook address the key requires).
- `amm-rpc`'s `V4PoolConfig` now *retains* the hook address (previously derived-then-dropped); threaded config → `PoolPlan` → pool.
- Test `retained_identity_reconstructs_pool_id` proves the retained fields (with a non-zero hook) re-hash to `pool_id` — i.e. the pool is genuinely swap-encodable.
- The V4/concentrated quoting math was independently audited (adversarial review + live differential harness): **no correctness bugs**.

### 3.5 Cross-cutting concerns

- **Approvals** — the build layer stays pure but is now *informative*: every `PreparedSwap` carries an `ApprovalRequirement { spender, token, min_allowance }` so the caller knows exactly what to approve (the #1 real revert cause) without reverse-engineering it from calldata. `ApprovalMode` still controls encoding: `AssumeApproved` emits only the swap; `Erc20` signals a prepended approve; `Permit2 { signature }` inlines the permit. Permit2 typed-data *construction* (building the EIP-712 to sign) is deferred to a later phase.
- **Fee-on-transfer tokens: out of scope for v1** (E20). Standard-ERC-20 assumption; no metadata or guard. A FoT token would revert on V3/V4/Curve by contract design and mis-quote on V2 — documented, not handled. Tests use non-FoT pairs so the wei-exact proof holds.
- **Deadline / recipient resolution** (E19): `build_swap` is pure and clockless — it requires `Deadline::AtTimestamp` + `Recipient::To`, returning `UnresolvedDeadline`/`UnresolvedRecipient` for the relative/implicit variants. A thin edge helper `resolve(opts, now, sender) -> ExecutionOptions` turns `FromNow(ttl)`/`Sender` into absolutes (v2-sdk's ttl-vs-absolute split), so no silent `U256::MAX` deadline ever reaches the chain.
- **`price_limit`** is honored by V3/V4/Slipstream and **rejected** (`UnsupportedProtocol`) by V2/Curve rather than silently dropped; enforced single-hop-only.
- **Native ETH** (`Currency::Native`) — **both directions in Phase 1**, driven by the type, not a flag: a `Native` *input* sets `UnsignedTx.value` and routes through the wrapping path (Universal Router for V4/UR-capable, or the router's ETH entrypoint for V2/V3); a `Native` *output* appends an unwrap so the recipient gets raw ETH. The builder resolves `Native → ctx.weth` for the pool swap.
- **Slippage:** `build_swap` applies `slippage.min_amount_out(quoted_out)` (clamped ≥ 0); `build_swap_exact_out` applies `slippage.max_amount_in(quoted_in)`. The caller never computes the bound, and the applied floor is echoed back as `PreparedSwap.min_received`. For native exact-out, `UnsignedTx.value` is the **`max_amount_in`** (not the nominal), so the router refunds the unused ETH.
- **Config** (`ChainConfig`, `#[non_exhaustive]`): `{ chain, weth, native_sentinel, routers }`, keyed per chain — mirrors how `amm-rpc` holds factory addresses. `weth` is what `Currency::Native` resolves to; `native_sentinel` is what native encodes as on-chain, **defaulting to `address(0)`** (V4/UR convention) but overridable for ecosystems using a different placeholder (e.g. `0xEeeE…EEeE`). `routers` holds the v2/v3 routers, Universal Router, PoolManager, and Permit2 addresses.

### 3.6 Error handling

`BuildError` (typed, `#[non_exhaustive]`, no panics — same discipline as `QuoteError`, with `#[source]`/`#[from]` chains like `alloy::contract::Error`):
`UnsupportedProtocol`, `MissingChainConfig { chain: ChainId, what: MissingAddr }` (structured, not a string — `MissingAddr` is an enum: `V2Router | V3Router | UniversalRouter | PoolManager | Permit2 | Weth`), `UnresolvedRecipient`, `UnresolvedDeadline`, `NativeMismatch` (native on both sides), `AssetNotInPool { input, output }`, `Overflow`. (The dead `SlippageUnderflow` marker from the draft is dropped — `Slippage::min_amount_out` saturates to zero, so it never fires.)

### 3.7 Testing — differential execution on a managed fork is MANDATORY

The bar: **every pool's built calldata must be proven on-chain to deliver the quoted amount**, not just byte-compared and not just "a swap happened." A build that encodes the wrong index type or ABI variant can revert or misroute funds; only real execution catches that. The gold standard is **differential execution**:

```
fetch pool → our quote Q → build UnsignedTx → submit on a forked node
          → decode actual on-chain output O → assert O == Q (wei-exact)
```

This is strictly stronger than a `minOut = 0` / `balance > 0` smoke test — it closes the loop between quoting (already proven wei-exact by `differential.rs`) and building.

**`ForkExec` harness** — a lean, self-contained execution harness in amm-rs (not coupled to any external app harness). It exposes exactly what an execution test needs and manages the fork lifecycle:
- `ForkExec<P: Provider>` — **generic** over the provider (don't hand-type alloy's nested filler stack). Built via `ProviderBuilder::new().connect_anvil_with_config(|a| a.fork(url).fork_block_number(B))` (the current alloy 2.3 method — `on_anvil_with_config` is deprecated). Dev-deps: alloy `provider-anvil-node` (spawn) **+** `provider-anvil-api` (cheatcodes) **+** `node-bindings`. No manual ports, sleeps, or leaked child processes.
- `fund(who, asset, amount)` — instant funding via `anvil_set_storage_at` on the token's `balanceOf` slot. **Also `anvil_set_balance(who, 100 ETH)`** — an impersonated sender needs gas ETH or every tx fails "insufficient funds."
- `approve(who, token, spender)` — impersonate + ERC-20 `approve`.
- `submit(&UnsignedTx, from) -> ExecOutput` — a real tx from an impersonated `from`; decodes the router return **and** the recipient's balance delta. Takes `&UnsignedTx` (via the `From` bridge), not loose args.
- `snapshot()` / `revert(id)` — `anvil_snapshot`/`anvil_revert` for **per-test isolation**.
- **Same-block discipline:** the quote's reserves are fetched at the fork's *exact* block (assert `get_block_number() == B` and read through the fork provider), or the wei-exact assertion is an off-by-one flake.

The Anvil backend is **swappable**: the same `ForkExec` interface can later run on in-process `revm` + `foundry-fork-db` (offline-reproducible, per-commit-CI speed) without touching the tests. Anvil-first for fidelity; revm later for scale.

**Assertion precision** mirrors `differential.rs`: **wei-exact** for constant-product/stableswap (V2, Curve stable, Aerodrome); **bounded-ppm** for large concentrated-liquidity swaps (V3/V4/Slipstream), where the only divergence is our finite tick-window fetch, not an encoding error. Small in-window concentrated swaps are asserted wei-exact.

- **Differential-execution test per pool (mandatory, gating):** the flow above via `ForkExec`, covering **all four directions the protocol supports** — exact-in ERC-20, exact-out ERC-20, native-in, native-out — not just exact-in. No protocol lands without on-chain proof of each. (The draft plan proved only 1 of 4; that was flagged and fixed.)
- **Golden calldata vectors:** decode our own built calldata and assert every field (selector, path, `amountOutMin`/`amountInMax` = the slippage bound, recipient, deadline) — a fast, network-free regression guard.
- **Unit:** slippage-bound application, native-in/out value+wrap/unwrap logic, approval-mode branching, exact-out symmetry — all network-free.

### 3.8 Curve — test-matrix-first, all 12 variants

Per the execution-correctness bar, Curve is built **test-matrix-first**:

1. **Construct the test matrix before writing encoders.** Enumerate the dimensions every Curve pool must be tested across — variant (plain / lending / meta / CryptoSwap / Tricrypto / ng), index type (`int128` vs `uint256`), entrypoint (`exchange` vs `exchange_underlying`), return-value presence, `receiver` param, and native-coin handling — and pin a representative real mainnet pool per cell.
2. **Implement all 12 variants, most-popular-first** (plain StableSwap `int128`, then CryptoSwap/Tricrypto `uint256`, then meta/underlying, then lending/ng), each landing only when its matrix row passes a **real on-chain execution test** (§3.7).
3. Full quote/execute parity is the goal — no Curve pool quotable but not buildable — reached incrementally with each variant gated by its own on-chain proof.

---

## 4. Phase 2 — Idiomatic API review & refactor

A dedicated quality phase: apply the idiomatic principles surfaced while designing the execution API (E14/E15) **uniformly across the whole codebase**, not just the new code. The trigger was concrete — designing `ExecutionOptions` exposed boolean-blindness and defaulting choices worth auditing everywhere.

**Split into three steps, gated so nothing changes without approval:**

1. **Audit (read-only).** Sweep `amm-core`, `amm-rpc`, and the new `amm-client` surface for non-idiomatic patterns:
   - boolean blindness (bare `bool`/`Option` where an enum states intent);
   - missing `#[non_exhaustive]` on public enums/structs that will grow;
   - primitive obsession (raw `u32`/`String` where a newtype carries meaning — e.g. the `PoolId` string that encodes `chain:exchange:address`);
   - stringly-typed identifiers; `panic!`/`unwrap`/`expect` on non-invariant paths; `Default` impls that invent unsafe values;
   - error-type ergonomics; builder ergonomics; trait object-safety and sealing choices;
   - naming against the repo's "a name states its contents" rule.
2. **Benchmark & suggest.** For each finding, cite how a respected library does it (alloy's builder/`Network`/`SolType` idioms, the Uniswap SDK's `Currency`/`Percent`/`TradeType`, `rust_decimal`/`num-rational`, the Rust API Guidelines) and produce a **suggestions doc** (`docs/specs/…-idiomatic-review.md`) ranked by impact/risk. **User approves before any refactor.**
3. **Refactor.** Apply the approved changes, each behind the existing test suite (quoting stays wei-exact; the live differential harness guards behavior). Semver-breaking changes are batched and called out.

**Sequencing:** the audit (step 1) can run early — even in parallel with Phase 1 — so foundational patterns are flagged before more code is built on them; the refactor (step 3) lands after Phase 1's types exist, so old and new are made consistent in one pass.

---

## 5. Phase 3 — `wallet` (deferred; own spec)

A generic, AMM-agnostic engine: given any `UnsignedTx`, pick a healthy wallet from a per-chain pool, sequence its nonce, apply a gas/fee strategy, sign, submit, and track to confirmation. Its public API mentions no `Pool` or `Quote` — that's what makes it extractable.

### 5.1 Synthesis from local research

- **`standard-rs` = the signing half.** `SignerCore` object-safe trait: `sign(bytes, Algorithm, KeyIdentifier)` over secp256k1/P-256/ed25519/schnorr, backing local keys / HD-mnemonic / YubiHSM. The `key_id` routing is exactly how one signer holds many keys, and it's already non-EVM-ready. Wraps alloy for EVM. **Lacks** any nonce/pool/lifecycle/registry — it's a signing library, not an engine.
- **`garden-relayer` = the execution half.** `NoncePool` (`Arc<Mutex<BTreeSet<u64>>>` per chain: `get_next`/`rollback`/`reset`), a Redis-backed `Pending → Submitted → Confirmed` state machine via **atomic Lua scripts** (multi-instance-safe), three decoupled tokio tasks (executor / confirmation / cleanup), and an alloy encrypted-keystore + optional AWS Secrets Manager loader. **Built for one signer per chain**, and has **no fee bumping** (drops-and-resubmits).

**The design delta:** generalize garden-relayer's single-signer-per-chain machinery to a **registry/pool of many wallets keyed by (chain, address)**, each with its own nonce-owning actor (W3), on `standard-rs`'s custody model (W1/W2), and add the **EIP-1559 fee-bump/replacement state machine** garden-relayer lacks.

### 5.2 External research — OUTSTANDING

The code-level study of **OpenZeppelin-Relayer** (open-source Rust relayer: pluggable signers local/AWS-KMS/GCP-KMS/Turnkey/Fireblocks/Vault, nonce + gas management), **Privy**, **tx-sitter**, and **Rundler** did not complete (interrupted mid-read of OZ's `Signer` trait + tx state machine). It must finish before the Phase-2 signer trait (W2) and fee-bump mechanics are locked. Early finding: OZ's `Signer` signs a full `NetworkTransactionData` (not a raw hash), with a separate `DataSignerTrait` for message/typed-data — a shape to weigh against `standard-rs`'s raw-bytes `SignerCore`.

---

## 6. Resolutions (2026-08-07 review)

1. **Native ETH** (E9) — **both native-in and native-out** in Phase 1. Full round-trip trading; native-out is one extra unwrap command.
2. **Curve coverage** — **all 12 variants**, but **test-matrix-first** and popular-first, each gated by a real on-chain execution test (§3.8). On-chain execution proof is mandatory for *every* protocol, not just Curve (§3.7).
3. **Dispatch** (E7) — **trait upcasting** `&dyn Pool → &dyn Any` (Rust 1.86), not a hand-written `as_any`. Zero implementor boilerplate; keeps the pool set open. Requires MSRV bump to 1.86 (E13).
4. **Rename** (E12) — **`amm-client`, rename last**: execution lands inside `amm-rpc` first, the rename is a single final commit.
