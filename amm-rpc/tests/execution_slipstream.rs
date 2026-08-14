//! Wei-exact execution proof for Aerodrome Slipstream on Base — all four swap directions.
//!
//! Builds calldata via the `Executable` trait, submits it to a pinned
//! in-process revm fork, and asserts that the on-chain output equals the
//! off-chain quote to the wei.  Covers:
//!
//! 1. Exact-in ERC-20 (USDC → WETH)
//! 2. Exact-out ERC-20 (spend USDC for exact WETH)
//! 3. Native-in (ETH → USDC)
//! 4. Native-out (USDC → ETH)
//!
//! Plus one large-swap bounded-ppm block documenting the concentrated-liquidity
//! finite-window divergence bound (spec §2).
//!
//! All tests are `#[ignore]`d and gated on `$AMM_RPC_FORK_URL_BASE`.

#![cfg(test)]

#[path = "support/mod.rs"]
mod support;

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256, address};
use alloy::providers::Provider;
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::{ExchangeId, PoolKey};
use amm_core::primitives::ratio::Bps;
use amm_core::slippage::Slippage;
use amm_core::traits::pool::Pool;
use amm_rpc::execution::{
    ChainConfig, Currency, CurrencyAmount, Deadline, ExecutionOptions, Recipient, Routers,
    as_executable,
};
use amm_rpc::source::StateSource;

// ── addresses ────────────────────────────────────────────────────────────────

