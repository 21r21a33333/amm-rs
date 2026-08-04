//! Per-protocol pure quoters.
//!
//! Each submodule adapts one AMM family to the core [`Pool`](crate::traits::pool)
//! trait, wei-exact against that protocol's on-chain contract. Quoters are pure:
//! they hold a snapshot of pool state and do no I/O (state fetching lives in the
//! separate `amm-rpc` crate).

#[cfg(feature = "aerodrome")]
pub mod aerodrome;
#[cfg(any(feature = "uniswap-v3", feature = "uniswap-v4", feature = "aerodrome"))]
mod concentrated;
#[cfg(feature = "curve")]
pub mod curve;
#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3", feature = "uniswap-v4"))]
pub mod uniswap;

#[cfg(any(
    feature = "uniswap-v2",
    feature = "uniswap-v3",
    feature = "uniswap-v4",
    feature = "aerodrome"
))]
use crate::primitives::asset::AssetId;

/// Resolve swap direction for a 2-asset pool.
///
/// `Some(true)` ⇒ `assets[0] → assets[1]` (`zero_for_one`); `Some(false)` ⇒ the
/// reverse; `None` if `(from, to)` is not this pool's pair. Shared by every
/// 2-asset quoter (Uniswap V2/V3/V4, Aerodrome).
#[cfg(any(
    feature = "uniswap-v2",
    feature = "uniswap-v3",
    feature = "uniswap-v4",
    feature = "aerodrome"
))]
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
