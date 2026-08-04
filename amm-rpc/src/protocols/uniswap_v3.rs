//! Uniswap V3 on-chain state source: fetch each pool's price, liquidity, fee,
//! and a window of its tick liquidity, then build a quotable [`UniswapV3Pool`].
//!
//! A V3 quote depends not on a single spot value but on the liquidity
//! distribution across ticks, so a refresh is **two dependent rounds** pinned to
//! the same block: first `slot0` (price + active tick), `liquidity`, `fee`, and
//! `tickSpacing`; then — now that the active tick is known — a window of
//! `ticks(t)` around it. The window ([`TICK_WINDOW`] spacings each side) bounds
//! the fetch; swaps that would cross beyond it are not represented.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use amm_core::primitives::asset::AssetId;
use amm_core::primitives::pool::{PoolId, PoolKey};
use amm_core::protocols::uniswap::v3::{TickData, TickInfo, UniswapV3Pool};
use amm_core::traits::pool::Pool;

use crate::error::RpcError;
use crate::multicall::{self, Call, CallResult};
use crate::source::{StateSource, pool_id};

/// How many tick-spacings each side of the active tick to fetch. 101 ticks per
/// pool covers the liquidity a normal swap crosses; a fee tier's whole range is
/// far larger, so very large swaps may be under-represented.
const TICK_WINDOW: i32 = 50;

/// Calls in round 1, per pool: `slot0`, `liquidity`, `fee`, `tickSpacing`.
const ROUND1_CALLS: usize = 4;

sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96, int24 tick, uint16 observationIndex,
            uint16 observationCardinality, uint16 observationCardinalityNext,
            uint8 feeProtocol, bool unlocked
        );
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross, int128 liquidityNet, uint256 feeGrowthOutside0X128,
            uint256 feeGrowthOutside1X128, int56 tickCumulativeOutside,
            uint160 secondsPerLiquidityOutsideX128, uint32 secondsOutside, bool initialized
        );
    }
}

/// A [`StateSource`] for Uniswap V3 pools over a provider `P`.
///
/// `refresh` is implemented (self-contained — it reads fee and tick spacing
/// on-chain, so a [`PoolKey`] needs only the address and its two assets).
/// `discover` is a follow-up.
pub struct UniswapV3Source<P> {
    provider: P,
}

impl<P: Provider> UniswapV3Source<P> {
    /// Wrap a provider as a Uniswap V3 state source.
    pub fn new(provider: P) -> Self {
        Self { provider }
    }
}

/// A pool's round-1 state: identity plus the price/liquidity/fee/spacing needed
/// to lay out its round-2 tick window and build it.
struct PoolState {
    address: Address,
    assets: [AssetId; 2],
    id: PoolId,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    fee_pips: u32,
    tick_spacing: i32,
}

/// Decode a pool's four round-1 results, or `None` if any reverted / mis-decoded.
fn decode_state(key: &PoolKey, results: &[CallResult]) -> Option<PoolState> {
    let assets: [AssetId; 2] = match key.assets.as_slice() {
        [a, b] => [*a, *b],
        _ => return None,
    };
    let address = key.address.parse::<Address>().ok()?;
    let slot0 = IUniswapV3Pool::slot0Call::abi_decode_returns(&results[0].return_data).ok()?;
    let liquidity =
        IUniswapV3Pool::liquidityCall::abi_decode_returns(&results[1].return_data).ok()?;
    let fee = IUniswapV3Pool::feeCall::abi_decode_returns(&results[2].return_data).ok()?;
    let spacing =
        IUniswapV3Pool::tickSpacingCall::abi_decode_returns(&results[3].return_data).ok()?;
    Some(PoolState {
        address,
        assets,
        id: pool_id(key),
        sqrt_price_x96: U256::from(slot0.sqrtPriceX96),
        tick: slot0.tick.as_i32(),
        liquidity,
        fee_pips: u32::try_from(fee).unwrap_or(3_000),
        tick_spacing: spacing.as_i32(),
    })
}

/// Round 1: `slot0` + `liquidity` + `fee` + `tickSpacing` per parseable pool.
/// Returns the calls and the keys that made it in (index-aligned in strides of
/// [`ROUND1_CALLS`]).
fn round1_calls(keys: &[PoolKey]) -> (Vec<Call>, Vec<&PoolKey>) {
    let mut calls = Vec::new();
    let mut kept = Vec::new();
    for key in keys {
        let Ok(pool) = key.address.parse::<Address>() else {
            continue;
        };
        let at = |data: Vec<u8>| Call {
            target: pool,
            call_data: data.into(),
        };
        calls.push(at(IUniswapV3Pool::slot0Call {}.abi_encode()));
        calls.push(at(IUniswapV3Pool::liquidityCall {}.abi_encode()));
        calls.push(at(IUniswapV3Pool::feeCall {}.abi_encode()));
        calls.push(at(IUniswapV3Pool::tickSpacingCall {}.abi_encode()));
        kept.push(key);
    }
    (calls, kept)
}

/// Round 2: one `ticks(t)` per tick-spacing in the window around each pool's
/// active tick. Returns the calls and, for each, `(state_index, tick)`.
fn round2_calls(states: &[PoolState]) -> (Vec<Call>, Vec<(usize, i32)>) {
    let mut calls = Vec::new();
    let mut refs = Vec::new();
    for (idx, state) in states.iter().enumerate() {
        let center = (state.tick / state.tick_spacing) * state.tick_spacing;
        for step in -TICK_WINDOW..=TICK_WINDOW {
            let tick = center + step * state.tick_spacing;
            let Ok(tick24) = alloy::primitives::aliases::I24::try_from(tick) else {
                continue;
            };
            calls.push(Call {
                target: state.address,
                call_data: IUniswapV3Pool::ticksCall { tick: tick24 }
                    .abi_encode()
                    .into(),
            });
            refs.push((idx, tick));
        }
    }
    (calls, refs)
}

