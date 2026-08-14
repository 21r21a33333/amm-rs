//! Wei-exact execution proof for Curve pools on Ethereum mainnet.
//!
//! Builds calldata via the `Executable` trait, submits it to a pinned
//! in-process revm fork, and asserts that the on-chain output equals the
//! off-chain quote within the documented tolerance. Covers:
//!
//! **StableI128 — Curve 3pool (`0xbEbc44...`)**
//! 1. DAI → USDC (`i=0, j=1`)
//! 2. USDC → USDT (`i=1, j=2`)
//!
//! Curve StableSwap has no tick window — the off-chain quote is exact to
//! within the deployed `get_dy` rounding of -1 wei.
//!
//! **CryptoU256UseEth — tricrypto2 (`0xD51a44...`)**
//! 3. USDT → WETH (`i=0, j=2`)
//!
//! tricrypto2's deployed CryptoSwap solver matches the curve-math quote to the
//! wei (observed shortfall 0), so this direction is asserted wei-exact.
//!
//! **StableI128Ng — StableSwapNG (`0x4DEcE6...`)**
//! 4. USDC → crvUSD (`i=0, j=1`)
//!
//! StableSwap-NG shares the same `exchange(int128,…)` calldata as classic
//! StableI128; tolerance is ≤1-wei (same rounding as 3pool).
//!
//! **CryptoU256Receiver — TwoCryptoNG (`0x592878...`)**
//! 5. WETH → TC_NG_TOKEN (`i=0, j=1`)
//!
//! Twocrypto-NG's `exchange(…, receiver)` ABI (no `use_eth`). Asserted
//! wei-exact (observed shortfall 0 on mainnet block 20_000_000).
//!
//! All tests are `#[ignore]`d and gated on `$AMM_RPC_FORK_URL`.

#![cfg(test)]
#![cfg(feature = "curve")]

#[path = "support/mod.rs"]
#[allow(dead_code)]
mod support;

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256, address};
use alloy::providers::Provider;
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::PoolKey;
use amm_core::primitives::ratio::Bps;
use amm_core::slippage::Slippage;
use amm_core::traits::pool::Pool;
use amm_rpc::execution::{
    ChainConfig, Currency, CurrencyAmount, Deadline, ExecutionOptions, Recipient, as_executable,
};
use amm_rpc::source::StateSource;

// ── addresses ────────────────────────────────────────────────────────────────

/// Curve 3pool on Ethereum mainnet.
const POOL_ADDR: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

/// tricrypto2 (TriCryptoV1) on Ethereum mainnet.
const TRICRYPTO2_ADDR: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");

/// StableSwapNG pool on Ethereum mainnet (USDC/crvUSD).
const STABLE_NG_ADDR: Address = address!("4DEcE678ceceb27446b35C672dC7d61F30bAD69E");

/// TwoCryptoNG pool on Ethereum mainnet (WETH/TC_NG_TOKEN).
const TWOCRYPTO_NG_ADDR: Address = address!("592878b920101946fb5915ab97961bc546f211cc");

/// DAI on Ethereum mainnet; `balanceOf` mapping is at storage slot 2.
const DAI_ADDR: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
/// USDC on Ethereum mainnet; `balanceOf` mapping is at storage slot 9.
const USDC_ADDR: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
/// USDT on Ethereum mainnet; `balanceOf` mapping is at storage slot 2.
const USDT_ADDR: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
/// WBTC on Ethereum mainnet.
const WBTC_ADDR: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
/// WETH on Ethereum mainnet (also required by ChainConfig).
const WETH_ADDR: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// crvUSD on Ethereum mainnet (coin[1] of the StableSwapNG pool above).
const CRVUSD_ADDR: Address = address!("f939E0A03FB07F59A73314E73794Be0E57ac1b4E");

/// TC_NG_TOKEN: coin[1] of the TwoCryptoNG pool above.
const TC_NG_TOKEN_ADDR: Address = address!("1cfa5641c01406aB8AC350dEd7d735ec41298372");

