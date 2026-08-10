# amm-rs — Multi-Protocol Execution Design (V3 / Slipstream / Aerodrome / Curve / V4)

**Status:** complete — architecture, phasing, and byte-exact per-protocol ABIs settled (verified against primary sources). Extends the V2 slice ([2026-08-07-amm-execution-and-wallet-design.md](2026-08-07-amm-execution-and-wallet-design.md)). Ready for implementation planning.
**Date:** 2026-08-10

**Goal:** Complete the `amm-client` execution layer — swap-calldata builders for the remaining protocols behind the *same* `Executable` trait, each proven wei-exact on a real fork via the existing revm `Fork` harness.

**Reuses, unchanged:** the `Executable` trait + `as_executable` dispatch, `Currency`/`Route`/`PreparedSwap`/`ExecutionOptions`/`ChainConfig`/`BuildError`, and the revm `Fork` harness + wei-exact proof pattern. Each new protocol is one sealed `impl Executable for <Pool>` + one `execution_<proto>.rs` proof.

---

## 1. The command / multicall layer (the one new architectural piece)

V2's router had native entrypoints, so every V2 swap is a single call. The remaining routers need multi-step assembly for native-ETH / fee / permit. Rather than one grand "command framework" (premature abstraction — the mechanisms are genuinely different), we add **two small, single-responsibility helpers**, each used only by the protocols that need it:

### 1a. `multicall` — for SwapRouter02-family (Uniswap V3, Aerodrome Slipstream)
```rust
// amm-client
/// Assemble SwapRouter02 sub-calls. Uniswap's rule: a lone call is passed
/// through raw; >1 is wrapped in `multicall(bytes[])`. When a timestamp bound is
/// needed, `multicall(uint256 deadline, bytes[])` is used instead.
pub fn encode_multicall(deadline: Option<u64>, calls: Vec<Bytes>) -> Bytes;
```
Native-out appends `unwrapWETH9(minOut, recipient)`; native-in exact-out appends `refundETH()`. Single ERC-20 exact-in stays a raw `exactInputSingle` (no multicall overhead).

### 1b. `RoutePlanner` — for the Universal Router (Uniswap V4)
```rust
// amm-client
/// Accumulate (command_byte, input_bytes) pairs, then encode
/// `execute(bytes commands, bytes[] inputs)` (optionally with a deadline).
pub struct RoutePlanner { /* commands: Vec<u8>, inputs: Vec<Bytes> */ }
impl RoutePlanner {
    pub fn add(&mut self, command: u8, input: Bytes) -> &mut Self;
    pub fn encode(&self, deadline: Option<u64>) -> Bytes; // execute(...) calldata
}
```
V4 native/permit are commands in the stream (`WRAP_ETH` / `PERMIT2_PERMIT` / `V4_SWAP` / `UNWRAP_WETH` / `SWEEP`).

**Curve and Aerodrome-Solidly touch neither** — they're single direct calls.

The `Executable::build_swap` signature is unchanged; each encoder just assembles its final `UnsignedTx.data` via the helper it needs (or none).

---

## 2. Phasing (easy → hard, layer-first)

Each phase = one sealed `impl Executable` + a wei-exact `Fork` proof (all four directions where the protocol supports them), phase-reviewed and committed before the next.

| # | Phase | Builds | Notes |
|---|---|---|---|
| **E1** | **Uniswap V3** | the `multicall` helper (1a) + `exactInputSingle`/`exactOutputSingle` | fee tier from the pool; `sqrtPriceLimit` from `price_limit` (single-hop only); native via multicall + `unwrapWETH9`/`refundETH`. Establishes the multicall layer. |
| **E2** | **Aerodrome Slipstream** | reuses E1's `multicall` | V3-twin; **pools keyed by `tickSpacing`, not fee tier** — the one real difference. Base chain. |
| **E3** | **Aerodrome (Solidly)** | direct router call | `Route { from, to, stable, factory }` array; `stable` + `factory` from pool; native entrypoints like V2; **no exact-out** (Solidly has none). Base chain. |
| **E4** | **Curve** | direct pool `exchange` calls | §3 — test-matrix-first, popular variants first, then all 12. |
| **E5** | **Uniswap V4** | the `RoutePlanner` (1b) + Permit2 | hardest: UR command stream, `PoolKey` from the retained identity fields, Permit2 approval path, native-as-first-class-currency. Needs a harness Permit2 helper. |

### Concentrated-liquidity assertion precision (E1/E2/E5)
Per the V2-slice spec §3.7: **wei-exact for small in-window swaps; bounded-ppm for large** concentrated swaps (the finite tick-window fetch is the only divergence). The proof uses swap sizes small enough for wei-exact, with an explicit bounded-ppm assertion for a large-swap case.

