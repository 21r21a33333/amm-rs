//! Uniswap V2 on-chain state source: batch-fetch `getReserves()` for a set of
//! pools and decode each into a quotable [`UniswapV2Pool`].
//!
//! `refresh` pins the read to a block and batches every pool's `getReserves`
//! into one Multicall3 `aggregate3` (per-call `allowFailure`), so a reverting
//! pool is skipped rather than failing the whole batch.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use amm_core::primitives::asset::{AssetId, ChainId};
use amm_core::primitives::pool::{ExchangeId, PoolId, PoolKey};
use amm_core::primitives::ratio::Bps;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_core::traits::pool::Pool;

use crate::discover;
use crate::error::RpcError;
use crate::multicall::{self, Call};
use crate::source::StateSource;

/// The default Uniswap V2 swap fee (30 bps) used when a [`PoolKey`] carries none.
const DEFAULT_FEE_BPS: u32 = 30;

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }
    #[sol(rpc)]
    interface IUniswapV2Factory {
        function getPair(address tokenA, address tokenB) external view returns (address pair);
    }
}

/// A [`StateSource`] for Uniswap V2 (and V2-fork) pools over a provider `P`.
///
/// `refresh` works with any source. `discover` (factory `getPair` enumeration)
/// requires a factory + fee — construct with [`with_factory`](Self::with_factory).
pub struct UniswapV2Source<P> {
    provider: P,
    factory: Option<Address>,
    fee_bps: u32,
}

impl<P: Provider> UniswapV2Source<P> {
    /// Wrap a provider for refresh-only use (`discover` errors without a factory).
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            factory: None,
            fee_bps: DEFAULT_FEE_BPS,
        }
    }

    /// Wrap a provider plus the V2 `factory` and the pools' swap `fee_bps`, so
    /// `discover` can enumerate pools via `getPair`.
    pub fn with_factory(provider: P, factory: Address, fee_bps: u32) -> Self {
        Self {
            provider,
            factory: Some(factory),
            fee_bps,
        }
    }
}

/// Build a [`UniswapV2Pool`] from a key and its fetched reserves. `None` if the
/// key is not a 2-asset pool. `reserves` are index-aligned with `key.assets`
/// (which must be address-sorted, i.e. `[token0, token1]`).
fn build_pool(key: &PoolKey, reserve0: U256, reserve1: U256) -> Option<Box<dyn Pool>> {
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
    let fee_bps = key.fee_bps.map_or(DEFAULT_FEE_BPS, |b| u32::from(b.0));
    Some(Box::new(UniswapV2Pool::new(
        id,
        assets,
        [reserve0, reserve1],
        fee_bps,
    )))
}

#[async_trait::async_trait]
impl<P: Provider + Send + Sync> StateSource for UniswapV2Source<P> {
    async fn discover(
        &self,
        chain: &ChainId,
        assets: &[AssetId],
    ) -> Result<Vec<PoolKey>, RpcError> {
        let Some(factory) = self.factory else {
            return Err(RpcError::Internal(
                "UniswapV2Source::discover requires a factory; construct with with_factory".into(),
            ));
        };
        let pairs = discover::sorted_pairs(assets);
        let calls: Vec<Call> = pairs
            .iter()
            .map(|(t0, t1)| Call {
                target: factory,
                call_data: IUniswapV2Factory::getPairCall {
                    tokenA: discover::asset_address(t0),
                    tokenB: discover::asset_address(t1),
                }
                .abi_encode()
                .into(),
            })
            .collect();
        let results = multicall::aggregate3(&self.provider, calls, BlockId::latest()).await?;

        let fee = Bps(self.fee_bps as u16);
        Ok(pairs
            .into_iter()
            .zip(results)
            .filter_map(|((t0, t1), result)| {
                let addr = discover::decode_pool_address(&result)?;
                Some(PoolKey {
                    exchange: ExchangeId::new("uniswap-v2"),
                    chain: *chain,
                    address: addr.to_string(),
                    assets: vec![t0, t1],
                    fee_bps: Some(fee),
                })
            })
            .collect())
    }

    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError> {
        // One getReserves() call per parseable key, tracking which keys made it in
        // so the batched results line up.
        let mut calls: Vec<Call> = Vec::new();
        let mut targets: Vec<&PoolKey> = Vec::new();
        for key in keys {
            if let Ok(target) = key.address.parse::<Address>() {
                calls.push(Call {
                    target,
                    call_data: IUniswapV2Pair::getReservesCall {}.abi_encode().into(),
                });
                targets.push(key);
            }
        }

        let results = multicall::aggregate3(&self.provider, calls, at).await?;

        // Keep the pools whose read succeeded and decoded; drop reverts.
        let pools = targets
            .into_iter()
            .zip(results)
            .filter_map(|(key, result)| {
                let decoded =
                    IUniswapV2Pair::getReservesCall::abi_decode_returns(&result.return_data)
                        .ok()?;
                build_pool(
                    key,
                    U256::from(decoded.reserve0),
                    U256::from(decoded.reserve1),
                )
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
            exchange: ExchangeId::new("uniswap-v2"),
            chain: ChainId(1),
            address: "0x00000000000000000000000000000000000000aa".to_string(),
            assets: vec![asset(0x01), asset(0x02)],
            fee_bps: None,
        }
    }

    #[test]
    fn build_pool_decodes_reserves_into_a_quotable_v2_pool() {
        // A balanced 1M/1M pool built from fetched reserves must quote like Task 10's
        // constant-product unit: 1000 in, default 30 bps → 996 out.
        let pool = build_pool(&key(), U256::from(1_000_000u64), U256::from(1_000_000u64))
            .expect("2-asset key builds a pool");
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
        assert!(build_pool(&k, U256::from(1u64), U256::from(1u64)).is_none());
    }

    /// End-to-end refresh against a forked mainnet RPC. Gated: set
    /// `AMM_RPC_FORK_URL` (e.g. `anvil --fork-url <archive>`) and run with
    /// `cargo test -p amm-rpc -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
    async fn refresh_mainnet_usdc_weth_pool_quotes() {
        use alloy::primitives::address;

        let Ok(url) = std::env::var("AMM_RPC_FORK_URL") else {
            return;
        };
        let provider = crate::provider::make_provider(&url).unwrap();
        let source = UniswapV2Source::new(provider);

        // Mainnet USDC/WETH V2 pair; token0 = USDC (0xA0b8…) < token1 = WETH (0xC02a…).
        let usdc = AssetId::new(
            ChainId(1),
            address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").into_word(),
        );
        let weth = AssetId::new(
            ChainId(1),
            address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").into_word(),
        );
        let key = PoolKey {
            exchange: ExchangeId::new("uniswap-v2"),
            chain: ChainId(1),
            address: "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc".to_string(),
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
            "refreshed pool must quote a positive output"
        );
    }
}