/// DAI `balanceOf` storage slot (ERC-20 mapping base slot).
const DAI_SLOT: u64 = 2;
/// USDC `balanceOf` storage slot.
const USDC_SLOT: u64 = 9;
/// USDT `balanceOf` storage slot.
const USDT_SLOT: u64 = 2;
/// WETH `balanceOf` storage slot.
const WETH_SLOT: u64 = 3;
/// WBTC `balanceOf` storage slot.
const WBTC_SLOT: u64 = 0;

/// Pinned mainnet block. Override with `AMM_FORK_BLOCK` to run against a
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

/// Fetch the Curve 3pool from `provider` pinned to [`fork_block`].
///
/// Panics when the pool fails to refresh — that is always a broken test setup,
/// not a production error.
async fn fetch_3pool(provider: &impl Provider) -> Box<dyn Pool> {
    use amm_rpc::protocols::curve::{CurvePoolConfig, CurveSource};
    use curve_adapter::CurveVariant;

    let dai = asset(1, DAI_ADDR);
    let usdc = asset(1, USDC_ADDR);
    let usdt = asset(1, USDT_ADDR);

    let config = CurvePoolConfig {
        address: POOL_ADDR,
        variant: CurveVariant::StableSwapV1,
        coins: vec![dai, usdc, usdt],
        decimals: vec![18, 6, 6],
        base_pool: None,
        eth_variant: None,
    };

    let source = CurveSource::new(provider, vec![config]);
    let key = PoolKey {
        exchange: amm_core::primitives::pool::ExchangeId::new("curve"),
        chain: ChainId(1),
        address: POOL_ADDR.to_string(),
        assets: vec![dai, usdc, usdt],
        fee_bps: None,
    };

    let mut pools = source
        .refresh(&[key], BlockId::number(fork_block()))
        .await
        .expect("CurveSource::refresh must succeed");

    assert_eq!(pools.len(), 1, "expected exactly one pool from refresh");
    pools.remove(0)
}

/// Fetch the StableSwapNG pool (USDC/crvUSD) from `provider` pinned to [`fork_block`].
///
/// Panics when the pool fails to refresh — that is always a broken test setup,
/// not a production error.
async fn fetch_stable_ng(provider: &impl Provider) -> Box<dyn Pool> {
    use amm_rpc::protocols::curve::{CurvePoolConfig, CurveSource};
    use curve_adapter::CurveVariant;

    let usdc = asset(1, USDC_ADDR);
    let crvusd = asset(1, CRVUSD_ADDR);

    let config = CurvePoolConfig {
        address: STABLE_NG_ADDR,
        variant: CurveVariant::StableSwapNG,
        coins: vec![usdc, crvusd],
        decimals: vec![6, 18],
        base_pool: None,
        eth_variant: None,
    };

    let source = CurveSource::new(provider, vec![config]);
    let key = PoolKey {
        exchange: amm_core::primitives::pool::ExchangeId::new("curve"),
        chain: ChainId(1),
        address: STABLE_NG_ADDR.to_string(),
        assets: vec![usdc, crvusd],
        fee_bps: None,
    };

    let mut pools = source
        .refresh(&[key], BlockId::number(fork_block()))
        .await
        .expect("CurveSource::refresh must succeed for StableSwapNG");

    assert_eq!(pools.len(), 1, "expected exactly one StableSwapNG pool");
    pools.remove(0)
}

