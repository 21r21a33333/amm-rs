//! The [`StateSource`] trait: discover pools for a set of assets and refresh
//! their on-chain state into quotable [`Pool`] values.

use alloy::eips::BlockId;
use amm_core::primitives::asset::{AssetId, ChainId};
use amm_core::primitives::pool::{PoolId, PoolKey};
use amm_core::traits::pool::Pool;

use crate::error::RpcError;

/// The canonical identity for a fetched pool: `chain:exchange:address`. Shared by
/// every source so the id format lives in one place.
pub(crate) fn pool_id(key: &PoolKey) -> PoolId {
    PoolId::new(&format!(
        "{}:{}:{}",
        key.chain.0,
        key.exchange.as_str(),
        key.address
    ))
}

/// A source of on-chain pool state.
///
/// Implementors fetch pool descriptors and state from a chain and hand back
/// quotable [`Pool`] trait objects. Consumers that already hold pool state
/// (subgraph, database, fixtures) can skip this entirely and construct the
/// `amm-core` pool structs directly.
#[async_trait::async_trait]
pub trait StateSource {
    /// Discover the pools on `chain` that trade among `assets`, returning their
    /// descriptors (no swap state yet).
    async fn discover(&self, chain: &ChainId, assets: &[AssetId])
    -> Result<Vec<PoolKey>, RpcError>;

    /// Refresh the on-chain state of `keys` as of block `at`, returning a
    /// quotable [`Pool`] per key that decoded successfully. Pools whose reads
    /// reverted or failed to decode are omitted rather than failing the batch.
    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError>;
}
