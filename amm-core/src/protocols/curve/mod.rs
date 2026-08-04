//! Curve-family quoter: [`pool::CurvePool`] wraps the `curve-math` engine, which
//! covers every StableSwap and CryptoSwap variant. The N-asset coin-index
//! resolution shared by the trait impls lives here.

pub mod pool;

use crate::primitives::asset::AssetId;

/// Resolve `(i, j)` coin indices for an N-asset pool.
///
/// `None` if either coin is absent from the pool or `i == j`.
pub(crate) fn coin_indices(
    coins: &[AssetId],
    from: &AssetId,
    to: &AssetId,
) -> Option<(usize, usize)> {
    let i = coins.iter().position(|c| c == from)?;
    let j = coins.iter().position(|c| c == to)?;
    match i == j {
        true => None,
        false => Some((i, j)),
    }
}
