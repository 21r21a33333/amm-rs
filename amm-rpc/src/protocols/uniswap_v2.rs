//! Uniswap V2 on-chain state source: batch-fetch `getReserves()` for a set of
//! pools and decode each into a quotable [`UniswapV2Pool`].
//!
//! `refresh` pins the read to a block and uses one Multicall3 round trip
//! (`tryAggregate(false, …)`), so a reverting pool surfaces as a skipped entry
//! rather than failing the whole batch.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use amm_core::primitives::asset::AssetId;
use amm_core::primitives::pool::{PoolId, PoolKey};
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_core::traits::pool::Pool;

use crate::error::RpcError;
use crate::source::StateSource;

/// The default Uniswap V2 swap fee (30 bps) used when a [`PoolKey`] carries none.
const DEFAULT_FEE_BPS: u32 = 30;

sol! {
    #[sol(rpc)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }
}

/// A [`StateSource`] for Uniswap V2 (and V2-fork) pools over a provider `P`.
///
/// `refresh` is implemented; `discover` (factory `getPair` enumeration) is a
/// follow-up — construct [`PoolKey`]s from a subgraph or config for now.
pub struct UniswapV2Source<P> {
    provider: P,
}

impl<P: Provider> UniswapV2Source<P> {
    /// Wrap a provider as a Uniswap V2 state source.
    pub fn new(provider: P) -> Self {
        Self { provider }
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
        _chain: &amm_core::primitives::asset::ChainId,
        _assets: &[AssetId],
    ) -> Result<Vec<PoolKey>, RpcError> {
        Err(RpcError::Internal(
            "UniswapV2Source::discover is not yet implemented; build PoolKeys from a subgraph or config".into(),
        ))
    }

    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError> {
        // Add one getReserves() call per parseable key, tracking which keys made
        // it in so the results line up.
        let mut multicall = self
            .provider
            .multicall()
            .dynamic::<IUniswapV2Pair::getReservesCall>();
        let mut targets: Vec<&PoolKey> = Vec::new();
        for key in keys {
            match key.address.parse::<Address>() {
                Ok(address) => {
                    let pair = IUniswapV2Pair::new(address, &self.provider);
                    multicall = multicall.add_dynamic(pair.getReserves());
                    targets.push(key);
                }
                Err(_) => continue,
            }
        }

        let results = multicall
            .block(at)
            .try_aggregate(false)
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;

        // Keep the pools whose read succeeded and decoded; drop reverts.
        let pools = targets
            .into_iter()
            .zip(results)
            .filter_map(|(key, result)| {
                let r = result.ok()?;
                build_pool(key, U256::from(r.reserve0), U256::from(r.reserve1))
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
