//! Table-driven execution proof for Uniswap V3 — all swap directions.
//!
//! Uses the Phase-1 `support::run_case` engine. Each row in `v3_cases()` is a
//! fully self-describing [`support::Case`] that the engine executes against a
//! live revm fork.  Coverage:
//!
//! 1. Exact-in Forward  (USDC → WETH)
//! 2. Exact-in Reverse  (WETH → USDC)
//! 3. Exact-out Forward (USDC → exact WETH)
//! 4. Exact-out Reverse (WETH → exact USDC)
//! 5. Native-in         (ETH → USDC)
//! 6. Native-out        (USDC → ETH)
//! 7. Distinct recipient (exact-in, USDC → WETH, output goes to 0xD1…)
//! 8. Large swap        (100 000 USDC → WETH, WeiExact — documents any
//!    concentrated-liquidity tick-window divergence as a
//!    hard failure rather than a ppm-bounded tolerance)
//!
//! V3 routes via SwapRouter02 with a direct ERC-20 approval — NOT the
//! Universal Router / Permit2.  There is no Permit2 row.
//!
//! All tests are `#[ignore]`d and gated on `$AMM_RPC_FORK_URL`.

#![cfg(test)]

#[path = "support/mod.rs"]
mod support;

use alloy::primitives::U256;
use amm_core::primitives::asset::ChainId;
use amm_rpc::execution::routing::ExactOutPolicy;
use support::{ApprovalKind, Case, Direction, Expect, RecipientKind, Trade};

// ── Case table ─────────────────────────────────────────────────────────────────

/// All V3 swap cases for the `usdc_weth_v3_005` fixture.
///
/// Fixture layout: token0 = USDC (6 decimals, slot 9),
///                 token1 = WETH (18 decimals, slot 3).
/// `Forward` → USDC→WETH; `Reverse` → WETH→USDC.
///
/// V3 routes via SwapRouter02 with a direct ERC-20 approval — `ApprovalKind::Erc20`
/// for all token-input rows.  No Permit2 row (only V4 uses Permit2).
fn v3_cases() -> Vec<Case> {
    vec![
        // 1. Exact-in Forward: 1 000 USDC → WETH
        Case {
            name: "v3_exact_in_forward_usdc_weth",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000u64), // 1 000 USDC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 2. Exact-in Reverse: 0.3 WETH → USDC
        Case {
            name: "v3_exact_in_reverse_weth_usdc",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Reverse,
            trade: Trade::ExactIn {
                amount_in: U256::from(300_000_000_000_000_000u128), // 0.3 WETH (18 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 3. Exact-out Forward: receive exact 0.1 WETH, pay USDC
        Case {
            name: "v3_exact_out_forward_usdc_to_exact_weth",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Forward,
            trade: Trade::ExactOut {
                amount_out: U256::from(100_000_000_000_000_000u128), // 0.1 WETH
                policy: ExactOutPolicy::Strict,
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::Exact,
        },
        // 4. Exact-out Reverse: receive exact 500 USDC, pay WETH
        Case {
            name: "v3_exact_out_reverse_weth_to_exact_usdc",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Reverse,
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
        // 5. Native-in: 1 ETH → USDC (pool math via WETH side, Reverse direction)
        //    The V3 pool has WETH as token1; ETH→USDC means WETH-in → USDC-out
        //    which is Reverse (token1 → token0).
        //    NOTE: proven by the old wei_exact_v3_all_directions Direction 3 block.
        Case {
            name: "v3_native_in_eth_to_usdc",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Reverse,
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
            },
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 6. Native-out: 1 000 USDC → ETH (pool math via WETH side, Forward direction)
        //    USDC→WETH (Forward) with native_out=true unwraps WETH to ETH on delivery.
        //    NOTE: proven by the old wei_exact_v3_all_directions Direction 4 block.
        Case {
            name: "v3_native_out_usdc_to_eth",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000u64), // 1 000 USDC (6 dp)
            },
            native_in: false,
            native_out: true,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 7. Distinct recipient: exact-in Forward, output sent to 0xD1…
        Case {
            name: "v3_exact_in_forward_distinct_recipient",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000u64), // 1 000 USDC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Distinct,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 8. Large swap: 100 000 USDC → WETH — documents concentrated-liquidity
        //    tick-window divergence as a hard WeiExact failure (no ppm tolerance).
        //    If tick truncation causes divergence, this is a real finding for the
        //    controller; the old test used 500 ppm tolerance.
        Case {
            name: "v3_large_exact_in_forward_100k_usdc",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(100_000_000_000u64), // 100 000 USDC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // OrBetter exact-out: caller asks for exactly 0.1 WETH but accepts more.
        // The executor backward-solves the input, then runs exact-in with the
        // floor pinned to the target — delivering ≥ 0.1 WETH (AtLeast). Exercises
        // the OrBetter degrade path on a family that DOES support Strict exact-out.
        Case {
            name: "v3_exact_out_orbetter_forward_usdc_weth",
            chain: ChainId(1),
            pools: &["usdc_weth_v3_005"],
            direction: Direction::Forward,
            trade: Trade::ExactOut {
                amount_out: U256::from(100_000_000_000_000_000u128), // 0.1 WETH
                policy: ExactOutPolicy::OrBetter,
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::AtLeast,
        },
        // Note: V3 routes via SwapRouter02 with a direct ERC-20 approval — it
        // never uses Permit2 or the Universal Router, so there is no Permit2 row.
    ]
}

// ── Fork matrix ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL"]
async fn v3_matrix() {
    let Some((mut fork, _)) = support::open_fork("AMM_RPC_FORK_URL", 20_000_000, ChainId(1)).await
    else {
        return;
    };
    let cfg = support::mainnet_chain_config();
    for case in v3_cases() {
        support::run_case(&mut fork, &cfg, &case).await;
    }
}
