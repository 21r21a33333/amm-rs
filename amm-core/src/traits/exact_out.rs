//! The [`ExactOut`] extension trait: reverse (exact-output) quoting.

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::traits::pool::Pool;

/// Pools that can answer "how much `from` input is needed to receive exactly
/// `amount_out`".
///
/// Closed-form for constant-product pools; numerical (bounded search) for
/// tick/curve pools.
pub trait ExactOut: Pool {
    /// The `from` input amount required to receive exactly `amount_out`.
    #[must_use = "discarding the input quote silently loses the required amount or hides a quote error"]
    fn quote_exact_out(
        &self,
        amount_out: &AssetAmount,
        from: &AssetId,
    ) -> Result<AssetAmount, QuoteError>;
}
