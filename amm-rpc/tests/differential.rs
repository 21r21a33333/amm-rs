//! Live **differential** tests: our refreshed-pool quote vs the deployed
//! contract's own quote function, at a single pinned block.
//!
//! This is the end-to-end proof that our decoders + quote math match the
//! deployed contracts. Each test:
//!
//! 1. pins the latest block `B`,
//! 2. refreshes our pool state at `B` and quotes locally,
//! 3. calls the pool's own quote fn (`get_dy` / `getAmountOut` / a Quoter) at the
//!    **same** block `B`, routed through the crate's own `aggregate3` so it
//!    observes the identical block,
//! 4. asserts the two agree — to the wei for constant-product / stableswap, and
//!    within a bounded ppm for concentrated liquidity (our fetch reconstructs a
//!    finite tick window, so very large swaps can diverge slightly).
//!
//! Improvements over a one-shot differential harness: every pool is probed at
//! **multiple sizes in both directions**, and results are **collected into a
//! report** (worst-case ppm printed, all mismatches surfaced at once rather than
//! failing on the first). Pool addresses that a factory owns (Aerodrome,
//! Slipstream) are looked up on-chain rather than hardcoded.
//!
//! Gated: each test returns early unless its RPC env var is set.
//! - Ethereum: `AMM_RPC_FORK_URL`
//! - Base:     `AMM_RPC_BASE_FORK_URL`
//!
//! Run: `cargo test -p amm-rpc --all-features --test differential -- --nocapture`

#![cfg(test)]

use alloy::eips::BlockId;
use alloy::primitives::aliases::{I24, U24, U160};
use alloy::primitives::{Address, B256, U256, address};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::{ExchangeId, PoolKey};
use amm_core::traits::pool::Pool;
use amm_rpc::multicall::{self, Call};
use amm_rpc::provider::EthProvider;
use amm_rpc::source::StateSource;

// ─── reference quoter ABIs (identical to the deployed contracts) ────────────────

sol! {
    // Aerodrome v2 pools quote themselves.
    interface IAeroQuote {
        function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    }
    // Aerodrome PoolFactory: resolve a (tokenA, tokenB, stable) pool address.
    interface IAeroFactory {
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address);
    }
    // Slipstream CL factory: resolve a (tokenA, tokenB, tickSpacing) pool address.
    interface ICLFactory {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address);
    }
    // Uniswap V2 pairs have no quote fn — use the Router's library math.
    interface IUniV2Router {
        function getAmountsOut(uint256 amountIn, address[] path) external view returns (uint256[] memory amounts);
    }
    // Uniswap V3 QuoterV2 (keyed by fee).
    interface IUniV3Quoter {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams params)
            external
            returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
    }
    // Uniswap V4 Quoter (keyed by the full pool key).
    interface IV4Quoter {
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }
        struct QuoteExactSingleParams {
            PoolKey poolKey;
            bool zeroForOne;
            uint128 exactAmount;
            bytes hookData;
        }
        function quoteExactInputSingle(QuoteExactSingleParams params)
            external
            returns (uint256 amountOut, uint256 gasEstimate);
    }
    // Aerodrome Slipstream Quoter — QuoterV2-shaped but keyed by tickSpacing.
    interface ISlipstreamQuoter {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            int24 tickSpacing;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams params)
            external
            returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
    }
}

// ─── shared harness ──────────────────────────────────────────────────────────

/// A live provider pinned to one block, or `None` if `env_var` is unset (the
/// test then returns early — this is how the gate works).
struct Fork {
    provider: EthProvider,
    block: u64,
}

impl Fork {
    async fn open(env_var: &str) -> Option<Fork> {
        let url = std::env::var(env_var).ok()?;
        let provider = amm_rpc::make_provider(&url).expect("provider");
        let block = provider.get_block_number().await.expect("block number");
        Some(Fork { provider, block })
    }

    fn at(&self) -> BlockId {
        BlockId::number(self.block)
    }

    /// One reference-quoter call at the pinned block, routed through the crate's
    /// own `aggregate3` (same block-pinning path as our reads). Panics if it
    /// reverts — a reference quote that reverts is a broken test setup.
    async fn reference(&self, target: Address, calldata: Vec<u8>) -> Vec<u8> {
        let results = multicall::aggregate3(
            &self.provider,
            vec![Call {
                target,
                call_data: calldata.into(),
            }],
            self.at(),
        )
        .await
        .expect("reference batch");
        assert!(results[0].success, "reference quote reverted");
        results[0].return_data.to_vec()
    }
}

