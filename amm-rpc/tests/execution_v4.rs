//! Table-driven execution proof for Uniswap V4 — native and WETH-currency pool
//! swap directions.
//!
//! Uses the Phase-1 `support::run_case` engine. Each row in `v4_cases()` is a
//! fully self-describing [`support::Case`] that the engine executes against a
//! live revm fork.  Coverage:
//!
//! **`eth_usdc_v4_005` (native pool — currency0 = address(0), currency1 = USDC)**
//!
//! Native ETH is `currency0` (address(0) sorts before any non-zero EVM address),
//! so `Forward` = ETH→USDC and `Reverse` = USDC→ETH.
//!
//! 1. Exact-in  Forward  (ETH → USDC):  `native_in=true`, no approval.
//! 2. Exact-in  Reverse  (USDC → ETH):  `native_out=true`, `ApprovalKind::Permit2`.
//! 3. Exact-out Forward  (ETH → exact USDC): `native_in=true`, no approval.
//! 4. Exact-out Reverse  (USDC → exact ETH): `native_out=true`, `ApprovalKind::Permit2`.
//! 5. Distinct recipient (exact-in ETH→USDC, output to 0xD1…).
//!
//! **`usdc_weth_v4_005` (WETH-currency pool — currency0 = USDC, currency1 = WETH)**
//!
//! USDC < WETH numerically, so `Forward` = USDC→WETH and `Reverse` = WETH→USDC.
//! The Universal Router WRAP_ETH / UNWRAP_WETH commands handle native ↔ WETH
//! conversion transparently around the V4 swap.
//!
//! 6.  Exact-in Forward  (USDC → WETH):          `ApprovalKind::Permit2`.
//! 7.  Exact-in Reverse  (WETH → USDC):          `ApprovalKind::Permit2` (ERC-20 WETH).
//! 8.  Native-in exact-in  (ETH → USDC via wrap): `native_in=true`, no approval.
//! 9.  Native-out exact-in (USDC → ETH via unwrap): `native_out=true`, `Permit2`.
//! 10. Native-in exact-out (ETH → exact USDC via wrap): `native_in=true`, no approval.
//!
//! V4 routes through the **Universal Router + Permit2**.  All ERC-20 input rows
//! use `ApprovalKind::Permit2` (never `Erc20`).  Native ETH input carries no
//! approval (`tx.value` is used instead).
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

