//! Wei-exact execution proof for Uniswap V2 — all four swap directions.
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
//! All tests are `#[ignore]`d and gated on `$AMM_RPC_FORK_URL`.

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

/// Uniswap V2 Router02 on Ethereum mainnet.
const ROUTER: Address = address!("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
/// USDC on Ethereum mainnet; `balanceOf` mapping is at storage slot 9.
const USDC_ADDR: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
/// WETH on Ethereum mainnet.
const WETH_ADDR: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// UniV2 USDC/WETH pair.
const PAIR: Address = address!("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
/// USDC `balanceOf` storage slot (keccak mapping key).
const USDC_SLOT: u64 = 9;
/// Pinned mainnet block.  Override with `AMM_FORK_BLOCK` to run against a
/// recent block on a non-archive RPC.
fn fork_block() -> u64 {
    std::env::var("AMM_FORK_BLOCK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000_000)
}

// ── local helpers ─────────────────────────────────────────────────────────────

/// Wrap a raw EVM address into the `AssetId` used by the pool layer.
fn asset(chain: u64, token: Address) -> AssetId {
    AssetId::new(ChainId(chain), token.into_word())
}

/// Build a two-asset `PoolKey` with assets in address-sorted `[token0, token1]`
/// order, matching the Uniswap V2 pair convention.
fn key2(exchange: &str, chain: u64, pool: Address, a: Address, b: Address) -> PoolKey {
    let (t0, t1) = match a < b {
        true => (a, b),
        false => (b, a),
    };
    PoolKey {
        exchange: ExchangeId::new(exchange),
        chain: ChainId(chain),
        address: pool.to_string(),
        assets: vec![asset(chain, t0), asset(chain, t1)],
        fee_bps: None,
    }
}

/// Fetch the USDC/WETH UniV2 pool from `provider`, pinned to [`fork_block`].
///
/// Panics when the pool fails to refresh — that is always a broken test setup,
/// not a production error.
async fn fetch_v2_pool(provider: &impl Provider) -> Box<dyn Pool> {
    let source = amm_rpc::protocols::uniswap_v2::UniswapV2Source::new(provider);
    let key = key2("uniswap-v2", 1, PAIR, USDC_ADDR, WETH_ADDR);
    let mut pools = source
        .refresh(&[key], BlockId::number(fork_block()))
        .await
        .expect("UniswapV2Source::refresh");
    assert_eq!(pools.len(), 1, "expected exactly one pool from refresh");
    pools.remove(0)
}

/// Resolved execution options shared across all swap directions.
///
/// - 50 bps slippage
/// - Explicit recipient (`sender`)
/// - Absolute deadline (`u64::MAX / 2`) — no `resolve` call needed
fn exec_opts(sender: Address) -> ExecutionOptions {
    ExecutionOptions::new(Slippage::from_bps(Bps(50)))
        .with_recipient(Recipient::To(sender))
        .with_deadline(Deadline::AtTimestamp(u64::MAX / 2))
}

// ── selfcheck ─────────────────────────────────────────────────────────────────

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

    let usdc: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        .parse()
        .unwrap();
    let holder: Address = "0x000000000000000000000000000000000000bE00"
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

// ── proof ─────────────────────────────────────────────────────────────────────

/// Wei-exact V2 execution proof — all four swap directions.
///
/// Each direction:
/// 1. Saves a fork snapshot.
/// 2. Funds the impersonated sender via storage-slot injection.
/// 3. Builds calldata via the `Executable` trait.
/// 4. Submits via [`Fork::submit`] (synchronous in-process EVM).
/// 5. Asserts the on-chain balance delta equals the off-chain quote to the wei.
/// 6. Reverts the snapshot so each direction starts from clean state.
///
/// Requires `flavor = "multi_thread"` because `foundry_fork_db::SharedBackend`
/// internally calls `tokio::task::block_in_place` to park the RPC polling loop.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn wei_exact_v2_all_directions() {
    let url = match std::env::var("AMM_RPC_FORK_URL") {
        Ok(v) => v,
        Err(_) => return,
    };

    let alloy_provider = alloy::providers::ProviderBuilder::new()
        .connect_http(url.parse().expect("invalid RPC url"));

    let mut fork = support::fork_at(&url, fork_block(), ChainId(1), alloy_provider.clone()).await;

    assert_eq!(
        fork.block_number(),
        fork_block(),
        "fork must be pinned to fork_block()"
    );

    // ── shared constants ─────────────────────────────────────────────────────

    let usdc = asset(1, USDC_ADDR);
    let weth = asset(1, WETH_ADDR);
    let sender = Address::repeat_byte(0xBE);

    // Per-chain config: WETH as wrapped native, V2 router set.
    // `Routers` is #[non_exhaustive] so struct literals are forbidden outside
    // the crate; start from Default and set only the field we need.
    let mut routers = Routers::default();
    routers.v2 = Some(ROUTER);
    let cfg = ChainConfig::new(ChainId(1), weth).with_routers(routers);

    let opts = exec_opts(sender);

    // Fetch the pool once; each direction reads from the same pinned state.
    let pool = fetch_v2_pool(fork.provider()).await;
    let exe = as_executable(pool.as_ref()).expect("UniswapV2Pool must be Executable");

    // ── Direction 1: Exact-in ERC-20 (USDC → WETH) ──────────────────────────
    {
        let snap = fork.snapshot();

        let amt = U256::from(1_000_000_000u64); // 1 000 USDC (6 dec)
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

        // Fund USDC (2× the swap amount), then approve exactly what the library requests.
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, U256::from(2_000_000_000u64));
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

        let target = U256::from(100_000_000_000_000_000u128); // 0.1 WETH
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

        // Fund USDC generously (100× quoted input), then approve exactly what the library requests.
        let fund_usdc = quoted_in.raw * U256::from(100u64);
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, fund_usdc);
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

        let eth_in = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
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

        let amt = U256::from(1_000_000_000u64); // 1 000 USDC
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

        // Fund USDC, then approve exactly what the library requests.
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, U256::from(2_000_000_000u64));
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
}
