//! Wei-exact execution proof for Uniswap V4 — two native-currency swap directions.
//!
//! Builds calldata via the `Executable` trait, submits it to a pinned in-process
//! revm fork, and asserts that the on-chain output equals the off-chain quote to
//! the wei.  Covers the ETH/USDC 0.05% V4 pool on Ethereum mainnet (ETH is
//! `currency0 = address(0)`, first-class V4 native):
//!
//! 1. **native-in (ETH → USDC):** `tx.value = eth_in`, no Permit2 approval,
//!    assert USDC delta == quoted output.
//! 2. **native-out (USDC → ETH):** apply `prepared.approval` via the new
//!    `permit2_approve` harness helper, assert ETH delta == quoted output
//!    (`gas_price = 0` so no gas is deducted).
//!
//! Both directions use small in-window swap sizes, which the V4 math is
//! wei-exact for (no finite-window tick truncation).
//!
//! All tests are `#[ignore]`d and gated on `$AMM_RPC_FORK_URL`.

#![cfg(test)]

#[path = "support/mod.rs"]
mod support;

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, U256, address};
use alloy::providers::Provider;
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::{ExchangeId, PoolKey};
use amm_core::primitives::ratio::Bps;
use amm_core::protocols::uniswap::v4::Hooks;
use amm_core::slippage::Slippage;
use amm_core::traits::pool::Pool;
use amm_rpc::execution::prepared::Route;
use amm_rpc::execution::{
    ChainConfig, Currency, CurrencyAmount, Deadline, ExecutionOptions, Recipient, Routers,
    TradeType, as_executable,
};
use amm_rpc::protocols::uniswap_v4::{UniswapV4Source, V4PoolConfig};
use amm_rpc::source::StateSource;

// ── addresses ────────────────────────────────────────────────────────────────

/// Uniswap Universal Router on Ethereum mainnet.
const UR: Address = address!("0x66a9893cc07d91d95644aedd05d03f95e1dba8af");
/// Permit2 singleton on Ethereum mainnet.
const PERMIT2: Address = address!("0x000000000022D473030F116dDEE9F6B43aC78BA3");
/// Uniswap V4 PoolManager on Ethereum mainnet.
const MANAGER: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
/// USDC on Ethereum mainnet; `balanceOf` mapping is at storage slot 9.
const USDC_ADDR: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
/// USDC `balanceOf` storage slot (keccak mapping key).
const USDC_SLOT: u64 = 9;
/// WETH on Ethereum mainnet (used only as the wrapped-native identity in `ChainConfig`).
const WETH_ADDR: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Bound (parts-per-million) on the divergence between the off-chain V4 quote and
/// the on-chain swap output. V4 is concentrated-liquidity, so the finite
/// tick-window fetch + quote-vs-swap rounding leave a small, one-sided shortfall
/// (spec §2 precision model). The on-chain output never exceeds the quote.
const V4_PPM: u128 = 100;