/// Fetch the TwoCryptoNG pool (WETH/TC_NG_TOKEN) from `provider` pinned to [`fork_block`].
///
/// Panics when the pool fails to refresh — that is always a broken test setup,
/// not a production error.
async fn fetch_twocrypto_ng(provider: &impl Provider) -> Box<dyn Pool> {
    use amm_rpc::protocols::curve::{CurvePoolConfig, CurveSource};
    use curve_adapter::CurveVariant;

    let weth = asset(1, WETH_ADDR);
    let tc_ng_token = asset(1, TC_NG_TOKEN_ADDR);

    let config = CurvePoolConfig {
        address: TWOCRYPTO_NG_ADDR,
        variant: CurveVariant::TwoCryptoNG,
        coins: vec![weth, tc_ng_token],
        decimals: vec![18, 18],
        base_pool: None,
        eth_variant: None,
    };

    let source = CurveSource::new(provider, vec![config]);
    let key = PoolKey {
        exchange: amm_core::primitives::pool::ExchangeId::new("curve"),
        chain: ChainId(1),
        address: TWOCRYPTO_NG_ADDR.to_string(),
        assets: vec![weth, tc_ng_token],
        fee_bps: None,
    };

    let mut pools = source
        .refresh(&[key], BlockId::number(fork_block()))
        .await
        .expect("CurveSource::refresh must succeed for TwoCryptoNG");

    assert_eq!(pools.len(), 1, "expected exactly one TwoCryptoNG pool");
    pools.remove(0)
}

/// Fetch the tricrypto2 pool from `provider` pinned to [`fork_block`].
///
/// Panics when the pool fails to refresh — that is always a broken test setup,
/// not a production error.
async fn fetch_tricrypto2(provider: &impl Provider) -> Box<dyn Pool> {
    use amm_rpc::protocols::curve::{CurvePoolConfig, CurveSource};
    use curve_adapter::CurveVariant;

    let usdt = asset(1, USDT_ADDR);
    let wbtc = asset(1, WBTC_ADDR);
    let weth = asset(1, WETH_ADDR);

    let config = CurvePoolConfig {
        address: TRICRYPTO2_ADDR,
        variant: CurveVariant::TriCryptoV1,
        coins: vec![usdt, wbtc, weth],
        decimals: vec![6, 8, 18],
        base_pool: None,
        eth_variant: None,
    };

    let source = CurveSource::new(provider, vec![config]);
    let key = PoolKey {
        exchange: amm_core::primitives::pool::ExchangeId::new("curve"),
        chain: ChainId(1),
        address: TRICRYPTO2_ADDR.to_string(),
        assets: vec![usdt, wbtc, weth],
        fee_bps: None,
    };

    let mut pools = source
        .refresh(&[key], BlockId::number(fork_block()))
        .await
        .expect("CurveSource::refresh must succeed for tricrypto2");

    assert_eq!(pools.len(), 1, "expected exactly one tricrypto2 pool");
    pools.remove(0)
}

/// Resolved execution options shared across all Curve swap directions.
///
/// - 50 bps slippage
/// - Explicit recipient (`sender`)
/// - Absolute far-future deadline (Curve's classic `exchange` ignores it at
///   the ABI level, but we still enforce resolution in `resolve_swap`).
fn exec_opts(sender: Address) -> ExecutionOptions {
    ExecutionOptions::new(Slippage::from_bps(Bps(50)))
        .with_recipient(Recipient::To(sender))
        .with_deadline(Deadline::AtTimestamp(u64::MAX / 2))
}

// ── proof: StableI128 — Curve 3pool ──────────────────────────────────────────

