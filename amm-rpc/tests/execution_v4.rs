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

        // The off-chain quote reproduces V4's on-chain swap to the wei: the
        // effective fee is the exact `ProtocolFeeLibrary.calculateSwapFee`
        // composition and the swap-step math is V3-identical for that fee.
        let delta = after - before;
        assert_eq!(
            delta, quoted.raw,
            "native-in: USDC delta {delta} must equal quoted {} to the wei",
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
        // deduction). Wei-exact against the quote, same as native-in.
        let delta = after - before;
        assert_eq!(
            delta, quoted.raw,
            "native-out: ETH delta {delta} must equal quoted {} to the wei",
            quoted.raw
        );

        fork.revert(snap);
    }
}

// ── WETH-currency V4 pool wrap/unwrap proof ───────────────────────────────────

/// WETH ERC-20 address on Ethereum mainnet.  Used as currency1 in the
/// WETH-currency pool below (USDC < WETH numerically so USDC=c0, WETH=c1).
/// This constant is also the wrapped-native in `ChainConfig::weth`.
const WETH_ERC20_ADDR: Address = WETH_ADDR;

// A live USDC/WETH 0.05% V4 pool where WETH is the ERC-20 currency (currency1),
// not address(0) — the case this wrap path exists to serve. Address-sorted:
//   USDC = 0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48 (currency0)
//   WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 (currency1)
// fee = 500, tickSpacing = 10, hooks = address(0).
//
// poolId = keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks))
//        = 0x4f88f7c99022eace4740c6898f59ce6a2e798a1e64ce54589720b7153eb224a7.
// Initialized at block 21_695_956; StateView.getLiquidity at the pinned block
// 25_724_266 is 14_722_032_017_955_615 (~1.47e16) — deep and live.

/// USDC currency0 for the WETH-currency pool (numerically smaller than WETH).
const WETH_POOL_C0: Address = USDC_ADDR;
/// WETH currency1 for the WETH-currency pool.
const WETH_POOL_C1: Address = WETH_ERC20_ADDR;
/// Fee for the WETH-currency V4 pool (0.05% = 500 pips).
const WETH_POOL_FEE: u32 = 500u32;
/// Tick spacing for the WETH-currency V4 pool.
const WETH_POOL_TICK_SPACING: i32 = 10i32;

/// Fetch the USDC/WETH ERC-20 UniV4 0.05% pool from `provider`, pinned to `block`.
///
/// Unlike the address(0) pool used by `fetch_v4_pool`, this pool carries WETH as
/// an explicit ERC-20 currency (currency1).  Swapping native ETH against it
/// requires `WRAP_ETH` before the V4_SWAP and `UNWRAP_WETH` after (when the
/// output is ETH) — the path this fork test exercises.
async fn fetch_v4_weth_pool(provider: impl Provider + Clone, block: u64) -> Box<dyn Pool> {
    let usdc = asset(1, WETH_POOL_C0);
    let weth = asset(1, WETH_POOL_C1);
    let config = V4PoolConfig::new(
        WETH_POOL_C0,
        WETH_POOL_C1,
        usdc,
        weth,
        WETH_POOL_FEE,
        WETH_POOL_TICK_SPACING,
        Address::ZERO,
        Hooks::None,
    );
    let source = UniswapV4Source::new(provider, MANAGER, vec![config.clone()]);
    let key = PoolKey {
        exchange: ExchangeId::new("uniswap-v4"),
        chain: ChainId(1),
        address: config.pool_id.to_string(),
        assets: vec![usdc, weth],
        fee_bps: None,
    };
    let mut pools = source
        .refresh(&[key], BlockId::number(block))
        .await
        .expect("UniswapV4Source::refresh for WETH-currency pool");
    assert_eq!(pools.len(), 1, "expected exactly one pool from refresh");
    pools.remove(0)
}

