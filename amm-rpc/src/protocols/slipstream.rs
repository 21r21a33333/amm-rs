//! Aerodrome **Slipstream** on-chain state source: concentrated-liquidity pools
//! that are a Uniswap V3 fork, so they build the shared-engine
//! [`AerodromeSlipstreamPool`] from the same slot0 + tick-window fetch as V3.
//!
//! Two things differ from Uniswap V3, which is why this can't reuse the V3
//! source directly:
//! - the swap fee is read per pool via `fee()` (a gauge can set/update it),
//!   rather than being the fixed tier V3 exposes;
//! - `slot0()` drops V3's `feeProtocol` field and `ticks()` inserts
//!   staked-liquidity / reward fields — so both need Slipstream's own ABI. Only
//!   `liquidityGross`/`liquidityNet` (still fields 0–1) feed the swap math.
//!
//! Refresh is the same two dependent rounds as V3: reads of `slot0`, `fee`,
//! `liquidity`, and `tickSpacing`, then a window of `ticks(t)` around the active
//! tick. Reading `tickSpacing` on-chain keeps the [`PoolKey`] self-contained
//! (address plus its two assets), matching the V3 source.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use amm_core::primitives::asset::AssetId;
use amm_core::primitives::pool::{PoolId, PoolKey};
use amm_core::protocols::aerodrome::slipstream::{AerodromeSlipstreamPool, TickData, TickInfo};
use amm_core::traits::pool::Pool;

use crate::error::RpcError;
use crate::multicall::{self, Call, CallResult};
use crate::source::StateSource;

/// How many tick-spacings each side of the active tick to fetch (as V3).
const TICK_WINDOW: i32 = 50;

/// Calls in round 1, per pool: `slot0`, `fee`, `liquidity`, `tickSpacing`.
const ROUND1_CALLS: usize = 4;

sol! {
    #[sol(rpc)]
    interface ICLPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96, int24 tick, uint16 observationIndex,
            uint16 observationCardinality, uint16 observationCardinalityNext, bool unlocked
        );
        function fee() external view returns (uint24);
        function liquidity() external view returns (uint128);
        function tickSpacing() external view returns (int24);
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross, int128 liquidityNet, int128 stakedLiquidityNet,
            uint256 feeGrowthOutside0X128, uint256 feeGrowthOutside1X128,
            uint256 rewardGrowthOutsideX128, int56 tickCumulativeOutside,
            uint160 secondsPerLiquidityOutsideX128, uint32 secondsOutside, bool initialized
        );
    }
}

/// A [`StateSource`] for Aerodrome Slipstream pools over a provider `P`.
///
/// `refresh` is implemented (self-contained — it reads the fee and tick spacing
/// on-chain, so a [`PoolKey`] needs only the address and its two assets).
/// `discover` (factory `getPool` by tick spacing) is a follow-up.
pub struct SlipstreamSource<P> {
    provider: P,
}

impl<P: Provider> SlipstreamSource<P> {
    /// Wrap a provider as an Aerodrome Slipstream state source.
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
    let slot0 = ICLPool::slot0Call::abi_decode_returns(&results[0].return_data).ok()?;
    let fee = ICLPool::feeCall::abi_decode_returns(&results[1].return_data).ok()?;
    let liquidity = ICLPool::liquidityCall::abi_decode_returns(&results[2].return_data).ok()?;
    let spacing = ICLPool::tickSpacingCall::abi_decode_returns(&results[3].return_data).ok()?;
    Some(PoolState {
        address,
        assets,
        id: PoolId::new(&format!(
            "{}:{}:{}",
            key.chain.0,
            key.exchange.as_str(),
            key.address
        )),
        sqrt_price_x96: U256::from(slot0.sqrtPriceX96),
        tick: slot0.tick.as_i32(),
        liquidity,
        fee_pips: fee.to::<u32>(),
        tick_spacing: spacing.as_i32(),
    })
}

/// Round 1: `slot0` + `fee` + `liquidity` + `tickSpacing` per parseable pool.
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
        calls.push(at(ICLPool::slot0Call {}.abi_encode()));
        calls.push(at(ICLPool::feeCall {}.abi_encode()));
        calls.push(at(ICLPool::liquidityCall {}.abi_encode()));
        calls.push(at(ICLPool::tickSpacingCall {}.abi_encode()));
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
                call_data: ICLPool::ticksCall { tick: tick24 }.abi_encode().into(),
            });
            refs.push((idx, tick));
        }
    }
    (calls, refs)
}

