//! Wei-exact execution proof for Aerodrome (Solidly) on Base — four swap directions.
//!
//! Builds calldata via the `Executable` trait, submits it to a pinned
//! in-process revm fork, and asserts that the on-chain output equals the
//! off-chain quote to the wei.  Covers:
//!
//! 1. Volatile exact-in ERC-20 (USDC → WETH)
//! 2. Volatile native-in (ETH → USDC)
//! 3. Volatile native-out (USDC → ETH)
//! 4. Stable exact-in ERC-20 (USDC → USDbC)
//!
//! Solidly vAMM/sAMM have NO tick window — the off-chain quote matches
//! on-chain exactly at any size.  No bounded-ppm block is needed.
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
use amm_core::protocols::aerodrome::BASE_POOL_FACTORY;
use amm_core::slippage::Slippage;
use amm_core::traits::pool::Pool;
use amm_rpc::execution::{
    ChainConfig, Currency, CurrencyAmount, Deadline, ExecutionOptions, Recipient, Routers,
    as_executable,
};
use amm_rpc::source::StateSource;

// ── addresses ────────────────────────────────────────────────────────────────

/// Aerodrome (Solidly) Router on Base.
const ROUTER: Address = address!("0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");
/// WETH on Base (wrapped native).
const WETH_ADDR: Address = address!("0x4200000000000000000000000000000000000006");
/// USDC on Base (native Circle); `balanceOf` mapping is at storage slot 9.
const USDC_ADDR: Address = address!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
/// USDbC on Base (Bridged USDC).
const USDBC_ADDR: Address = address!("0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA");

/// Aerodrome volatile WETH/USDC pool on Base (token0 = WETH, token1 = USDC).
const VOLATILE_POOL_ADDR: &str = "0xcDAC0d6c6C59727a65F871236188350531885C43";
/// Aerodrome stable USDC/USDbC pool on Base (token0 = USDC, token1 = USDbC).
const STABLE_POOL_ADDR: &str = "0x27a8Afa3Bd49406e48a074350fB7b2020c43B2bD";

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

/// Fetch both Aerodrome pools (volatile WETH/USDC and stable USDC/USDbC)
/// from `provider`, pinned to [`fork_block`].
///
/// Returns `(volatile_pool, stable_pool)`.
///
/// Panics when either pool fails to refresh — that is always a broken test
/// setup, not a production error.
async fn fetch_aerodrome_pools(provider: &impl Provider) -> (Box<dyn Pool>, Box<dyn Pool>) {
    let source = amm_rpc::protocols::aerodrome::AerodromeSource::new(provider, BASE_POOL_FACTORY);

    let weth = asset(8453, WETH_ADDR);
    let usdc = asset(8453, USDC_ADDR);
    let usdbc = asset(8453, USDBC_ADDR);

    // Volatile pool: token0 = WETH (0x4200…), token1 = USDC (0x8335…).
    // Address-sorted: WETH < USDC so [weth, usdc].
    let volatile_key = PoolKey {
        exchange: ExchangeId::new("aerodrome"),
        chain: ChainId(8453),
        address: VOLATILE_POOL_ADDR.to_string(),
        assets: vec![weth, usdc],
        fee_bps: None,
    };

    // Stable pool: token0 = USDC (0x8335…), token1 = USDbC (0xd9aA…).
    // Address-sorted: USDC < USDbC so [usdc, usdbc].
    let stable_key = PoolKey {
        exchange: ExchangeId::new("aerodrome"),
        chain: ChainId(8453),
        address: STABLE_POOL_ADDR.to_string(),
        assets: vec![usdc, usdbc],
        fee_bps: None,
    };

    let mut pools = source
        .refresh(&[volatile_key, stable_key], BlockId::number(fork_block()))
        .await
        .expect("AerodromeSource::refresh");

    assert_eq!(pools.len(), 2, "expected exactly two pools from refresh");
    let stable_pool = pools.remove(1);
    let volatile_pool = pools.remove(0);
    (volatile_pool, stable_pool)
}

/// Resolved execution options shared across all swap directions.
///
/// - 50 bps slippage
/// - Explicit recipient (`sender`)
/// - Absolute deadline (`u64::MAX / 2`) — no `resolve` call needed.
fn exec_opts(sender: Address) -> ExecutionOptions {
    ExecutionOptions::new(Slippage::from_bps(Bps(50)))
        .with_recipient(Recipient::To(sender))
        .with_deadline(Deadline::AtTimestamp(u64::MAX / 2))
}

// ── proof ─────────────────────────────────────────────────────────────────────