---

## 3. Curve — test-matrix-first, popular-first, all 12

Per the locked decision (spec §3.8): construct the **test matrix before encoders**, implement most-popular variants first, each gated by its own on-chain wei-exact test, extending to all 12.

1. **Test matrix (before any Curve encoder):** enumerate the dimensions every Curve pool must be tested across — variant kind, index type (`int128` vs `uint256`), entrypoint (`exchange` vs `exchange_underlying`), receiver-param presence, return-value presence, native/`use_eth` handling — and pin one representative real mainnet pool per cell.
2. **Order:** plain StableSwap (`int128`) → Tricrypto/CryptoSwap (`uint256`) → stETH/LST-style → meta/underlying → lending → NG variants.
3. **Dispatch:** our pool already classifies its variant for quoting; map each variant kind → the correct `exchange` signature (§4.4). A variant not yet encoded returns `BuildError::UnsupportedProtocol` (quoting still works).

---

## 4. Per-protocol encoding — verified ABIs

Byte-exact from primary sources; verbatim `sol!` blocks + selectors live in the plan's task code. The load-bearing facts and per-direction recipes:

### 4.1 Uniswap V3 — SwapRouter02 `0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45`
- `exactInputSingle((tokenIn,tokenOut,uint24 fee,recipient,amountIn,amountOutMinimum,uint160 sqrtPriceLimitX96))` = `0x04e45aaf`; `exactOutputSingle(…amountOut,amountInMaximum,…)` = `0x5023b4df`. **No `deadline` in the struct** (SwapRouter02's key difference) — deadline goes in `multicall(uint256,bytes[])` = `0x5ae401dc`.
- **`fee` maps 1:1** from the pool's stored tier (500/3000/10000 as raw `uint24`). `sqrtPriceLimitX96 = 0` = no limit (single-hop only). Approve **the router directly** (no Permit2).
- Native via the `multicall` helper: **native-in** → `tokenIn=WETH`, `msg.value=amountIn` (exact-out also appends `refundETH()` = `0x12210e8a`); **native-out** → `recipient=ADDRESS_THIS (address(2))` + `unwrapWETH9(min, user)` = `0x49404b7c`. Six-case table verified.

### 4.2 Aerodrome Slipstream — SwapRouter `0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5` (Base)
- V3-`SwapRouter`-style (the older one, **`deadline` IS in the struct**). **CRITICAL: `int24 tickSpacing` replaces `uint24 fee`** in the same slot — struct `(tokenIn,tokenOut,int24 tickSpacing,recipient,deadline,amountIn,amountOutMinimum,uint160 sqrtPriceLimitX96)`. Selector differs from Uniswap's (`sol!` computes it). Native = same `multicall`+`unwrapWETH9`/`refundETH` pattern. `tickSpacing` from the pool's stored spacing. WETH(Base)=`0x4200…0006`.

### 4.3 Aerodrome (Solidly Router) — `0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43` (Base)
- `swapExactTokensForTokens(amountIn, amountOutMin, Route[] routes, to, deadline)` = `0xcac88ea9`, with **`Route { address from; address to; bool stable; address factory }`** (Aerodrome added `factory` — 4-field tuple). Native entrypoints `swapExactETHForTokens`/`swapExactTokensForETH` (V2-style). **No exact-out** (Solidly lineage — `build_swap_exact_out` → `UnsupportedProtocol`). `stable` + `factory` from the pool. FoT variants exist but are out of scope (E20).

### 4.4 Curve `exchange` variant map — direct pool calls (no router)
Four signature families, dispatched by the variant kind our pool already classifies at discovery. `i`/`j` = coin positions in the pool's `coins()` order (we already store this); `min_dy` (4th arg, universal) = the slippage floor; `dx` = amountIn.

| Family | Signature (selector) | Applies to |
|---|---|---|
| **StableI128** | `exchange(int128,int128,uint256,uint256)` `0x3df02124` (void) | classic StableSwap plain / lending / meta (int128 indices) |
| **StableI128Ng** | `exchange(int128,int128,uint256,uint256[,address])` `0x3df02124`/`0xddc1f59d` (returns uint256) | StableSwap-NG |
| **CryptoU256UseEth** | `exchange(uint256,uint256,uint256,uint256,bool[,address])` `0x394747c5`/`0xce7d6503` (payable) | classic CryptoSwap / Tricrypto / Tricrypto-NG |
| **CryptoU256Receiver** | `exchange(uint256,uint256,uint256,uint256[,address])` `0x5b41b908`/`0xa64833a0` (**NO `use_eth`**) | **Twocrypto-NG** |

- **Two burn-in traps:** (a) **Twocrypto-NG dropped `use_eth`** — its 5th arg is `receiver`, not `bool`; encoding it with the CryptoU256UseEth builder emits the wrong selector and reverts. (b) classic Tricrypto has **no receiver** (only `use_eth`); Tricrypto-NG added receiver.
- **`use_eth`:** default `false` (trade WETH via ERC-20); set `true` only when deliberately funding `msg.value` on a payable crypto pool. StableSwap is never payable.
- **`exchange_underlying`** (`0xa6417ed6` int128 / `0x65b2489b` uint256, + receiver forms) for lending/meta trading the underlying coin — **lowest priority** (index space differs — needs a separate underlying-index map); most volume routes plain `exchange`.
- **Implementation order** (§3): plain StableSwap int128 → classic Tricrypto/CryptoSwap uint256 → StableSwap-NG → Twocrypto-NG → Tricrypto-NG → underlying/lending tail. `sol!` needs one interface per exact arity (overloads are declaration-order-fragile) — a separate interface per family, never overloaded together.
- Curve rejects `price_limit` (`UnsupportedProtocol`) and has no exact-out.

### 4.5 Uniswap V4 — Universal Router (configurable; default `0x66a9893cc07d91d95644aedd05d03f95e1dba8af`)
- `execute(bytes commands, bytes[] inputs, uint256 deadline)` = `0x3593564c`. Built via the `RoutePlanner`. Command bytes: `V4_SWAP=0x10`, `WRAP_ETH=0x0b`, `UNWRAP_WETH=0x0c`, `PERMIT2_PERMIT=0x0a`, `SWEEP=0x04` (bare byte = revert-on-fail; do NOT set `0x80`).
- `V4_SWAP` input = `abi.encode(bytes actions, bytes[] params)`. Single-hop exact-in actions = `0x06 0c 0f` (`SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE_ALL`); exact-out = `0x08 0c 0f`. `params[0]` = `ExactInputSingleParams{ PoolKey, bool zeroForOne, uint128 amountIn, uint128 amountOutMinimum, bytes hookData }`; `PoolKey{ currency0, currency1, uint24 fee, int24 tickSpacing, address hooks }` ← **maps directly to our retained `key_fee`/`tick_spacing`/`hooks_address`**. `SETTLE_ALL`/`TAKE_ALL` params = `(Currency, uint256)`.
- Native is a first-class currency (`address(0)`); wrap/unwrap only when **pool-currency-native XOR user-currency-native**. Approval via **Permit2** (`spender = Permit2`, then `PERMIT2_PERMIT` in the stream) — the `Fork` harness gains a Permit2 helper for the E5 proof.
- **GOTCHA (pin this):** do NOT generate `sol!` from v4-periphery `main` — it has an extra `minHopPriceX36` field absent from the deployed router; use the deployed struct layout above or every swap reverts. Router address is configurable (two live mainnet URs).

---

## 5. Cross-cutting

- **Approvals:** V3/Slipstream/Aerodrome/Curve → approve the respective router directly (`ApprovalRequirement.spender = router`). **V4 → Permit2**: approve Permit2, then `PERMIT2_PERMIT` in the stream (`spender = Permit2`); the `Fork` harness gains a Permit2 approval helper for the E5 proof.
- **Fee/identity source:** each encoder reads its identity from the concrete pool via the downcast it already does (V3 fee tier, Slipstream tickSpacing, Curve coin indices, Aerodrome stable/factory, V4 the retained `key_fee`/`tick_spacing`/`hooks_address`). `Route.fee_tiers` carries it for the future multi-hop path.
- **Native (`Currency::Native`):** per-protocol (V3 multicall wrap/unwrap; Aerodrome-Solidly native entrypoints; Curve `use_eth`/payable where applicable; V4 native-as-currency). `PreparedSwap.approval == None` for native input everywhere.
- **`price_limit`:** honored by V3/Slipstream/V4 (single-hop `sqrtPriceLimit`); rejected (`UnsupportedProtocol`) by Curve/Aerodrome-Solidly which have no price bound.
- **Harness reuse:** every proof uses the existing revm `Fork`; E2/E3 fork **Base** (need a Base archive RPC), E1/E4/E5 fork Ethereum.

---

## 6. Testing bar (unchanged)

Every protocol direction is proven by a wei-exact `Fork` proof driven through the full consumer path (build → fund → apply `PreparedSwap.approval` → submit → assert delta == quote), `#[ignore]`d/gated on `$AMM_RPC_FORK_URL` (+ a Base RPC for E2/E3). No protocol lands without its on-chain proof.
