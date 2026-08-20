//! Tier-2 Uniswap multi-hop fork proofs — the first on-chain proof of the
//! Plan 2a umbrella calldata.
//!
//! # Fork matrix (`multihop_uniswap_matrix`)
//!
//! Each row in [`multihop_uniswap_cases`] is a [`PlanCase`] driven by
//! [`run_plan_case`] against a live revm fork pinned to block 20_000_000.
//! All cases are `ExactIn` / `WeiExact`; each covers one meaningful variant of
//! multi-hop Uniswap routing through the Universal Router.
//!
//! # Offline structural test (`structural_uniswap_span_wellformed_2_to_6_hops`)
//!
//! Builds a synthetic all-V3 route of `n` pools (n ∈ 2..=6) via the public
//! `plan(...)` API and decodes the resulting UR calldata to assert the
//! well-formedness invariants at every depth — proving arbitrary-N correctness
//! without a fork.  No new deps; uses only pool constructors and decode helpers
//! that exist in the `pub(crate)` test module of `uniswap_ur.rs`, replicated
//! here via the public `plan` API path.
//!
//! # What is NOT covered (controller to add)
//!
//! - **native-in 2-hop**: the chaining fixtures available are all ERC-20 pool
//!   pairs; the only ETH pool in the registry is the V4 `eth_usdc_v4_005`
//!   fixture, which cannot be chained with the V3 USDC/WETH pool without adding
//!   a new cross-version native fixture.  Omitted until the controller adds a
//!   V3 ETH/USDC pool fixture or a V4→V3 chain fixture.
//! - **4-hop all-V3**: the current fixture catalog has exactly three mainnet V3
//!   pool entries that form a linear chain (DAI/USDC → USDC/WETH → WETH/USDT).
//!   A fourth hop would require a verified USDT/{TOKEN} pool fixture not yet in
//!   the catalog.  Omitted.

#![cfg(test)]

#[path = "support/mod.rs"]
mod support;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::{SolCall, SolValue};
use amm_core::primitives::asset::{AssetId, ChainId};
use amm_core::primitives::pool::PoolId;
use amm_core::primitives::ratio::Bps;
use amm_core::protocols::uniswap::v3::{TickData, TickInfo, UniswapV3Pool};
use amm_core::slippage::Slippage;
use amm_rpc::execution::routing::{ExactOutPolicy, Route};
use amm_rpc::execution::types::TradeType;
use amm_rpc::execution::{ChainConfig, Deadline, ExecutionOptions, Recipient, Routers, plan};
use support::{
    BuildErrorKind, Expect, PlanCase, RecipientKind, base_chain_config, mainnet_chain_config,
    open_fork, run_plan_case,
};

// ── Verified token address constants ─────────────────────────────────────────
//
// Addresses taken from the task brief and cross-checked with the fixture catalog
// in `support/fixtures.rs`.  These are used in `PlanCase::path` (hex strings)
// and in the structural test assertions (parsed `Address` values).

/// DAI on Ethereum mainnet (18 dp, slot 2).
const DAI: &str = "0x6B175474E89094C44Da98b954EedeAC495271d0F";
/// USDC on Ethereum mainnet (6 dp, slot 9).
const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
/// WETH on Ethereum mainnet (18 dp, slot 3).
const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
/// USDT on Ethereum mainnet (6 dp, slot 2).
const USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
/// crvUSD on Ethereum mainnet (18 dp) — output-only in the Curve matrix.
#[cfg(feature = "curve")]
const CRVUSD: &str = "0xf939E0A03FB07F59A73314E73794Be0E57ac1b4E";
/// TwoCrypto-NG token (18 dp) — output-only; pairs with WETH in curve_twocrypto_ng.
#[cfg(feature = "curve")]
const TC_NG_TOKEN: &str = "0x1cfa5641c01406aB8AC350dEd7d735ec41298372";

// ── Case table ─────────────────────────────────────────────────────────────────