/// Wei-exact Curve 3pool execution proof — two swap directions.
///
/// Each direction:
/// 1. Saves a fork snapshot.
/// 2. Funds the impersonated sender via storage-slot injection.
/// 3. Builds calldata via the `Executable` trait (`as_executable`).
/// 4. Applies the `prepared.approval` exactly (approve the POOL, which pulls
///    via `transferFrom`).
/// 5. Submits via [`Fork::submit`] (synchronous in-process EVM).
/// 6. Asserts the on-chain balance delta equals the off-chain quote to the wei.
/// 7. Reverts the snapshot so each direction starts from clean state.
///
/// Requires `flavor = "multi_thread"` because `foundry_fork_db::SharedBackend`
/// internally calls `tokio::task::block_in_place`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn wei_exact_curve_3pool_stable_i128() {
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

    let dai = asset(1, DAI_ADDR);
    let usdc = asset(1, USDC_ADDR);
    let usdt = asset(1, USDT_ADDR);
    let weth = asset(1, WETH_ADDR);

    let sender = Address::repeat_byte(0xBE);

    // Curve tx.to is the pool itself — no router address needed.
    let cfg = ChainConfig::new(ChainId(1), weth);
    let opts = exec_opts(sender);

    // Fetch 3pool once; each direction reads from the same pinned state.
    let pool = fetch_3pool(fork.provider()).await;
    let exe = as_executable(pool.as_ref()).expect("CurvePool must be Executable");

    // ── Direction 1: DAI → USDC (i=0, j=1) ──────────────────────────────────
    {
        let snap = fork.snapshot();

        // 1 000 DAI (18 dec)
        let amt = U256::from(1_000_000_000_000_000_000_000u128); // 1000 DAI
        let quoted = pool
            .quote(&AssetAmount::new(dai, amt), &usdc)
            .expect("pool must quote DAI→USDC");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(dai),
                    raw: amt,
                },
                Currency::Token(usdc),
                &quoted,
                &opts,
            )
            .expect("build_swap DAI→USDC must succeed");

        // Fund DAI (2× the swap amount) via slot injection. Fail-fast balance
        // readback verifies the slot number is correct for this token.
        fork.fund_erc20(sender, DAI_ADDR, DAI_SLOT, amt * U256::from(2u64));
        let readback = fork.erc20_balance(DAI_ADDR, sender);
        assert!(
            readback >= amt,
            "DAI slot 2 injection failed: readback {readback} < {amt}; wrong slot?"
        );

        // Apply the approval exactly as specified by the library: spender is
        // the pool (Curve pulls via `transferFrom`, not a router).
        if let Some(req) = &prepared.approval {
            let token_addr = Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        let before = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "DAI→USDC exchange reverted"
        );
        let after = fork.erc20_balance(USDC_ADDR, sender);
        let out_delta = after - before;

        // Curve's deployed get_dy rounds down by 1 (`dy = xp[j] - y - 1`); the external
        // curve-math quote omits that -1, so the quote is optimistic by at most 1 wei.
        // The encoder delivers the exact on-chain amount; assert it is within 1 wei and
        // never exceeds the quote.
        let delta = out_delta;
        assert!(
            delta <= quoted.raw && quoted.raw - delta <= U256::from(1u64),
            "DAI→USDC: on-chain USDC delta {delta} must be within 1 wei of (and <=) quoted {}",
            quoted.raw
        );

        fork.revert(snap);
    }

    // ── Direction 2: USDC → USDT (i=1, j=2) ─────────────────────────────────
    {
        let snap = fork.snapshot();

        // 1 000 USDC (6 dec)
        let amt = U256::from(1_000_000_000u64); // 1000 USDC
        let quoted = pool
            .quote(&AssetAmount::new(usdc, amt), &usdt)
            .expect("pool must quote USDC→USDT");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdc),
                    raw: amt,
                },
                Currency::Token(usdt),
                &quoted,
                &opts,
            )
            .expect("build_swap USDC→USDT must succeed");

        // Fund USDC (2× the swap amount) via slot 9.
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, amt * U256::from(2u64));
        let readback = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            readback >= amt,
            "USDC slot 9 injection failed: readback {readback} < {amt}; wrong slot?"
        );

        // Apply the pool approval.
        if let Some(req) = &prepared.approval {
            let token_addr = Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        let before = fork.erc20_balance(USDT_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "USDC→USDT exchange reverted"
        );
        let after = fork.erc20_balance(USDT_ADDR, sender);
        let out_delta = after - before;

        // Curve's deployed get_dy rounds down by 1 (`dy = xp[j] - y - 1`); the external
        // curve-math quote omits that -1, so the quote is optimistic by at most 1 wei.
        // The encoder delivers the exact on-chain amount; assert it is within 1 wei and
        // never exceeds the quote.
        let delta = out_delta;
        assert!(
            delta <= quoted.raw && quoted.raw - delta <= U256::from(1u64),
            "USDC→USDT: on-chain USDT delta {delta} must be within 1 wei of (and <=) quoted {}",
            quoted.raw
        );

        fork.revert(snap);
    }
}

