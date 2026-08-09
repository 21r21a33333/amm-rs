# amm-rs — Execution & Wallet Design

**Status:** Phase-1 design settled (4 review questions resolved §5); Phase-2 (wallet) deferred pending external research. Ready for implementation planning.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Currency {
    Native,            // the chain's native coin (ETH) — wrapped/unwrapped automatically
    Token(AssetId),    // an ERC-20, addressed directly
}

/// An amount of a Currency (mirrors amm-core's AssetAmount).
pub struct CurrencyAmount { pub currency: Currency, pub raw: U256 }

/// Safe-by-construction build options. Slippage is REQUIRED (no `Default` invents
/// one — a silent slippage default is an MEV footgun); every other field has a
/// safe default. Constructed fluently via `ExecutionOptions::new(slippage).with_*`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ExecutionOptions {
    pub recipient: Recipient,         // Sender (default) | To(Address) — enum, not Option<Address>
    pub slippage: Slippage,           // required; reuse amm-core's value object
    pub deadline: Deadline,           // FromNow(Duration) | AtTimestamp(u64) | AtBlock(u64) | None
    pub price_limit: Option<Price>,   // genuinely optional → Option; V3/V4/Slipstream sqrtPriceLimit
    pub approval: ApprovalMode,       // AssumeApproved (default) | Erc20 | Permit2 { signature }
}

pub trait Executable {
    fn build_swap(
        &self,
        ctx: &ChainConfig,             // weth + native sentinel + per-protocol routers for this chain
        amount_in: CurrencyAmount,
        to: Currency,
        quoted_out: &AssetAmount,      // from Pool::quote() — still WETH/AssetId-denominated
        opts: &ExecutionOptions,
    ) -> Result<UnsignedTx, BuildError>;

    fn build_swap_exact_out(
        &self,
        ctx: &ChainConfig,
        amount_out: CurrencyAmount,
        from: Currency,
        quoted_in: &AssetAmount,       // from ExactOut::quote_exact_out()
        opts: &ExecutionOptions,
    ) -> Result<UnsignedTx, BuildError>;
}
```

The builder resolves `Currency::Native → ctx.weth` for the actual pool swap and derives wrap/unwrap from which side is `Native`:
```
ETH → USDC:  in Native,        to Token(usdc)  ⇒ wrap, set value, swap WETH→USDC
USDC → ETH:  in Token(usdc),   to Native       ⇒ swap USDC→WETH, unwrap to recipient
WETH → USDC: in Token(weth),   to Token(usdc)  ⇒ plain ERC-20 swap, no wrap/unwrap
```

`amm-client` provides the bridge from a fetched `Box<dyn Pool>` to its encoder:

```rust
// upcast to Any, then downcast to the concrete type PoolKind names
pub fn as_executable(pool: &dyn Pool) -> Option<&dyn Executable> {
    let any: &dyn core::any::Any = pool;   // stable trait upcast (Rust ≥ 1.86)
    match pool.kind() {
        PoolKind::UniswapV2  => any.downcast_ref::<UniswapV2Pool>().map(|p| p as &dyn Executable),
        PoolKind::UniswapV3  => any.downcast_ref::<UniswapV3Pool>().map(|p| p as &dyn Executable),
        PoolKind::UniswapV4  => any.downcast_ref::<UniswapV4Pool>().map(|p| p as &dyn Executable),
        PoolKind::Curve      => any.downcast_ref::<CurvePool>().map(|p| p as &dyn Executable),
        PoolKind::Aerodrome | PoolKind::Slipstream => /* … */ None,
        _ => None,
    }
}
```

Hot-path cost: one `PoolKind` integer compare + one `TypeId` (128-bit) compare, then the normal vtable call for the encode — negligible next to the ABI encoding itself.

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

- **Approvals** (`ApprovalMode`): the build layer stays pure. `AssumeApproved` emits only the swap; `Erc20Approve` signals the caller/Phase-2 to prepend an approve; `Permit2 { signature }` inlines the permit. Permit2-first matches Uniswap/0x/Balancer.
- **Native ETH** (`Currency::Native`) — **both directions in Phase 1**, driven by the type, not a flag: a `Native` *input* sets `UnsignedTx.value` and routes through the wrapping path (Universal Router for V4/UR-capable, or the router's ETH entrypoint for V2/V3); a `Native` *output* appends an unwrap so the recipient gets raw ETH. The builder resolves `Native → ctx.weth` for the pool swap.
- **Slippage:** `build_swap` applies `slippage.min_amount_out(quoted_out)`; `build_swap_exact_out` applies `slippage.max_amount_in(quoted_in)`. The caller never computes the bound.
- **Config** (`ChainConfig`, `#[non_exhaustive]`): `{ chain, weth, native_sentinel, routers }`, keyed per chain — mirrors how `amm-rpc` holds factory addresses. `weth` is what `Currency::Native` resolves to; `native_sentinel` is what native encodes as on-chain, **defaulting to `address(0)`** (V4/UR convention) but overridable for ecosystems using a different placeholder (e.g. `0xEeeE…EEeE`). `routers` holds the v2/v3 routers, Universal Router, PoolManager, and Permit2 addresses.

### 3.6 Error handling

`BuildError` (typed, no panics — same discipline as `QuoteError`):
`UnsupportedProtocol`, `MissingChainConfig { chain, what }`, `SlippageUnderflow`, `NativeMismatch` (native intent vs the assets actually swapped), `AssetNotInPool`, `Overflow`.

### 3.7 Testing — on-chain execution proof is MANDATORY

The bar: **every pool's built calldata must be proven on-chain**, not just byte-compared. A build that encodes the wrong index type or ABI variant can revert or misroute funds; only actual execution catches that.

- **On-chain execution test per pool (mandatory, gating):** submit/simulate the built `UnsignedTx` against a forked node (`eth_call` for output-equality, and a state-changing `eth_sendTransaction`/`eth_simulateV1` where balance deltas must be checked), asserting the recipient receives the quoted output (wei-exact where the protocol is deterministic) and no revert. No protocol lands without this.
- **Golden calldata vectors:** encode a known swap per protocol, assert bytes against a real router transaction pulled from chain (fast, deterministic regression guard).
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
