//! Aerodrome (Solidly) on-chain state source: batch-fetch each pool's reserves,
//! stable flag, token decimals, and fee, then build the matching volatile or
//! stable quoter.
//!
//! Six calls per pool go into one block-pinned Multicall3 `aggregate3`:
//! `getReserves`, `stable`, `token0.decimals`, `token1.decimals`, and both
//! `getFee` variants (the stable flag then selects which fee applies).

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use amm_core::primitives::asset::AssetId;
use amm_core::primitives::pool::{PoolId, PoolKey};
use amm_core::protocols::aerodrome::stable::AerodromeStablePool;
use amm_core::protocols::aerodrome::volatile::AerodromeVolatilePool;
use amm_core::traits::pool::Pool;

use crate::error::RpcError;
use crate::multicall::{self, Call, CallResult};
use crate::source::StateSource;

/// The number of calls batched per pool (see the module docs).
const CALLS_PER_POOL: usize = 6;

sol! {
    #[sol(rpc)]
    interface IAerodromePool {
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
        function stable() external view returns (bool);
    }
    #[sol(rpc)]
    interface IAerodromeFactory {
        function getFee(address pool, bool stable) external view returns (uint256);
    }
    #[sol(rpc)]
    interface IErc20 {
        function decimals() external view returns (uint8);
    }
}

/// A [`StateSource`] for Aerodrome volatile + stable pools over a provider `P`.
///
/// `refresh` is implemented; `discover` (factory `getPool` enumeration) is a
/// follow-up. Slipstream (concentrated) pools are handled by the V3-family
/// source, not here.
pub struct AerodromeSource<P> {
    provider: P,
    factory: Address,
}

impl<P: Provider> AerodromeSource<P> {
    /// Wrap a provider and the Aerodrome `PoolFactory` address.
    pub fn new(provider: P, factory: Address) -> Self {
        Self { provider, factory }
    }
}

/// The six calls that fetch one pool's state, in a fixed order.
fn pool_calls(
    factory: Address,
    pool: Address,
    token0: Address,
    token1: Address,
) -> [Call; CALLS_PER_POOL] {
    let fee_call = |stable: bool| Call {
        target: factory,
        call_data: IAerodromeFactory::getFeeCall { pool, stable }
            .abi_encode()
            .into(),
    };
    [
        Call {
            target: pool,
            call_data: IAerodromePool::getReservesCall {}.abi_encode().into(),
        },
        Call {
            target: pool,
            call_data: IAerodromePool::stableCall {}.abi_encode().into(),
        },
        Call {
            target: token0,
            call_data: IErc20::decimalsCall {}.abi_encode().into(),
        },
        Call {
            target: token1,
            call_data: IErc20::decimalsCall {}.abi_encode().into(),
        },
        fee_call(true),
        fee_call(false),
    ]
}

/// The decoded state one pool's six results carry.
struct PoolState {
    reserve0: U256,
    reserve1: U256,
    stable: bool,
    decimals0: u8,
    decimals1: u8,
    fee_bps: u32,
}

/// Decode one pool's six [`CallResult`]s, or `None` if any required read
/// reverted or failed to decode.
fn decode_state(results: &[CallResult]) -> Option<PoolState> {
    let reserves =
        IAerodromePool::getReservesCall::abi_decode_returns(&results[0].return_data).ok()?;
    let stable = IAerodromePool::stableCall::abi_decode_returns(&results[1].return_data).ok()?;
    let decimals0 = IErc20::decimalsCall::abi_decode_returns(&results[2].return_data).ok()?;
    let decimals1 = IErc20::decimalsCall::abi_decode_returns(&results[3].return_data).ok()?;
    // Pick the fee variant the stable flag selects.
    let fee_result = match stable {
        true => &results[4],
        false => &results[5],
    };
    let fee = IAerodromeFactory::getFeeCall::abi_decode_returns(&fee_result.return_data).ok()?;
    Some(PoolState {
        reserve0: reserves.reserve0,
        reserve1: reserves.reserve1,
        stable,
        decimals0,
        decimals1,
        fee_bps: u32::try_from(fee).unwrap_or(u32::MAX),
    })
}