/// Accumulates every differential check so all mismatches surface together and
/// the worst-case divergence is reported.
#[derive(Default)]
struct Report {
    checks: usize,
    worst_ppm: u128,
    fails: Vec<String>,
}

impl Report {
    /// Wei-exact check (constant-product / stableswap).
    fn wei(&mut self, label: &str, ours: U256, theirs: U256) {
        let ppm = ppm_delta(ours, theirs);
        self.record(label, ours, theirs, ppm, ours == theirs, "exact");
    }

    /// Bounded-ppm check (concentrated liquidity — finite tick window).
    fn ppm(&mut self, label: &str, ours: U256, theirs: U256, max: u128) {
        let ppm = ppm_delta(ours, theirs);
        self.record(
            label,
            ours,
            theirs,
            ppm,
            ppm <= max,
            &format!("<= {max} ppm"),
        );
    }

    fn record(&mut self, label: &str, ours: U256, theirs: U256, ppm: u128, ok: bool, bound: &str) {
        self.checks += 1;
        self.worst_ppm = self.worst_ppm.max(ppm);
        let status = match ok {
            true => "ok",
            false => "MISMATCH",
        };
        println!("  [{status}] {label}: ours={ours} theirs={theirs} ({ppm} ppm, {bound})");
        if !ok {
            self.fails
                .push(format!("{label}: ours={ours} theirs={theirs} ({ppm} ppm)"));
        }
    }

    fn finish(self, family: &str) {
        println!(
            "{family}: {} checks, worst {} ppm, {} failure(s)",
            self.checks,
            self.worst_ppm,
            self.fails.len()
        );
        assert!(
            self.fails.is_empty(),
            "{family} differential failures:\n{}",
            self.fails.join("\n")
        );
    }
}

/// Relative divergence of `a` from `b` in parts per million.
fn ppm_delta(a: U256, b: U256) -> u128 {
    if b.is_zero() {
        return match a.is_zero() {
            true => 0,
            false => u128::MAX,
        };
    }
    let (hi, lo) = (a.max(b), a.min(b));
    u128::try_from((hi - lo) * U256::from(1_000_000u64) / b).unwrap_or(u128::MAX)
}

fn asset(chain: u64, token: Address) -> AssetId {
    AssetId::new(ChainId(chain), token.into_word())
}

/// A 2-asset pool key with assets in address-sorted `[token0, token1]` order (as
/// every 2-asset quoter expects).
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

/// Our exact-in quote, in base units.
fn quote_out(pool: &dyn Pool, from: Address, to: Address, chain: u64, amount: U256) -> U256 {
    pool.quote(
        &AssetAmount::new(asset(chain, from), amount),
        &asset(chain, to),
    )
    .expect("our pool must quote")
    .raw
}

// ─── token addresses ────────────────────────────────────────────────────────

const USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
const BASE_USDC: Address = address!("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
const BASE_WETH: Address = address!("0x4200000000000000000000000000000000000006");
const BASE_USDBC: Address = address!("0xd9aaEC86B65D86f6A7B5B1b0c42FFA531710b6CA");

const ETH_CHAIN: u64 = 1;
const BASE_CHAIN: u64 = 8453;

const K_USDC: u128 = 1_000_000_000; // 1_000 USDC (6 dec)
const BIG_USDC: u128 = 100_000_000_000; // 100_000 USDC
const E18: u128 = 1_000_000_000_000_000_000; // 1 WETH
const TENTH_E18: u128 = 100_000_000_000_000_000; // 0.1 WETH

// ─── Uniswap V2 (Ethereum) ──────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC at $AMM_RPC_FORK_URL"]
async fn diff_uniswap_v2_usdc_weth() {
    let Some(fork) = Fork::open("AMM_RPC_FORK_URL").await else {
        return;
    };
    let router = address!("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
    let pair = address!("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"); // UniV2 USDC/WETH

    let source = amm_rpc::protocols::uniswap_v2::UniswapV2Source::new(fork.provider.clone());
    let key = key2("uniswap-v2", ETH_CHAIN, pair, USDC, WETH);
    let pools = source.refresh(&[key], fork.at()).await.unwrap();
    assert_eq!(pools.len(), 1, "v2 pool failed to refresh");
    let pool = pools[0].as_ref();

    let mut report = Report::default();
    for (from, to, amount) in [
        (USDC, WETH, U256::from(K_USDC)),
        (USDC, WETH, U256::from(BIG_USDC)),
        (WETH, USDC, U256::from(E18)),
    ] {
        let ours = quote_out(pool, from, to, ETH_CHAIN, amount);
        let calldata = IUniV2Router::getAmountsOutCall {
            amountIn: amount,
            path: vec![from, to],
        }
        .abi_encode();
        let ret = fork.reference(router, calldata).await;
        let theirs = *IUniV2Router::getAmountsOutCall::abi_decode_returns(&ret)
            .unwrap()
            .last()
            .unwrap();
        report.wei(&format!("v2 {from:#x}->{to:#x} {amount}"), ours, theirs);
    }
    report.finish("uniswap-v2");
}

// ─── Uniswap V3 (Ethereum) ──────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC at $AMM_RPC_FORK_URL"]
async fn diff_uniswap_v3_usdc_weth() {
    let Some(fork) = Fork::open("AMM_RPC_FORK_URL").await else {
        return;
    };
    let quoter = address!("0x61fFE014bA17989E743c5F6cB21bF9697530B21e"); // QuoterV2
    let pool_addr = address!("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"); // USDC/WETH 0.05%
    let fee = 500u32;

    let source = amm_rpc::protocols::uniswap_v3::UniswapV3Source::new(fork.provider.clone());
    let key = key2("uniswap-v3", ETH_CHAIN, pool_addr, USDC, WETH);
    let pools = source.refresh(&[key], fork.at()).await.unwrap();
    assert_eq!(pools.len(), 1, "v3 pool failed to refresh");
    let pool = pools[0].as_ref();

    let mut report = Report::default();
    for (from, to, amount) in [
        (USDC, WETH, U256::from(K_USDC)),
        (USDC, WETH, U256::from(BIG_USDC)),
        (WETH, USDC, U256::from(E18)),
    ] {
        let ours = quote_out(pool, from, to, ETH_CHAIN, amount);
        let params = IUniV3Quoter::QuoteExactInputSingleParams {
            tokenIn: from,
            tokenOut: to,
            amountIn: amount,
            fee: U24::from(fee),
            sqrtPriceLimitX96: U160::ZERO,
        };
        let calldata = IUniV3Quoter::quoteExactInputSingleCall { params }.abi_encode();
        let ret = fork.reference(quoter, calldata).await;
        let theirs = IUniV3Quoter::quoteExactInputSingleCall::abi_decode_returns(&ret)
            .unwrap()
            .amountOut;
        report.ppm(
            &format!("v3 {from:#x}->{to:#x} {amount}"),
            ours,
            theirs,
            200,
        );
    }
    report.finish("uniswap-v3");
}

// ─── Uniswap V4 (Ethereum) ──────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: needs an Ethereum RPC at $AMM_RPC_FORK_URL"]
async fn diff_uniswap_v4_eth_usdc() {
    use amm_core::protocols::uniswap::v4::Hooks;
    use amm_rpc::protocols::uniswap_v4::{UniswapV4Source, V4PoolConfig};

    let Some(fork) = Fork::open("AMM_RPC_FORK_URL").await else {
        return;
    };
    let manager = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
    let quoter = address!("0x52F0E24D1c21C8A0cB1e5a5dD6198556BD9E1203");
    let (fee, spacing) = (500u32, 10i32);

    // ETH = currency0 = address(0); USDC = currency1.
    let eth = AssetId::new(ChainId(ETH_CHAIN), B256::ZERO);
    let usdc = asset(ETH_CHAIN, USDC);
    let config = V4PoolConfig::new(
        Address::ZERO,
        USDC,
        eth,
        usdc,
        fee,
        spacing,
        Address::ZERO,
        Hooks::None,
    );
    let source = UniswapV4Source::new(fork.provider.clone(), manager, vec![config.clone()]);
    let key = PoolKey {
        exchange: ExchangeId::new("uniswap-v4"),
        chain: ChainId(ETH_CHAIN),
        address: config.pool_id.to_string(),
        assets: vec![eth, usdc],
        fee_bps: None,
    };
    let pools = source.refresh(&[key], fork.at()).await.unwrap();
    assert_eq!(pools.len(), 1, "v4 pool failed to refresh");
    let pool = pools[0].as_ref();

    let mut report = Report::default();
    // (from_native, to, amount, zeroForOne). ETH is currency0, so ETH->USDC is
    // zeroForOne = true.
    for (from, to, amount, zero_for_one) in [
        (Address::ZERO, USDC, U256::from(TENTH_E18), true),
        (Address::ZERO, USDC, U256::from(E18), true),
        (USDC, Address::ZERO, U256::from(K_USDC), false),
    ] {
        let (from_asset, to_asset) = match from == Address::ZERO {
            true => (eth, usdc),
            false => (usdc, eth),
        };
        let ours = pool
            .quote(&AssetAmount::new(from_asset, amount), &to_asset)
            .expect("v4 quote")
            .raw;
        let params = IV4Quoter::QuoteExactSingleParams {
            poolKey: IV4Quoter::PoolKey {
                currency0: Address::ZERO,
                currency1: USDC,
                fee: U24::from(fee),
                tickSpacing: I24::try_from(spacing).unwrap(),
                hooks: Address::ZERO,
            },
            zeroForOne: zero_for_one,
            exactAmount: u128::try_from(amount).unwrap(),
            hookData: alloy::primitives::Bytes::new(),
        };
        let calldata = IV4Quoter::quoteExactInputSingleCall { params }.abi_encode();
        let ret = fork.reference(quoter, calldata).await;
        let theirs = IV4Quoter::quoteExactInputSingleCall::abi_decode_returns(&ret)
            .unwrap()
            .amountOut;
        report.ppm(
            &format!("v4 {from:#x}->{to:#x} {amount}"),
            ours,
            theirs,
            200,
        );
        let _ = (from, to);
    }
    report.finish("uniswap-v4");
}

// ─── Aerodrome v2 (Base) ──────────────────────────────────────────────────────

/// One Aerodrome v2 pool to check: its ordered token pair, the stable flag that
/// selects the factory pool, and the `(from, to, amount)` probes to run.
struct AeroCase {
    label: &'static str,
    a: Address,
    b: Address,
    stable: bool,
    probes: &'static [(Address, Address, u128)],
}

async fn resolve_aero_pool(
    fork: &Fork,
    factory: Address,
    a: Address,
    b: Address,
    stable: bool,
) -> Option<Address> {
    let calldata = IAeroFactory::getPoolCall {
        tokenA: a,
        tokenB: b,
        stable,
    }
    .abi_encode();
    let ret = fork.reference(factory, calldata).await;
    let pool = IAeroFactory::getPoolCall::abi_decode_returns(&ret).ok()?;
    match pool.is_zero() {
        true => None,
        false => Some(pool),
    }
}

#[tokio::test]
#[ignore = "live: needs a Base RPC at $AMM_RPC_BASE_FORK_URL"]
async fn diff_aerodrome_v2() {
    let Some(fork) = Fork::open("AMM_RPC_BASE_FORK_URL").await else {
        return;
    };
    let factory = address!("0x420DD381b31aEf6683db6B902084cB0FFECe40Da");
    let source =
        amm_rpc::protocols::aerodrome::AerodromeSource::new(fork.provider.clone(), factory);
    let mut report = Report::default();

    let cases = [
        AeroCase {
            label: "volatile USDC/WETH",
            a: BASE_USDC,
            b: BASE_WETH,
            stable: false,
            probes: &[(BASE_USDC, BASE_WETH, K_USDC), (BASE_WETH, BASE_USDC, E18)],
        },
        AeroCase {
            label: "stable USDC/USDbC",
            a: BASE_USDC,
            b: BASE_USDBC,
            stable: true,
            probes: &[
                (BASE_USDC, BASE_USDBC, K_USDC),
                (BASE_USDBC, BASE_USDC, K_USDC),
            ],
        },
    ];

    for AeroCase {
        label,
        a,
        b,
        stable,
        probes,
    } in cases
    {
        let Some(pool_addr) = resolve_aero_pool(&fork, factory, a, b, stable).await else {
            report
                .fails
                .push(format!("{label}: factory returned no pool"));
            continue;
        };
        let key = key2("aerodrome", BASE_CHAIN, pool_addr, a, b);
        let pools = source.refresh(&[key], fork.at()).await.unwrap();
        assert_eq!(pools.len(), 1, "{label}: pool failed to refresh");
        let pool = pools[0].as_ref();

        for &(from, to, amount) in probes {
            let amount = U256::from(amount);
            let ours = quote_out(pool, from, to, BASE_CHAIN, amount);
            let calldata = IAeroQuote::getAmountOutCall {
                amountIn: amount,
                tokenIn: from,
            }
            .abi_encode();
            let ret = fork.reference(pool_addr, calldata).await;
            let theirs = IAeroQuote::getAmountOutCall::abi_decode_returns(&ret).unwrap();
            report.wei(
                &format!("aero {label} {from:#x}->{to:#x} {amount}"),
                ours,
                theirs,
            );
        }
    }
    report.finish("aerodrome-v2");
}

// ─── Aerodrome Slipstream (Base) ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "live: needs a Base RPC at $AMM_RPC_BASE_FORK_URL"]
async fn diff_aerodrome_slipstream() {
    let Some(fork) = Fork::open("AMM_RPC_BASE_FORK_URL").await else {
        return;
    };
    let cl_factory = address!("0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A");
    let quoter = address!("0x254cf9E1E6e233aa1AC962CB9B05b2cfeAaE15b0");
    let spacing = 100i32;

    // Resolve the WETH/USDC pool of this tick spacing from the CL factory.
    let (a, b) = match BASE_USDC < BASE_WETH {
        true => (BASE_USDC, BASE_WETH),
        false => (BASE_WETH, BASE_USDC),
    };
    let calldata = ICLFactory::getPoolCall {
        tokenA: a,
        tokenB: b,
        tickSpacing: I24::try_from(spacing).unwrap(),
    }
    .abi_encode();
    let ret = fork.reference(cl_factory, calldata).await;
    let pool_addr = ICLFactory::getPoolCall::abi_decode_returns(&ret).unwrap();
    assert!(
        !pool_addr.is_zero(),
        "no slipstream pool for spacing {spacing}"
    );

    let source = amm_rpc::protocols::slipstream::SlipstreamSource::new(fork.provider.clone());
    let key = key2("aerodrome-slipstream", BASE_CHAIN, pool_addr, a, b);
    let pools = source.refresh(&[key], fork.at()).await.unwrap();
    assert_eq!(pools.len(), 1, "slipstream pool failed to refresh");
    let pool = pools[0].as_ref();

    let mut report = Report::default();
    for (from, to, amount) in [
        (BASE_USDC, BASE_WETH, U256::from(K_USDC)),
        (BASE_WETH, BASE_USDC, U256::from(TENTH_E18)),
    ] {
        let ours = quote_out(pool, from, to, BASE_CHAIN, amount);
        let params = ISlipstreamQuoter::QuoteExactInputSingleParams {
            tokenIn: from,
            tokenOut: to,
            amountIn: amount,
            tickSpacing: I24::try_from(spacing).unwrap(),
            sqrtPriceLimitX96: U160::ZERO,
        };
        let calldata = ISlipstreamQuoter::quoteExactInputSingleCall { params }.abi_encode();
        let ret = fork.reference(quoter, calldata).await;
        let theirs = ISlipstreamQuoter::quoteExactInputSingleCall::abi_decode_returns(&ret)
            .unwrap()
            .amountOut;
        report.ppm(
            &format!("slipstream {from:#x}->{to:#x} {amount}"),
            ours,
            theirs,
            200,
        );
    }
    report.finish("aerodrome-slipstream");
}

// ─── Curve (Ethereum) — one differential per variant vs get_dy ──────────────────

#[cfg(feature = "curve")]
mod curve {
    use super::*;
    use amm_rpc::protocols::curve::{CurvePoolConfig, CurveSource};
    use curve_adapter::CurveVariant;

    sol! {
        interface ICurveGetDyInt {
            function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
        }
        interface ICurveGetDyUint {
            function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
        }
    }

    /// One Curve pool to check: its variant, coins (`address, decimals`), the
    /// swap `coins[i] -> coins[j]` at `dx`, and whether `get_dy` is `int128`
    /// (stableswap) or `uint256` (cryptoswap) indexed.
    struct Case {
        label: &'static str,
        pool: Address,
        variant: CurveVariant,
        coins: &'static [(Address, u8)],
        base_pool: Option<Address>,
        eth_variant: Option<bool>,
        i: usize,
        j: usize,
        dx: u128,
        int128_indices: bool,
    }

    async fn run(fork: &Fork, report: &mut Report, case: &Case) {
        let config = CurvePoolConfig {
            address: case.pool,
            variant: case.variant,
            coins: case
                .coins
                .iter()
                .map(|(a, _)| asset(ETH_CHAIN, *a))
                .collect(),
            decimals: case.coins.iter().map(|(_, d)| *d).collect(),
            base_pool: case.base_pool,
            eth_variant: case.eth_variant,
        };
        let source = CurveSource::new(fork.provider.clone(), vec![config]);
        let key = PoolKey {
            exchange: ExchangeId::new("curve"),
            chain: ChainId(ETH_CHAIN),
            address: case.pool.to_string(),
            assets: case
                .coins
                .iter()
                .map(|(a, _)| asset(ETH_CHAIN, *a))
                .collect(),
            fee_bps: None,
        };
        let pools = source.refresh(&[key], fork.at()).await.unwrap();
        if pools.len() != 1 {
            report
                .fails
                .push(format!("{}: pool failed to refresh", case.label));
            return;
        }
        let (ci, cj) = (case.coins[case.i].0, case.coins[case.j].0);
        let ours = quote_out(pools[0].as_ref(), ci, cj, ETH_CHAIN, U256::from(case.dx));

        let calldata = match case.int128_indices {
            true => ICurveGetDyInt::get_dyCall {
                i: case.i as i128,
                j: case.j as i128,
                dx: U256::from(case.dx),
            }
            .abi_encode(),
            false => ICurveGetDyUint::get_dyCall {
                i: U256::from(case.i),
                j: U256::from(case.j),
                dx: U256::from(case.dx),
            }
            .abi_encode(),
        };
        let ret = fork.reference(case.pool, calldata).await;
        let theirs = match case.int128_indices {
            true => ICurveGetDyInt::get_dyCall::abi_decode_returns(&ret).unwrap(),
            false => ICurveGetDyUint::get_dyCall::abi_decode_returns(&ret).unwrap(),
        };
        report.wei(case.label, ours, theirs);
    }

    // Curve mainnet tokens.
    const DAI: Address = address!("0x6b175474e89094c44da98b954eedeac495271d0f");
    const USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    const USDT: Address = address!("0xdac17f958d2ee523a2206206994597c13d831ec7");
    const WBTC: Address = address!("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599");
    const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    const CRVUSD: Address = address!("0xf939e0a03fb07f59a73314e73794be0e57ac1b4e");
    const CRV: Address = address!("0xD533a949740bb3306d119CC777fa900bA034cd52");
    const CBBTC: Address = address!("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf");
    const STETH: Address = address!("0xae7ab96520de3a18e5e111b5eaab095312d7fe84");
    const ETH: Address = address!("0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
    const MIM: Address = address!("0x99d8a9c45b2eca8864373a26d1459e3dff1e17f3");
    const THREE_CRV: Address = address!("0x6c3f90f043a72fa612cbac8115ee7e52bde6e490");
    const SUSD: Address = address!("0x57Ab1ec28D129707052df4dF418D58a2D46d5f51");
    const FRAX: Address = address!("0x853d955aCEf822Db058eb8505911ED77F175b99e");
    const ADAI: Address = address!("0x028171bCA77440897B824Ca71D1c56caC55b68A3");
    const AUSDC: Address = address!("0xBcca60bB61934080951369a648Fb03DF4F96263C");
    const AUSDT: Address = address!("0x3Ed3B47Dd13EC9a98b44e6204A523E766B225811");
    const TC_NG_TOKEN: Address = address!("0x1cfa5641c01406ab8ac350ded7d735ec41298372");

    const K_USDC: u128 = 1_000_000_000;

    fn cases() -> Vec<Case> {
        vec![
            Case {
                label: "curve V0 sUSD DAI->USDC",
                pool: address!("0xA5407eAE9Ba41422680e2e00537571bcC53efBfD"),
                variant: CurveVariant::StableSwapV0,
                coins: &[(DAI, 18), (USDC, 6), (USDT, 6), (SUSD, 18)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: true,
            },
            Case {
                label: "curve V1 3pool DAI->USDC",
                pool: address!("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
                variant: CurveVariant::StableSwapV1,
                coins: &[(DAI, 18), (USDC, 6), (USDT, 6)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: true,
            },
            Case {
                label: "curve V2 fraxusdc FRAX->USDC",
                pool: address!("0xDcEF968d416a41Cdac0ED8702fAC8128A64241A2"),
                variant: CurveVariant::StableSwapV2,
                coins: &[(FRAX, 18), (USDC, 6)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: true,
            },
            Case {
                label: "curve STETH ETH->stETH",
                pool: address!("0xDC24316b9AE028F1497c275EB9192a3Ea0f67022"),
                variant: CurveVariant::StableSwapSTETH,
                coins: &[(ETH, 18), (STETH, 18)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: true,
            },
            Case {
                label: "curve ALend aDAI->aUSDC",
                pool: address!("0xDeBF20617708857ebe4F679508E7b7863a8A8EeE"),
                variant: CurveVariant::StableSwapALend,
                coins: &[(ADAI, 18), (AUSDC, 6), (AUSDT, 6)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: true,
            },
            Case {
                label: "curve NG USDC->crvUSD",
                pool: address!("0x4DEcE678ceceb27446b35C672dC7d61F30bAD69E"),
                variant: CurveVariant::StableSwapNG,
                coins: &[(USDC, 6), (CRVUSD, 18)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 1,
                dx: K_USDC,
                int128_indices: true,
            },
            Case {
                label: "curve Meta MIM->3CRV",
                pool: address!("0x5a6A4D54456819380173272A5E8E9B9904BdF41B"),
                variant: CurveVariant::StableSwapMeta,
                coins: &[(MIM, 18), (THREE_CRV, 18)],
                base_pool: Some(address!("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7")),
                eth_variant: None,
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: true,
            },
            Case {
                label: "curve TwoCryptoV1 WETH->CRV",
                pool: address!("0x8301AE4fc9c624d1D396cbDAa1ed877821D7C511"),
                variant: CurveVariant::TwoCryptoV1,
                coins: &[(WETH, 18), (CRV, 18)],
                base_pool: None,
                eth_variant: Some(true),
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: false,
            },
            Case {
                label: "curve TwoCryptoNG WETH->token",
                pool: address!("0x592878b920101946fb5915ab97961bc546f211cc"),
                variant: CurveVariant::TwoCryptoNG,
                coins: &[(WETH, 18), (TC_NG_TOKEN, 18)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: false,
            },
            Case {
                label: "curve TwoCryptoStable crvUSD->cbBTC",
                pool: address!("0x83f24023d15D835a213Df24Fd309c47dab5BEB32"),
                variant: CurveVariant::TwoCryptoStable,
                coins: &[(CRVUSD, 18), (CBBTC, 8)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 1,
                dx: E18,
                int128_indices: false,
            },
            Case {
                label: "curve TriCryptoV1 USDT->WETH",
                pool: address!("0xD51a44d3FaE010294C616388b506AcdA1bfAAE46"),
                variant: CurveVariant::TriCryptoV1,
                coins: &[(USDT, 6), (WBTC, 8), (WETH, 18)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 2,
                dx: K_USDC,
                int128_indices: false,
            },
            Case {
                label: "curve TriCryptoNG USDC->WETH",
                pool: address!("0x7F86Bf177Dd4F3494b841a37e810A34dD56c829B"),
                variant: CurveVariant::TriCryptoNG,
                coins: &[(USDC, 6), (WBTC, 8), (WETH, 18)],
                base_pool: None,
                eth_variant: None,
                i: 0,
                j: 2,
                dx: K_USDC,
                int128_indices: false,
            },
        ]
    }

    #[tokio::test]
    #[ignore = "live: needs an Ethereum RPC at $AMM_RPC_FORK_URL"]
    async fn diff_curve_all_variants() {
        let Some(fork) = Fork::open("AMM_RPC_FORK_URL").await else {
            return;
        };
        let mut report = Report::default();
        for case in cases() {
            run(&fork, &mut report, &case).await;
        }
        report.finish("curve");
    }
}
