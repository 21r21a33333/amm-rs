//! The core [`Pool`] trait: the open, object-safe interface every AMM pool
//! implements.

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::PoolId;
use crate::traits::exact_out::ExactOut;
use crate::traits::introspect::Introspect;
use crate::traits::limits::Limits;
use crate::traits::pricing::Pricing;

/// An AMM pool that can quote swaps.
///
/// This is deliberately minimal and **object-safe** (`&self` methods, no
/// generics, no `async`), so a heterogeneous set of pools can be held as
/// `Box<dyn Pool>` and any third party can implement it for their own AMM
/// without touching this crate. Richer capabilities (exact-out, spot price,
/// price impact, limits) live in opt-in extension traits, reachable from a
/// `dyn Pool` via the `as_*` accessors below.
pub trait Pool: Send + Sync {
    /// This pool's stable identifier.
    fn id(&self) -> &PoolId;

    /// The assets this pool trades.
    fn assets(&self) -> &[AssetId];

    /// Exact-in quote: how much of `to` is received for `amount_in`, inclusive
    /// of fees and price impact and wei-exact against the on-chain contract.
    ///
    /// Returns [`QuoteError::AssetNotInPool`] if the pool does not trade the
    /// `amount_in.asset -> to` pair.
    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError>;

    /// This pool as an [`ExactOut`] quoter, or `None` if it doesn't support
    /// reverse quoting. Lets a consumer holding `&dyn Pool` reach the opt-in
    /// capabilities; implementing pools override to return `Some(self)`.
    fn as_exact_out(&self) -> Option<&dyn ExactOut> {
        None
    }

    /// This pool as a [`Pricing`] source (spot price / price impact), or `None`.
    fn as_pricing(&self) -> Option<&dyn Pricing> {
        None
    }

    /// This pool as an [`Introspect`] source (fee / reserve / kind), or `None`.
    fn as_introspect(&self) -> Option<&dyn Introspect> {
        None
    }

    /// This pool as a [`Limits`] source (sizing / price-bounded quotes), or `None`.
    fn as_limits(&self) -> Option<&dyn Limits> {
        None
    }
}

/// Compile-time guarantee that `Pool` stays object-safe (`dyn`-compatible):
/// this fails to compile if a future change (a generic method, an `async fn`,
/// or a `Self: Sized`-free requirement) breaks it. Object-safety is load-bearing
/// — `path::quote_path` and `amm-rpc` both hold `dyn Pool`.
const _: fn(&dyn Pool) = |_| {};
