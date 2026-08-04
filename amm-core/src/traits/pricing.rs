//! The [`Pricing`] extension trait: marginal (spot) price and price impact.

use alloy_primitives::U256;

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::price::Price;
use crate::primitives::ratio::Bps;
use crate::traits::pool::Pool;

/// Pools that can report a marginal (zero-size) price and derived price impact.
pub trait Pricing: Pool {
    /// The marginal price of `quote` per `base` (the price of an infinitesimal swap).
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError>;

    /// Price impact of swapping `amount_in` for `to`, in basis points: how far
    /// the realized output falls below the spot-implied output.
    ///
    /// Default impl: `(spot_implied_out - actual_out) / spot_implied_out`,
    /// floored to whole basis points. Impact is in `[0, 10_000]` bps.
    fn price_impact(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<Bps, QuoteError> {
        let ideal = self
            .spot_price(&amount_in.asset, to)?
            .convert(amount_in)?
            .raw;
        let actual = self.quote(amount_in, to)?.raw;
        if ideal.is_zero() || actual >= ideal {
            return Ok(Bps(0));
        }
        let scaled = (ideal - actual)
            .checked_mul(U256::from(10_000u64))
            .ok_or(QuoteError::Overflow)?;
        Ok(Bps(u16::try_from(scaled / ideal).unwrap_or(u16::MAX)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::ChainId;
    use crate::primitives::pool::PoolId;
    use crate::primitives::ratio::Ratio;
    use alloy_primitives::B256;

    fn asset(low: u8) -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[low]))
    }

    /// Spot is 1:1; the actual quote is 1% worse — so impact is 100 bps.
    struct FakePool {
        id: PoolId,
        assets: [AssetId; 2],
    }

    impl Pool for FakePool {
        fn id(&self) -> &PoolId {
            &self.id
        }

        fn assets(&self) -> &[AssetId] {
            &self.assets
        }

        fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
            Ok(AssetAmount::new(
                *to,
                amount_in.raw * U256::from(99u64) / U256::from(100u64),
            ))
        }
    }

    impl Pricing for FakePool {
        fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError> {
            let one = Ratio::new(U256::from(1u64), U256::from(1u64)).unwrap();
            Ok(Price::new(*base, *quote, one).unwrap())
        }
    }

    #[test]
    fn price_impact_default_computes_bps() {
        let (a, b) = (asset(0xaa), asset(0xbb));
        let pool = FakePool {
            id: PoolId::new("1:fake:0x0"),
            assets: [a, b],
        };
        let impact = pool
            .price_impact(&AssetAmount::new(a, U256::from(100u64)), &b)
            .unwrap();
        assert_eq!(impact, Bps(100)); // 1% below spot
    }
}
