//! Table-driven execution proof for Uniswap V2 — all swap directions.
//!
//! Uses the Phase-1 `support::run_case` engine. Each row in `v2_cases()` is a
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
//!
//! (V2 routes via SwapRouter02 with a direct ERC-20 approval — no Permit2 row.)
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

/// All V2 swap cases for the `usdc_weth_v2` fixture.
///
/// Fixture layout: token0 = USDC (6 decimals), token1 = WETH (18 decimals).
/// `Forward` → USDC→WETH; `Reverse` → WETH→USDC.
fn v2_cases() -> Vec<Case> {
    vec![
        // 1. Exact-in Forward: 1 000 USDC → WETH
        Case {
            name: "v2_exact_in_forward_usdc_weth",
            chain: ChainId(1),
            pools: &["usdc_weth_v2"],
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
            name: "v2_exact_in_reverse_weth_usdc",
            chain: ChainId(1),
            pools: &["usdc_weth_v2"],
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
            name: "v2_exact_out_forward_usdc_to_exact_weth",
            chain: ChainId(1),
            pools: &["usdc_weth_v2"],
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
            name: "v2_exact_out_reverse_weth_to_exact_usdc",
            chain: ChainId(1),
            pools: &["usdc_weth_v2"],
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
        //    The V2 pool has WETH as token1; ETH→USDC means WETH-in → USDC-out
        //    which is Reverse (token1 → token0).
        Case {
            name: "v2_native_in_eth_to_usdc",
            chain: ChainId(1),
            pools: &["usdc_weth_v2"],
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
        Case {
            name: "v2_native_out_usdc_to_eth",
            chain: ChainId(1),
            pools: &["usdc_weth_v2"],
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
            name: "v2_exact_in_forward_distinct_recipient",
            chain: ChainId(1),
            pools: &["usdc_weth_v2"],
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
        // Note: V2 routes via SwapRouter02 with a direct ERC-20 approval — it
        // never uses Permit2 or the Universal Router, so there is no Permit2 row.
    ]
}

// ── Fork matrix ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL"]
async fn v2_matrix() {
    let Some((mut fork, _)) = support::open_fork("AMM_RPC_FORK_URL", 20_000_000, ChainId(1)).await
    else {
        return;
    };
    let cfg = support::mainnet_chain_config();
    for case in v2_cases() {
        support::run_case(&mut fork, &cfg, &case).await;
    }
}

// ── Selfcheck ──────────────────────────────────────────────────────────────────

/// Verify that [`Fork::fund_erc20`], [`Fork::snapshot`], and [`Fork::revert`]
/// round-trip correctly on a real mainnet fork.
///
/// Sequence:
/// 1. Assert the test holder starts with zero USDC.
/// 2. Take a snapshot.
/// 3. Fund the holder with 1 USDC (6 decimals) via storage-slot injection.
/// 4. Assert balance == 1 USDC.
/// 5. Revert to the snapshot.
/// 6. Assert balance is restored to zero.
///
/// Requires `$AMM_RPC_FORK_URL` to point to an archive node; pinned to
/// Ethereum mainnet block 20_000_000.  USDC balance slot is 9.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn selfcheck_fund_and_snapshot_roundtrip() {
    let url = match std::env::var("AMM_RPC_FORK_URL") {
        Ok(v) => v,
        Err(_) => return,
    };

    let usdc: alloy::primitives::Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        .parse()
        .unwrap();
    let holder: alloy::primitives::Address = "0x000000000000000000000000000000000000bE00"
        .parse()
        .unwrap();
    let amount = U256::from(1_000_000u64); // 1 USDC (6 decimals)

    let alloy_provider = alloy::providers::ProviderBuilder::new()
        .connect_http(url.parse().expect("invalid RPC url"));

    let mut fork = support::fork_at(&url, 20_000_000, ChainId(1), alloy_provider).await;

    // ── pre: balance must be 0 ────────────────────────────────────────────
    let before = fork.erc20_balance(usdc, holder);
    assert_eq!(before, U256::ZERO, "test holder already had a balance");

    // ── snapshot → fund → assert balance == amount ───────────────────────
    let snap = fork.snapshot();
    fork.fund_erc20(holder, usdc, 9, amount);
    let funded = fork.erc20_balance(usdc, holder);
    assert_eq!(funded, amount, "funded balance mismatch");

    // ── revert → assert balance == 0 ─────────────────────────────────────
    fork.revert(snap);
    let after = fork.erc20_balance(usdc, holder);
    assert_eq!(after, U256::ZERO, "balance should be 0 after revert");
}
