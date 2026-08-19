//! Table-driven execution proof for Aerodrome (Solidly) on Base.
//!
//! Uses the `support::run_case` engine. Each row in `aerodrome_cases()` is a
//! fully self-describing [`support::Case`] that the engine executes against a
//! live revm fork. Coverage:
//!
//! **`aero_vol_weth_usdc` (volatile vAMM — WETH/USDC on Base)**
//!
//! Fixture layout: token0 = WETH (18 dp, slot 3), token1 = USDC (6 dp, slot 9).
//! `Forward` = WETH→USDC; `Reverse` = USDC→WETH.
//!
//! 1. Exact-in Forward       (WETH→USDC)                    — WeiExact
//! 2. Exact-in Reverse       (USDC→WETH)                    — WeiExact
//! 3. Native-in              (ETH→USDC)                     — WeiExact
//! 4. Native-out             (USDC→ETH; WETH-side → unwrap) — WeiExact
//! 5. Distinct recipient     (WETH→USDC, output to 0xD1…)   — WeiExact
//! 6. Exact-out reject       (any direction)                 — RejectBuild(UnsupportedProtocol)
//!
//! **`aero_stable_usdc_usdbc` (stable sAMM — USDC/USDbC on Base)**
//!
//! Fixture layout: token0 = USDC (6 dp, slot 9), token1 = USDbC (6 dp).
//! INPUT is always USDC — USDbC is a proxy with a non-standard balance layout
//! that resists slot-stuffing, so it is only usable as swap output.
//!
//! 7. Exact-in Forward (USDC→USDbC) — WeiExact (proves the stable-swap math)
//!
//! # Aerodrome-specific notes
//!
//! - Aerodrome is **Solidly-style: exact-in only**. `build_swap_exact_out` returns
//!   `BuildError::UnsupportedProtocol` (not `UnsupportedExactOut`). Row 6 locks
//!   that behaviour.
//! - Approvals are **direct ERC-20** to the Aerodrome router (`ApprovalKind::Erc20`).
//!   No Permit2.
//! - `swapExactTokensForTokens` has a `to` recipient param → distinct-recipient IS
//!   supported (row 5).
//! - Native ETH: the Aerodrome router exposes swap-ETH functions. Rows 3 and 4
//!   exercise the native-in and native-out paths. If the library's Aerodrome
//!   executable does not wire the native swap functions, those rows will fail on
//!   the fork and the controller will adjust.
//!
//! All tests are `#[ignore]`d and gated on `$AMM_RPC_FORK_URL_BASE`.

#![cfg(test)]

#[path = "support/mod.rs"]
mod support;

use alloy::primitives::U256;
use amm_core::primitives::asset::ChainId;
use amm_rpc::execution::routing::ExactOutPolicy;
use support::{ApprovalKind, BuildErrorKind, Case, Direction, Expect, RecipientKind, Trade};

// ── Case table ─────────────────────────────────────────────────────────────────

