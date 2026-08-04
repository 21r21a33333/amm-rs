//! Uniswap V3 concentrated-liquidity quoter.
//!
//! A thin adapter over the shared [`concentrated`](super::concentrated) tick
//! engine: V3 resolves swap direction and feeds its single fee tier, and the
//! engine does the Q64.96 tick-crossing math.

use alloy_primitives::U256;

use super::concentrated::{self, SwapState};
use super::two_asset_direction;
use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::{PoolId, PoolKind};
use crate::primitives::price::Price;
use crate::primitives::ratio::Bps;
use crate::traits::exact_out::ExactOut;
use crate::traits::introspect::Introspect;
use crate::traits::limits::{LimitedQuote, Limits};
use crate::traits::pool::Pool;
use crate::traits::pricing::Pricing;

pub use super::concentrated::{TickData, TickInfo};

/// A Uniswap V3 concentrated-liquidity pool over two assets.
///
/// `assets[0]`/`assets[1]` are the pool's `token0`/`token1` (address-sorted, as
/// Uniswap orders them). `sqrt_price_x96` is the current Q64.96 price and
/// `fee_pips` the fee tier in millionths (3000 = 0.30%).
#[derive(Clone, Debug)]
pub struct UniswapV3Pool {
    id: PoolId,
    assets: [AssetId; 2],
    sqrt_price_x96: U256,
    liquidity: u128,
    tick: i32,
    fee_pips: u32,
    tick_data: TickData,
}

impl UniswapV3Pool {
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

    /// The market snapshot handed to the shared engine. V3's fee is the same in
    /// both directions.
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

impl Pool for UniswapV3Pool {
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

impl ExactOut for UniswapV3Pool {
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

impl Pricing for UniswapV3Pool {
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError> {
        let zero_for_one = self.direction(base, quote)?;
        concentrated::spot_price(base, quote, self.sqrt_price_x96, zero_for_one)
    }
}

impl Introspect for UniswapV3Pool {
    fn fee_bps(&self, source: &AssetId, destination: &AssetId) -> Option<Bps> {
        // V3 fee tiers are in pips (millionths); 100 pips = 1 bp.
        two_asset_direction(&self.assets, source, destination)
            .map(|_| Bps((self.fee_pips / 100) as u16))
    }

    fn reserve(&self, _asset: &AssetId) -> Option<AssetAmount> {
        // V3 liquidity is spread across ticks; there is no single per-asset reserve.
        None
    }

    fn kind(&self) -> PoolKind {
        PoolKind::UniswapV3
    }
}

impl Limits for UniswapV3Pool {
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
    use crate::primitives::ratio::Ratio;
    use crate::protocols::uniswap::concentrated::fixtures::{
        SQRT_1_1, dai, full_range_ticks, usdc, weth,
    };
    use crate::protocols::uniswap::concentrated::set_tick_bit;
    use std::collections::HashMap;

    fn price(base: AssetId, quote: AssetId, n: u64, d: u64) -> Price {
        Price::new(
            base,
            quote,
            Ratio::new(U256::from(n), U256::from(d)).unwrap(),
        )
        .unwrap()
    }

    /// A full-range USDC/WETH pool at tick 0 (1:1), 0.30% fee, 1e18 liquidity.
    fn full_range_pool() -> UniswapV3Pool {
        let liq: i128 = 1_000_000_000_000_000_000;
        UniswapV3Pool::new(
            PoolId::new("1:univ3:0xfull"),
            [usdc(), weth()],
            U256::from(SQRT_1_1),
            liq as u128,
            0,
            3000,
            full_range_ticks(liq),
        )
    }

    /// A thin full-range base (1e17) plus a concentrated 9e17 position in [-60, 60];
    /// active liquidity 1e18 at tick 0.
    fn two_position_pool() -> UniswapV3Pool {
        let wide = 100_000_000_000_000_000i128; // 1e17, full range
        let conc = 900_000_000_000_000_000i128; // 9e17, [-60, 60]
        let mut ticks = HashMap::new();
        let mut bitmap = HashMap::new();
        for (t, net) in [
            (-887_220i32, wide),
            (-60, conc),
            (60, -conc),
            (887_220, -wide),
        ] {
            ticks.insert(
                t,
                TickInfo {
                    liquidity_net: net,
                    initialized: true,
                },
            );
            set_tick_bit(&mut bitmap, t, 60);
        }
        UniswapV3Pool::new(
            PoolId::new("1:univ3:0x2pos"),
            [usdc(), weth()],
            U256::from(SQRT_1_1),
            (wide + conc) as u128,
            0,
            3000,
            TickData {
                ticks,
                bitmap,
                spacing: 60,
            },
        )
    }

