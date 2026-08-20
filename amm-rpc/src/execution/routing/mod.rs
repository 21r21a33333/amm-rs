//! The routing layer's public types. A `Route` is the single execution-time
//! route type — one or more pools traversed in order. A single-hop swap is
//! just a 1-pool route. The planner turns a `Route` into encoded calldata by
//! dispatching to per-pool `Executable` encoders.

pub mod family;
pub mod router_span;
pub use router_span::{RouterKind, RouterSpan, partition};

use amm_core::primitives::asset::AssetId;
use amm_core::traits::pool::Pool;

use crate::execution::error::BuildError;
use crate::execution::types::TradeType;

/// An ordered route through one or more liquidity pools.
///
/// Invariant enforced by [`Route::validate`]: `path.len() == pools.len() + 1`.
/// Pool `i` swaps `path[i] → path[i+1]`; both assets must belong to that pool.
pub struct Route<'a> {
    /// The pools to traverse, in order.
    pub pools: Vec<&'a dyn Pool>,
    /// The token sequence: `[in, hop₁, …, out]`. One entry longer than `pools`.
    pub path: Vec<AssetId>,
    /// Whether the exact constraint is on the input or output side.
    pub trade_type: TradeType,
}

/// What to do when a pool along the route cannot perform a true exact-out swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExactOutPolicy {
    /// Return an error rather than approximate.
    Strict,
    /// Backward-solve and execute as exact-in with `min_out = target`; the
    /// caller receives at least the requested amount.
    OrBetter,
}

impl<'a> Route<'a> {
    /// Structural validation: non-empty, path/pool arity, and per-hop asset
    /// membership. Returns `Ok(())` on a well-formed route.
    ///
    /// Errors:
    /// - [`BuildError::EmptyRoute`] — `pools` is empty.
    /// - [`BuildError::DisjointRoute { at }`] — `path.len() != pools.len() + 1`
    ///   (reported as `at: 0`), or hop `at`'s two path assets are equal or not
    ///   both present in `pools[at]`.
    pub fn validate(&self) -> Result<(), BuildError> {
        // A route with no pools cannot execute any swap.
        if self.pools.is_empty() {
            return Err(BuildError::EmptyRoute);
        }

        // The path must have exactly one more entry than the pool list.
        if self.path.len() != self.pools.len() + 1 {
            return Err(BuildError::DisjointRoute { at: 0 });
        }

        // Each hop: the two path assets must be distinct and both present in
        // the pool at that position.
        for (i, pool) in self.pools.iter().enumerate() {
            let (from, to) = (self.path[i], self.path[i + 1]);
            let assets = pool.assets();
            if from == to || !assets.contains(&from) || !assets.contains(&to) {
                return Err(BuildError::DisjointRoute { at: i });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, U256};
    use amm_core::primitives::asset::{AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::protocols::uniswap::v2::UniswapV2Pool;

    /// Build a throwaway `AssetId` from a single discriminant byte.
    fn asset(byte: u8) -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[byte]))
    }

    /// Build a minimal UniswapV2Pool with the given two assets.
    fn pool(a: AssetId, b: AssetId) -> UniswapV2Pool {
        UniswapV2Pool::new(
            PoolId::new("1:univ2:test"),
            [a, b],
            [U256::from(1_000_000u64); 2],
            30,
        )
    }

    #[test]
    fn valid_two_hop_route_passes_validation() {
        let (a, b, c) = (asset(0xaa), asset(0xbb), asset(0xcc));
        let p1 = pool(a, b);
        let p2 = pool(b, c);
        let r = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        assert!(r.validate().is_ok());
    }

    #[test]
    fn empty_route_is_rejected() {
        let r: Route<'_> = Route {
            pools: vec![],
            path: vec![],
            trade_type: TradeType::ExactIn,
        };
        assert!(matches!(r.validate(), Err(BuildError::EmptyRoute)));
    }

    #[test]
    fn path_length_must_be_pools_plus_one() {
        let (a, b, c) = (asset(0xaa), asset(0xbb), asset(0xcc));
        let p1 = pool(a, b);
        // path has 3 entries but only 1 pool — 3 != 1+1
        let r = Route {
            pools: vec![&p1],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        assert!(matches!(
            r.validate(),
            Err(BuildError::DisjointRoute { .. })
        ));
    }

    #[test]
    fn hop_assets_must_be_in_their_pool() {
        let (a, b, c, z) = (asset(0xaa), asset(0xbb), asset(0xcc), asset(0xff));
        let p1 = pool(a, b); // knows about a and b only
        let p2 = pool(b, c);
        // path claims a→z at hop 0, but z ∉ p1
        let r = Route {
            pools: vec![&p1, &p2],
            path: vec![a, z, c],
            trade_type: TradeType::ExactIn,
        };
        assert!(matches!(
            r.validate(),
            Err(BuildError::DisjointRoute { at: 0 })
        ));
    }
}