/// Aerodrome Slipstream SwapRouter on Base.
const ROUTER: Address = address!("0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5");
/// USDC on Base (native Circle); `balanceOf` mapping is at storage slot 9.
const USDC_ADDR: Address = address!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
/// WETH on Base (wrapped native).
const WETH_ADDR: Address = address!("0x4200000000000000000000000000000000000006");
/// Aerodrome Slipstream WETH/USDC CL pool on Base (tick spacing 100).
const POOL_ADDR: &str = "0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59";
/// USDC `balanceOf` storage slot (keccak mapping key).
const USDC_SLOT: u64 = 9;
/// Pinned Base block.  Override with `AMM_FORK_BLOCK` to run against a
/// recent block on a non-archive RPC (Base RPC is non-archive).
fn fork_block() -> u64 {
    std::env::var("AMM_FORK_BLOCK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000_000)
}

// ── local helpers ─────────────────────────────────────────────────────────────

/// Wrap a raw EVM address into the `AssetId` used by the pool layer.
fn asset(chain: u64, token: Address) -> AssetId {
    AssetId::new(ChainId(chain), token.into_word())
}

/// Fetch the WETH/USDC Aerodrome Slipstream pool from `provider`,
/// pinned to [`fork_block`].
///
/// Panics when the pool fails to refresh — that is always a broken test setup,
/// not a production error.
async fn fetch_slipstream_pool(provider: &impl Provider) -> Box<dyn Pool> {
    let source = amm_rpc::protocols::slipstream::SlipstreamSource::new(provider);
    // token0 = WETH, token1 = USDC (address-sorted order on Base).
    let weth = asset(8453, WETH_ADDR);
    let usdc = asset(8453, USDC_ADDR);
    let key = PoolKey {
        exchange: ExchangeId::new("aerodrome-slipstream"),
        chain: ChainId(8453),
        address: POOL_ADDR.to_string(),
        assets: vec![weth, usdc],
        fee_bps: None,
    };
    let mut pools = source
        .refresh(&[key], BlockId::number(fork_block()))
        .await
        .expect("SlipstreamSource::refresh");
    assert_eq!(pools.len(), 1, "expected exactly one pool from refresh");
    pools.remove(0)
}

/// Resolved execution options shared across all swap directions.
///
/// - 50 bps slippage
/// - Explicit recipient (`sender`)
/// - Absolute deadline (`u64::MAX / 2`) — no `resolve` call needed.
///   Slipstream embeds the deadline inside the params struct so the far-future
///   absolute timestamp passes without any timestamp resolution.
fn exec_opts(sender: Address) -> ExecutionOptions {
    ExecutionOptions::new(Slippage::from_bps(Bps(50)))
        .with_recipient(Recipient::To(sender))
        .with_deadline(Deadline::AtTimestamp(u64::MAX / 2))
}

// ── proof ─────────────────────────────────────────────────────────────────────

/// Wei-exact Slipstream execution proof — all four swap directions plus a
/// large-swap bounded-ppm block.
///
/// Each direction:
/// 1. Saves a fork snapshot.
/// 2. Funds the impersonated sender via storage-slot injection.
/// 3. Asserts the USDC slot is correct (fail-fast before submitting).
/// 4. Builds calldata via the `Executable` trait (`as_executable`).
/// 5. Submits via [`Fork::submit`] (synchronous in-process EVM).
/// 6. Asserts the on-chain balance delta equals the off-chain quote to the wei.
/// 7. Reverts the snapshot so each direction starts from clean state.
///
/// Requires `flavor = "multi_thread"` because `foundry_fork_db::SharedBackend`
/// internally calls `tokio::task::block_in_place` to park the RPC polling loop.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked Base RPC at $AMM_RPC_FORK_URL_BASE"]
async fn wei_exact_slipstream_all_directions() {
    let url = match std::env::var("AMM_RPC_FORK_URL_BASE") {
        Ok(v) => v,
        Err(_) => return,
    };

    let alloy_provider = alloy::providers::ProviderBuilder::new()
        .connect_http(url.parse().expect("invalid RPC url"));

    let mut fork =
        support::fork_at(&url, fork_block(), ChainId(8453), alloy_provider.clone()).await;

    assert_eq!(
        fork.block_number(),
        fork_block(),
        "fork must be pinned to fork_block()"
    );

    // ── shared constants ─────────────────────────────────────────────────────

    // token0 = WETH, token1 = USDC (address-sorted on Base).
    let weth = asset(8453, WETH_ADDR);
    let usdc = asset(8453, USDC_ADDR);
    let sender = Address::repeat_byte(0xBE);

    // Per-chain config: WETH as wrapped native, Slipstream router set.
    // `Routers` is #[non_exhaustive] so struct literals are forbidden outside
    // the crate; start from Default and set only the field we need.
    let mut routers = Routers::default();
    routers.slipstream = Some(ROUTER);
    let cfg = ChainConfig::new(ChainId(8453), weth).with_routers(routers);

    let opts = exec_opts(sender);

    // Fetch the pool once; each direction reads from the same pinned state.
    let pool = fetch_slipstream_pool(fork.provider()).await;
    let exe = as_executable(pool.as_ref()).expect("AerodromeSlipstreamPool must be Executable");

    // ── Direction 1: Exact-in ERC-20 (USDC → WETH) ──────────────────────────
    {
        let snap = fork.snapshot();

        let amt = U256::from(10_000_000u64); // 10 USDC (6 dec)
        let quoted = pool
            .quote(&AssetAmount::new(usdc, amt), &weth)
            .expect("pool must quote USDC→WETH");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdc),
                    raw: amt,
                },
                Currency::Token(weth),
                &quoted,
                &opts,
            )
            .expect("build_swap ERC-20 exact-in must succeed");

        // Fund USDC (2× the swap amount).
        let fund_amount = U256::from(20_000_000u64);
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, fund_amount);
        // Fail-fast USDC slot check: confirm the slot write landed correctly.
        assert_eq!(
            fork.erc20_balance(USDC_ADDR, sender),
            fund_amount,
            "USDC balance slot may be wrong — check USDC_SLOT constant"
        );

        // Apply approval exactly as the library requests (never hardcode spender/amount).
        if let Some(req) = &prepared.approval {
            let token_addr = alloy::primitives::Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        let before = fork.erc20_balance(WETH_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "exact-in ERC-20 swap reverted"
        );
        let after = fork.erc20_balance(WETH_ADDR, sender);

        assert_eq!(
            after - before,
            quoted.raw,
            "exact-in ERC-20: WETH delta must equal quoted output"
        );

        fork.revert(snap);
    }

    // ── Direction 2: Exact-out ERC-20 (spend USDC for exact WETH) ───────────
    {
        let snap = fork.snapshot();

        let target = U256::from(1_000_000_000_000_000u128); // 0.001 WETH
        let quoted_in = pool
            .as_exact_out()
            .expect("pool must expose exact-out")
            .quote_exact_out(&AssetAmount::new(weth, target), &usdc)
            .expect("pool must quote exact-out WETH←USDC");

        let prepared = exe
            .build_swap_exact_out(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(weth),
                    raw: target,
                },
                Currency::Token(usdc),
                &quoted_in,
                &opts,
            )
            .expect("build_swap_exact_out ERC-20 must succeed");

        // Fund USDC generously (100× quoted input).
        let fund_amount = quoted_in.raw * U256::from(100u64);
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, fund_amount);
        // Fail-fast USDC slot check.
        assert_eq!(
            fork.erc20_balance(USDC_ADDR, sender),
            fund_amount,
            "USDC balance slot may be wrong — check USDC_SLOT constant"
        );

        // Apply approval exactly as the library requests.
        if let Some(req) = &prepared.approval {
            let token_addr = alloy::primitives::Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        let weth_before = fork.erc20_balance(WETH_ADDR, sender);
        let usdc_before = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "exact-out ERC-20 swap reverted"
        );
        let weth_after = fork.erc20_balance(WETH_ADDR, sender);
        let usdc_after = fork.erc20_balance(USDC_ADDR, sender);

        // Received exactly the target WETH.
        assert_eq!(
            weth_after - weth_before,
            target,
            "exact-out ERC-20: WETH received must equal target"
        );

        // USDC spent must not exceed the max_spent ceiling.
        let usdc_spent = usdc_before - usdc_after;
        let max_spent = prepared
            .max_spent
            .expect("max_spent must be Some for exact-out")
            .raw;
        assert!(
            usdc_spent <= max_spent,
            "exact-out ERC-20: USDC spent ({usdc_spent}) must be <= max_spent ({max_spent})"
        );

        fork.revert(snap);
    }

    // ── Direction 3: Native-in (ETH → USDC) ─────────────────────────────────
    {
        let snap = fork.snapshot();

        let eth_in = U256::from(3_000_000_000_000_000u128); // 0.003 ETH
        // Pool math treats the ETH side as WETH.
        let quoted = pool
            .quote(&AssetAmount::new(weth, eth_in), &usdc)
            .expect("pool must quote WETH→USDC");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Native,
                    raw: eth_in,
                },
                Currency::Token(usdc),
                &quoted,
                &opts,
            )
            .expect("build_swap native-in must succeed");

        // No ERC-20 approval needed for native-in.
        assert!(
            prepared.approval.is_none(),
            "native-in swap must carry no ERC-20 approval"
        );

        // Fund sender with eth_in + 1 ETH buffer.  gas_price=0 so no gas is
        // deducted, but tx.value still consumes the exact eth_in amount.
        let eth_buffer = U256::from(1_000_000_000_000_000_000u128);
        fork.fund_native(sender, eth_in + eth_buffer);

        let before = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "native-in (ETH→USDC) swap reverted"
        );
        let after = fork.erc20_balance(USDC_ADDR, sender);

        assert_eq!(
            after - before,
            quoted.raw,
            "native-in: USDC delta must equal quoted output"
        );

        fork.revert(snap);
    }

    // ── Direction 4: Native-out (USDC → ETH) ─────────────────────────────────
    {
        let snap = fork.snapshot();

        let amt = U256::from(10_000_000u64); // 10 USDC (6 dec)
        let quoted = pool
            .quote(&AssetAmount::new(usdc, amt), &weth)
            .expect("pool must quote USDC→WETH");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdc),
                    raw: amt,
                },
                Currency::Native,
                &quoted,
                &opts,
            )
            .expect("build_swap native-out must succeed");

        // Fund USDC, then perform the fail-fast slot check.
        let fund_amount = U256::from(20_000_000u64);
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, fund_amount);
        assert_eq!(
            fork.erc20_balance(USDC_ADDR, sender),
            fund_amount,
            "USDC balance slot may be wrong — check USDC_SLOT constant"
        );

        // Apply approval exactly as the library requests.
        if let Some(req) = &prepared.approval {
            let token_addr = alloy::primitives::Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        // Fund some ETH for gas headroom (gas_price=0 means no deduction, but
        // the EVM checks the sender's balance covers tx.value which is zero here).
        fork.fund_native(sender, U256::from(1_000_000_000_000_000_000u128));

        let before = fork.native_balance(sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "native-out (USDC→ETH) swap reverted"
        );
        let after = fork.native_balance(sender);

        // gas_price=0 so ETH delta == unwrapped WETH output with no gas deduction.
        assert_eq!(
            after - before,
            quoted.raw,
            "native-out: ETH delta must equal quoted WETH output"
        );

        fork.revert(snap);
    }

    // ── Large-swap bounded-ppm block (USDC → WETH, 100k USDC) ───────────────
    //
    // Documents the concentrated-liquidity finite-window divergence bound (spec §2).
    // PPM = 500 corresponds to 0.05%.  If the Slipstream pool math is wei-exact for
    // this size, diff == 0 and the assertion still passes.  If tick-window truncation
    // causes a minor divergence, the ppm bound documents the acceptable margin.
    {
        const PPM: u128 = 500;

        let snap = fork.snapshot();

        let large_amt = U256::from(100_000_000_000u64); // 100 000 USDC (6 dec)
        let quoted = pool
            .quote(&AssetAmount::new(usdc, large_amt), &weth)
            .expect("pool must quote large USDC→WETH");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdc),
                    raw: large_amt,
                },
                Currency::Token(weth),
                &quoted,
                &opts,
            )
            .expect("build_swap large exact-in must succeed");

        // Fund USDC (2× the swap amount) and perform the fail-fast slot check.
        let fund_amount = U256::from(200_000_000_000u64);
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, fund_amount);
        assert_eq!(
            fork.erc20_balance(USDC_ADDR, sender),
            fund_amount,
            "USDC balance slot may be wrong — check USDC_SLOT constant"
        );

        // Apply approval exactly as the library requests.
        if let Some(req) = &prepared.approval {
            let token_addr = alloy::primitives::Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        let before = fork.erc20_balance(WETH_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "large exact-in swap reverted"
        );
        let after = fork.erc20_balance(WETH_ADDR, sender);

        let delta = after - before;
        let diff = delta.abs_diff(quoted.raw);
        let tolerance = quoted.raw * U256::from(PPM) / U256::from(1_000_000u64);
        assert!(
            diff <= tolerance,
            "large-swap: on-chain delta {delta} differs from quote {quoted_raw} by {diff} \
             which exceeds {PPM} ppm tolerance {tolerance}",
            quoted_raw = quoted.raw,
        );

        fork.revert(snap);
    }
}