/// All V4 swap cases across both the native pool and the WETH-currency pool.
///
/// **`eth_usdc_v4_005` layout:**
/// - `token0` = address(0) (native ETH), `token1` = USDC (6 dp, slot 9).
/// - `Forward` = ETH→USDC (native_in=true, no Permit2).
/// - `Reverse` = USDC→ETH (native_out=true, ApprovalKind::Permit2 for USDC input).
///
/// **`usdc_weth_v4_005` layout:**
/// - `token0` = USDC (6 dp, slot 9), `token1` = WETH (18 dp, slot 3).
/// - `Forward` = USDC→WETH; `Reverse` = WETH→USDC.
/// - Native wrap/unwrap cases set `native_in`/`native_out` on the WETH side.
///
/// V4 is the only protocol that uses `ApprovalKind::Permit2` (all ERC-20 inputs).
fn v4_cases() -> Vec<Case> {
    vec![
        // ── eth_usdc_v4_005: native pool ──────────────────────────────────────

        // 1. Exact-in Forward: 0.01 ETH → USDC
        //    ETH is currency0 (address(0)) → Forward direction.
        //    Native ETH input: tx.value = eth_in, no Permit2 approval.
        Case {
            name: "v4_native_pool_exact_in_forward_eth_usdc",
            chain: ChainId(1),
            pools: &["eth_usdc_v4_005"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(10_000_000_000_000_000u64), // 0.01 ETH (18 dp)
            },
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20, // irrelevant: native_in means no ERC-20 approval
            expect: Expect::WeiExact,
        },
        // 2. Exact-in Reverse: 100 USDC → ETH
        //    USDC is currency1 → Reverse direction (token1 → token0).
        //    ERC-20 input (USDC) approved via Permit2.
        Case {
            name: "v4_native_pool_exact_in_reverse_usdc_eth",
            chain: ChainId(1),
            pools: &["eth_usdc_v4_005"],
            direction: Direction::Reverse,
            trade: Trade::ExactIn {
                amount_in: U256::from(100_000_000u64), // 100 USDC (6 dp)
            },
            native_in: false,
            native_out: true,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Permit2,
            expect: Expect::WeiExact,
        },
        // 3. Exact-out Forward: ETH → exact 10 USDC
        //    Native ETH input (tx.value = max_in with slippage); exact USDC output.
        Case {
            name: "v4_native_pool_exact_out_forward_eth_to_exact_usdc",
            chain: ChainId(1),
            pools: &["eth_usdc_v4_005"],
            direction: Direction::Forward,
            trade: Trade::ExactOut {
                amount_out: U256::from(10_000_000u64), // 10 USDC (6 dp)
                policy: ExactOutPolicy::Strict,
            },
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20, // irrelevant: native_in
            expect: Expect::Exact,
        },
        // 4. Exact-out Reverse: USDC → exact 0.001 ETH
        //    USDC input via Permit2; exact ETH output.
        Case {
            name: "v4_native_pool_exact_out_reverse_usdc_to_exact_eth",
            chain: ChainId(1),
            pools: &["eth_usdc_v4_005"],
            direction: Direction::Reverse,
            trade: Trade::ExactOut {
                amount_out: U256::from(1_000_000_000_000_000u64), // 0.001 ETH (18 dp)
                policy: ExactOutPolicy::Strict,
            },
            native_in: false,
            native_out: true,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Permit2,
            expect: Expect::Exact,
        },
        // 5. Distinct recipient: exact-in ETH→USDC, output sent to 0xD1…
        //    Mirrors the old `v4_exact_in_delivers_to_distinct_recipient` test.
        //    Native ETH input, no Permit2; output must arrive at DISTINCT_RECIPIENT.
        Case {
            name: "v4_native_pool_exact_in_forward_distinct_recipient",
            chain: ChainId(1),
            pools: &["eth_usdc_v4_005"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(10_000_000_000_000_000u64), // 0.01 ETH (18 dp)
            },
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Distinct,
            approval: ApprovalKind::Erc20, // irrelevant: native_in
            expect: Expect::WeiExact,
        },
        // ── usdc_weth_v4_005: WETH-currency pool ─────────────────────────────

        // 6. Exact-in Forward: 100 USDC → WETH (ERC-20 WETH, no unwrap)
        //    USDC is currency0 → Forward direction.
        //    USDC input via Permit2; WETH output delivered as ERC-20.
        Case {
            name: "v4_weth_pool_exact_in_forward_usdc_weth",
            chain: ChainId(1),
            pools: &["usdc_weth_v4_005"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(100_000_000u64), // 100 USDC (6 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Permit2,
            expect: Expect::WeiExact,
        },
        // 7. Exact-in Reverse: 0.01 WETH → USDC (ERC-20 WETH input, no wrap)
        //    WETH is currency1 → Reverse direction (token1 → token0).
        //    ERC-20 WETH input via Permit2; USDC output.
        Case {
            name: "v4_weth_pool_exact_in_reverse_weth_usdc",
            chain: ChainId(1),
            pools: &["usdc_weth_v4_005"],
            direction: Direction::Reverse,
            trade: Trade::ExactIn {
                amount_in: U256::from(10_000_000_000_000_000u64), // 0.01 WETH (18 dp)
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Permit2,
            expect: Expect::WeiExact,
        },
        // 8. Native-in exact-in: ETH → USDC via WETH-currency pool (WRAP_ETH path)
        //    The encoder emits [WRAP_ETH, V4_SWAP]: router wraps ETH to WETH,
        //    settles the WETH debt from its own balance.  tx.value = eth_in;
        //    no Permit2 approval.  Mirrors old Direction 1 of
        //    `wei_exact_v4_weth_pool_wrap_directions`.
        //
        //    Direction: Reverse (WETH=token1 is the input side; USDC=token0 is output)
        //    with native_in=true so the runner sends ETH via tx.value.
        Case {
            name: "v4_weth_pool_native_in_exact_in_eth_to_usdc_wrap",
            chain: ChainId(1),
            pools: &["usdc_weth_v4_005"],
            direction: Direction::Reverse,
            trade: Trade::ExactIn {
                amount_in: U256::from(10_000_000_000_000_000u64), // 0.01 ETH (18 dp)
            },
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20, // irrelevant: native_in
            expect: Expect::WeiExact,
        },
        // 9. Native-out exact-in: USDC → ETH via WETH-currency pool (UNWRAP_WETH path)
        //    The encoder emits [V4_SWAP, UNWRAP_WETH]: pool outputs WETH to the
        //    router; UNWRAP_WETH converts to native ETH for the recipient.  tx.value = 0;
        //    USDC input via Permit2.  Mirrors old Direction 2 of
        //    `wei_exact_v4_weth_pool_wrap_directions`.
        //
        //    Direction: Forward (USDC=token0 is input; WETH=token1 output side)
        //    with native_out=true so output is unwrapped to ETH.
        Case {
            name: "v4_weth_pool_native_out_exact_in_usdc_to_eth_unwrap",
            chain: ChainId(1),
            pools: &["usdc_weth_v4_005"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(100_000_000u64), // 100 USDC (6 dp)
            },
            native_in: false,
            native_out: true,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Permit2,
            expect: Expect::WeiExact,
        },
        // 10. Native-in exact-out: ETH → exact USDC via WETH-currency pool (WRAP+UNWRAP path)
        //     The encoder emits [WRAP_ETH, V4_SWAP, UNWRAP_WETH]: wraps slippage-padded
        //     max ETH; SETTLE drains only the actual cost; UNWRAP_WETH returns leftover
        //     WETH as ETH to the recipient.  Mirrors old Direction 3 of
        //     `wei_exact_v4_weth_pool_wrap_directions`.
        //
        //     Direction: Reverse (WETH=token1 is input side; USDC=token0 is exact output)
        //     with native_in=true so ETH is sent via tx.value.
        Case {
            name: "v4_weth_pool_native_in_exact_out_eth_to_exact_usdc_wrap",
            chain: ChainId(1),
            pools: &["usdc_weth_v4_005"],
            direction: Direction::Reverse,
            trade: Trade::ExactOut {
                amount_out: U256::from(10_000_000u64), // 10 USDC (6 dp)
                policy: ExactOutPolicy::Strict,
            },
            native_in: true,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20, // irrelevant: native_in
            expect: Expect::Exact,
        },
    ]
}

// ── Fork matrix ────────────────────────────────────────────────────────────────

/// V4 matrix — runs all `v4_cases()` rows against a single pinned fork.
///
/// Block 25_724_266 is the V4 post-launch archive block used by the old manual
/// tests.  The `run_case` engine refreshes pool state at the fork's own block,
/// so an `AMM_FORK_BLOCK` override transparently shifts the whole suite.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL"]
async fn v4_matrix() {
    let Some((mut fork, _)) = support::open_fork("AMM_RPC_FORK_URL", 25_724_266, ChainId(1)).await
    else {
        return;
    };
    let cfg = support::mainnet_chain_config();
    for case in v4_cases() {
        support::run_case(&mut fork, &cfg, &case).await;
    }
}