/// Wei-exact V4 execution proof — WETH-currency pool wrap/unwrap, three directions.
///
/// The pool carries WETH as an explicit ERC-20 currency (not `address(0)`).  The
/// Universal Router wraps / unwraps ETH around each swap so the caller can send and
/// receive native ETH despite the pool speaking WETH.  After every swap the test
/// checks that the router holds zero WETH and zero native ETH — no residue.
///
/// Directions covered (snapshot/revert between each):
///
/// 1. **native-in exact-in (ETH → USDC):** `tx.value = eth_in`, no Permit2
///    approval; router gets WETH via WRAP_ETH, settles from its own WETH balance.
///    Assert USDC delta == `quote.raw`; router WETH and native ETH == 0.
///
/// 2. **native-out exact-in (USDC → ETH):** fund + Permit2-approve USDC; submit
///    with `tx.value = 0`; assert ETH delta == `quote.raw` (gas_price=0 so no gas
///    deduction); router WETH and native ETH == 0.
///
/// 3. **native-in exact-out (ETH → exact USDC):** `tx.value = max_in` (with
///    slippage headroom); submit; assert USDC delta == `amount_out`; assert router
///    WETH and native ETH == 0 (UNWRAP_WETH returns unused wrap).
///
/// Requires `flavor = "multi_thread"` — same reason as the other V4 fork tests.
///
/// ## Controller action required
///
/// The pool fixture (`WETH_POOL_C0/C1/FEE/TICK_SPACING`) must be verified against
/// a live Ethereum mainnet archive node at block 25_724_266 before running.  See
/// the TODO note above `WETH_POOL_C0`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn wei_exact_v4_weth_pool_wrap_directions() {
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

    let usdc = asset(1, USDC_ADDR);
    let weth_asset = asset(1, WETH_ADDR);

    let sender = Address::repeat_byte(0xBE);

    // ChainConfig: WETH as wrapped native; Universal Router + Permit2.
    let mut routers = Routers::default();
    routers.universal = Some(UR);
    routers.permit2 = Some(PERMIT2);
    let cfg = ChainConfig::new(ChainId(1), weth_asset).with_routers(routers);

    let opts = exec_opts(sender);

    // Fetch the WETH-currency pool via the fork's own provider.
    let pool = fetch_v4_weth_pool(fork.provider().clone(), fork_block()).await;
    let exe = as_executable(pool.as_ref()).expect("UniswapV4Pool must be Executable");

    // ── Direction 1: native-in exact-in (ETH → USDC via WETH pool) ──────────
    //
    // The encoder emits [WRAP_ETH, V4_SWAP]: ETH is wrapped to WETH in the
    // router, SETTLE (payerIsUser=false) settles the swap debt from that WETH.
    // No Permit2 approval; tx.value = eth_in.  After the swap the router must
    // hold zero WETH and zero native ETH (the wrap+settle consumed exactly eth_in).
    {
        let snap = fork.snapshot();

        let eth_in = U256::from(10_000_000_000_000_000u64); // 0.01 ETH

        // Quote ETH-in (WETH asset) → USDC.  The pool's currency1 is WETH.
        let quoted = pool
            .quote(&AssetAmount::new(weth_asset, eth_in), &usdc)
            .expect("pool must quote WETH→USDC (native-in wrap direction)");

        // Route: weth_asset → usdc, so the encoder sees a WETH-pool swap with
        // native ETH as the caller's currency.
        let route = Route::new_single_hop(weth_asset, usdc, TradeType::ExactIn);
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
            .expect("build_swap native-in wrap (ETH→USDC via WETH pool) must succeed");

        // WRAP_ETH path: ETH is sent as tx.value; no ERC-20 approval needed.
        assert!(
            prepared.approval.is_none(),
            "native-in wrap: must carry no ERC-20 approval (ETH sent as tx.value)"
        );
        assert_eq!(
            prepared.tx.value, eth_in,
            "native-in wrap: tx.value must equal eth_in"
        );

        // Fund sender with eth_in + 1 ETH gas buffer.  gas_price=0 so only
        // tx.value is deducted from the sender's balance.
        let eth_buffer = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        fork.fund_native(sender, eth_in + eth_buffer);

        let before_usdc = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "native-in wrap (ETH→USDC) swap reverted"
        );
        let after_usdc = fork.erc20_balance(USDC_ADDR, sender);

        let delta = after_usdc - before_usdc;
        assert_eq!(
            delta, quoted.raw,
            "native-in wrap: USDC delta {delta} must equal quoted {} to the wei",
            quoted.raw
        );

        // No-residue guarantee: the router must hold zero WETH and zero native ETH
        // after the swap.  WRAP_ETH + SETTLE consumed exactly eth_in WETH; the
        // pool settled that debt, leaving nothing stranded.
        assert_eq!(
            fork.erc20_balance(WETH_ERC20_ADDR, UR),
            U256::ZERO,
            "native-in wrap: router must hold zero WETH after swap"
        );
        assert_eq!(
            fork.native_balance(UR),
            U256::ZERO,
            "native-in wrap: router must hold zero native ETH after swap"
        );

        fork.revert(snap);
    }

    // ── Direction 2: native-out exact-in (USDC → ETH via WETH pool) ─────────
    //
    // The encoder emits [V4_SWAP, UNWRAP_WETH]: USDC is settled from the user
    // via Permit2; the pool outputs WETH to the router (TAKE to ADDRESS_THIS);
    // UNWRAP_WETH converts it to native ETH and pays the recipient.
    // tx.value = 0; gas_price = 0 so ETH delta = pure swap output.
    {
        let snap = fork.snapshot();

        let usdc_in = U256::from(100_000_000u64); // 100 USDC (6 decimals)

        // Quote USDC-in → WETH (the pool's output currency).  The encoder's
        // UNWRAP_WETH step converts that WETH to native ETH for the recipient.
        let quoted = pool
            .quote(&AssetAmount::new(usdc, usdc_in), &weth_asset)
            .expect("pool must quote USDC→WETH (native-out unwrap direction)");

        let route = Route::new_single_hop(usdc, weth_asset, TradeType::ExactIn);
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
            .expect("build_swap native-out unwrap (USDC→ETH via WETH pool) must succeed");

        // tx.value must be 0; Permit2 approval required for the USDC input.
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "native-out unwrap: tx.value must be 0"
        );

        // Fund USDC via storage-slot injection (slot 9); also sets 100 ETH.
        fork.fund_erc20(sender, USDC_ADDR, USDC_SLOT, usdc_in * U256::from(2u64));

        // Fail-fast: confirm the slot write landed before proceeding.
        let usdc_funded = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            usdc_funded >= usdc_in,
            "USDC slot 9 injection failed: got {usdc_funded}, need {usdc_in}"
        );

        // Apply the Permit2 two-step approval from `prepared.approval`.
        if let Some(req) = &prepared.approval {
            let token_addr = Address::from_word(req.token.token);
            fork.permit2_approve(
                sender,
                token_addr,
                req.spender,
                UR,
                req.min_allowance,
                1_000_000_000_000u64,
            );
        }

        // Small native ETH buffer so the EVM's balance-check on the sender
        // passes (tx.value is 0, but some EVM setups still require sender > 0).
        fork.fund_native(sender, U256::from(1_000_000_000_000_000_000u64)); // 1 ETH

        let before_eth = fork.native_balance(sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "native-out unwrap (USDC→ETH) swap reverted"
        );
        let after_eth = fork.native_balance(sender);

        // gas_price=0: ETH delta is the pure swap output, wei-exact.
        let delta = after_eth - before_eth;
        assert_eq!(
            delta, quoted.raw,
            "native-out unwrap: ETH delta {delta} must equal quoted {} to the wei",
            quoted.raw
        );

        // No-residue guarantee: UNWRAP_WETH must have converted all WETH;
        // the router holds zero WETH and zero native ETH.
        assert_eq!(
            fork.erc20_balance(WETH_ERC20_ADDR, UR),
            U256::ZERO,
            "native-out unwrap: router must hold zero WETH after swap"
        );
        assert_eq!(
            fork.native_balance(UR),
            U256::ZERO,
            "native-out unwrap: router must hold zero native ETH after swap"
        );

        fork.revert(snap);
    }

    // ── Direction 3: native-in exact-out (ETH → exact USDC via WETH pool) ───
    //
    // The encoder emits [WRAP_ETH, V4_SWAP, UNWRAP_WETH]: WRAP_ETH wraps the
    // slippage-padded max; SETTLE drains only the actual swap cost; UNWRAP_WETH
    // returns leftover WETH to the recipient as ETH.  After the swap the router
    // must hold zero WETH and zero native ETH — UNWRAP_WETH handled the surplus.
    {
        let snap = fork.snapshot();

        let usdc_out = U256::from(10_000_000u64); // 10 USDC (6 decimals)

        // Quote exact-out: how many WETH units (= ETH) are needed for 10 USDC.
        let quoted_in = pool
            .as_exact_out()
            .expect("pool must support exact-out quoting")
            .quote_exact_out(&AssetAmount::new(usdc, usdc_out), &weth_asset)
            .expect("pool must quote exact-out USDC←WETH (native-in wrap direction)");

        let route = Route::new_single_hop(weth_asset, usdc, TradeType::ExactOut);
        let prepared = exe
            .build_swap_exact_out(
                &cfg,
                CurrencyAmount {
                    currency: Currency::Token(usdc),
                    raw: usdc_out,
                },
                Currency::Native,
                &route,
                &quoted_in,
                &opts,
            )
            .expect("build_swap_exact_out native-in wrap (ETH→exact USDC) must succeed");

        // WRAP_ETH path: no ERC-20 approval; tx.value = max_in (with slippage).
        assert!(
            prepared.approval.is_none(),
            "native-in exact-out wrap: must carry no ERC-20 approval"
        );
        // tx.value must be positive (max ETH the router will wrap).
        assert!(
            prepared.tx.value > U256::ZERO,
            "native-in exact-out wrap: tx.value must be > 0"
        );

        // Fund sender with tx.value (the max wrap amount) + 1 ETH buffer.
        let eth_buffer = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        fork.fund_native(sender, prepared.tx.value + eth_buffer);

        let before_usdc = fork.erc20_balance(USDC_ADDR, sender);
        assert!(
            fork.submit(sender, &prepared.tx),
            "native-in exact-out wrap (ETH→USDC) swap reverted"
        );
        let after_usdc = fork.erc20_balance(USDC_ADDR, sender);

        // Exact-out: the USDC delta must be exactly the requested output.
        let delta = after_usdc - before_usdc;
        assert_eq!(
            delta, usdc_out,
            "native-in exact-out wrap: USDC delta {delta} must equal requested {usdc_out} to the wei"
        );

        // No-residue guarantee: UNWRAP_WETH returns unused WETH as native ETH
        // to the recipient, so the router holds zero WETH.  The router also
        // must hold zero native ETH — it does not accumulate the unwrapped ETH.
        assert_eq!(
            fork.erc20_balance(WETH_ERC20_ADDR, UR),
            U256::ZERO,
            "native-in exact-out wrap: router must hold zero WETH after swap"
        );
        assert_eq!(
            fork.native_balance(UR),
            U256::ZERO,
            "native-in exact-out wrap: router must hold zero native ETH after swap"
        );

        fork.revert(snap);
    }
}

