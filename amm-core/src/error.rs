//! Error types for `amm-core`.
//!
//! [`QuoteError`] is the single canonical *math* error (quoting and pricing).
//! [`ParseError`] is separate and covers *string construction* (`FromStr`,
//! [`AssetAmount::from_decimal`](crate::primitives::asset::AssetAmount::from_decimal)),
//! which is not a math failure. I/O errors live in the separate `amm-rpc` crate.

use crate::primitives::asset::AssetId;

/// A math error produced while quoting a swap or computing a price.
///
/// Deliberately carries no I/O concerns; `amm-rpc` wraps this in its own error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QuoteError {
    /// An amount's asset did not match the expected asset.
    #[error("asset mismatch: expected {expected}, got {got}")]
    AssetMismatch {
        /// The asset the operation expected.
        expected: AssetId,
        /// The asset that was actually supplied.
        got: AssetId,
    },
    /// The requested `input -> output` pair is not served by this pool.
    #[error("assets not in pool: {input} -> {output}")]
    AssetNotInPool {
        /// The requested input asset.
        input: AssetId,
        /// The requested output asset.
        output: AssetId,
    },
    /// The pool has insufficient liquidity to serve the requested swap.
    #[error("insufficient liquidity")]
    InsufficientLiquidity,
    /// A price limit was reached before the swap could complete.
    #[error("price limit crossed")]
    PriceLimitCrossed,
    /// An intermediate computation overflowed (or underflowed) the integer range.
    #[error("arithmetic overflow")]
    Overflow,
    /// This pool does not support the requested operation (e.g. exact-out).
    #[error("operation unsupported by this pool")]
    Unsupported,
}

/// A string-parsing / construction error.
///
/// Distinct from [`QuoteError`]: a malformed input is not a math failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// An asset id was not of the form `<chain>:0x<hex>`.
    #[error("malformed asset id (expected `<chain>:0x<hex>`)")]
    AssetId,
    /// A decimal amount string was malformed or had too many fractional digits.
    #[error("malformed decimal amount")]
    Decimal,
    /// The parsed value overflows `U256`.
    #[error("value overflows U256")]
    Overflow,
}
