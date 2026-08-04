//! The [`Limits`] extension trait: sizing and price-bounded (partial-fill) quotes.

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::price::Price;
use crate::traits::pool::Pool;

/// The outcome of a price-bounded quote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitedQuote {
    /// The input actually consumed (may be less than requested if `limited`).
    pub amount_in: AssetAmount,
    /// The output produced.
    pub amount_out: AssetAmount,
    /// Whether the price limit was reached before all input was consumed.
    pub limited: bool,
}

/// Pools that can report sizing limits and price-bounded quotes.
pub trait Limits: Pool {
    /// The maximum `from` input the pool can absorb for `from -> to` before it
    /// is exhausted (or all liquidity is crossed), if bounded.
    fn max_amount_in(&self, from: &AssetId, to: &AssetId) -> Option<AssetAmount>;

    /// Quote `amount_in -> to` bounded by the price `limit`, reporting partial
    /// fills via [`LimitedQuote::limited`].
    fn quote_with_limit(
        &self,
        amount_in: &AssetAmount,
        to: &AssetId,
        limit: Price,
    ) -> Result<LimitedQuote, QuoteError>;
}