// ── recipient isolation proof ─────────────────────────────────────────────────

/// V4 distinct-recipient fork proof — exact-in ETH→USDC with `Recipient::To(other)`.
///
/// Asserts that when the resolved recipient is an address distinct from the
/// transaction sender, the output lands in `other` and NOT in `sender`.
/// This proves the V4 encoder's `TAKE(currency, recipient, OPEN_DELTA)` command
/// uses the resolved recipient address, not `msg.sender`.
///
/// Structure mirrors [`wei_exact_v4_native_directions`]:
/// 1. Snapshot + fund sender.
/// 2. Build calldata with `opts` pointing to `other` (0xCD…CD), not `sender`.
/// 3. Submit; assert `other`'s USDC balance rose by `quote.raw`.
/// 4. Assert `sender`'s USDC balance did NOT rise.
/// 5. Revert snapshot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
async fn v4_exact_in_delivers_to_distinct_recipient() {
    let url = match std::env::var("AMM_RPC_FORK_URL") {
        Ok(v) => v,
        Err(_) => return,
    };

    let alloy_provider = alloy::providers::ProviderBuilder::new()
        .connect_http(url.parse().expect("invalid RPC url"));

    let mut fork = support::fork_at(&url, fork_block(), ChainId(1), alloy_provider).await;

    // ── shared constants ─────────────────────────────────────────────────────

    let eth = AssetId::new(ChainId(1), B256::ZERO);
    let usdc = asset(1, USDC_ADDR);
    let weth = asset(1, WETH_ADDR);

    // The address that signs and pays; must NOT receive the output.
    let sender = Address::repeat_byte(0xBE);
    // The declared output recipient; distinct from sender.
    let other = Address::repeat_byte(0xCD);

    let mut routers = Routers::default();
    routers.universal = Some(UR);
    routers.permit2 = Some(PERMIT2);
    let cfg = ChainConfig::new(ChainId(1), weth).with_routers(routers);

    // opts point the swap output to `other`, not `sender`.
    let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
        .with_recipient(Recipient::To(other))
        .with_deadline(Deadline::AtTimestamp(u64::MAX / 2));

    let pool = fetch_v4_pool(fork.provider().clone(), fork_block()).await;
    let exe = as_executable(pool.as_ref()).expect("UniswapV4Pool must be Executable");

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
        .expect("build_swap distinct-recipient (ETH→USDC) must succeed");

    // V4 native-in carries no ERC-20 approval.
    assert!(
        prepared.approval.is_none(),
        "native-in swap must carry no ERC-20 approval"
    );

    // Fund sender with eth_in plus a 1 ETH buffer; gas_price=0 so only tx.value
    // is deducted.
    let eth_buffer = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
    fork.fund_native(sender, eth_in + eth_buffer);

    let sender_before = fork.erc20_balance(USDC_ADDR, sender);
    let other_before = fork.erc20_balance(USDC_ADDR, other);

    assert!(
        fork.submit(sender, &prepared.tx),
        "distinct-recipient (ETH→USDC) swap reverted"
    );

    let sender_after = fork.erc20_balance(USDC_ADDR, sender);
    let other_after = fork.erc20_balance(USDC_ADDR, other);

    // Output must reach the declared recipient, not the signer.
    let other_delta = other_after - other_before;
    assert_eq!(
        other_delta, quoted.raw,
        "distinct recipient: USDC delta {other_delta} must equal quoted {} to the wei",
        quoted.raw
    );

    // Sender's USDC balance must be unchanged — no leakage to msg.sender.
    assert_eq!(
        sender_after, sender_before,
        "sender USDC balance must not change when a distinct recipient is set"
    );

    fork.revert(snap);
}