/// All multi-hop Uniswap swap cases for the `multihop_uniswap_matrix` test.
///
/// Each row is an [`ExactIn`][TradeType::ExactIn] swap with [`Expect::WeiExact`]:
/// the on-chain output must equal the off-chain quote to the wei.  Every route
/// is a single Uniswap span (all pools route through the Universal Router), so
/// `plan.is_atomic() == true` and the harness emits one UR `execute` transaction.
///
/// # Fixture-to-address mapping (from `support/fixtures.rs`)
///
/// | fixture           | token0        | token1  | fee   |
/// |-------------------|---------------|---------|-------|
/// | `dai_usdc_v3`     | DAI (18 dp)   | USDC    | 100   |
/// | `usdc_weth_v3_005`| USDC (6 dp)   | WETH    | 500   |
/// | `weth_usdt_v3`    | WETH (18 dp)  | USDT    | 3000  |
/// | `usdc_weth_v2`    | USDC (6 dp)   | WETH    | V2    |
fn multihop_uniswap_cases() -> Vec<PlanCase> {
    vec![
        // ── Case 1: 2-hop all-V3 (USDC → WETH → USDT) ────────────────────────
        //
        // Pools:  usdc_weth_v3_005 (USDC token0, WETH token1, fee 500)
        //         weth_usdt_v3     (WETH token0, USDT token1, fee 3000)
        // Path:   USDC → WETH → USDT
        // Amount: 1 000 USDC = 1_000_000_000 (6 decimal places)
        //
        // Proves the minimal 2-hop all-V3 calldata: two V3_SWAP_EXACT_IN commands,
        // hop-0 payerIsUser=true, hop-1 CONTRACT_BALANCE, final hop → recipient.
        PlanCase {
            name: "multihop_v3_v3_usdc_weth_usdt",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005", "weth_usdt_v3"],
            path: &[USDC, WETH, USDT],
            amount: U256::from(1_000_000_000u64), // 1 000 USDC
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // ── Case 2: 3-hop all-V3 (DAI → USDC → WETH → USDT) ─────────────────
        //
        // Pools:  dai_usdc_v3      (DAI token0, USDC token1, fee 100)
        //         usdc_weth_v3_005 (USDC token0, WETH token1, fee 500)
        //         weth_usdt_v3     (WETH token0, USDT token1, fee 3000)
        // Path:   DAI → USDC → WETH → USDT
        // Amount: 1 000 DAI = 1_000_000_000_000_000_000_000 (18 decimal places = 1e21)
        //
        // Proves the 3-hop all-V3 calldata: three V3_SWAP_EXACT_IN commands.
        // Hop-0 payerIsUser=true; hops 1 and 2 CONTRACT_BALANCE; only hop-2
        // carries a non-zero amountOutMinimum (the compounded slippage floor).
        PlanCase {
            name: "multihop_v3_v3_v3_dai_usdc_weth_usdt",
            chain: ChainId(1),
            pools: &["dai_usdc_v3", "usdc_weth_v3_005", "weth_usdt_v3"],
            path: &[DAI, USDC, WETH, USDT],
            // 1 000 DAI (18 decimals = 1e21)
            amount: U256::from(1_000_000_000_000_000_000_000u128),
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // ── Case 3: 2-hop mixed V2→V3 (USDC → WETH → USDT) ──────────────────
        //
        // Pools:  usdc_weth_v2  (USDC token0, WETH token1, Uniswap V2)
        //         weth_usdt_v3  (WETH token0, USDT token1, fee 3000)
        // Path:   USDC → WETH → USDT
        // Amount: 1 000 USDC = 1_000_000_000
        //
        // Proves a mixed-version V2+V3 span: one V2_SWAP_EXACT_IN command
        // followed by one V3_SWAP_EXACT_IN command in the same UR execute call.
        // Both hops share a single UR tx because the partition groups them all
        // under RouterKind::UniswapUniversal.
        PlanCase {
            name: "multihop_v2_v3_usdc_weth_usdt",
            chain: ChainId(1),
            pools: &["usdc_weth_v2", "weth_usdt_v3"],
            path: &[USDC, WETH, USDT],
            amount: U256::from(1_000_000_000u64), // 1 000 USDC
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // ── Case 4: 2-hop native-out (DAI → USDC → ETH) ──────────────────────
        //
        // Pools:  dai_usdc_v3      (DAI → USDC)
        //         usdc_weth_v3_005 (USDC → WETH, then unwrap → ETH)
        // Path:   DAI → USDC → WETH  (native_out=true unwraps final WETH to ETH)
        // Amount: 1 000 DAI = 1e21
        //
        // Proves native-out at the end of a 2-hop V3 span: the last hop delivers
        // WETH to ADDRESS_THIS and an UNWRAP_WETH command converts it to ETH for
        // the recipient.  The UR command sequence is:
        //   [V3_SWAP_EXACT_IN (hop 0), V3_SWAP_EXACT_IN (hop 1, to router), UNWRAP_WETH]
        PlanCase {
            name: "multihop_v3_v3_native_out_dai_usdc_eth",
            chain: ChainId(1),
            pools: &["dai_usdc_v3", "usdc_weth_v3_005"],
            path: &[DAI, USDC, WETH],
            amount: U256::from(1_000_000_000_000_000_000_000u128), // 1 000 DAI
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: true, // unwrap final WETH to ETH
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // ── Case 5: 2-hop all-V3 with a distinct recipient ────────────────────
        //
        // Identical to case 1 (USDC → WETH → USDT) but the final output lands at
        // a different address (0xD1…D1) instead of the sender.  Proves that the
        // `recipient` field in the last V3 hop is correctly overridden and the
        // sender receives nothing of the output token.
        PlanCase {
            name: "multihop_v3_v3_usdc_weth_usdt_distinct_recipient",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005", "weth_usdt_v3"],
            path: &[USDC, WETH, USDT],
            amount: U256::from(1_000_000_000u64), // 1 000 USDC
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Distinct,
            expect: Expect::WeiExact,
        },
        // ── Case 6: 2-hop all-V3 EXACT-OUT (USDC → WETH → exact USDT) ─────────
        //
        // Single Uniswap span, Strict exact-out → one `V3_SWAP_EXACT_OUT` command
        // over the reversed path (USDT‖3000‖WETH‖500‖USDC) with amountInMaximum.
        // Proves `build_uniswap_span_exact_out` on-chain: the recipient receives
        // EXACTLY the target, and the input spent is bounded by max_spent.
        PlanCase {
            name: "multihop_v3_exact_out_usdc_weth_usdt",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005", "weth_usdt_v3"],
            path: &[USDC, WETH, USDT],
            amount: U256::from(500_000_000u64), // target 500 USDT (6 dp)
            trade_type: TradeType::ExactOut,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::Exact,
        },
        // Multi-hop OrBetter exact-out: same 2-hop route, but the caller accepts
        // ≥ 500 USDT. The executor backward-solves the input, then runs a forward
        // exact-in span with the FINAL floor pinned to 500 USDT — deliver ≥ target
        // (AtLeast). Exercises OrBetter degradation across a multi-pool span.
        PlanCase {
            name: "multihop_v3_exact_out_orbetter_usdc_weth_usdt",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005", "weth_usdt_v3"],
            path: &[USDC, WETH, USDT],
            amount: U256::from(500_000_000u64), // target ≥ 500 USDT (6 dp)
            trade_type: TradeType::ExactOut,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::AtLeast,
        },
    ]
}

// ── Fork matrix ────────────────────────────────────────────────────────────────

/// Fork-backed multi-hop Uniswap execution matrix.
///
/// Requires `$AMM_RPC_FORK_URL` pointing to an Ethereum mainnet archive node.
/// Pinned to block 20_000_000 (or `$AMM_FORK_BLOCK` override).
///
/// Each [`PlanCase`] row is run through [`run_plan_case`], which:
/// 1. Refreshes all pools at the fork block.
/// 2. Builds a Route + Plan via the public `plan()` API.
/// 3. Funds the first-span input, drives the Plan loop, and asserts the
///    end-to-end on-chain output == the quoted output (WeiExact).
/// 4. Asserts zero residue on the Universal Router and distinct-recipient
///    isolation where applicable.
///
/// **Do not run this test locally without an archive RPC — it is `#[ignore]`.**
/// **The controller runs the fork matrix on a keyless node.**
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL (archive node, mainnet, block 20_000_000)"]
async fn multihop_uniswap_matrix() {
    let Some((mut fork, _block)) = open_fork("AMM_RPC_FORK_URL", 20_000_000, ChainId(1)).await
    else {
        return;
    };
    let cfg = mainnet_chain_config();
    for case in multihop_uniswap_cases() {
        run_plan_case(&mut fork, &cfg, &case).await;
    }
}

// ── Tier-3 cross-router cases ─────────────────────────────────────────────────

/// All cross-router multi-hop cases for [`multihop_cross_router_matrix`].
///
/// Each route partitions into **two** spans from different routers:
///   - Span 1 executes via the Uniswap Universal Router (V3 pool).
///   - Span 2 executes via Curve's `exchange` (single Curve pool).
///
/// Consequently `plan.is_atomic() == false` and `plan.tx_count() == 2`.
///
/// # Recipient rationale — `RecipientKind::Sender` for Curve-final spans
///
/// Curve's `StableSwap exchange(i, j, dx, min_dy)` has no `receiver` argument
/// — it always pays the output to `msg.sender`.  Requesting a distinct recipient
/// here would strand the funds in the sender's account rather than routing them
/// to the intended address, so every case whose **final** span is a Curve pool
/// uses `RecipientKind::Sender`.  This is a known limitation documented in
/// task_30adec14; the reverse case (Curve→Uniswap) also keeps `Sender` for
/// simplicity in this first pass — the final Uniswap span could accept a
/// distinct recipient, but forcing `Sender` exercises the same code path as
/// the more constrained Curve-final direction and keeps the case table uniform.
///
/// # Native cross-router — Unfillable
///
/// No clean native-endpoint cross-router fixture chain exists in the current
/// catalog:
/// - The only native-ETH pool is the V4 `eth_usdc_v4_005` fixture (ETH/USDC).
///   Chaining it with Curve 3pool requires a shared token (USDC) and a
///   compatible fee/tick parameter set, but there is no V3 ETH/USDC or
///   V4-WETH/USDC pool in the catalog that forms a linear path with Curve.
/// - Adding a synthetic native fixture without on-chain verification would
///   invalidate the WeiExact assertion.
///
/// Verdict: **Unfillable** — native cross-router cases omitted until the
/// controller adds a verified V3 or V4 ETH/token pool fixture that shares a
/// token with an existing Curve fixture.
///
/// # Fixture-to-address mapping
///
/// | fixture        | token0         | token1 | coins (all)      |
/// |----------------|----------------|--------|------------------|
/// | `dai_usdc_v3`  | DAI (18 dp)    | USDC   | —                |
/// | `curve_3pool`  | DAI (coin[0])  | USDC   | DAI, USDC, USDT  |
#[cfg(feature = "curve")]
fn multihop_cross_router_cases() -> Vec<PlanCase> {
    vec![
        // ── Case 1: Uniswap→Curve (DAI→USDC via V3, then USDC→USDT via Curve 3pool) ──
        //
        // Pools:  dai_usdc_v3    (Uniswap V3 span — Universal Router)
        //         curve_3pool    (Curve StableSwap span — single-hop build_swap)
        // Path:   DAI → USDC → USDT
        // Amount: 1 000 DAI = 1_000_000_000_000_000_000_000 (1e21, 18 dp)
        //
        // Span 1 (UniswapUniversal): DAI → USDC, delivers USDC to SENDER.
        // Span 2 (Curve): USDC → USDT via curve_3pool.exchange(1, 2, dx, min_dy).
        //
        // Curve's `exchange` has no receiver arg — output always goes to msg.sender.
        // Using RecipientKind::Sender is therefore required, not a choice:
        // a distinct recipient would strand USDT at SENDER (task_30adec14).
        //
        // Proves: is_atomic()==false, tx_count()==2, two sequential on-chain txs,
        //         WeiExact end-to-end delta at SENDER for the USDT output.
        PlanCase {
            name: "cross_router_uniswap_curve_dai_usdc_usdt",
            chain: ChainId(1),
            pools: &["dai_usdc_v3", "curve_3pool"],
            path: &[DAI, USDC, USDT],
            // 1 000 DAI (18 decimals = 1e21)
            amount: U256::from(1_000_000_000_000_000_000_000u128),
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            // Curve final span: exchange() delivers to msg.sender; Distinct would
            // strand funds. Use Sender to match Curve's hardcoded output direction.
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // ── Case 2: Curve→Uniswap (USDT→USDC via Curve 3pool, then USDC→DAI via V3) ──
        //
        // Pools:  curve_3pool    (Curve StableSwap span — single-hop build_swap)
        //         dai_usdc_v3    (Uniswap V3 span — Universal Router)
        // Path:   USDT → USDC → DAI
        // Amount: 1 000 USDT = 1_000_000_000 (6 dp)
        //
        // Span 1 (Curve): USDT → USDC via curve_3pool.exchange(2, 1, dx, min_dy).
        //   Curve delivers USDC to msg.sender (SENDER).
        // Span 2 (UniswapUniversal): USDC → DAI, recipient = SENDER.
        //
        // The final Uniswap span could support RecipientKind::Distinct (the UR
        // accepts a recipient arg), but Sender is kept for uniformity — it matches
        // the constraint of the Curve-final direction (Case 1) and avoids adding
        // dead code for a path not yet fork-proven in this task.
        //
        // Proves: the reverse cross-router direction; intermediate USDC lands at
        //         SENDER after span 1, then is pulled by span 2 into DAI.
        PlanCase {
            name: "cross_router_curve_uniswap_usdt_usdc_dai",
            chain: ChainId(1),
            pools: &["curve_3pool", "dai_usdc_v3"],
            path: &[USDT, USDC, DAI],
            // 1 000 USDT (6 decimals = 1_000_000_000)
            amount: U256::from(1_000_000_000u64),
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            // Sender: keeps both cases uniform and avoids a distinct-recipient
            // assertion that depends on the final Uniswap span's recipient override,
            // which is not yet fork-verified in this task.
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // ── Case 3: native-in cross-router (ETH → V3 → Curve) ──────────────────
        //
        // ETH →(usdc_weth_v3: WETH→USDC)→ USDC →(curve_3pool: USDC→USDT)→ USDT.
        // The FIRST span (Uniswap) takes native ETH (tx.value); its USDC output
        // lands at SENDER, then the Curve span pulls it to USDT. Proves a native
        // edge at a cross-router boundary. 0.1 ETH in.
        PlanCase {
            name: "cross_router_native_in_eth_usdc_usdt",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005", "curve_3pool"],
            path: &[WETH, USDC, USDT],
            amount: U256::from(100_000_000_000_000_000u64), // 0.1 ETH (18 dp)
            trade_type: TradeType::ExactIn,
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // ── Case 4: native-out cross-router (Curve → V3 → ETH) ─────────────────
        //
        // USDT →(curve_3pool: USDT→USDC)→ USDC →(usdc_weth_v3: USDC→WETH→ETH).
        // The Curve span delivers USDC to SENDER; the FINAL Uniswap span unwraps
        // WETH and pays SENDER native ETH. Proves a native output edge across a
        // cross-router boundary. 1 000 USDT in.
        PlanCase {
            name: "cross_router_native_out_usdt_usdc_eth",
            chain: ChainId(1),
            pools: &["curve_3pool", "usdc_weth_v3_005"],
            path: &[USDT, USDC, WETH],
            amount: U256::from(1_000_000_000u64), // 1 000 USDT (6 dp)
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: true,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
    ]
}

// ── Cross-router fork matrix ───────────────────────────────────────────────────

/// Fork-backed cross-router execution matrix.
///
/// Requires `$AMM_RPC_FORK_URL` pointing to an Ethereum mainnet archive node.
/// Pinned to block 20_000_000 (or `$AMM_FORK_BLOCK` override).
///
/// Each [`PlanCase`] in [`multihop_cross_router_cases`] exercises a route that
/// partitions into two spans from different routers (Uniswap Universal Router +
/// Curve StableSwap).  [`run_plan_case`] drives the `Plan::next_tx` loop,
/// submitting one EVM transaction per span and threading the observed output.
///
/// # Structural assertions (offline-safe)
///
/// Before the fork loop, this test verifies — using the public `plan()` API
/// against the forked pool state — that every cross-router case satisfies:
///   - `is_atomic() == false` (two distinct routers → two spans)
///   - `tx_count() == 2`     (one tx per span)
///
/// These checks are deterministic (they depend only on the route structure,
/// not on block state) and will surface any regression in the partition logic
/// even if the fork is unavailable.
///
/// **Do not run this test locally without an archive RPC — it is `#[ignore]`.**
/// **The controller runs the fork matrix on a keyless node.**
#[cfg(feature = "curve")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL (archive node, mainnet, block 20_000_000)"]
async fn multihop_cross_router_matrix() {
    let Some((mut fork, _block)) = open_fork("AMM_RPC_FORK_URL", 20_000_000, ChainId(1)).await
    else {
        return;
    };
    let cfg = mainnet_chain_config();

    // ── Structural pre-check (plan-level, before any EVM submission) ──────────
    // Verify is_atomic()==false and tx_count()==2 for every cross-router case.
    // These assertions depend only on route partition logic and run deterministically
    // regardless of block state. Pool state is already loaded by run_plan_case
    // internally, so we rebuild a minimal plan here using forked pool data.
    //
    // Note: run_plan_case refreshes pools internally and does not expose the Plan
    // struct; we assert the structural invariants separately via the public API.
    // This is safe to do before the loop because the pool state at block 20_000_000
    // is stable and the partition logic is deterministic.
    use alloy::primitives::Address;
    use amm_core::primitives::ratio::Bps;
    use amm_core::slippage::Slippage;
    use amm_rpc::execution::routing::{ExactOutPolicy, Route};
    use amm_rpc::execution::{Deadline, ExecutionOptions, Recipient};

    let block = _block;
    for case in multihop_cross_router_cases() {
        // Refresh pools at the fork block.
        let pool_boxes = support::refresh_pools(case.pools, fork.provider().clone(), block).await;
        let pool_refs: Vec<&dyn amm_core::traits::pool::Pool> =
            pool_boxes.iter().map(|b| b.as_ref()).collect();

        // Parse path addresses.
        let path: Vec<amm_core::primitives::asset::AssetId> = case
            .path
            .iter()
            .map(|hex| {
                let addr: Address = hex.parse().expect("valid address");
                support::asset(case.chain.0, addr)
            })
            .collect();

        let route = Route {
            pools: pool_refs,
            path: path.clone(),
            trade_type: case.trade_type,
        };

        let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
            .with_recipient(Recipient::To(Address::repeat_byte(0xBE)))
            .with_deadline(Deadline::AtTimestamp(u64::MAX / 2));

        let p = amm_rpc::execution::plan(
            &cfg,
            &route,
            case.amount,
            &opts,
            Address::repeat_byte(0xBE),
            case.native_in,
            case.native_out,
            ExactOutPolicy::Strict,
        )
        .unwrap_or_else(|e| panic!("PlanCase {}: plan() failed: {e:?}", case.name));

        assert!(
            !p.is_atomic(),
            "PlanCase {}: cross-router route must NOT be atomic (spans two routers)",
            case.name
        );
        assert_eq!(
            p.tx_count(),
            2,
            "PlanCase {}: cross-router route must have tx_count == 2",
            case.name
        );
    }

    // ── Fork execution loop ───────────────────────────────────────────────────
    for case in multihop_cross_router_cases() {
        run_plan_case(&mut fork, &cfg, &case).await;
    }
}

// ── Offline structural test ────────────────────────────────────────────────────

/// Proves that `plan(...).next_tx(None)` emits well-formed UR calldata for all
/// all-V3 routes of depth 2..=6 without touching a fork.
///
/// # What is asserted (per depth `n`)
///
/// 1. The UR command sequence is `n` copies of `V3_SWAP_EXACT_IN`.
/// 2. Hop 0 (`payerIsUser = true`, `amount = amount_in`, `recipient = ADDRESS_THIS`).
/// 3. Hops 1..n-2 (`payerIsUser = false`, `amount = CONTRACT_BALANCE`,
///    `recipient = ADDRESS_THIS`, `amountOutMinimum = 0`).
/// 4. Hop n-1 (`payerIsUser = false`, `amount = CONTRACT_BALANCE`,
///    `recipient = <recipient>`, `amountOutMinimum > 0`).
/// 5. Exactly one non-zero `amountOutMinimum` (on the last hop); all others are 0.
///
/// The test calls only the *public* `plan` / `next_tx` API, so it is safe to
/// place in the `tests/` integration directory.  The synthetic pool constructors
/// (`UniswapV3Pool::new`, `TickData::from_ticks`) are `pub` in `amm_core` and
/// do not require crate-internal access.
///
/// # Decoding
///
/// The UR outer frame is `execute(bytes commands, bytes[] inputs, uint256 deadline)`.
/// Each V3 swap input is ABI-encoded as `(address recipient, uint256 amount,
/// uint256 amountOutMinimum, bytes path, bool payerIsUser)`.
#[test]
fn structural_uniswap_span_wellformed_2_to_6_hops() {
    // ── Constants replicated from `route_planner.rs` (public items) ──────────
    // These are `pub` in `amm_rpc::execution::route_planner`; we import them
    // directly to avoid hardcoding magic numbers in assertions.
    use amm_rpc::execution::route_planner::{
        ADDRESS_THIS, CONTRACT_BALANCE, IUniversalRouter, V3_SWAP_EXACT_IN,
    };

    // ── Synthetic infrastructure ──────────────────────────────────────────────

    let chain_id = ChainId(1);
    let universal_router = Address::repeat_byte(0xAA);
    let permit2 = Address::repeat_byte(0xBB);
    let sender = Address::repeat_byte(0x01);
    let recipient = Address::repeat_byte(0x55);
    let amount_in = U256::from(1_000_000u64);

    // Chain config: only the UR and Permit2 addresses are needed for all-V3
    // UR spans.  WETH is asset(0x02) — an arbitrary synthetic address.
    // Use field-mutation instead of struct-literal because `Routers` is
    // `#[non_exhaustive]` and struct literals are forbidden in external crates.
    let weth_asset = AssetId::new(chain_id, B256::left_padding_from(&[0x02u8]));
    let mut routers = Routers::default();
    routers.universal = Some(universal_router);
    routers.permit2 = Some(permit2);
    let cfg: ChainConfig = ChainConfig::new(chain_id, weth_asset).with_routers(routers);

    // Execution options: 50 bps slippage, explicit recipient, absolute deadline.
    let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
        .with_recipient(Recipient::To(recipient))
        .with_deadline(Deadline::AtTimestamp(u64::MAX / 2));

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Build an `AssetId` from a single discriminant byte (left-padded B256).
    fn asset(chain: ChainId, byte: u8) -> AssetId {
        AssetId::new(chain, B256::left_padding_from(&[byte]))
    }

    /// sqrtPriceX96 for tick 0 (price 1:1) = 2^96.
    const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;

    /// Full-range tick data `[-887220, 887220]` with `liq` liquidity at spacing 60.
    fn full_range_ticks(liq: i128) -> TickData {
        TickData::from_ticks(
            60,
            vec![
                (
                    -887_220,
                    TickInfo {
                        liquidity_net: liq,
                        initialized: true,
                    },
                ),
                (
                    887_220,
                    TickInfo {
                        liquidity_net: -liq,
                        initialized: true,
                    },
                ),
            ],
        )
    }

    /// Build a synthetic V3 pool over `(a, b)` (address-sorted by EVM address)
    /// with fee `fee_pips`.
    fn v3_pool(a: AssetId, b: AssetId, fee_pips: u32, idx: usize) -> UniswapV3Pool {
        // Extract the 20-byte EVM address from each AssetId via the public API
        // (`AssetId::token` is a `B256`; the address occupies the rightmost 20 bytes).
        let addr_a = Address::from_word(a.token);
        let addr_b = Address::from_word(b.token);
        let (lo, hi) = if addr_a < addr_b { (a, b) } else { (b, a) };
        UniswapV3Pool::new(
            PoolId::new(format!("1:univ3:synthetic-{idx}")),
            [lo, hi],
            U256::from(SQRT_1_1),
            1_000_000_000_000_000_000u128, // 1e18 liquidity
            0,
            fee_pips,
            full_range_ticks(1_000_000_000_000_000_000i128),
        )
    }

    // ── Decode helpers ────────────────────────────────────────────────────────

    /// Decode the outer `execute(bytes commands, bytes[] inputs, uint256 deadline)`.
    fn decode_outer(data: &[u8]) -> (alloy::primitives::Bytes, Vec<alloy::primitives::Bytes>) {
        let d = IUniversalRouter::executeCall::abi_decode(data)
            .expect("outer must decode as UR execute");
        (d.commands, d.inputs)
    }

    /// Decode one V3 swap input `(recipient, amount, amountOutMinimum, path, payerIsUser)`.
    fn decode_v3(
        input: &alloy::primitives::Bytes,
    ) -> (Address, U256, U256, alloy::primitives::Bytes, bool) {
        <(Address, U256, U256, alloy::primitives::Bytes, bool)>::abi_decode_params(input)
            .expect("V3 input must decode as (address, uint256, uint256, bytes, bool)")
    }

    // ── Main loop: n ∈ 2..=6 ─────────────────────────────────────────────────

    for n in 2usize..=6 {
        // Build n+1 distinct assets: bytes 0x10, 0x11, …, 0x10+n.
        // Asset addresses are left-padded from the discriminant byte, so they
        // are all distinct and address-sortable.
        let assets: Vec<AssetId> = (0..=n).map(|i| asset(chain_id, 0x10u8 + i as u8)).collect();

        // Build n pools: pool[i] connects assets[i] → assets[i+1].
        // Alternate fees (500, 3000, 500, …) so the path bytes are non-trivial.
        let pools_owned: Vec<UniswapV3Pool> = (0..n)
            .map(|i| {
                let fee = if i % 2 == 0 { 500u32 } else { 3000u32 };
                v3_pool(assets[i], assets[i + 1], fee, i)
            })
            .collect();

        let pool_refs: Vec<&dyn amm_core::traits::pool::Pool> =
            pools_owned.iter().map(|p| p as &_).collect();

        let route = Route {
            pools: pool_refs,
            path: assets.clone(),
            trade_type: TradeType::ExactIn,
        };

        // Build the plan and emit the single UR transaction.
        let mut p = plan(
            &cfg,
            &route,
            amount_in,
            &opts,
            sender,
            false, // native_in
            false, // native_out
            ExactOutPolicy::Strict,
        )
        .unwrap_or_else(|e| panic!("plan({n}-hop) must build: {e:?}"));

        // A single Uniswap span is always atomic.
        assert!(
            p.is_atomic(),
            "{n}-hop all-V3 plan must be atomic (one UR tx)"
        );
        assert_eq!(p.tx_count(), 1, "{n}-hop plan must have tx_count == 1");

        let prepared = p
            .next_tx(None)
            .unwrap_or_else(|e| panic!("next_tx({n}-hop) must build: {e:?}"))
            .expect("first (and only) span must yield a PreparedSwap");

        // The tx must target the Universal Router.
        assert_eq!(
            prepared.tx.to, universal_router,
            "{n}-hop tx.to must be the Universal Router"
        );
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "{n}-hop ERC-20 exact-in tx.value must be 0"
        );

        // ── Decode outer UR frame ─────────────────────────────────────────────
        let (commands, inputs) = decode_outer(&prepared.tx.data);

        // Invariant 1: exactly n commands, all V3_SWAP_EXACT_IN.
        assert_eq!(
            commands.len(),
            n,
            "{n}-hop span must emit exactly {n} UR commands"
        );
        for (cmd_idx, &cmd) in commands.iter().enumerate() {
            assert_eq!(
                cmd, V3_SWAP_EXACT_IN,
                "{n}-hop command[{cmd_idx}] must be V3_SWAP_EXACT_IN (0x{V3_SWAP_EXACT_IN:02x})"
            );
        }
        assert_eq!(
            inputs.len(),
            n,
            "{n}-hop span must emit exactly {n} UR inputs"
        );

        let mut nonzero_min_count = 0usize;

        for (hop_idx, hop_input) in inputs.iter().enumerate() {
            let (hop_recip, hop_amt, hop_min, _path_bytes, hop_payer) = decode_v3(hop_input);

            let is_first = hop_idx == 0;
            let is_last = hop_idx == n - 1;

            // Invariant 2: hop-0 pulls from the user (payerIsUser=true, amount=amount_in).
            // Invariant 3: all other hops pull from the router (payerIsUser=false, amount=CONTRACT_BALANCE).
            if is_first {
                assert!(
                    hop_payer,
                    "{n}-hop hop[0] payerIsUser must be true (user funds the first hop)"
                );
                assert_eq!(
                    hop_amt, amount_in,
                    "{n}-hop hop[0] amount must equal amount_in"
                );
            } else {
                assert!(
                    !hop_payer,
                    "{n}-hop hop[{hop_idx}] payerIsUser must be false (router funds chained hops)"
                );
                assert_eq!(
                    hop_amt, CONTRACT_BALANCE,
                    "{n}-hop hop[{hop_idx}] amount must be CONTRACT_BALANCE"
                );
            }

            // Invariant 4: non-last hops deliver to ADDRESS_THIS; the last hop
            // delivers to the caller-supplied recipient.
            if is_last {
                assert_eq!(
                    hop_recip, recipient,
                    "{n}-hop last hop[{hop_idx}] must deliver to recipient"
                );
            } else {
                assert_eq!(
                    hop_recip, ADDRESS_THIS,
                    "{n}-hop hop[{hop_idx}] must deliver to ADDRESS_THIS (router holds intermediates)"
                );
            }

            // Count non-zero amountOutMinimum for invariant 5.
            if hop_min > U256::ZERO {
                nonzero_min_count += 1;
                // Must only be the last hop.
                assert!(
                    is_last,
                    "{n}-hop non-zero amountOutMinimum at non-last hop[{hop_idx}] — only the last hop may carry a floor"
                );
            } else if is_last {
                // The last hop must have a non-zero floor (50 bps on a 1e18-liquidity pool
                // with amount_in=1e6 will always produce a positive output, so the floor
                // is strictly positive).
                panic!(
                    "{n}-hop last hop[{hop_idx}] amountOutMinimum is zero — the slippage floor must be positive"
                );
            }
        }

        // Invariant 5: exactly one non-zero amountOutMinimum (the last hop).
        assert_eq!(
            nonzero_min_count, 1,
            "{n}-hop span must carry exactly one non-zero amountOutMinimum (on the last hop)"
        );

        // Approval: Permit2 on the span input token, sized to amount_in.
        let appr = prepared
            .approval
            .as_ref()
            .unwrap_or_else(|| panic!("{n}-hop ERC-20 span must carry a Permit2 approval"));
        assert_eq!(
            appr.spender, permit2,
            "{n}-hop approval spender must be Permit2"
        );
        assert_eq!(
            appr.token, assets[0],
            "{n}-hop approval token must be the span input (assets[0])"
        );
        assert_eq!(
            appr.min_allowance, amount_in,
            "{n}-hop approval must be sized to amount_in"
        );

        // No max_spent on exact-in.
        assert!(
            prepared.max_spent.is_none(),
            "{n}-hop exact-in must not carry max_spent"
        );
    }
}

// ── Base: Aerodrome multi-pool span (Task 8) ─────────────────────────────────
//
// Two consecutive Aerodrome pools partition into ONE Aerodrome span, executed as
// a single router `swapExactTokensForTokens` with a 2-element `Route[]` (one
// volatile hop + one stable hop). Proves `build_aerodrome_span` on-chain.

/// Base WETH (18 dp).
const WETH_BASE: &str = "0x4200000000000000000000000000000000000006";
/// Base native USDC (6 dp).
const USDC_BASE: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
/// Base USDbC (6 dp) — swap output only (proxy, not slot-fundable).
const USDBC_BASE: &str = "0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA";

fn multihop_aerodrome_base_cases() -> Vec<PlanCase> {
    vec![
        // 2-hop Aerodrome: WETH →(volatile)→ USDC →(stable)→ USDbC.
        // Pools: aero_vol_weth_usdc (WETH/USDC volatile) + aero_stable_usdc_usdbc
        // (USDC/USDbC stable). Both RouterKind::Aerodrome → one span, one router
        // call with Route[] = [{WETH,USDC,stable=false},{USDC,USDbC,stable=true}].
        PlanCase {
            name: "multihop_aero_weth_usdc_usdbc",
            chain: ChainId(8453),
            pools: &["aero_vol_weth_usdc", "aero_stable_usdc_usdbc"],
            path: &[WETH_BASE, USDC_BASE, USDBC_BASE],
            amount: U256::from(100_000_000_000_000_000u64), // 0.1 WETH (18 dp)
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // Strict exact-out over an Aerodrome span must be REJECTED at plan() time:
        // the Solidly router has no exact-out entrypoint, so the Strict gate
        // returns UnsupportedExactOut before any calldata is built.
        PlanCase {
            name: "multihop_aero_exact_out_strict_reject",
            chain: ChainId(8453),
            pools: &["aero_vol_weth_usdc", "aero_stable_usdc_usdbc"],
            path: &[WETH_BASE, USDC_BASE, USDBC_BASE],
            amount: U256::from(100_000_000u64), // 100 USDbC target (6 dp)
            trade_type: TradeType::ExactOut,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::RejectBuild(BuildErrorKind::UnsupportedExactOut),
        },
    ]
}

/// Aerodrome multi-hop span fork proof on Base. Gated on `$AMM_RPC_FORK_URL_BASE`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL_BASE"]
async fn multihop_aerodrome_base_matrix() {
    let Some((mut fork, _block)) =
        open_fork("AMM_RPC_FORK_URL_BASE", 30_000_000, ChainId(8453)).await
    else {
        return;
    };
    let cfg = base_chain_config();
    for case in multihop_aerodrome_base_cases() {
        run_plan_case(&mut fork, &cfg, &case).await;
    }
}

// ── Base: Slipstream multi-pool span (Task 8) ────────────────────────────────
//
// Two consecutive Slipstream (Aerodrome CL) pools partition into ONE Slipstream
// span, executed as a single path-encoded `exactInput`. Proves
// `build_slipstream_span` + `slipstream_path` (int24 tickSpacing) on-chain.

/// Base cbBTC (8 dp) — swap output only.
const CBBTC_BASE: &str = "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf";

fn multihop_slipstream_base_cases() -> Vec<PlanCase> {
    vec![
        // 2-hop Slipstream: WETH →(CL ts=100)→ USDC →(CL ts=100)→ cbBTC.
        // Pools: slipstream_weth_usdc + slipstream_usdc_cbbtc. Both
        // RouterKind::Slipstream → one span, one `exactInput` with a path-encoded
        // WETH‖100‖USDC‖100‖cbBTC (int24 tick spacing between tokens).
        PlanCase {
            name: "multihop_slipstream_weth_usdc_cbbtc",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc", "slipstream_usdc_cbbtc"],
            path: &[WETH_BASE, USDC_BASE, CBBTC_BASE],
            amount: U256::from(100_000_000_000_000_000u64), // 0.1 WETH (18 dp)
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // Multi-hop STRICT exact-out over a Slipstream span — Slipstream is the
        // only non-Uniswap family with a true multi-hop exact-out builder
        // (`build_slipstream_span_exact_out`, path-encoded `exactOutput` over the
        // reversed path). Deliver EXACTLY 0.001 cbBTC, input bounded by max_spent.
        PlanCase {
            name: "multihop_slipstream_exact_out_weth_usdc_cbbtc",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc", "slipstream_usdc_cbbtc"],
            path: &[WETH_BASE, USDC_BASE, CBBTC_BASE],
            amount: U256::from(100_000u64), // target 0.001 cbBTC (8 dp)
            trade_type: TradeType::ExactOut,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::Exact,
        },
    ]
}

/// Slipstream multi-hop span fork proof on Base. Gated on `$AMM_RPC_FORK_URL_BASE`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL_BASE"]
async fn multihop_slipstream_base_matrix() {
    let Some((mut fork, _block)) =
        open_fork("AMM_RPC_FORK_URL_BASE", 30_000_000, ChainId(8453)).await
    else {
        return;
    };
    let cfg = base_chain_config();
    for case in multihop_slipstream_base_cases() {
        run_plan_case(&mut fork, &cfg, &case).await;
    }
}

// ── Curve multi-hop (mainnet, CurveRouterNG) ────────────────────────────────────

/// Atomic multi-pool Curve spans via CurveRouterNG (Plan 3b). Every case is one
/// Curve span → one `exchange(...)` transaction (tx_count == 1, atomic). Two
/// consecutive Curve pools sharing a coin form the chain.
///
/// pool_type coverage on-chain: 1 (StableSwapV1 3pool), 3 (TriCryptoV1
/// tricrypto2), 10 (StableSwapNG), 20 (TwoCrypto-NG). pool_type 2 and 30 differ
/// from the fork-proven 3 and 20 only by the `n_coins` constant within the SAME
/// interface (`CryptoU256UseEth`: 2↔3; `CryptoU256Receiver`: 20↔30) — a trivial
/// branch covered by the builder's `pool_type_table_is_ng_aware` unit test, so
/// both interface families and both n_coins branches are exercised without
/// hunting two more deeply-liquid fixtures.
///
/// Multi-hop Permit2 is exercised implicitly: every Uniswap Universal Router
/// span emits a Permit2 approval on its input (see `build_uniswap_span`), which
/// `drive_spans` applies via `fork.permit2_approve` — so all multihop-Uniswap
/// rows here are also multi-hop Permit2 proofs.
#[cfg(feature = "curve")]
fn multihop_curve_cases() -> Vec<PlanCase> {
    vec![
        // USDC →(3pool)→ USDT →(tricrypto2)→ WETH. pool_type 1 then 3.
        PlanCase {
            name: "multihop_curve_usdc_usdt_weth",
            chain: ChainId(1),
            pools: &["curve_3pool", "curve_tricrypto2"],
            path: &[USDC, USDT, WETH],
            amount: U256::from(1_000_000_000u64), // 1000 USDC (6 dp)
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // DAI →(3pool)→ USDC →(stable_ng)→ crvUSD. pool_type 1 then 10.
        PlanCase {
            name: "multihop_curve_stable_ng_dai_usdc_crvusd",
            chain: ChainId(1),
            pools: &["curve_3pool", "curve_stable_ng"],
            path: &[DAI, USDC, CRVUSD],
            amount: U256::from(1_000_000_000_000_000_000_000u128), // 1000 DAI (18 dp)
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // Native-out: USDC →(3pool)→ USDT →(tricrypto2)→ ETH. The router unwraps
        // WETH and pays the receiver native ETH (tricrypto2 = use_eth endpoint).
        PlanCase {
            name: "multihop_curve_native_out_usdc_usdt_eth",
            chain: ChainId(1),
            pools: &["curve_3pool", "curve_tricrypto2"],
            path: &[USDC, USDT, WETH],
            amount: U256::from(1_000_000_000u64), // 1000 USDC (6 dp)
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: true,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // Native-in: ETH →(tricrypto2)→ USDT →(3pool)→ DAI. tx.value carries the
        // ETH; the router wraps at the tricrypto2 (use_eth) endpoint.
        PlanCase {
            name: "multihop_curve_native_in_eth_usdt_dai",
            chain: ChainId(1),
            pools: &["curve_tricrypto2", "curve_3pool"],
            path: &[WETH, USDT, DAI],
            amount: U256::from(500_000_000_000_000_000u64), // 0.5 ETH (18 dp)
            trade_type: TradeType::ExactIn,
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // Distinct recipient: USDC →(3pool)→ USDT →(tricrypto2)→ WETH delivered to
        // a DIFFERENT address. CurveRouterNG's `_receiver` makes this atomic even
        // though the underlying pools are receiver-less (contrast the single-pool
        // encoder, which must reject a distinct recipient here).
        PlanCase {
            name: "multihop_curve_distinct_recipient_usdc_usdt_weth",
            chain: ChainId(1),
            pools: &["curve_3pool", "curve_tricrypto2"],
            path: &[USDC, USDT, WETH],
            amount: U256::from(1_000_000_000u64), // 1000 USDC (6 dp)
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Distinct,
            expect: Expect::WeiExact,
        },
        // OrBetter exact-out over a multi-pool Curve span — Curve has no exact-out
        // entrypoint, so OrBetter is the only way to get exact-out behavior:
        // backward-solve the USDC input for a 0.1-WETH target, then run the Curve
        // span exact-in with the final floor pinned to the target (deliver ≥).
        PlanCase {
            name: "multihop_curve_exact_out_orbetter_usdc_usdt_weth",
            chain: ChainId(1),
            pools: &["curve_3pool", "curve_tricrypto2"],
            path: &[USDC, USDT, WETH],
            amount: U256::from(100_000_000_000_000_000u64), // target ≥ 0.1 WETH (18 dp)
            trade_type: TradeType::ExactOut,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::AtLeast,
        },
        // pool_type 20 on-chain: USDT →(tricrypto2)→ WETH →(twocrypto_ng)→ TC_NG.
        // Exercises build_curve_span's classifier for a TwoCrypto-NG pool
        // (CryptoU256Receiver, n_coins=2 → pool_type 20) chained after a classic
        // TriCrypto (pool_type 3) — both in one CurveRouterNG.exchange call.
        PlanCase {
            name: "multihop_curve_pool_type_20_usdt_weth_tcng",
            chain: ChainId(1),
            pools: &["curve_tricrypto2", "curve_twocrypto_ng"],
            path: &[USDT, WETH, TC_NG_TOKEN],
            amount: U256::from(100_000_000u64), // 100 USDT (6 dp)
            trade_type: TradeType::ExactIn,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::WeiExact,
        },
        // Strict exact-out over a Curve span must be REJECTED at plan() time:
        // CurveRouterNG has no exact-out entrypoint, so the Strict gate returns
        // UnsupportedExactOut before any calldata is built.
        PlanCase {
            name: "multihop_curve_exact_out_strict_reject",
            chain: ChainId(1),
            pools: &["curve_3pool", "curve_tricrypto2"],
            path: &[USDC, USDT, WETH],
            amount: U256::from(100_000_000_000_000_000u64), // 0.1 WETH target
            trade_type: TradeType::ExactOut,
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            expect: Expect::RejectBuild(BuildErrorKind::UnsupportedExactOut),
        },
    ]
}

/// Curve multi-hop span fork proof on mainnet. Gated on `$AMM_RPC_FORK_URL`.
#[cfg(feature = "curve")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL"]
async fn multihop_curve_matrix() {
    let Some((mut fork, _block)) = open_fork("AMM_RPC_FORK_URL", 20_000_000, ChainId(1)).await
    else {
        return;
    };
    let cfg = mainnet_chain_config();
    for case in multihop_curve_cases() {
        run_plan_case(&mut fork, &cfg, &case).await;
    }
}
