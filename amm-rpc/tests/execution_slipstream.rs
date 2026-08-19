//! Table-driven execution proof for Aerodrome Slipstream on Base — all swap
//! directions.
//!
//! Uses the Phase-1 `support::run_case` engine. Each row in
//! `slipstream_cases()` is a fully self-describing [`support::Case`] that the
//! engine executes against a live revm fork.  Coverage:
//!
//! **`slipstream_weth_usdc` (Slipstream CL pool — WETH/USDC on Base)**
//!
//! Fixture layout: token0 = WETH (18 dp, slot 3), token1 = USDC (6 dp, slot 9).
//! `Forward` = WETH→USDC; `Reverse` = USDC→WETH.
//!
//! 1. Exact-in Forward       (WETH→USDC)                                — WeiExact
//! 2. Exact-in Reverse       (USDC→WETH)                                — WeiExact
//! 3. Exact-out Forward      (WETH→exact USDC; spend WETH)              — Exact
//! 4. Exact-out Reverse      (USDC→exact WETH; spend USDC)              — Exact
//! 5. Native-in              (ETH→USDC; pool math via WETH=token0)      — WeiExact
//! 6. Native-out             (USDC→ETH; WETH unwrapped)                 — WeiExact
//! 7. Distinct recipient     (USDC→WETH, output to 0xD1…)               — WeiExact
//! 8. Large swap             (100 000 USDC→WETH, WeiExact — hard proof,
//!    no ppm tolerance; documents any CL tick-window divergence as a
//!    real finding for the controller)
//!
//! # Slipstream-specific notes
//!
//! - Slipstream is **concentrated-liquidity (V3-style): supports exact-in AND
//!   exact-out**. Exact-out rows use `ExactOutPolicy::Strict` and
//!   `Expect::Exact` — they are real proofs, NOT rejects.
//! - Approvals are **direct ERC-20 to the Slipstream SwapRouter**
//!   (`ApprovalKind::Erc20`). No Permit2.
//! - The SwapRouter accepts a `recipient` param → distinct-recipient IS
//!   supported (row 7).
//! - Native ETH is routed via the router's ETH helper functions (rows 5–6).
//!   If the library's Slipstream executable does not wire the native swap
//!   functions, those rows will revert on-chain and the controller will adjust.
//! - Large swap (row 8) uses `Expect::WeiExact` (no ppm tolerance). V3's
//!   large-swap turned out wei-exact on the fork; if Slipstream's does not,
//!   that is a real finding for the controller.
//!
//! All tests are `#[ignore]`d and gated on `$AMM_RPC_FORK_URL_BASE`.

#![cfg(test)]

#[path = "support/mod.rs"]
mod support;

use alloy::primitives::U256;
use amm_core::primitives::asset::ChainId;
use amm_rpc::execution::routing::ExactOutPolicy;
use support::{ApprovalKind, Case, Direction, Expect, RecipientKind, Trade};

// ── Case table ─────────────────────────────────────────────────────────────────