// ── proof: CryptoU256UseEth — tricrypto2 ─────────────────────────────────────

/// Wei-exact tricrypto2 execution proof — USDT → WETH direction.
///
/// 1. Saves a fork snapshot.
/// 2. Funds the impersonated sender with USDT via storage-slot injection.
/// 3. Builds calldata via `as_executable` → `build_swap` (CryptoU256UseEth arm).
/// 4. Applies the approval (pool pulls via `transferFrom`).
/// 5. Submits via [`Fork::submit`].
/// 6. Asserts the WETH balance delta equals the off-chain curve-math quote to
///    the wei (observed shortfall 0 on mainnet block 20_000_000).
///
/// Requires `flavor = "multi_thread"` for `foundry_fork_db::SharedBackend`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn wei_exact_tricrypto2_crypto_u256_use_eth() {
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

    let usdt = asset(1, USDT_ADDR);
    let weth_asset = asset(1, WETH_ADDR);

    let sender = Address::repeat_byte(0xCF);

    let cfg = ChainConfig::new(ChainId(1), weth_asset);
    let opts = exec_opts(sender);

    // Fetch tricrypto2 once.
    let pool = fetch_tricrypto2(fork.provider()).await;
    let exe = as_executable(pool.as_ref()).expect("CurvePool must be Executable");

    // ── Direction: USDT → WETH (i=0, j=2) ───────────────────────────────────
    {
        let snap = fork.snapshot();

        // 1 000 USDT (6 dec)
        let amt = U256::from(1_000_000_000u64); // 1000 USDT
        let quoted = pool
            .quote(&AssetAmount::new(usdt, amt), &weth_asset)
            .expect("pool must quote USDT→WETH");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdt),
                    raw: amt,
                },
                Currency::Token(weth_asset),
                &quoted,
                &opts,
            )
            .expect("build_swap USDT→WETH (CryptoU256UseEth) must succeed");

        // Fund USDT (2× the swap amount) via slot 2. Fail-fast readback
        // verifies the slot is correct for USDT on mainnet.
        fork.fund_erc20(sender, USDT_ADDR, USDT_SLOT, amt * U256::from(2u64));
        let readback = fork.erc20_balance(USDT_ADDR, sender);
        assert!(
            readback >= amt,
            "USDT slot 2 injection failed: readback {readback} < {amt}; wrong slot?"
        );

        // Apply the approval: the pool (tricrypto2) pulls USDT via transferFrom.
        if let Some(req) = &prepared.approval {
            let token_addr = Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        // Confirm WBTC is unused (not coin j here); measure WETH delta only.
        let weth_before = fork.erc20_balance(WETH_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "USDT→WETH exchange on tricrypto2 reverted"
        );
        let weth_after = fork.erc20_balance(WETH_ADDR, sender);
        let delta = weth_after - weth_before;

        // tricrypto2's deployed CryptoSwap solver matches the curve-math quote to
        // the wei (observed shortfall 0), so this direction is asserted wei-exact.
        assert_eq!(
            delta, quoted.raw,
            "USDT→WETH: on-chain WETH delta must equal the quoted output"
        );

        // Unused coins must not have changed balance.
        let wbtc_before = U256::ZERO; // sender started with 0
        let wbtc_after = fork.erc20_balance(WBTC_ADDR, sender);
        assert_eq!(
            wbtc_after, wbtc_before,
            "WBTC balance must be unchanged (only USDT→WETH)"
        );

        fork.revert(snap);
    }
}

// ── proof: StableI128Ng — StableSwapNG ───────────────────────────────────────