/// Build the matching Aerodrome quoter from a key and its decoded state. `None`
/// if the key is not a 2-asset pool. `reserves`/`decimals` are index-aligned
/// with `key.assets` (address-sorted `[token0, token1]`).
fn build_pool(key: &PoolKey, state: &PoolState) -> Option<Box<dyn Pool>> {
    let assets: [AssetId; 2] = match key.assets.as_slice() {
        [a, b] => [*a, *b],
        _ => return None,
    };
    let id = PoolId::new(&format!(
        "{}:{}:{}",
        key.chain.0,
        key.exchange.as_str(),
        key.address
    ));
    let reserves = [state.reserve0, state.reserve1];
    match state.stable {
        true => Some(Box::new(AerodromeStablePool::new(
            id,
            assets,
            reserves,
            [state.decimals0, state.decimals1],
            state.fee_bps,
        ))),
        false => Some(Box::new(AerodromeVolatilePool::new(
            id,
            assets,
            reserves,
            state.fee_bps,
        ))),
    }
}

#[async_trait::async_trait]
impl<P: Provider + Send + Sync> StateSource for AerodromeSource<P> {
    async fn discover(
        &self,
        _chain: &amm_core::primitives::asset::ChainId,
        _assets: &[AssetId],
    ) -> Result<Vec<PoolKey>, RpcError> {
        Err(RpcError::Internal(
            "AerodromeSource::discover is not yet implemented; build PoolKeys from a subgraph or config".into(),
        ))
    }

    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError> {
        // Six calls per parseable 2-asset key; track which keys made it in so the
        // fixed-stride result chunks line up.
        let mut calls: Vec<Call> = Vec::new();
        let mut targets: Vec<&PoolKey> = Vec::new();
        for key in keys {
            let pool = match key.address.parse::<Address>() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let (token0, token1) = match key.assets.as_slice() {
                [a, b] => (Address::from_word(a.token), Address::from_word(b.token)),
                _ => continue,
            };
            calls.extend(pool_calls(self.factory, pool, token0, token1));
            targets.push(key);
        }

        let results = multicall::aggregate3(&self.provider, calls, at).await?;

        let pools = targets
            .into_iter()
            .enumerate()
            .filter_map(|(i, key)| {
                let chunk = results.get(i * CALLS_PER_POOL..(i + 1) * CALLS_PER_POOL)?;
                let state = decode_state(chunk)?;
                build_pool(key, &state)
            })
            .collect();
        Ok(pools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use amm_core::primitives::asset::{AssetAmount, ChainId};
    use amm_core::primitives::pool::ExchangeId;

    fn asset(byte: u8) -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[byte]))
    }

    fn key() -> PoolKey {
        PoolKey {
            exchange: ExchangeId::new("aerodrome"),
            chain: ChainId(1),
            address: "0x00000000000000000000000000000000000000aa".to_string(),
            assets: vec![asset(0x01), asset(0x02)],
            fee_bps: None,
        }
    }

    #[test]
    fn stable_flag_selects_the_stable_quoter() {
        let e18 = 1_000_000_000_000_000_000u128;
        let state = PoolState {
            reserve0: U256::from(1_000_000u128 * e18),
            reserve1: U256::from(1_000_000u128 * e18),
            stable: true,
            decimals0: 18,
            decimals1: 18,
            fee_bps: 5,
        };
        let pool = build_pool(&key(), &state).unwrap();
        // Near-parity swap on a balanced stable pool (proves the stable curve, not
        // constant product, was built).
        let out = pool
            .quote(
                &AssetAmount::new(asset(0x01), U256::from(e18)),
                &asset(0x02),
            )
            .unwrap();
        assert!(out.raw > U256::from(99u128 * e18 / 100) && out.raw < U256::from(e18));
    }

    #[test]
    fn no_stable_flag_selects_the_volatile_quoter_and_uses_fetched_fee() {
        let state = PoolState {
            reserve0: U256::from(1_000_000u64),
            reserve1: U256::from(1_000_000u64),
            stable: false,
            decimals0: 6,
            decimals1: 6,
            fee_bps: 30,
        };
        let pool = build_pool(&key(), &state).unwrap();
        // Fee-first constant product (proves the volatile quoter, not the stable
        // curve, was built): 1000 in, 30 bps → 996 out.
        let out = pool
            .quote(
                &AssetAmount::new(asset(0x01), U256::from(1000u64)),
                &asset(0x02),
            )
            .unwrap();
        assert_eq!(out.raw, U256::from(996u64));
    }

    #[test]
    fn build_pool_rejects_a_non_two_asset_key() {
        let mut k = key();
        k.assets = vec![asset(0x01)];
        let state = PoolState {
            reserve0: U256::from(1u64),
            reserve1: U256::from(1u64),
            stable: false,
            decimals0: 18,
            decimals1: 18,
            fee_bps: 30,
        };
        assert!(build_pool(&k, &state).is_none());
    }
}
