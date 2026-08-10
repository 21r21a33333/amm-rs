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
#[cfg(any(feature = "uniswap-v3", feature = "uniswap-v4", feature = "aerodrome"))]
pub use concentrated::sqrt_price_limit_x96;
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

/// Tighten an exact-out input to the wei by binary search. Given a `candidate`
/// that already covers `target` (`forward(candidate) ≥ target`) and a monotonic
/// non-decreasing `forward` quote, returns the smallest input that still delivers
/// at least `target`. `forward` returns `None` for an input too small to quote,
/// treated as under-delivering.
///
/// Solvers that invert the swap curve (Newton, closed-form ceilings) round their
/// input up for safety and can land a few — or, on decimal-mismatched pools where
/// the output barely moves per input wei, many — wei above the true minimum. The
/// search converges in `log2(candidate)` steps, so a loose upper bound is
/// tightened without an unbounded linear scan.
#[cfg(any(feature = "aerodrome", feature = "curve"))]
pub(crate) fn minimal_exact_out_input(
    candidate: alloy_primitives::U256,
    target: alloy_primitives::U256,
    forward: impl Fn(alloy_primitives::U256) -> Option<alloy_primitives::U256>,
) -> alloy_primitives::U256 {
    use alloy_primitives::U256;
    // Invariant: forward(lo) < target ≤ forward(hi). lo = 0 under-delivers a
    // positive target; hi = candidate covers it by contract.
    let (mut lo, mut hi) = (U256::ZERO, candidate);
    while hi - lo > U256::from(1u64) {
        let mid = lo + (hi - lo) / U256::from(2u64);
        match forward(mid) {
            Some(out) if out >= target => hi = mid,
            _ => lo = mid,
        }
    }
    hi
}

#[cfg(all(test, any(feature = "aerodrome", feature = "curve")))]
mod tests {
    use super::minimal_exact_out_input;
    use alloy_primitives::U256;

    #[test]
    fn tightens_across_a_flat_forward_without_a_linear_scan() {
        // `forward(x) = ⌊x / 1e6⌋` stays flat for a million-wei band before it
        // clears the target — the decimal-mismatch shape a per-wei scan hangs on.
        let target = U256::from(5u64);
        let forward = |x: U256| Some(x / U256::from(1_000_000u64));
        // A candidate a whole flat band above the true minimum (5e6).
        let needed = minimal_exact_out_input(U256::from(6_000_000u64), target, forward);
        assert_eq!(needed, U256::from(5_000_000u64));
        assert!(forward(needed - U256::from(1u64)).unwrap() < target);
    }

    #[test]
    fn treats_an_unquotable_input_as_under_delivering() {
        // `forward` returns `None` below a floor (too small to quote).
        let target = U256::from(10u64);
        let forward = |x: U256| (x >= U256::from(3u64)).then_some(x);
        assert_eq!(
            minimal_exact_out_input(U256::from(100u64), target, forward),
            U256::from(10u64)
        );
    }
}