/// All Aerodrome swap cases, grouped by fixture.
///
/// # Input token / slot notes
///
/// `aero_vol_weth_usdc`:
///   - Forward (WETH→USDC): INPUT = WETH token0 (slot 3, known-good). ✓
///   - Reverse (USDC→WETH): INPUT = USDC token1 (slot 9, known-good). ✓
///   - Native-in (ETH→USDC): INPUT = native ETH, funded via `fund_native`. ✓
///   - Native-out (USDC→ETH): INPUT = USDC token1 (slot 9, known-good). ✓
///   - Distinct recipient: INPUT = WETH token0 (slot 3). ✓
///   - Exact-out reject: ExactOut direction Forward; build rejected before funding. ✓
///
/// `aero_stable_usdc_usdbc`:
///   - Forward (USDC→USDbC): INPUT = USDC token0 (slot 9, known-good). ✓
///   - Reverse (USDbC→USDC): INPUT = USDbC token1 (slot 0, TODO). ⚠ See note.
fn aerodrome_cases() -> Vec<Case> {
    vec![
        // ── aero_vol_weth_usdc ────────────────────────────────────────────────

        // 1. Exact-in Forward: 0.5 WETH → USDC
        //    INPUT = WETH (token0, slot 3). OUTPUT = USDC (token1).
        //    Solidly vAMM has no tick window — off-chain quote matches on-chain
        //    exactly at any size. WeiExact is the expected outcome.
        Case {
            name: "aero_vol_exact_in_forward_weth_usdc",
            chain: ChainId(8453),
            pools: &["aero_vol_weth_usdc"],
            direction: Direction::Forward, // WETH → USDC
            trade: Trade::ExactIn {
                amount_in: U256::from(500_000_000_000_000_000u128), // 0.5 WETH (18 dp)
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
            name: "aero_vol_exact_in_reverse_usdc_weth",
            chain: ChainId(8453),
            pools: &["aero_vol_weth_usdc"],
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
        // 3. Native-in: 0.001 ETH → USDC
        //    INPUT = native ETH (pool math: WETH=token0). No ERC-20 approval.
        //    Forward direction because WETH=token0; native_in=true tells the
        //    runner to use Currency::Native and fund via `fund_native`.
        //    Note: if the Aerodrome Executable does not wire the native-ETH
        //    swap path (swapExactETHForTokens), this row will revert on-chain
        //    and the controller will adjust.
        Case {
            name: "aero_vol_native_in_eth_to_usdc",
            chain: ChainId(8453),
            pools: &["aero_vol_weth_usdc"],
            direction: Direction::Forward, // WETH side → USDC
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000_000_000u128), // 0.001 ETH (18 dp)
            },
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20, // irrelevant for native-in; runner skips
            expect: Expect::WeiExact,
        },
        // 4. Native-out: 1 000 USDC → ETH (WETH unwrapped)
        //    INPUT = USDC (token1, slot 9). OUTPUT = native ETH (WETH unwrapped).
        //    Reverse direction: USDC=token1 → WETH=token0, native_out=true.
        //    Note: if Aerodrome's Executable doesn't wire swapExactTokensForETH,
        //    this row will revert on-chain.
        Case {
            name: "aero_vol_native_out_usdc_to_eth",
            chain: ChainId(8453),
            pools: &["aero_vol_weth_usdc"],
            direction: Direction::Reverse, // USDC → WETH-side → ETH unwrap
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000u64), // 1 000 USDC (6 dp)
            },
            native_in: false,
            native_out: true,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 5. Distinct recipient: 0.5 WETH → USDC, output sent to 0xD1…
        //    Aerodrome's swapExactTokensForTokens accepts a `to` param.
        //    INPUT = WETH (token0, slot 3). Direction Forward.
        Case {
            name: "aero_vol_exact_in_forward_distinct_recipient",
            chain: ChainId(8453),
            pools: &["aero_vol_weth_usdc"],
            direction: Direction::Forward, // WETH → USDC
            trade: Trade::ExactIn {
                amount_in: U256::from(500_000_000_000_000_000u128), // 0.5 WETH (18 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Distinct,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // 6. Exact-out reject: Aerodrome is Solidly-style (exact-in only).
        //    build_swap_exact_out must return BuildError::UnsupportedProtocol
        //    (NOT UnsupportedExactOut — that is the multi-hop router error).
        //    No funding occurs; the build is rejected before any EVM submission.
        Case {
            name: "aero_vol_exact_out_reject_unsupported",
            chain: ChainId(8453),
            pools: &["aero_vol_weth_usdc"],
            direction: Direction::Forward, // WETH → USDC (exact-out target)
            trade: Trade::ExactOut {
                amount_out: U256::from(500_000_000u64), // 500 USDC (6 dp)
                policy: ExactOutPolicy::Strict,
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::RejectBuild(BuildErrorKind::UnsupportedProtocol),
        },
        // ── aero_stable_usdc_usdbc ────────────────────────────────────────────

        // 7. Exact-in Forward: 1 000 USDC → USDbC
        //    INPUT = USDC (token0, slot 9, known-good). OUTPUT = USDbC (token1).
        //    Stable sAMM: off-chain quote matches on-chain exactly at normal sizes.
        Case {
            name: "aero_stable_exact_in_forward_usdc_usdbc",
            chain: ChainId(8453),
            pools: &["aero_stable_usdc_usdbc"],
            direction: Direction::Forward, // USDC → USDbC
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000u64), // 1 000 USDC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        // Reverse (USDbC → USDC): USDbC as the FUNDED INPUT. USDbC's balanceOf
        // lives at Solidity slot 51 (verified on-chain — beyond the 0..40 range
        // originally probed), so it can be slot-funded after all. Proves the
        // stable-swap in the opposite direction with a proxy-token input.
        Case {
            name: "aero_stable_exact_in_reverse_usdbc_usdc",
            chain: ChainId(8453),
            pools: &["aero_stable_usdc_usdbc"],
            direction: Direction::Reverse, // USDbC → USDC
            trade: Trade::ExactIn {
                amount_in: U256::from(1_000_000_000u64), // 1 000 USDbC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        },
        //
        // Note: OrBetter exact-out on Aerodrome degrades to exact-in with the
        // floor pinned to the target inside the `plan()` executor — not the
        // single-pool `Executable` path this runner drives (which would return
        // UnsupportedProtocol from `build_swap_exact_out`). The family-agnostic
        // OrBetter degradation is fork-proven end-to-end via the Curve span in
        // `multihop_curve_exact_out_orbetter_usdc_usdt_weth` (execution_multihop.rs).
    ]
}

// ── Fork matrix ────────────────────────────────────────────────────────────────

/// Run all Aerodrome cases against a live Base fork pinned to block 30_000_000.
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
async fn aerodrome_matrix() {
    let Some((mut fork, _)) =
        support::open_fork("AMM_RPC_FORK_URL_BASE", 30_000_000, ChainId(8453)).await
    else {
        return;
    };
    let cfg = support::base_chain_config();
    for case in aerodrome_cases() {
        support::run_case(&mut fork, &cfg, &case).await;
    }
}