/// All Slipstream swap cases for the `slipstream_weth_usdc` fixture.
///
/// Fixture layout: token0 = WETH (18 decimals, slot 3),
///                 token1 = USDC (6 decimals, slot 9).
/// `Forward` = WETH→USDC; `Reverse` = USDC→WETH.
///
/// Slipstream routes via its own SwapRouter with a direct ERC-20 approval —
/// `ApprovalKind::Erc20` for all token-input rows. No Permit2.
fn slipstream_cases() -> Vec<Case> {
    vec![
        // 1. Exact-in Forward: 0.3 WETH → USDC
        //    INPUT = WETH (token0, slot 3). OUTPUT = USDC (token1).
        //    Slipstream CL: off-chain quote may match on-chain exactly for
        //    small-to-medium swaps. WeiExact is the asserted outcome.
        Case {
            name: "slipstream_exact_in_forward_weth_usdc",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Forward, // WETH → USDC
            trade: Trade::ExactIn {
                amount_in: U256::from(300_000_000_000_000_000u128), // 0.3 WETH (18 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 2. Exact-in Reverse: 1 000 USDC → WETH
        //    INPUT = USDC (token1, slot 9). OUTPUT = WETH (token0).
        Case {
            name: "slipstream_exact_in_reverse_usdc_weth",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Reverse, // USDC → WETH
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000u64), // 1 000 USDC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 3. Exact-out Forward: spend WETH for exact 500 USDC
        //    INPUT = WETH (token0, slot 3). OUTPUT = exact 500 USDC (token1).
        //    Slipstream is CL (V3-style): exact-out is a real proof, not a reject.
        //    ExactOutPolicy::Strict — builder must deliver exactly 500 USDC.
        Case {
            name: "slipstream_exact_out_forward_weth_to_exact_usdc",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Forward, // spend WETH side
            trade: Trade::ExactOut {
                amount_out: U256::from(500_000_000u64), // 500 USDC (6 dp)
                policy: ExactOutPolicy::Strict,
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::Exact,
        },
        // 4. Exact-out Reverse: spend USDC for exact 0.1 WETH
        //    INPUT = USDC (token1, slot 9). OUTPUT = exact 0.1 WETH (token0).
        //    ExactOutPolicy::Strict — builder must deliver exactly 0.1 WETH.
        Case {
            name: "slipstream_exact_out_reverse_usdc_to_exact_weth",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Reverse, // spend USDC side
            trade: Trade::ExactOut {
                amount_out: U256::from(100_000_000_000_000_000u128), // 0.1 WETH (18 dp)
                policy: ExactOutPolicy::Strict,
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::Exact,
        },
        // 5. Native-in: 0.003 ETH → USDC
        //    INPUT = native ETH (pool math: WETH=token0). No ERC-20 approval.
        //    Forward direction because WETH=token0; native_in=true tells the
        //    runner to use Currency::Native and fund via `fund_native`.
        //    Note: if the Slipstream Executable does not wire the native-ETH
        //    swap path, this row will revert on-chain and the controller adjusts.
        Case {
            name: "slipstream_native_in_eth_to_usdc",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Forward, // WETH side → USDC
            trade: Trade::ExactIn {
                amount_in: U256::from(3_000_000_000_000_000u128), // 0.003 ETH (18 dp)
            },
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20, // irrelevant for native-in; runner skips
            expect: Expect::WeiExact,
        },
        // 6. Native-out: 10 USDC → ETH (WETH unwrapped)
        //    INPUT = USDC (token1, slot 9). OUTPUT = native ETH (WETH unwrapped).
        //    Reverse direction: USDC=token1 → WETH=token0, native_out=true.
        //    Note: if Slipstream's Executable doesn't wire the native-out path,
        //    this row will revert on-chain and the controller adjusts.
        Case {
            name: "slipstream_native_out_usdc_to_eth",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Reverse, // USDC → WETH side → ETH unwrap
            trade: Trade::ExactIn {
                amount_in: U256::from(10_000_000u64), // 10 USDC (6 dp)
            },
            native_in: false,
            native_out: true,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 7. Distinct recipient: 1 000 USDC → WETH, output sent to 0xD1…
        //    Slipstream's SwapRouter has a `recipient` param → distinct-recipient
        //    IS supported. INPUT = USDC (token1, slot 9). Direction Reverse.
        Case {
            name: "slipstream_exact_in_reverse_distinct_recipient",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Reverse, // USDC → WETH
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000u64), // 1 000 USDC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Distinct,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 8. Large swap: 100 000 USDC → WETH — hard WeiExact proof (no ppm tolerance).
        //    Documents concentrated-liquidity tick-window divergence as a real
        //    finding for the controller. The old test used 500 ppm tolerance;
        //    this migration promotes it to WeiExact (same as V3's large-swap row)
        //    because V3's equivalent turned out to be exact on the fork.
        //    If Slipstream's tick truncation causes a divergence, that is a real
        //    finding and the controller will adjust the expectation.
        //    INPUT = USDC (token1, slot 9). OUTPUT = WETH (token0).
        Case {
            name: "slipstream_large_exact_in_reverse_100k_usdc",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Reverse, // USDC → WETH
            trade: Trade::ExactIn {
                amount_in: U256::from(100_000_000_000u64), // 100 000 USDC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // OrBetter exact-out: Slipstream supports Strict, but a caller may still
        // choose OrBetter — backward-solve the WETH input for a 500-USDC target,
        // then run exact-in with the floor pinned to 500 USDC (deliver ≥, AtLeast).
        Case {
            name: "slipstream_exact_out_orbetter_forward_weth_usdc",
            chain: ChainId(8453),
            pools: &["slipstream_weth_usdc"],
            direction: Direction::Forward, // spend WETH side
            trade: Trade::ExactOut {
                amount_out: U256::from(500_000_000u64), // 500 USDC (6 dp)
                policy: ExactOutPolicy::OrBetter,
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::AtLeast,
        },
    ]
}

// ── Fork matrix ────────────────────────────────────────────────────────────────

/// Run all Slipstream cases against a live Base fork pinned to block 30_000_000.
///
/// Skips gracefully when `$AMM_RPC_FORK_URL_BASE` is unset (offline CI).
/// Each case is snapshot/reverted so subsequent cases start from clean state.
///
/// # Chain config
///
/// Uses `support::base_chain_config()` which wires the Aerodrome router,
/// factory, and Slipstream router. WETH on Base is the wrapped-native asset.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL_BASE"]
async fn slipstream_matrix() {
    let Some((mut fork, _)) =
        support::open_fork("AMM_RPC_FORK_URL_BASE", 30_000_000, ChainId(8453)).await
    else {
        return;
    };
    let cfg = support::base_chain_config();
    for case in slipstream_cases() {
        support::run_case(&mut fork, &cfg, &case).await;
    }
}
