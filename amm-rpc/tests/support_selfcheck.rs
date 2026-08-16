//! Fixtures self-check: verify every mainnet fixture refreshes and balance
//! slots are correct on a real mainnet fork.
//!
//! Gated on `$AMM_RPC_FORK_URL`; `#[ignore]` keeps the test out of offline CI.
//! This test is verified against a live fork in Task 8.

#![cfg(test)]

#[path = "support/mod.rs"]
#[allow(dead_code)]
mod support;

use alloy::primitives::{Address, U256};
use amm_core::primitives::asset::ChainId;
use support::{ApprovalKind, Case, Direction, Expect, RecipientKind, Trade};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL (Ethereum mainnet archive node)"]
async fn fixtures_refresh_and_slots_are_correct_mainnet() {
    // Open the fork at block 20_000_000 (pre-V4). Each fixture is refreshed at
    // its own `default_block` so that V4 fixtures (launched after 20M) are also
    // exercised — the alloy provider can read any block regardless of the revm
    // fork's pinned block, and the slot-deal writes are block-agnostic here.
    let Some((mut fork, _pinned_block)) =
        support::open_fork("AMM_RPC_FORK_URL", 20_000_000, ChainId(1)).await
    else {
        return;
    };

    for fx in support::mainnet_fixtures() {
        let pool = support::refresh(fx, fork.provider().clone(), fx.default_block).await;
        assert!(
            pool.assets().len() >= 2,
            "{} must load at least two assets",
            fx.name
        );
        // Verify the token0 balance slot via a small deposit + read-back.
        if fx.token0.addr != Address::ZERO && fx.token0.balance_slot != 0 {
            let holder = Address::repeat_byte(0xBE);
            let amount = U256::from(1_000_000u64);
            let snap = fork.snapshot();
            fork.fund_erc20(holder, fx.token0.addr, fx.token0.balance_slot, amount);
            let bal = fork.erc20_balance(fx.token0.addr, holder);
            assert_eq!(
                bal, amount,
                "{}: token0 balance slot {} may be wrong",
                fx.name, fx.token0.balance_slot
            );
            fork.revert(snap);
        }
    }
}

/// End-to-end proof that the `run_case` engine executes a real swap and asserts
/// wei-exact output on-chain — the single-hop equivalent of the hand-written
/// `execution_v3.rs` proof, expressed as one `Case` row.
///
/// USDC→WETH on the 0.05% V3 pool: USDC (0xa0b8…) < WETH (0xC02a…), so USDC is
/// token0 and the direction is `Forward`. Set `AMM_FORK_BLOCK` to a recent block
/// to run against a keyless node (the runner refreshes at the fork's own block).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires $AMM_RPC_FORK_URL"]
async fn run_case_v3_exact_in_is_wei_exact() {
    let Some((mut fork, _)) = support::open_fork("AMM_RPC_FORK_URL", 20_000_000, ChainId(1)).await
    else {
        return;
    };
    let cfg = support::mainnet_chain_config();
    let case = Case {
        name: "v3_usdc_weth_exact_in",
        chain: ChainId(1),
        pools: &["usdc_weth_v3_005"],
        direction: Direction::Forward,
        trade: Trade::ExactIn {
            amount_in: U256::from(1_000_000_000u64), // 1000 USDC (6 decimals)
        },
        native_in: false,
        native_out: false,
        recipient: RecipientKind::Sender,
        approval: ApprovalKind::Erc20,
        expect: Expect::WeiExact,
    };
    // Panics internally on any assertion failure (revert, wrong output, residue).
    support::run_case(&mut fork, &cfg, &case).await;
}