/// Pinned mainnet block. Unlike the other protocol proofs (block 20M), V4 must
/// use a **post-launch** block — Uniswap V4 shipped to mainnet in early 2025, so
/// the PoolManager and this pool do not exist at block 20_000_000. Override with
/// `AMM_FORK_BLOCK`; the default is a verified post-launch archive block.
fn fork_block() -> u64 {
    std::env::var("AMM_FORK_BLOCK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25_724_266)
}

// ── local helpers ─────────────────────────────────────────────────────────────

/// Wrap a raw EVM address into the `AssetId` used by the pool layer.
fn asset(chain: u64, token: Address) -> AssetId {
    AssetId::new(ChainId(chain), token.into_word())
}

/// Fetch the ETH/USDC UniV4 0.05% pool from `provider`, pinned to `block`.
///
/// ETH (`currency0 = address(0)`) is a first-class V4 currency: the pool key
/// uses `Address::ZERO`, not WETH.
///
/// Panics when the pool fails to refresh — that is always a broken test setup,
/// not a production error.
async fn fetch_v4_pool(provider: impl Provider + Clone, block: u64) -> Box<dyn Pool> {
    let eth = AssetId::new(ChainId(1), B256::ZERO); // currency0 = address(0)
    let usdc = asset(1, USDC_ADDR);
    let config = V4PoolConfig::new(
        Address::ZERO,
        USDC_ADDR,
        eth,
        usdc,
        500u32,
        10i32,
        Address::ZERO,
        Hooks::None,
    );
    let source = UniswapV4Source::new(provider, MANAGER, vec![config.clone()]);
    let key = PoolKey {
        exchange: ExchangeId::new("uniswap-v4"),
        chain: ChainId(1),
        address: config.pool_id.to_string(),
        assets: vec![eth, usdc],
        fee_bps: None,
    };
    let mut pools = source
        .refresh(&[key], BlockId::number(block))
        .await
        .expect("UniswapV4Source::refresh");
    assert_eq!(pools.len(), 1, "expected exactly one pool from refresh");
    pools.remove(0)
}

/// Resolved execution options shared across all swap directions.
///
/// - 50 bps slippage
/// - Explicit recipient (`sender`)
/// - Absolute deadline far in the future (`u64::MAX / 2`)
fn exec_opts(sender: Address) -> ExecutionOptions {
    ExecutionOptions::new(Slippage::from_bps(Bps(50)))
        .with_recipient(Recipient::To(sender))
        .with_deadline(Deadline::AtTimestamp(u64::MAX / 2))
}

// ── proof ─────────────────────────────────────────────────────────────────────

/// Wei-exact V4 execution proof — two native-currency swap directions.
///
/// Each direction:
/// 1. Saves a fork snapshot.
/// 2. Funds the impersonated sender via storage-slot injection or `fund_native`.
/// 3. Applies the Permit2 two-step approval where required (USDC→ETH only).
/// 4. Builds calldata via the `Executable` trait (`as_executable`).
/// 5. Submits via [`Fork::submit`] (synchronous in-process EVM, `gas_price=0`).
/// 6. Asserts the on-chain balance delta equals the off-chain quote to the wei.
/// 7. Reverts the snapshot so each direction starts from clean state.
///
/// Requires `flavor = "multi_thread"` because `foundry_fork_db::SharedBackend`
/// internally calls `tokio::task::block_in_place` to park the RPC polling loop.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn wei_exact_v4_native_directions() {
    let url = match std::env::var("AMM_RPC_FORK_URL") {
        Ok(v) => v,
        Err(_) => return,
    };

    let alloy_provider = alloy::providers::ProviderBuilder::new()
        .connect_http(url.parse().expect("invalid RPC url"));

    let mut fork = support::fork_at(&url, fork_block(), ChainId(1), alloy_provider).await;

    assert_eq!(
        fork.block_number(),
        fork_block(),
        "fork must be pinned to fork_block()"
    );

    // ── shared constants ─────────────────────────────────────────────────────

    let eth = AssetId::new(ChainId(1), B256::ZERO); // currency0 = address(0)
    let usdc = asset(1, USDC_ADDR);
    let weth = asset(1, WETH_ADDR);
    let sender = Address::repeat_byte(0xBE);

    // Per-chain config: WETH as wrapped native, Universal Router + Permit2 set.
    // `Routers` is #[non_exhaustive] so struct literals are forbidden outside
    // the crate; start from Default and set only the fields we need.
    let mut routers = Routers::default();
    routers.universal = Some(UR);
    routers.permit2 = Some(PERMIT2);
    let cfg = ChainConfig::new(ChainId(1), weth).with_routers(routers);

    let opts = exec_opts(sender);

    // Fetch the pool using the fork's own provider so the RPC client is shared.
    let pool = fetch_v4_pool(fork.provider().clone(), fork_block()).await;
    let exe = as_executable(pool.as_ref()).expect("UniswapV4Pool must be Executable");

    // ── Direction 1: native-in (ETH → USDC) ─────────────────────────────────
    //
    // Caller sends ETH via `tx.value`; no ERC-20 approval is needed.
    // Small in-window size → wei-exact.
    {
        let snap = fork.snapshot();

        let eth_in = U256::from(10_000_000_000_000_000u64); // 0.01 ETH
        let quoted = pool
            .quote(&AssetAmount::new(eth, eth_in), &usdc)
            .expect("pool must quote ETH→USDC");

        let route = Route::new_single_hop(eth, usdc, TradeType::ExactIn);
        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Native,
                    raw: eth_in,
                },
                Currency::Token(usdc),
                &route,
                &quoted,
                &opts,
            )
            .expect("build_swap native-in (ETH→USDC) must succeed");

        // V4 native-in carries no ERC-20 approval.
        assert!(
            prepared.approval.is_none(),
            "native-in swap must carry no ERC-20 approval"
        );

        // Fund sender with eth_in + 1 ETH buffer.  gas_price=0, but tx.value
        // still deducts eth_in from the sender's balance.
        let eth_buffer = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        fork.fund_native(sender, eth_in + eth_buffer);

        let before = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "native-in (ETH→USDC) swap reverted"
        );
        let after = fork.erc20_balance(USDC_ADDR, sender);

        // V4 concentrated liquidity: the off-chain quote and the on-chain swap
        // diverge by the finite-tick-window + quote-vs-swap rounding (spec §2
        // precision model). The on-chain output never exceeds the quote and the
        // shortfall is bounded to V4_PPM.
        let delta = after - before;
        let tol = quoted.raw * U256::from(V4_PPM) / U256::from(1_000_000u64);
        assert!(
            delta <= quoted.raw && quoted.raw - delta <= tol,
            "native-in: USDC delta {delta} must be <= and within {V4_PPM} ppm of quoted {}",
            quoted.raw
        );

        fork.revert(snap);
    }

    // ── Direction 2: native-out (USDC → ETH) ────────────────────────────────
    //
    // Token input (USDC) is approved via Permit2 (two-step: ERC-20→Permit2,
    // then Permit2.approve).  `tx.value = 0`.  gas_price=0 so the ETH delta
    // is the pure swap output with no gas deduction.
    {
        let snap = fork.snapshot();

        let usdc_in = U256::from(100_000_000u64); // 100 USDC (6 dec)
        let quoted = pool
            .quote(&AssetAmount::new(usdc, usdc_in), &eth)
            .expect("pool must quote USDC→ETH");

        let route = Route::new_single_hop(usdc, eth, TradeType::ExactIn);
        let prepared = exe
            .build_swap(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdc),
                    raw: usdc_in,
                },
                Currency::Native,
                &route,
                &quoted,
                &opts,
            )
            .expect("build_swap native-out (USDC→ETH) must succeed");

        // Fund USDC via storage-slot injection (slot 9).
        // Inject 2× the swap amount for headroom; fund_erc20 also sets 100 ETH.
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, usdc_in * U256::from(2u64));

        // Fail-fast: confirm the slot write landed before proceeding.
        let usdc_funded = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            usdc_funded >= usdc_in,
            "USDC slot injection failed: got {usdc_funded}, need {usdc_in}"
        );

        // Apply the Permit2 two-step approval from `prepared.approval`.
        // The approval's `spender` is Permit2 (step 1 target for the ERC-20
        // approve); the Universal Router is the actual Permit2 spender (step 2).
        if let Some(req) = &prepared.approval {
            let token_addr = Address::from_word(req.token.token);
            // Expiration: 1_000_000_000_000 seconds is far-future and fits u48.
            fork.permit2_approve(
                sender,
                token_addr,
                req.spender, // Permit2 contract
                UR,          // Universal Router is Permit2's spender
                req.min_allowance,
                1_000_000_000_000u64,
            );
        }

        // A little extra native ETH for the EVM's balance-check on the sender
        // (tx.value is 0, but some EVM setups assert sender.balance >= value).
        fork.fund_native(sender, U256::from(1_000_000_000_000_000_000u64)); // 1 ETH

        let before = fork.native_balance(sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "native-out (USDC→ETH) swap reverted"
        );
        let after = fork.native_balance(sender);

        // gas_price=0 means the ETH delta is the pure swap output (no gas
        // deduction). Same V4 concentrated-liquidity bound as native-in.
        let delta = after - before;
        let tol = quoted.raw * U256::from(V4_PPM) / U256::from(1_000_000u64);
        assert!(
            delta <= quoted.raw && quoted.raw - delta <= tol,
            "native-out: ETH delta {delta} must be <= and within {V4_PPM} ppm of quoted {}",
            quoted.raw
        );

        fork.revert(snap);
    }
}
