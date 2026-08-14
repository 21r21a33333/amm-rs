//! Partition a route's pools into maximal runs that share one on-chain router;
//! each run becomes one atomic transaction.

use core::ops::Range;

use amm_core::primitives::pool::PoolKind;
use amm_core::traits::pool::Pool;

use crate::execution::error::BuildError;
use crate::execution::routing::Route;

/// The on-chain router a pool settles through — the grouping key.
///
/// Multiple `PoolKind` variants may share one physical router contract (e.g.
/// V2, V3, and V4 all go through Uniswap's Universal Router); this enum
/// captures that many-to-one mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouterKind {
    /// Uniswap Universal Router: handles V2, V3, and V4 pools.
    UniswapUniversal,
    /// Curve Finance router: handles StableSwap and CryptoSwap pools.
    Curve,
    /// Aerodrome/Velodrome router: handles volatile and stable AMM pools.
    Aerodrome,
    /// Aerodrome Slipstream router: concentrated liquidity (separate contract).
    Slipstream,
}

impl RouterKind {
    /// Classify a pool by its [`PoolKind`]. Returns `None` if the pool exposes
    /// no [`Introspect`] implementation (and thus cannot be routed).
    ///
    /// [`Introspect`]: amm_core::traits::introspect::Introspect
    pub fn of(pool: &dyn Pool) -> Option<RouterKind> {
        let kind = pool.as_introspect()?.kind();
        let router = match kind {
            // V2, V3, and V4 all settle through the Uniswap Universal Router.
            PoolKind::UniswapV2 | PoolKind::UniswapV3 | PoolKind::UniswapV4 => {
                RouterKind::UniswapUniversal
            }
            // Both Curve pool types share the same Curve router.
            PoolKind::CurveStable | PoolKind::CurveCrypto => RouterKind::Curve,
            // Volatile and stable Aerodrome pools share the Aerodrome router.
            PoolKind::AerodromeVolatile | PoolKind::AerodromeStable => RouterKind::Aerodrome,
            // Slipstream has its own dedicated router contract.
            PoolKind::Slipstream => RouterKind::Slipstream,
            // `PoolKind` is `#[non_exhaustive]`; future variants are unroutable
            // until an explicit mapping is added here.
            _ => return None,
        };
        Some(router)
    }
}

/// A maximal contiguous run of pools that share the same on-chain router.
///
/// `pools` is an index range into `Route::pools`; it never contains the
/// concrete pool references themselves so `Segment` is cheaply `Clone + Copy`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The router this segment will be submitted through.
    pub kind: RouterKind,
    /// Half-open index range `[start, end)` into the parent route's pool list.
    pub pools: Range<usize>,
}

