//! The [`Introspect`] extension trait: pool state introspection.

use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::PoolKind;
use crate::primitives::ratio::Bps;
use crate::traits::pool::Pool;

/// Pools that can expose their fee, reserves, and protocol kind.
pub trait Introspect: Pool {
    /// The swap fee for `source -> destination`, in basis points, if defined.
    fn fee_bps(&self, source: &AssetId, destination: &AssetId) -> Option<Bps>;

    /// The pool's reserve of `asset`, where the concept applies (e.g. V2/Curve).
    /// `None` for pools without a simple per-asset reserve (e.g. V3).
    fn reserve(&self, asset: &AssetId) -> Option<AssetAmount>;

    /// The pool's protocol family + variant.
    fn kind(&self) -> PoolKind;
}