/// Execution proof for StableSwapNG USDC → crvUSD — StableI128Ng arm.
///
/// StableSwapNG shares the same `exchange(int128,int128,uint256,uint256)`
/// calldata as classic StableI128. Tolerance is ≤1-wei (same rounding as
/// 3pool's deployed `get_dy`).
///
/// Requires `flavor = "multi_thread"` for `foundry_fork_db::SharedBackend`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn wei_exact_stable_ng_usdc_to_crvusd() {
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

    let usdc = asset(1, USDC_ADDR);
    let crvusd = asset(1, CRVUSD_ADDR);
    let weth_asset = asset(1, WETH_ADDR);

    let sender = Address::repeat_byte(0xD1);

    let cfg = ChainConfig::new(ChainId(1), weth_asset);
    let opts = exec_opts(sender);

    // Fetch StableSwapNG pool once.
    let pool = fetch_stable_ng(fork.provider()).await;
    let exe = as_executable(pool.as_ref()).expect("CurvePool (StableI128Ng) must be Executable");

    // ── Direction: USDC → crvUSD (i=0, j=1) ─────────────────────────────────
    {
        let snap = fork.snapshot();

        // 1 000 USDC (6 dec)
        let amt = U256::from(1_000_000_000u64); // 1000 USDC
        let quoted = pool
            .quote(&AssetAmount::new(usdc, amt), &crvusd)
            .expect("pool must quote USDC→crvUSD");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdc),
                    raw: amt,
                },
                Currency::Token(crvusd),
                &quoted,
                &opts,
            )
            .expect("build_swap USDC→crvUSD (StableI128Ng) must succeed");

        // Fund USDC (2× the swap amount) via slot 9. Fail-fast readback verifies
        // the slot number is correct.
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, amt * U256::from(2u64));
        let readback = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            readback >= amt,
            "USDC slot 9 injection failed: readback {readback} < {amt}; wrong slot?"
        );

        // Apply the pool approval: the StableSwapNG pool pulls via transferFrom.
        if let Some(req) = &prepared.approval {
            let token_addr = Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        let before = fork.erc20_balance(CRVUSD_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "USDC→crvUSD exchange on StableSwapNG reverted"
        );
        let after = fork.erc20_balance(CRVUSD_ADDR, sender);
        let delta = after - before;

        // StableSwap-NG deployed `get_dy` rounds down by 1 (same as 3pool):
        // `delta <= quoted && quoted - delta <= 1`.
        assert!(
            delta <= quoted.raw && quoted.raw - delta <= U256::from(1u64),
            "USDC→crvUSD: on-chain crvUSD delta {delta} must be within 1 wei of (and <=) quoted {}",
            quoted.raw
        );

        fork.revert(snap);
    }
}

// ── proof: CryptoU256UseEth — tricrypto2 native ETH in + out ─────────────────