/// Partition `route.pools` into maximal same-router [`Segment`]s.
///
/// The result is a total, ordered, gap-free partition of `0..route.pools.len()`:
/// - `segments[0].pools.start == 0`
/// - `segments.last().pools.end == route.pools.len()`
/// - `segments[i].pools.end == segments[i+1].pools.start` for every adjacent pair
///
/// Returns [`BuildError::UnsupportedProtocol`] if any pool has no classifiable
/// router (i.e. [`RouterKind::of`] returns `None`).
pub fn partition(route: &Route<'_>) -> Result<Vec<Segment>, BuildError> {
    let mut segments: Vec<Segment> = Vec::new();

    for (i, pool) in route.pools.iter().enumerate() {
        let kind = RouterKind::of(*pool).ok_or(BuildError::UnsupportedProtocol)?;

        // Extend the last segment when it already has this router kind;
        // otherwise open a fresh segment at this index.
        match segments.last_mut() {
            Some(seg) if seg.kind == kind => seg.pools.end = i + 1,
            _ => segments.push(Segment {
                kind,
                pools: i..i + 1,
            }),
        }
    }

    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::execution::types::TradeType;
    use alloy::primitives::B256;
    use amm_core::error::QuoteError;
    use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
    use amm_core::primitives::pool::{PoolId, PoolKind};
    use amm_core::primitives::ratio::Bps;
    use amm_core::traits::introspect::Introspect;
    use amm_core::traits::pool::Pool;

    // ── Minimal mock pool ────────────────────────────────────────────────────

    /// A stub pool that stores a fixed `PoolKind` and reports it via `Introspect`.
    /// All other trait methods return trivially correct values.
    struct MockPool {
        id: PoolId,
        assets: [AssetId; 2],
        kind: PoolKind,
    }

    impl MockPool {
        fn new(kind: PoolKind) -> Self {
            // Two distinct dummy assets so `Route::validate` would accept them.
            let a = AssetId::new(ChainId(1), B256::left_padding_from(&[1]));
            let b = AssetId::new(ChainId(1), B256::left_padding_from(&[2]));
            Self {
                id: PoolId::new("mock"),
                assets: [a, b],
                kind,
            }
        }
    }

    impl Pool for MockPool {
        fn id(&self) -> &PoolId {
            &self.id
        }

        fn assets(&self) -> &[AssetId] {
            &self.assets
        }

        fn quote(
            &self,
            _amount_in: &AssetAmount,
            _to: &AssetId,
        ) -> Result<AssetAmount, QuoteError> {
            // Not exercised by segmentation tests.
            Err(QuoteError::Unsupported)
        }

        /// Expose `Introspect` so `RouterKind::of` can read the stored kind.
        fn as_introspect(&self) -> Option<&dyn Introspect> {
            Some(self)
        }
    }

    impl Introspect for MockPool {
        fn fee_bps(&self, _source: &AssetId, _destination: &AssetId) -> Option<Bps> {
            None
        }

        fn reserve(&self, _asset: &AssetId) -> Option<AssetAmount> {
            None
        }

        fn kind(&self) -> PoolKind {
            self.kind
        }
    }

    // ── Convenience helpers ───────────────────────────────────────────────────

    fn univ2_pool() -> MockPool {
        MockPool::new(PoolKind::UniswapV2)
    }
    fn univ3_pool() -> MockPool {
        MockPool::new(PoolKind::UniswapV3)
    }
    fn univ4_pool() -> MockPool {
        MockPool::new(PoolKind::UniswapV4)
    }
    fn curve_pool() -> MockPool {
        MockPool::new(PoolKind::CurveStable)
    }
    fn aero_pool() -> MockPool {
        MockPool::new(PoolKind::AerodromeVolatile)
    }

    /// Build a throwaway `AssetId` from a single discriminant byte.
    fn asset(byte: u8) -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[byte]))
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn classifies_uniswap_versions_to_one_router() {
        assert_eq!(
            RouterKind::of(&univ2_pool()),
            Some(RouterKind::UniswapUniversal)
        );
        assert_eq!(
            RouterKind::of(&univ3_pool()),
            Some(RouterKind::UniswapUniversal)
        );
        assert_eq!(
            RouterKind::of(&univ4_pool()),
            Some(RouterKind::UniswapUniversal)
        );
        assert_eq!(RouterKind::of(&curve_pool()), Some(RouterKind::Curve));
    }

    #[test]
    fn contiguous_same_router_pools_form_one_segment() {
        // Route: V3, V4, CurveStable, AerodromeVolatile
        // Expected segments: [Uni(0..2), Curve(2..3), Aero(3..4)]
        let v3 = univ3_pool();
        let v4 = univ4_pool();
        let crv = curve_pool();
        let aero = aero_pool();

        // Build a minimal path with 5 distinct assets (4 pools → 5 tokens).
        let path = vec![
            asset(0x01),
            asset(0x02),
            asset(0x03),
            asset(0x04),
            asset(0x05),
        ];
        let route = Route {
            pools: vec![&v3, &v4, &crv, &aero],
            path,
            trade_type: TradeType::ExactIn,
        };

        let segs = partition(&route).unwrap();
        assert_eq!(segs.len(), 3);

        assert_eq!(segs[0].kind, RouterKind::UniswapUniversal);
        assert_eq!(segs[0].pools, 0..2);

        assert_eq!(segs[1].kind, RouterKind::Curve);
        assert_eq!(segs[1].pools, 2..3);

        assert_eq!(segs[2].kind, RouterKind::Aerodrome);
        assert_eq!(segs[2].pools, 3..4);
    }

    #[test]
    fn segmentation_is_a_total_ordered_partition() {
        let v3 = univ3_pool();
        let v4 = univ4_pool();
        let crv = curve_pool();
        let aero = aero_pool();

        let path = vec![
            asset(0x01),
            asset(0x02),
            asset(0x03),
            asset(0x04),
            asset(0x05),
        ];
        let route = Route {
            pools: vec![&v3, &v4, &crv, &aero],
            path,
            trade_type: TradeType::ExactIn,
        };

        let segs = partition(&route).unwrap();

        // Partition starts at the beginning of the pool list.
        assert_eq!(segs.first().unwrap().pools.start, 0);
        // Partition ends at the end of the pool list.
        assert_eq!(segs.last().unwrap().pools.end, route.pools.len());
        // Adjacent segments are contiguous — no gaps, no overlaps.
        for w in segs.windows(2) {
            assert_eq!(w[0].pools.end, w[1].pools.start);
        }
    }

    #[test]
    fn single_pool_route_yields_one_segment() {
        let v3 = univ3_pool();
        let route = Route {
            pools: vec![&v3],
            path: vec![asset(0x01), asset(0x02)],
            trade_type: TradeType::ExactIn,
        };
        let segs = partition(&route).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, RouterKind::UniswapUniversal);
        assert_eq!(segs[0].pools, 0..1);
    }

    #[test]
    fn all_same_router_pools_form_one_segment() {
        let v2 = univ2_pool();
        let v3 = univ3_pool();
        let v4 = univ4_pool();
        let route = Route {
            pools: vec![&v2, &v3, &v4],
            path: vec![asset(0x01), asset(0x02), asset(0x03), asset(0x04)],
            trade_type: TradeType::ExactIn,
        };
        let segs = partition(&route).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, RouterKind::UniswapUniversal);
        assert_eq!(segs[0].pools, 0..3);
    }

    #[test]
    fn pool_without_introspect_returns_unsupported_protocol_error() {
        /// A pool that intentionally does NOT expose `Introspect`.
        struct OpaquePool {
            id: PoolId,
            assets: [AssetId; 2],
        }

        impl Pool for OpaquePool {
            fn id(&self) -> &PoolId {
                &self.id
            }
            fn assets(&self) -> &[AssetId] {
                &self.assets
            }
            fn quote(&self, _: &AssetAmount, _: &AssetId) -> Result<AssetAmount, QuoteError> {
                Err(QuoteError::Unsupported)
            }
            // `as_introspect` returns `None` (the default) — intentionally opaque.
        }

        let opaque = OpaquePool {
            id: PoolId::new("opaque"),
            assets: [asset(0x01), asset(0x02)],
        };
        let route = Route {
            pools: vec![&opaque],
            path: vec![asset(0x01), asset(0x02)],
            trade_type: TradeType::ExactIn,
        };
        assert_eq!(partition(&route), Err(BuildError::UnsupportedProtocol));
    }

    /// Property: for any sequence of known router kinds the flattened segment
    /// ranges cover `0..n` exactly and are gap-free.
    #[test]
    fn property_partition_covers_all_pools_gap_free() {
        // Enumerate all `RouterKind` variants as representative `PoolKind`s.
        let all_kinds = [
            PoolKind::UniswapV2,
            PoolKind::UniswapV3,
            PoolKind::UniswapV4,
            PoolKind::CurveStable,
            PoolKind::CurveCrypto,
            PoolKind::AerodromeVolatile,
            PoolKind::AerodromeStable,
            PoolKind::Slipstream,
        ];

        // Try all non-empty prefixes of `all_kinds` to cover varying lengths
        // and all transitions between router kinds.
        for len in 1..=all_kinds.len() {
            let pools: Vec<MockPool> = all_kinds[..len].iter().map(|&k| MockPool::new(k)).collect();

            // Build a path with `len + 1` distinct assets.
            let path: Vec<AssetId> = (0..=len as u8).map(|b| asset(b + 1)).collect();

            let pool_refs: Vec<&dyn Pool> = pools.iter().map(|p| p as &dyn Pool).collect();
            let route = Route {
                pools: pool_refs,
                path,
                trade_type: TradeType::ExactIn,
            };

            let segs = partition(&route).unwrap();

            // Partition starts at 0.
            assert_eq!(segs.first().unwrap().pools.start, 0, "len={len}");
            // Partition ends at the pool count.
            assert_eq!(segs.last().unwrap().pools.end, len, "len={len}");
            // No gaps between adjacent segments.
            for w in segs.windows(2) {
                assert_eq!(w[0].pools.end, w[1].pools.start, "len={len}");
            }

            // Flattened kinds equal the input router-kind sequence.
            let flattened_kinds: Vec<RouterKind> = segs
                .iter()
                .flat_map(|seg| {
                    let k = seg.kind;
                    std::iter::repeat_n(k, seg.pools.len())
                })
                .collect();
            let expected_kinds: Vec<RouterKind> =
                pools.iter().map(|p| RouterKind::of(p).unwrap()).collect();
            assert_eq!(flattened_kinds, expected_kinds, "len={len}");
        }
    }
}