    #[test]
    fn quote_at_parity_is_input_minus_fee() {
        let pool = full_range_pool();
        let amount_in = U256::from(1_000_000_000u64);
        let out = pool
            .quote(&AssetAmount::new(usdc(), amount_in), &weth())
            .unwrap();
        assert!(out.raw < amount_in, "fee must reduce output");
        assert!(
            out.raw > amount_in * U256::from(996u64) / U256::from(1000u64),
            "output should be close to input minus 0.30% fee"
        );
    }

    #[test]
    fn quote_both_directions_stay_in_the_fee_band() {
        // The reverse direction must map correctly and land in the same fee band
        // as the forward one (input·996/1000 < out < input) on a symmetric pool.
        let pool = full_range_pool();
        let amount = U256::from(1_000_000_000u64);
        let lo = amount * U256::from(996u64) / U256::from(1000u64);
        for (from, to) in [(usdc(), weth()), (weth(), usdc())] {
            let out = pool.quote(&AssetAmount::new(from, amount), &to).unwrap();
            assert_eq!(out.asset, to);
            assert!(
                out.raw < amount && out.raw > lo,
                "out {} out of band",
                out.raw
            );
        }
    }

    #[test]
    fn quote_crossing_a_tick_gets_a_worse_rate() {
        let pool = two_position_pool();
        let big_in = U256::from(5_000_000_000_000_000u64); // crosses -60
        let big_out = pool
            .quote(&AssetAmount::new(usdc(), big_in), &weth())
            .unwrap()
            .raw;
        let tiny_in = U256::from(1_000_000_000u64); // stays in [-60, 60]
        let tiny_out = pool
            .quote(&AssetAmount::new(usdc(), tiny_in), &weth())
            .unwrap()
            .raw;
        // rate(big) < rate(tiny)  ⟺  big_out·tiny_in < tiny_out·big_in  (integer, no floats)
        assert!(
            big_out * tiny_in < tiny_out * big_in,
            "crossing swap rate {big_out}/{big_in} must be worse than tiny {tiny_out}/{tiny_in}"
        );
    }

    #[test]
    fn exact_out_input_actually_delivers_the_target() {
        let pool = full_range_pool();
        let want_out = U256::from(500_000_000u64);
        let needed = pool
            .quote_exact_out(&AssetAmount::new(weth(), want_out), &usdc())
            .unwrap();
        assert_eq!(needed.asset, usdc());
        let delivered = pool
            .quote(&AssetAmount::new(usdc(), needed.raw), &weth())
            .unwrap();
        assert!(
            delivered.raw >= want_out,
            "exact-out input {} delivered {} < target {want_out}",
            needed.raw,
            delivered.raw
        );
    }

    #[test]
    fn spot_price_at_tick_zero_is_one() {
        let pool = full_range_pool();
        let one = Ratio::new(U256::from(1u64), U256::from(1u64)).unwrap();
        assert_eq!(pool.spot_price(&usdc(), &weth()).unwrap().ratio(), &one);
        assert_eq!(pool.spot_price(&weth(), &usdc()).unwrap().ratio(), &one);
    }

    #[test]
    fn quote_unknown_pair_errors() {
        assert!(matches!(
            full_range_pool().quote(&AssetAmount::new(dai(), U256::from(1u64)), &weth()),
            Err(QuoteError::AssetNotInPool { .. })
        ));
    }

    #[test]
    fn quote_uninitialized_pool_is_insufficient_liquidity() {
        let pool = UniswapV3Pool::new(
            PoolId::new("1:univ3:0xempty"),
            [usdc(), weth()],
            U256::from(SQRT_1_1),
            0,
            0,
            3000,
            TickData {
                ticks: HashMap::new(),
                bitmap: HashMap::new(),
                spacing: 60,
            },
        );
        assert_eq!(
            pool.quote(&AssetAmount::new(usdc(), U256::from(1_000u64)), &weth()),
            Err(QuoteError::InsufficientLiquidity)
        );
    }

