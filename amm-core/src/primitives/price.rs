//! A directional, invertible exchange rate ([`Price`]).
//!
//! Unlike a bare ratio, a `Price` records *which* token is the base and which
//! is the quote, so `WETH→USDC` and its inverse are distinct, type-checked
//! values. Applying a price ([`Price::convert`]) is the only way to turn a base
//! amount into a quote amount, and it verifies the input's asset.

use core::cmp::Ordering;

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::ratio::{Ratio, Rounding};

/// An exact exchange rate of `quote` per `base` (`quote / base`, in raw units).
///
/// Construct via [`Price::new`]; the ratio is guaranteed non-zero and
/// `base != quote`, so [`Price::invert`] is total.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Price {
    base: AssetId,
    quote: AssetId,
    ratio: Ratio,
}

impl Price {
    /// Build a price of `quote` per `base`. `None` if `base == quote` or the
    /// ratio is zero (both are degenerate and would break `invert`).
    pub fn new(base: AssetId, quote: AssetId, ratio: Ratio) -> Option<Price> {
        match base == quote || ratio.is_zero() {
            true => None,
            false => Some(Price { base, quote, ratio }),
        }
    }

    /// The base asset (the denominator of the rate).
    pub fn base(&self) -> AssetId {
        self.base
    }

    /// The quote asset (the numerator of the rate).
    pub fn quote(&self) -> AssetId {
        self.quote
    }

    /// The underlying exact ratio, `quote` per `base`.
    pub fn ratio(&self) -> &Ratio {
        &self.ratio
    }

    /// The reciprocal price, `base` per `quote`. Total: a `Price` always has a
    /// non-zero ratio and distinct base/quote by construction.
    pub fn invert(self) -> Price {
        let ratio = self
            .ratio
            .invert()
            .expect("Price ratio is non-zero by construction");
        Price {
            base: self.quote,
            quote: self.base,
            ratio,
        }
    }

    /// Convert a base amount into a quote amount at this price (rounding down).
    /// `Err(AssetMismatch)` if `input.asset != self.base`.
    pub fn convert(&self, input: &AssetAmount) -> Result<AssetAmount, QuoteError> {
        match input.asset == self.base {
            false => Err(QuoteError::AssetMismatch {
                expected: self.base,
                got: input.asset,
            }),
            true => {
                let raw = self
                    .ratio
                    .apply(input.raw, Rounding::Down)
                    .ok_or(QuoteError::Overflow)?;
                Ok(AssetAmount {
                    asset: self.quote,
                    raw,
                })
            }
        }
    }

    /// Chain two prices along a path: `self` (`base→quote`) composed with `next`
    /// (`quote→other`) yields `base→other`. `Err(AssetMismatch)` if
    /// `self.quote != next.base`.
    pub fn compose(self, next: Price) -> Result<Price, QuoteError> {
        if self.quote != next.base {
            return Err(QuoteError::AssetMismatch {
                expected: self.quote,
                got: next.base,
            });
        }
        let ratio = self.ratio * next.ratio;
        // Fails only if the composed path returns to `base` (a cycle), which is
        // not a meaningful single price.
        Price::new(self.base, next.quote, ratio).ok_or(QuoteError::Unsupported)
    }
}

impl PartialOrd for Price {
    /// Prices are comparable only when they share the same `base` and `quote`
    /// orientation; otherwise the comparison is meaningless (`None`).
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match self.base == other.base && self.quote == other.quote {
            true => Some(self.ratio.cmp(&other.ratio)),
            false => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::ChainId;
    use alloy_primitives::{B256, U256};

    fn asset(chain: u64, low: u8) -> AssetId {
        AssetId::new(ChainId(chain), B256::left_padding_from(&[low]))
    }

    fn ratio(n: u64, d: u64) -> Ratio {
        Ratio::new(U256::from(n), U256::from(d)).unwrap()
    }

    #[test]
    fn convert_applies_ratio_and_tags_quote() {
        let (a, b) = (asset(1, 0xaa), asset(1, 0xbb));
        let p = Price::new(a, b, ratio(3, 1)).unwrap();
        let out = p.convert(&AssetAmount::new(a, U256::from(10u64))).unwrap();
        assert_eq!(out.asset, b);
        assert_eq!(out.raw, U256::from(30u64));
    }

    #[test]
    fn convert_rejects_wrong_base() {
        let (a, b) = (asset(1, 0xaa), asset(1, 0xbb));
        let p = Price::new(a, b, ratio(3, 1)).unwrap();
        let wrong = AssetAmount::new(b, U256::from(1u64));
        assert!(matches!(
            p.convert(&wrong),
            Err(QuoteError::AssetMismatch { .. })
        ));
    }

    #[test]
    fn invert_swaps_base_and_quote_and_ratio() {
        let (a, b) = (asset(1, 0xaa), asset(1, 0xbb));
        let inv = Price::new(a, b, ratio(3, 1)).unwrap().invert();
        assert_eq!(inv.base(), b);
        assert_eq!(inv.quote(), a);
        assert_eq!(inv.ratio(), &ratio(1, 3));
    }

    #[test]
    fn new_rejects_same_asset_and_zero_ratio() {
        let (a, b) = (asset(1, 0xaa), asset(1, 0xbb));
        assert!(Price::new(a, a, ratio(1, 1)).is_none());
        assert!(Price::new(a, b, ratio(0, 1)).is_none());
    }

    #[test]
    fn compose_chains_prices() {
        let (a, b, c) = (asset(1, 0xaa), asset(1, 0xbb), asset(1, 0xcc));
        let ab = Price::new(a, b, ratio(2, 1)).unwrap(); // 2 b per a
        let bc = Price::new(b, c, ratio(3, 1)).unwrap(); // 3 c per b
        let ac = ab.compose(bc).unwrap();
        assert_eq!(ac.base(), a);
        assert_eq!(ac.quote(), c);
        assert_eq!(ac.ratio(), &ratio(6, 1)); // 6 c per a
    }

    #[test]
    fn compose_rejects_disjoint_middle() {
        let (a, b, c) = (asset(1, 0xaa), asset(1, 0xbb), asset(1, 0xcc));
        let ab = Price::new(a, b, ratio(1, 1)).unwrap();
        let ca = Price::new(c, a, ratio(1, 1)).unwrap(); // base `c` != ab.quote `b`
        assert!(matches!(
            ab.compose(ca),
            Err(QuoteError::AssetMismatch { .. })
        ));
    }

    #[test]
    fn partial_ord_same_orientation_compares_ratios() {
        let (a, b) = (asset(1, 0xaa), asset(1, 0xbb));
        let lo = Price::new(a, b, ratio(1, 2)).unwrap();
        let hi = Price::new(a, b, ratio(3, 2)).unwrap();
        assert!(lo < hi);
    }

    #[test]
    fn partial_ord_different_orientation_is_none() {
        let (a, b, c) = (asset(1, 0xaa), asset(1, 0xbb), asset(1, 0xcc));
        let ab = Price::new(a, b, ratio(1, 1)).unwrap();
        let ac = Price::new(a, c, ratio(1, 1)).unwrap();
        assert_eq!(ab.partial_cmp(&ac), None);
    }
}
