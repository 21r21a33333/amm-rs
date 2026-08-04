//! Multi-hop composition: quote a swap through a sequence of pools.

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::traits::pool::Pool;

/// One hop of a path: quote through `pool`, producing the asset `to`.
pub struct Hop<'a> {
    /// The pool to quote through.
    pub pool: &'a dyn Pool,
    /// The output asset of this hop (becomes the next hop's input).
    pub to: AssetId,
}

/// Quote a swap through `hops`, threading each hop's output into the next.
///
/// Returns the final output amount, or the first hop's [`QuoteError`].
pub fn quote_path(start: &AssetAmount, hops: &[Hop<'_>]) -> Result<AssetAmount, QuoteError> {
    let mut amount = *start;
    for hop in hops {
        amount = hop.pool.quote(&amount, &hop.to)?;
    }
    Ok(amount)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::ChainId;
    use crate::primitives::pool::PoolId;
    use alloy_primitives::{B256, U256};

    fn asset(low: u8) -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[low]))
    }

    /// A pool that doubles the input; used to exercise the threading logic.
    struct Doubler {
        id: PoolId,
        assets: Vec<AssetId>,
    }

    impl Pool for Doubler {
        fn id(&self) -> &PoolId {
            &self.id
        }

        fn assets(&self) -> &[AssetId] {
            &self.assets
        }

        fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
            match self.assets.contains(&amount_in.asset) && self.assets.contains(to) {
                false => Err(QuoteError::AssetNotInPool {
                    input: amount_in.asset,
                    output: *to,
                }),
                true => Ok(AssetAmount::new(*to, amount_in.raw * U256::from(2u64))),
            }
        }
    }

    fn doubler(id: &str, x: AssetId, y: AssetId) -> Doubler {
        Doubler {
            id: PoolId::new(id),
            assets: vec![x, y],
        }
    }

    #[test]
    fn quote_path_threads_amount_through_hops() {
        let (a, b, c) = (asset(0xaa), asset(0xbb), asset(0xcc));
        let (p1, p2) = (doubler("1:d:0x1", a, b), doubler("1:d:0x2", b, c));
        let hops = [Hop { pool: &p1, to: b }, Hop { pool: &p2, to: c }];
        let out = quote_path(&AssetAmount::new(a, U256::from(10u64)), &hops).unwrap();
        assert_eq!(out.asset, c);
        assert_eq!(out.raw, U256::from(40u64)); // 10 -> 20 -> 40
    }

    #[test]
    fn quote_path_propagates_hop_error() {
        let (a, b, x) = (asset(0xaa), asset(0xbb), asset(0xff));
        let p1 = doubler("1:d:0x1", a, b);
        let hops = [Hop { pool: &p1, to: x }]; // p1 does not trade `x`
        assert!(matches!(
            quote_path(&AssetAmount::new(a, U256::from(10u64)), &hops),
            Err(QuoteError::AssetNotInPool { .. })
        ));
    }
}