/// Wei-exact tricrypto2 execution proof — native ETH as the input and output
/// currency, exercising the `use_eth` payable path of the `CryptoU256UseEth`
/// encoder for both directions.
///
/// Two directions, each snapshot/reverted independently:
///
/// **ETH-in (ETH → WBTC):** `Currency::Native` as input carries `tx.value = dx`
/// and no ERC-20 approval.  The pool receives ETH directly (use_eth=true) and
/// delivers WBTC.
///
/// **ETH-out (WBTC → ETH):** Token input (WBTC) with `Currency::Native` as
/// output.  `tx.value = 0`, normal ERC-20 approval for the pool.  The pool
/// unwraps WETH and sends native ETH to the sender.  `gas_price=0` (harness)
/// means the ETH delta is the pure swap output with no gas deduction.
///
/// Both directions are asserted wei-exact: tricrypto2's deployed CryptoSwap
/// solver matches the curve-math quote to the wei (observed shortfall 0 at
/// block 20_000_000).
///
/// Requires `flavor = "multi_thread"` for `foundry_fork_db::SharedBackend`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn wei_exact_curve_tricrypto2_native_eth() {
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

    let wbtc = asset(1, WBTC_ADDR);
    let weth_asset = asset(1, WETH_ADDR);

    let sender = Address::repeat_byte(0xE1);

    let cfg = ChainConfig::new(ChainId(1), weth_asset);
    let opts = exec_opts(sender);

    // Fetch tricrypto2 once; both directions read from the same pinned state.
    let pool = fetch_tricrypto2(fork.provider()).await;
    let exe =
        as_executable(pool.as_ref()).expect("CurvePool (CryptoU256UseEth) must be Executable");

    // ── Direction 1: native-in (ETH → WBTC, i=2, j=1) ───────────────────────
    //
    // Currency::Native as input → use_eth=true, tx.value = dx, no approval.
    // The pool receives ETH and delivers WBTC to the sender.
    {
        let snap = fork.snapshot();

        // 0.5 ETH (18 dec) — a modest in-window size for tricrypto2.
        let eth_in = U256::from(500_000_000_000_000_000u64); // 0.5 ETH
        let quoted = pool
            .quote(&AssetAmount::new(weth_asset, eth_in), &wbtc)
            .expect("pool must quote ETH→WBTC (via WETH slot)");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Native,
                    raw: eth_in,
                },
                Currency::Token(wbtc),
                &quoted,
                &opts,
            )
            .expect("build_swap ETH-in (CryptoU256UseEth) must succeed");

        // Native-in: no ERC-20 approval (pool receives ETH via msg.value).
        assert!(
            prepared.approval.is_none(),
            "ETH-in swap must carry no ERC-20 approval"
        );

        // tx.value must equal the input amount for native-in.
        assert_eq!(prepared.tx.value, eth_in, "ETH-in: tx.value must equal dx");

        // Fund sender with eth_in + 1 ETH buffer. gas_price=0 but tx.value
        // is deducted from the sender's balance by the EVM.
        let eth_buffer = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        fork.fund_native(sender, eth_in + eth_buffer);

        let wbtc_before = fork.erc20_balance(WBTC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "ETH-in (ETH→WBTC) exchange on tricrypto2 reverted"
        );
        let wbtc_after = fork.erc20_balance(WBTC_ADDR, sender);
        let delta = wbtc_after - wbtc_before;

        // tricrypto2's deployed CryptoSwap solver matches the curve-math quote
        // to the wei (observed shortfall 0), so assert wei-exact.
        assert_eq!(
            delta, quoted.raw,
            "ETH-in: on-chain WBTC delta {delta} must equal the quoted output {}",
            quoted.raw
        );

        fork.revert(snap);
    }

    // ── Direction 2: native-out (WBTC → ETH, i=1, j=2) ─────────────────────
    //
    // Token input (WBTC) with Currency::Native as output. use_eth=true so the
    // pool unwraps WETH and delivers native ETH to the sender. tx.value=0.
    // gas_price=0 (harness enforce) → the ETH delta is pure swap output.
    {
        let snap = fork.snapshot();

        // 0.01 WBTC (8 dec) — a modest amount well within the pool's reserves.
        let wbtc_in = U256::from(1_000_000u64); // 0.01 WBTC
        let quoted = pool
            .quote(&AssetAmount::new(wbtc, wbtc_in), &weth_asset)
            .expect("pool must quote WBTC→ETH (via WETH slot)");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(wbtc),
                    raw: wbtc_in,
                },
                Currency::Native,
                &quoted,
                &opts,
            )
            .expect("build_swap ETH-out (CryptoU256UseEth) must succeed");

        // Token input → tx.value must be 0.
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "ETH-out: tx.value must be 0 (pool pulls WBTC via transferFrom)"
        );

        // Fund WBTC (2× the swap amount) via slot 0. Fail-fast readback verifies
        // the slot number is correct for WBTC on mainnet.
        fork.fund_erc20(sender, WBTC_ADDR, WBTC_SLOT, wbtc_in * U256::from(2u64));
        let readback = fork.erc20_balance(WBTC_ADDR, sender);
        assert!(
            readback >= wbtc_in,
            "WBTC slot 0 injection failed: readback {readback} < {wbtc_in}; wrong slot?"
        );

        // Apply the pool approval: the pool (tricrypto2) pulls WBTC via transferFrom.
        if let Some(req) = &prepared.approval {
            let token_addr = Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        // Give sender a small native ETH seed so the EVM's balance check on
        // msg.sender passes (tx.value is 0, but revm asserts sender.balance >= value).
        fork.fund_native(sender, U256::from(1_000_000_000_000_000_000u64)); // 1 ETH

        let eth_before = fork.native_balance(sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "ETH-out (WBTC→ETH) exchange on tricrypto2 reverted"
        );
        let eth_after = fork.native_balance(sender);

        // gas_price=0 → ETH delta equals the raw swap output with no gas deduction.
        // Wei-exact: tricrypto2's on-chain solver matches the curve-math quote.
        let delta = eth_after - eth_before;
        assert_eq!(
            delta, quoted.raw,
            "ETH-out: on-chain ETH delta {delta} must equal the quoted output {}",
            quoted.raw
        );

        fork.revert(snap);
    }
}

