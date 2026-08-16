//! Route and span builder helpers for the execution matrix (Plan 3).
//!
//! These functions are consumed by the Plan 3 runner (Task 7+); they are
//! compiled here so the harness is ready.  The module-level `dead_code` allow
//! keeps clippy clean during Phase 1 before the runner imports them.
#![allow(dead_code)]

use alloy::providers::Provider;
use amm_core::primitives::asset::AssetId;
use amm_core::traits::pool::Pool;
use amm_rpc::execution::TradeType;
use amm_rpc::execution::routing::{Route, RouterKind, RouterSpan};

/// Fetch and refresh each named fixture at `block`, returning a `Box<dyn Pool>` per name.
///
/// Order is preserved: `pools[i]` corresponds to `names[i]`.
pub async fn refresh_pools(
    names: &[&str],
    provider: impl Provider + Clone,
    block: u64,
) -> Vec<Box<dyn Pool>> {
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        out.push(
            super::fixtures::refresh(super::fixtures::fixture(n), provider.clone(), block).await,
        );
    }
    out
}

/// Construct a [`Route`] from a slice of pool references, a token path, and a trade direction.
pub fn make_route<'a>(
    pools: Vec<&'a dyn Pool>,
    path: Vec<AssetId>,
    trade_type: TradeType,
) -> Route<'a> {
    Route {
        pools,
        path,
        trade_type,
    }
}

/// Construct a [`RouterSpan`] covering `pools` through the Uniswap Universal Router.
pub fn make_uniswap_span(pools: std::ops::Range<usize>) -> RouterSpan {
    RouterSpan {
        kind: RouterKind::UniswapUniversal,
        pools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_uniswap_span_shape() {
        let span = make_uniswap_span(0..2);
        assert_eq!(span.kind, RouterKind::UniswapUniversal);
        assert_eq!(span.pools, 0..2);
    }
}
