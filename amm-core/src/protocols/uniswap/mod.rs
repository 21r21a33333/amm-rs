//! Uniswap-family quoters (2-asset AMMs): V2 (constant product) and V3/V4
//! (concentrated liquidity, sharing an internal tick engine). The shared
//! two-asset direction plumbing lives here.

#[cfg(any(feature = "uniswap-v3", feature = "uniswap-v4"))]
mod concentrated;
#[cfg(feature = "uniswap-v2")]
pub mod v2;
#[cfg(feature = "uniswap-v3")]
pub mod v3;
#[cfg(feature = "uniswap-v4")]
pub mod v4;

use crate::primitives::asset::AssetId;

/// Resolve swap direction for a 2-asset pool.
///
/// `Some(true)` ⇒ `assets[0] → assets[1]` (`zero_for_one`); `Some(false)` ⇒ the
/// reverse; `None` if `(from, to)` is not this pool's pair. Shared by every
/// Uniswap-family quoter.
pub(crate) fn two_asset_direction(
    assets: &[AssetId; 2],
    from: &AssetId,
    to: &AssetId,
) -> Option<bool> {
    match (from, to) {
        (f, t) if *f == assets[0] && *t == assets[1] => Some(true),
        (f, t) if *f == assets[1] && *t == assets[0] => Some(false),
        _ => None,
    }
}