/// Reconstruct each pool's initialized ticks from the round-2 results.
fn tick_windows(
    n: usize,
    results: &[CallResult],
    refs: &[(usize, i32)],
) -> Vec<Vec<(i32, TickInfo)>> {
    let mut windows: Vec<Vec<(i32, TickInfo)>> = vec![Vec::new(); n];
    for (&(state_idx, tick), result) in refs.iter().zip(results) {
        let Ok(decoded) = IUniswapV3Pool::ticksCall::abi_decode_returns(&result.return_data) else {
            continue;
        };
        // Only initialized ticks (nonzero gross) carry crossable liquidity.
        if decoded.liquidityGross != 0 {
            windows[state_idx].push((
                tick,
                TickInfo {
                    liquidity_net: decoded.liquidityNet,
                    initialized: true,
                },
            ));
        }
    }
    windows
}

/// Build a [`UniswapV3Pool`] from a decoded state and its tick window.
fn build_pool(state: PoolState, ticks: Vec<(i32, TickInfo)>) -> Box<dyn Pool> {
    let tick_data = TickData::from_ticks(state.tick_spacing, ticks);
    Box::new(UniswapV3Pool::new(
        state.id,
        state.assets,
        state.sqrt_price_x96,
        state.liquidity,
        state.tick,
        state.fee_pips,
        tick_data,
    ))
}

#[async_trait::async_trait]
impl<P: Provider + Send + Sync> StateSource for UniswapV3Source<P> {
    async fn discover(
        &self,
        _chain: &amm_core::primitives::asset::ChainId,
        _assets: &[AssetId],
    ) -> Result<Vec<PoolKey>, RpcError> {
        Err(RpcError::Internal(
            "UniswapV3Source::discover is not yet implemented; build PoolKeys from a subgraph or config".into(),
        ))
    }

    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError> {
        // Round 1: price + liquidity + fee + spacing for every pool.
        let (round1_calls, round1_keys) = round1_calls(keys);
        let round1 = multicall::aggregate3(&self.provider, round1_calls, at).await?;

        let states: Vec<PoolState> = round1_keys
            .into_iter()
            .enumerate()
            .filter_map(|(i, key)| {
                decode_state(key, round1.get(i * ROUND1_CALLS..(i + 1) * ROUND1_CALLS)?)
            })
            .collect();

        // Round 2 (same block): a tick window around each active tick.
        let (round2_calls, refs) = round2_calls(&states);
        let round2 = multicall::aggregate3(&self.provider, round2_calls, at).await?;
        let windows = tick_windows(states.len(), &round2, &refs);

        Ok(states
            .into_iter()
            .zip(windows)
            .map(|(state, ticks)| build_pool(state, ticks))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use amm_core::primitives::asset::{AssetAmount, ChainId};

    fn usdc() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x01]))
    }
    fn weth() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x02]))
    }

    /// sqrtPriceX96 for tick 0 (1:1) = 2^96.
    const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;

    #[test]
    fn build_full_range_pool_from_ticks_quotes_in_the_fee_band() {
        // A full-range position at tick 0, 1e18 liquidity, 0.30% fee — the decoded
        // state a refresh would assemble. Must quote like the amm-core V3 unit.
        let liq: i128 = 1_000_000_000_000_000_000;
        let state = PoolState {
            address: Address::ZERO,
            assets: [usdc(), weth()],
            id: PoolId::new("1:uniswap-v3:0xfull"),
            sqrt_price_x96: U256::from(SQRT_1_1),
            tick: 0,
            liquidity: liq as u128,
            fee_pips: 3000,
            tick_spacing: 60,
        };
        let ticks = vec![
            (
                -887_220,
                TickInfo {
                    liquidity_net: liq,
                    initialized: true,
                },
            ),
            (
                887_220,
                TickInfo {
                    liquidity_net: -liq,
                    initialized: true,
                },
            ),
        ];
        let pool = build_pool(state, ticks);
        let amount_in = U256::from(1_000_000_000u64);
        let out = pool
            .quote(&AssetAmount::new(usdc(), amount_in), &weth())
            .unwrap();
        assert!(out.raw < amount_in);
        assert!(out.raw > amount_in * U256::from(996u64) / U256::from(1000u64));
    }

    /// End-to-end refresh against a forked mainnet RPC. Gated: set
    /// `AMM_RPC_FORK_URL` and run with `cargo test -p amm-rpc -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
    async fn refresh_mainnet_usdc_weth_v3_pool_quotes() {
        use alloy::primitives::address;

        let Ok(url) = std::env::var("AMM_RPC_FORK_URL") else {
            return;
        };
        let provider = crate::provider::make_provider(&url).unwrap();
        let source = UniswapV3Source::new(provider);

        // Mainnet USDC/WETH 0.05% pool; token0 = USDC, token1 = WETH.
        let usdc = AssetId::new(
            ChainId(1),
            address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").into_word(),
        );
        let weth = AssetId::new(
            ChainId(1),
            address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").into_word(),
        );
        let key = PoolKey {
            exchange: amm_core::primitives::pool::ExchangeId::new("uniswap-v3"),
            chain: ChainId(1),
            address: "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(),
            assets: vec![usdc, weth],
            fee_bps: None,
        };

        let pools = source.refresh(&[key], BlockId::latest()).await.unwrap();
        assert_eq!(pools.len(), 1);
        let out = pools[0]
            .quote(&AssetAmount::new(usdc, U256::from(1_000_000_000u64)), &weth)
            .unwrap();
        assert!(
            out.raw > U256::ZERO,
            "refreshed V3 pool must quote a positive output"
        );
    }
}