/// Reconstruct each pool's initialized ticks from the round-2 results. Only the
/// first two `ticks()` fields (gross/net) feed the swap math; the rest are
/// Slipstream's staked/reward bookkeeping and are ignored.
fn tick_windows(
    n: usize,
    results: &[CallResult],
    refs: &[(usize, i32)],
) -> Vec<Vec<(i32, TickInfo)>> {
    let mut windows: Vec<Vec<(i32, TickInfo)>> = vec![Vec::new(); n];
    for (&(state_idx, tick), result) in refs.iter().zip(results) {
        let Ok(decoded) = ICLPool::ticksCall::abi_decode_returns(&result.return_data) else {
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

/// Build an [`AerodromeSlipstreamPool`] from a decoded state and its tick window.
fn build_pool(state: PoolState, ticks: Vec<(i32, TickInfo)>) -> Box<dyn Pool> {
    let tick_data = TickData::from_ticks(state.tick_spacing, ticks);
    Box::new(AerodromeSlipstreamPool::new(
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
impl<P: Provider + Send + Sync> StateSource for SlipstreamSource<P> {
    async fn discover(
        &self,
        _chain: &amm_core::primitives::asset::ChainId,
        _assets: &[AssetId],
    ) -> Result<Vec<PoolKey>, RpcError> {
        Err(RpcError::Internal(
            "SlipstreamSource::discover is not yet implemented; build PoolKeys from the CL factory getPool(a, b, tickSpacing) or config".into(),
        ))
    }

    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError> {
        // Round 1: price + fee + liquidity + spacing for every pool.
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
    use amm_core::primitives::pool::ExchangeId;

    fn usdc() -> AssetId {
        AssetId::new(ChainId(8453), B256::left_padding_from(&[0x01]))
    }
    fn weth() -> AssetId {
        AssetId::new(ChainId(8453), B256::left_padding_from(&[0x02]))
    }

    /// sqrtPriceX96 for tick 0 (1:1) = 2^96.
    const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;

    /// Build a full-range pool from a decoded state + its tick window (the shape
    /// a refresh assembles) and quote through it — network-free. Same fee-band
    /// behaviour as V3 (shared engine), tagged Slipstream.
    #[test]
    fn build_full_range_pool_quotes_in_the_fee_band() {
        let liq: i128 = 1_000_000_000_000_000_000;
        let state = PoolState {
            address: Address::ZERO,
            assets: [usdc(), weth()],
            id: PoolId::new("8453:aerodrome-slipstream:0xfull"),
            sqrt_price_x96: U256::from(SQRT_1_1),
            tick: 0,
            liquidity: liq as u128,
            fee_pips: 500,
            tick_spacing: 100,
        };
        let ticks = vec![
            (
                -887_200,
                TickInfo {
                    liquidity_net: liq,
                    initialized: true,
                },
            ),
            (
                887_200,
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
        // 0.05% fee → output just below input.
        assert!(out.raw < amount_in);
        assert!(out.raw > amount_in * U256::from(999u64) / U256::from(1000u64));
    }

    /// End-to-end refresh against a forked Base RPC (Slipstream is a Base
    /// deployment). Gated: set `AMM_RPC_BASE_FORK_URL` and run with
    /// `cargo test -p amm-rpc -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a forked Base RPC at $AMM_RPC_BASE_FORK_URL"]
    async fn refresh_base_weth_usdc_slipstream_pool_quotes() {
        use alloy::primitives::address;

        let Ok(url) = std::env::var("AMM_RPC_BASE_FORK_URL") else {
            return;
        };
        let provider = crate::provider::make_provider(&url).unwrap();
        let source = SlipstreamSource::new(provider);

        // Base Aerodrome Slipstream WETH/USDC pool (tick spacing 100).
        // token0 = WETH, token1 = USDC (address-sorted).
        let weth = AssetId::new(
            ChainId(8453),
            address!("0x4200000000000000000000000000000000000006").into_word(),
        );
        let usdc = AssetId::new(
            ChainId(8453),
            address!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").into_word(),
        );
        let key = PoolKey {
            exchange: ExchangeId::new("aerodrome-slipstream"),
            chain: ChainId(8453),
            address: "0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59".to_string(),
            assets: vec![weth, usdc],
            fee_bps: None,
        };

        let pools = source.refresh(&[key], BlockId::latest()).await.unwrap();
        assert_eq!(pools.len(), 1);
        let out = pools[0]
            .quote(
                &AssetAmount::new(weth, U256::from(1_000_000_000_000_000_000u128)),
                &usdc,
            )
            .unwrap();
        assert!(
            out.raw > U256::ZERO,
            "refreshed Slipstream pool must quote a positive output"
        );
    }
}
