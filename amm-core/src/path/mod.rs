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

/// Boundary amounts along a path: `[start, out₁, …, outₙ]` (length `hops.len() + 1`),
/// threading each hop's output into the next. `amounts[i]` is what enters hop `i`;
/// `amounts[hops.len()]` is the final output.
pub fn quote_path_amounts(
    start: &AssetAmount,
    hops: &[Hop<'_>],
) -> Result<Vec<AssetAmount>, QuoteError> {
    let mut amounts = Vec::with_capacity(hops.len() + 1);
    amounts.push(*start);
    let mut amount = *start;
    for hop in hops {
        amount = hop.pool.quote(&amount, &hop.to)?;
        amounts.push(amount);
    }
    Ok(amounts)
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

/// Required boundary amounts for an exact-out path, solved backward from the
/// desired final output: `[in₀, …, inₙ₋₁, target]` (length `hops.len() + 1`).
/// `from` for hop `i` is `hops[i-1].to`, or `start_asset` for the first hop.
/// Errors `ExactOutUnavailable` if any pool lacks `ExactOut`.
pub fn quote_path_exact_out(
    target: &AssetAmount,
    start_asset: AssetId,
    hops: &[Hop<'_>],
) -> Result<Vec<AssetAmount>, QuoteError> {
    let mut required = vec![*target; hops.len() + 1];
    let mut want = *target;
    for i in (0..hops.len()).rev() {
        let from = match i {
            0 => start_asset,
            _ => hops[i - 1].to,
        };
        let ex = hops[i]
            .pool
            .as_exact_out()
            .ok_or_else(|| QuoteError::ExactOutUnavailable {
                pool: hops[i].pool.id().clone(),
            })?;
        want = ex.quote_exact_out(&want, &from)?;
        required[i] = want;
    }
    Ok(required)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::ChainId;
    use crate::primitives::pool::PoolId;
    use crate::traits::exact_out::ExactOut;
    use alloy_primitives::{B256, U256};

    fn asset(low: u8) -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[low]))
    }

    /// A pool that doubles the input on exact-in; halves it on exact-out so
    /// the two directions round-trip cleanly in tests.
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

        fn as_exact_out(&self) -> Option<&dyn ExactOut> {
            Some(self)
        }
    }

    impl ExactOut for Doubler {
        fn quote_exact_out(
            &self,
            amount_out: &AssetAmount,
            from: &AssetId,
        ) -> Result<AssetAmount, QuoteError> {
            match self.assets.contains(from) && self.assets.contains(&amount_out.asset) {
                false => Err(QuoteError::AssetNotInPool {
                    input: *from,
                    output: amount_out.asset,
                }),
                // Inverse of doubling: divide by 2.
                true => Ok(AssetAmount::new(*from, amount_out.raw / U256::from(2u64))),
            }
        }
    }

    fn doubler(id: &str, x: AssetId, y: AssetId) -> Doubler {
        Doubler {
            id: PoolId::new(id),
            assets: vec![x, y],
        }
    }

    /// A pool that has no exact-out capability; used to test the error path.
    struct NoExactOut {
        id: PoolId,
        assets: Vec<AssetId>,
    }

    impl Pool for NoExactOut {
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
                true => Ok(AssetAmount::new(*to, amount_in.raw)),
            }
        }
        // `as_exact_out` is not overridden — defaults to `None`.
    }

    fn no_exact_out(id: &str, x: AssetId, y: AssetId) -> NoExactOut {
        NoExactOut {
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
    fn quote_path_amounts_returns_each_boundary() {
        let a = asset(1);
        let b = asset(2);
        let c = asset(3);
        let p1 = doubler("p1", a, b);
        let p2 = doubler("p2", b, c);
        let hops = [Hop { pool: &p1, to: b }, Hop { pool: &p2, to: c }];
        let start = AssetAmount::new(a, U256::from(10u64));
        let amounts = quote_path_amounts(&start, &hops).unwrap();
        assert_eq!(amounts.len(), 3);
        assert_eq!(amounts[0].raw, U256::from(10u64)); // start
        assert_eq!(amounts[1].raw, U256::from(20u64)); // after p1
        assert_eq!(amounts[2].raw, U256::from(40u64)); // after p2
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

    #[test]
    fn quote_path_exact_out_solves_backward() {
        let a = asset(1);
        let b = asset(2);
        let c = asset(3);
        let p1 = doubler("p1", a, b); // exact-in doubles; exact-out halves
        let p2 = doubler("p2", b, c);
        let hops = [Hop { pool: &p1, to: b }, Hop { pool: &p2, to: c }];
        let target = AssetAmount::new(c, U256::from(40u64));
        let ins = quote_path_exact_out(&target, a, &hops).unwrap();
        assert_eq!(ins.len(), 3);
        assert_eq!(ins[0].raw, U256::from(10u64)); // required input to p1
        assert_eq!(ins[1].raw, U256::from(20u64)); // required input to p2 (== p1 out)
        assert_eq!(ins[2].raw, U256::from(40u64)); // target unchanged
    }

    #[test]
    fn quote_path_exact_out_errors_without_exact_out() {
        let a = asset(1);
        let b = asset(2);
        let p = no_exact_out("no-eo", a, b);
        let hops = [Hop { pool: &p, to: b }];
        let target = AssetAmount::new(b, U256::from(10u64));
        let result = quote_path_exact_out(&target, a, &hops);
        assert!(matches!(
            result,
            Err(QuoteError::ExactOutUnavailable { .. })
        ));
    }
}