// ── proof: CryptoU256Receiver — TwoCryptoNG ──────────────────────────────────

/// Execution proof for TwoCryptoNG WETH → TC_NG_TOKEN — CryptoU256Receiver arm.
///
/// Twocrypto-NG's `exchange(uint256,uint256,uint256,uint256,address)` ABI —
/// the 5th arg is `receiver`, not `use_eth`. Amount ≈ 0.1 WETH (1e17). The
/// deployed solver matches the curve-math quote to the wei (observed shortfall
/// 0), so this direction is asserted wei-exact.
///
/// Requires `flavor = "multi_thread"` for `foundry_fork_db::SharedBackend`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn wei_exact_twocrypto_ng_weth_to_tc_ng_token() {
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

    let weth_asset = asset(1, WETH_ADDR);
    let tc_ng_token = asset(1, TC_NG_TOKEN_ADDR);

    let sender = Address::repeat_byte(0xD2);

    let cfg = ChainConfig::new(ChainId(1), weth_asset);
    let opts = exec_opts(sender);

    // Fetch TwoCryptoNG pool once.
    let pool = fetch_twocrypto_ng(fork.provider()).await;
    let exe =
        as_executable(pool.as_ref()).expect("CurvePool (CryptoU256Receiver) must be Executable");

    // ── Direction: WETH → TC_NG_TOKEN (i=0, j=1) ────────────────────────────
    {
        let snap = fork.snapshot();

        // ≈ 0.1 WETH (18 dec)
        let amt = U256::from(100_000_000_000_000_000u128); // 1e17
        let quoted = pool
            .quote(&AssetAmount::new(weth_asset, amt), &tc_ng_token)
            .expect("pool must quote WETH→TC_NG_TOKEN");

        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(weth_asset),
                    raw: amt,
                },
                Currency::Token(tc_ng_token),
                &quoted,
                &opts,
            )
            .expect("build_swap WETH→TC_NG_TOKEN (CryptoU256Receiver) must succeed");

        // Fund WETH (2× the swap amount) via slot 3. Fail-fast readback verifies
        // the slot number is correct for WETH on mainnet.
        fork.fund_erc20(sender, WETH_ADDR, WETH_SLOT, amt * U256::from(2u64));
        let readback = fork.erc20_balance(WETH_ADDR, sender);
        assert!(
            readback >= amt,
            "WETH slot 3 injection failed: readback {readback} < {amt}; wrong slot?"
        );

        // Apply the pool approval: TwoCryptoNG pulls WETH via transferFrom.
        if let Some(req) = &prepared.approval {
            let token_addr = Address::from_word(req.token.token);
            fork.approve(sender, token_addr, req.spender, req.min_allowance);
        }

        let before = fork.erc20_balance(TC_NG_TOKEN_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "WETH→TC_NG_TOKEN exchange on TwoCryptoNG reverted"
        );
        let after = fork.erc20_balance(TC_NG_TOKEN_ADDR, sender);
        let delta = after - before;

        // Twocrypto-NG's deployed solver matches the curve-math quote to the wei
        // (observed shortfall 0), so this direction is asserted wei-exact.
        assert_eq!(
            delta, quoted.raw,
            "WETH→TC_NG_TOKEN: on-chain delta must equal the quoted output"
        );

        fork.revert(snap);
    }
}