/// Wei-exact Aerodrome-Solidly execution proof — four swap directions.
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
async fn wei_exact_aerodrome_all_directions() {
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

    let weth = asset(8453, WETH_ADDR);
    let usdc = asset(8453, USDC_ADDR);
    let usdbc = asset(8453, USDBC_ADDR);
    let sender = Address::repeat_byte(0xBE);

    // Per-chain config: WETH as wrapped native, Aerodrome router and factory.
    // `Routers` is #[non_exhaustive] so struct literals are forbidden outside
    // the crate; start from Default and set only the fields we need.
    let mut routers = Routers::default();
    routers.aerodrome = Some(ROUTER);
    routers.aerodrome_factory = Some(BASE_POOL_FACTORY);
    let cfg = ChainConfig::new(ChainId(8453), weth).with_routers(routers);

    let opts = exec_opts(sender);

    // Fetch both pools once; each direction reads from the same pinned state.
    let (volatile_pool, stable_pool) = fetch_aerodrome_pools(fork.provider()).await;
    let volatile_exe =
        as_executable(volatile_pool.as_ref()).expect("AerodromeVolatilePool must be Executable");
    let stable_exe =
        as_executable(stable_pool.as_ref()).expect("AerodromeStablePool must be Executable");

    // ── Direction 1: Volatile exact-in ERC-20 (USDC → WETH) ─────────────────
    {
        let snap = fork.snapshot();

        let amt = U256::from(1_000_000_000u64); // 1000 USDC (6 dec)
        let quoted = volatile_pool
            .quote(&AssetAmount::new(usdc, amt), &weth)
            .expect("volatile pool must quote USDC→WETH");

        let prepared = volatile_exe
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
            .expect("build_swap volatile ERC-20 exact-in must succeed");

        // Fund USDC (2× the swap amount).
        let fund_amount = U256::from(2_000_000_000u64);
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
            "volatile exact-in ERC-20 (USDC→WETH) swap reverted"
        );
        let after = fork.erc20_balance(WETH_ADDR, sender);

        let delta = after - before;
        assert_eq!(
            delta, quoted.raw,
            "volatile exact-in ERC-20: WETH delta must equal quoted output"
        );

        fork.revert(snap);
    }

    // ── Direction 2: Volatile native-in (ETH → USDC) ────────────────────────
    {
        let snap = fork.snapshot();

        let eth_in = U256::from(1_000_000_000_000_000u128); // 0.001 ETH
        // Pool math treats the ETH side as WETH.
        let quoted = volatile_pool
            .quote(&AssetAmount::new(weth, eth_in), &usdc)
            .expect("volatile pool must quote WETH→USDC");

        let prepared = volatile_exe
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
            .expect("build_swap volatile native-in must succeed");

        // No ERC-20 approval needed for native-in.
        assert!(
            prepared.approval.is_none(),
            "volatile native-in swap must carry no ERC-20 approval"
        );

        // Fund sender with eth_in + 1 ETH buffer.  gas_price=0 so no gas is
        // deducted, but tx.value still consumes the exact eth_in amount.
        let eth_buffer = U256::from(1_000_000_000_000_000_000u128);
        fork.fund_native(sender, eth_in + eth_buffer);

        let before = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "volatile native-in (ETH→USDC) swap reverted"
        );
        let after = fork.erc20_balance(USDC_ADDR, sender);

        let delta = after - before;
        assert_eq!(
            delta, quoted.raw,
            "volatile native-in: USDC delta must equal quoted output"
        );

        fork.revert(snap);
    }

    // ── Direction 3: Volatile native-out (USDC → ETH) ───────────────────────
    {
        let snap = fork.snapshot();

        let amt = U256::from(1_000_000_000u64); // 1000 USDC (6 dec)
        let quoted = volatile_pool
            .quote(&AssetAmount::new(usdc, amt), &weth)
            .expect("volatile pool must quote USDC→WETH");

        let prepared = volatile_exe
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
            .expect("build_swap volatile native-out must succeed");

        // Fund USDC, then perform the fail-fast slot check.
        let fund_amount = U256::from(2_000_000_000u64);
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
            "volatile native-out (USDC→ETH) swap reverted"
        );
        let after = fork.native_balance(sender);

        // gas_price=0 so ETH delta == unwrapped WETH output with no gas deduction.
        let delta = after - before;
        assert_eq!(
            delta, quoted.raw,
            "volatile native-out: ETH delta must equal quoted WETH output"
        );

        fork.revert(snap);
    }

    // ── Direction 4: Stable exact-in ERC-20 (USDC → USDbC) ─────────────────
    {
        let snap = fork.snapshot();

        let amt = U256::from(1_000_000_000u64); // 1000 USDC (6 dec)
        let quoted = stable_pool
            .quote(&AssetAmount::new(usdc, amt), &usdbc)
            .expect("stable pool must quote USDC→USDbC");

        let prepared = stable_exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdc),
                    raw: amt,
                },
                Currency::Token(usdbc),
                &quoted,
                &opts,
            )
            .expect("build_swap stable ERC-20 exact-in must succeed");

        // Fund USDC (2× the swap amount) and perform the fail-fast slot check.
        let fund_amount = U256::from(2_000_000_000u64);
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

        let before = fork.erc20_balance(USDBC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "stable exact-in ERC-20 (USDC→USDbC) swap reverted"
        );
        let after = fork.erc20_balance(USDBC_ADDR, sender);

        let delta = after - before;
        assert_eq!(
            delta, quoted.raw,
            "stable exact-in ERC-20: USDbC delta must equal quoted output"
        );

        fork.revert(snap);
    }
}
