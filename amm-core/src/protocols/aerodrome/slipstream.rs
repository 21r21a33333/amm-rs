//! Aerodrome Slipstream quoter — concentrated liquidity with identical tick math
//! to Uniswap V3. A thin adapter over the shared [`super::concentrated`] engine;
//! it differs from V3 only in its [`PoolKind`].

use alloy_primitives::U256;

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::{PoolId, PoolKind};
use crate::primitives::price::Price;
use crate::primitives::ratio::Bps;
use crate::protocols::concentrated::{self, SwapState};
use crate::protocols::two_asset_direction;
use crate::traits::exact_out::ExactOut;
use crate::traits::introspect::Introspect;
use crate::traits::limits::{LimitedQuote, Limits};
use crate::traits::pool::Pool;
use crate::traits::pricing::Pricing;

pub use crate::protocols::concentrated::{TickData, TickInfo};

/// An Aerodrome Slipstream concentrated-liquidity pool over two assets.
///
/// State and math are the same as Uniswap V3: `assets[0]`/`assets[1]` are
/// `token0`/`token1`, `sqrt_price_x96` the current Q64.96 price, `fee_pips` the
/// fee tier in millionths.
#[derive(Clone, Debug)]
pub struct AerodromeSlipstreamPool {
    id: PoolId,
    assets: [AssetId; 2],
    sqrt_price_x96: U256,
    liquidity: u128,
    tick: i32,
    fee_pips: u32,
    tick_data: TickData,
}

impl AerodromeSlipstreamPool {
    /// Construct a pool from a slot0 + liquidity + tick-state snapshot.
    pub fn new(
        id: PoolId,
        assets: [AssetId; 2],
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
        fee_pips: u32,
        tick_data: TickData,
    ) -> Self {
        Self {
            id,
            assets,
            sqrt_price_x96,
            liquidity,
            tick,
            fee_pips,
            tick_data,
        }
    }

    /// The market snapshot handed to the shared engine.
    fn state(&self) -> SwapState<'_> {
        SwapState {
            sqrt_price_x96: self.sqrt_price_x96,
            tick: self.tick,
            liquidity: self.liquidity,
            fee_pips: self.fee_pips,
            ticks: &self.tick_data,
        }
    }

    /// Resolve `zero_for_one` for a `from -> to` swap, or the not-in-pool error.
    fn direction(&self, from: &AssetId, to: &AssetId) -> Result<bool, QuoteError> {
        two_asset_direction(&self.assets, from, to).ok_or(QuoteError::AssetNotInPool {
            input: *from,
            output: *to,
        })
    }
}

impl Pool for AerodromeSlipstreamPool {
    fn id(&self) -> &PoolId {
        &self.id
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
        let zero_for_one = self.direction(&amount_in.asset, to)?;
        let out = concentrated::amount_out(&self.state(), zero_for_one, amount_in.raw)?;
        Ok(AssetAmount::new(*to, out))
    }
}

impl ExactOut for AerodromeSlipstreamPool {
    fn quote_exact_out(
        &self,
        amount_out: &AssetAmount,
        from: &AssetId,
    ) -> Result<AssetAmount, QuoteError> {
        let zero_for_one = self.direction(from, &amount_out.asset)?;
        let needed = concentrated::amount_in(&self.state(), zero_for_one, amount_out.raw)?;
        Ok(AssetAmount::new(*from, needed))
    }
}

impl Pricing for AerodromeSlipstreamPool {
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError> {
        let zero_for_one = self.direction(base, quote)?;
        concentrated::spot_price(base, quote, self.sqrt_price_x96, zero_for_one)
    }
}

impl Introspect for AerodromeSlipstreamPool {
    fn fee_bps(&self, source: &AssetId, destination: &AssetId) -> Option<Bps> {
        two_asset_direction(&self.assets, source, destination)
            .map(|_| Bps((self.fee_pips / 100) as u16))
    }

    fn reserve(&self, _asset: &AssetId) -> Option<AssetAmount> {
        None
    }

    fn kind(&self) -> PoolKind {
        PoolKind::Slipstream
    }
}

impl Limits for AerodromeSlipstreamPool {
    fn max_amount_in(&self, from: &AssetId, to: &AssetId) -> Option<AssetAmount> {
        let zero_for_one = two_asset_direction(&self.assets, from, to)?;
        concentrated::max_amount_in(&self.state(), zero_for_one)
            .map(|raw| AssetAmount::new(*from, raw))
    }

    fn quote_with_limit(
        &self,
        amount_in: &AssetAmount,
        to: &AssetId,
        limit: Price,
    ) -> Result<LimitedQuote, QuoteError> {
        let zero_for_one = self.direction(&amount_in.asset, to)?;
        concentrated::quote_with_limit(
            &self.state(),
            &self.assets,
            zero_for_one,
            amount_in,
            to,
            &limit,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::concentrated::fixtures::{SQRT_1_1, full_range_ticks, usdc, weth};

    fn full_range_pool() -> AerodromeSlipstreamPool {
        let liq: i128 = 1_000_000_000_000_000_000;
        AerodromeSlipstreamPool::new(
            PoolId::new("1:aero-cl:0xfull"),
            [usdc(), weth()],
            U256::from(SQRT_1_1),
            liq as u128,
            0,
            3000,
            full_range_ticks(liq),
        )
    }

    #[test]
    fn quote_uses_the_concentrated_engine_and_tags_slipstream() {
        // Same 1:1 fee-band behaviour as V3 (shared engine), but kind() = Slipstream.
        let pool = full_range_pool();
        let amount_in = U256::from(1_000_000_000u64);
        let out = pool
            .quote(&AssetAmount::new(usdc(), amount_in), &weth())
            .unwrap();
        assert!(out.raw < amount_in);
        assert!(out.raw > amount_in * U256::from(996u64) / U256::from(1000u64));
        assert_eq!(pool.kind(), PoolKind::Slipstream);
    }

    #[test]
    fn exact_out_input_delivers_at_least_the_target() {
        let pool = full_range_pool();
        let want = U256::from(500_000_000u64);
        let needed = pool
            .quote_exact_out(&AssetAmount::new(weth(), want), &usdc())
            .unwrap();
        let delivered = pool
            .quote(&AssetAmount::new(usdc(), needed.raw), &weth())
            .unwrap();
        assert!(delivered.raw >= want);
    }

    #[test]
    fn fee_bps_and_reserve_introspection() {
        let pool = full_range_pool();
        assert_eq!(pool.fee_bps(&usdc(), &weth()), Some(Bps(30))); // 3000 pips
        assert_eq!(pool.reserve(&usdc()), None); // concentrated: no single reserve
    }
}