    #[test]
    fn quote_exceeding_total_liquidity_is_insufficient_not_a_partial_fill() {
        // Liquidity confined to [-60, 60] with empty space below it. A huge input
        // exhausts the band and runs the price to the extreme — a partial fill,
        // which `quote` must reject rather than silently returning a short output.
        let conc = 900_000_000_000_000_000i128; // 9e17 in [-60, 60] only
        let mut ticks = HashMap::new();
        let mut bitmap = HashMap::new();
        for (t, net) in [(-60i32, conc), (60, -conc)] {
            ticks.insert(
                t,
                TickInfo {
                    liquidity_net: net,
                    initialized: true,
                },
            );
            set_tick_bit(&mut bitmap, t, 60);
        }
        let pool = UniswapV3Pool::new(
            PoolId::new("1:univ3:0xnarrow"),
            [usdc(), weth()],
            U256::from(SQRT_1_1),
            conc as u128,
            0,
            3000,
            TickData {
                ticks,
                bitmap,
                spacing: 60,
            },
        );
        let huge = U256::from(1_000_000_000_000_000_000u64); // 1e18 ≫ band capacity (~2.7e15)
        assert_eq!(
            pool.quote(&AssetAmount::new(usdc(), huge), &weth()),
            Err(QuoteError::InsufficientLiquidity)
        );
    }

    #[test]
    fn fee_and_kind_introspection() {
        let pool = full_range_pool();
        assert_eq!(pool.fee_bps(&usdc(), &weth()), Some(Bps(30))); // 3000 pips
        assert_eq!(pool.fee_bps(&usdc(), &dai()), None); // not this pool's pair
        assert_eq!(pool.reserve(&usdc()), None); // no simple V3 reserve
        assert_eq!(pool.kind(), PoolKind::UniswapV3);
    }

    #[test]
    fn quote_with_limit_far_limit_fills_fully_like_a_plain_quote() {
        let pool = full_range_pool();
        let req = U256::from(1_000_000_000u64);
        let far = price(usdc(), weth(), 1, 1_000_000);
        let q = pool
            .quote_with_limit(&AssetAmount::new(usdc(), req), &weth(), far)
            .unwrap();
        assert!(!q.limited);
        assert_eq!(q.amount_in.raw, req);
        let plain = pool.quote(&AssetAmount::new(usdc(), req), &weth()).unwrap();
        assert_eq!(q.amount_out.raw, plain.raw);
    }

    #[test]
    fn quote_with_limit_tight_limit_fills_partially() {
        let pool = full_range_pool();
        let req = U256::from(100_000_000_000_000_000u64); // 1e17 — would move price ~10%
        let tight = price(usdc(), weth(), 9999, 10000); // 0.9999, just below 1:1
        let q = pool
            .quote_with_limit(&AssetAmount::new(usdc(), req), &weth(), tight)
            .unwrap();
        assert!(q.limited);
        assert!(q.amount_in.raw > U256::ZERO && q.amount_in.raw < req);
        assert!(q.amount_out.raw > U256::ZERO);
    }

    #[test]
    fn quote_with_limit_price_already_past_swaps_nothing() {
        let pool = full_range_pool();
        let above = price(usdc(), weth(), 2, 1);
        let q = pool
            .quote_with_limit(
                &AssetAmount::new(usdc(), U256::from(1_000u64)),
                &weth(),
                above,
            )
            .unwrap();
        assert!(q.limited);
        assert_eq!(q.amount_in.raw, U256::ZERO);
        assert_eq!(q.amount_out.raw, U256::ZERO);
    }

    #[test]
    fn quote_with_limit_rejects_a_limit_about_other_assets() {
        let pool = full_range_pool();
        let bad = price(dai(), weth(), 1, 1);
        assert!(matches!(
            pool.quote_with_limit(
                &AssetAmount::new(usdc(), U256::from(1_000u64)),
                &weth(),
                bad
            ),
            Err(QuoteError::AssetNotInPool { .. })
        ));
    }

    #[test]
    fn max_amount_in_is_bounded_and_gated_by_pair() {
        let pool = full_range_pool();
        let maxin = pool.max_amount_in(&usdc(), &weth()).unwrap();
        assert!(maxin.raw > U256::ZERO);
        assert_eq!(maxin.asset, usdc());
        assert_eq!(pool.max_amount_in(&usdc(), &dai()), None);
    }
}
