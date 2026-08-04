//! Uniswap V3 concentrated-liquidity quoter.
//!
//! Tick-crossing Q64.96 arithmetic is delegated to the `uniswap_v3_math` crate;
//! this module supplies the swap loop, direction handling, and the mapping onto
//! the core [`Pool`] trait. A single signed engine ([`UniswapV3Pool::simulate`])
//! serves both exact-in and exact-out: `compute_swap_step` selects the mode from
//! the sign of the specified amount, so exact-out is just a negative input.

use std::collections::HashMap;

use alloy_primitives::{I256, U256};

use super::two_asset_direction;
use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::{PoolId, PoolKind};
use crate::primitives::price::Price;
use crate::primitives::ratio::{Bps, Ratio};
use crate::traits::exact_out::ExactOut;
use crate::traits::introspect::Introspect;
use crate::traits::pool::Pool;
use crate::traits::pricing::Pricing;

/// Liquidity bookkeeping for a single initialized tick.
#[derive(Clone, Debug)]
pub struct TickInfo {
    /// Net liquidity added when the tick is crossed left-to-right.
    pub liquidity_net: i128,
    /// Whether the tick is initialized (carries a position boundary).
    pub initialized: bool,
}

/// The tick state a V3 swap traverses: per-tick liquidity, the initialization
/// bitmap, and the fee-tier tick spacing.
#[derive(Clone, Debug)]
pub struct TickData {
    /// Initialized ticks keyed by tick index.
    pub ticks: HashMap<i32, TickInfo>,
    /// Tick-initialization bitmap words keyed by word position.
    pub bitmap: HashMap<i16, U256>,
    /// Tick spacing for this fee tier (e.g. 60 for the 0.30% tier).
    pub spacing: i32,
}

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

/// The result of running the swap loop.
struct SwapOutcome {
    /// Total input consumed (including fees).
    amount_in: U256,
    /// Total output produced.
    amount_out: U256,
    /// `true` if the price limit was reached before the specified amount was
    /// fully consumed (a partial fill).
    limited: bool,
}

/// The next initialized-tick boundary reached in a step, with its price.
struct NextTick {
    tick: i32,
    price: U256,
    initialized: bool,
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

    /// Run the tick-crossing swap loop toward `limit`.
    ///
    /// `amount_specified` is signed: positive is exact-in (drains toward zero as
    /// input is consumed), negative is exact-out (rises toward zero as output is
    /// produced). `compute_swap_step` picks the mode from that sign.
    fn simulate(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        limit: U256,
    ) -> Result<SwapOutcome, QuoteError> {
        // A snapshot with no tick data cannot be priced.
        if self.tick_data.ticks.is_empty() || self.tick_data.bitmap.is_empty() {
            return Err(QuoteError::InsufficientLiquidity);
        }

        let exact_in = amount_specified >= I256::ZERO;
        let mut remaining = amount_specified;
        let mut sqrt = self.sqrt_price_x96;
        let mut tick = self.tick;
        let mut liquidity = self.liquidity;
        let mut amount_in = U256::ZERO;
        let mut amount_out = U256::ZERO;

        while remaining != I256::ZERO && sqrt != limit {
            let next = self.next_tick(tick, zero_for_one)?;
            let target = step_target(zero_for_one, next.price, limit);

            let (next_sqrt, step_in, step_out, step_fee) =
                uniswap_v3_math::swap_math::compute_swap_step(
                    sqrt,
                    target,
                    liquidity,
                    remaining,
                    self.fee_pips,
                )
                .map_err(|_| QuoteError::Overflow)?;

            amount_in = amount_in
                .checked_add(step_in)
                .and_then(|v| v.checked_add(step_fee))
                .ok_or(QuoteError::Overflow)?;
            amount_out = amount_out
                .checked_add(step_out)
                .ok_or(QuoteError::Overflow)?;

            // Move `remaining` toward zero. Step amounts are bounded by pool
            // liquidity, so the signed arithmetic cannot genuinely overflow;
            // `overflowing_*` keeps it panic-free regardless.
            remaining = match exact_in {
                true => {
                    remaining
                        .overflowing_sub(I256::from_raw(step_in.overflowing_add(step_fee).0))
                        .0
                }
                false => remaining.overflowing_add(I256::from_raw(step_out)).0,
            };

            let price_start = sqrt;
            sqrt = next_sqrt;
            (tick, liquidity) =
                self.advance_tick(tick, liquidity, zero_for_one, next, price_start, sqrt)?;
        }

        Ok(SwapOutcome {
            amount_in,
            amount_out,
            limited: remaining != I256::ZERO,
        })
    }

    /// Resolve the next initialized-tick boundary from the bitmap (clamped to the
    /// valid tick range), with its sqrt price.
    fn next_tick(&self, tick: i32, zero_for_one: bool) -> Result<NextTick, QuoteError> {
        let (raw, initialized) =
            uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                &self.tick_data.bitmap,
                tick,
                self.tick_data.spacing,
                zero_for_one,
            )
            .map_err(|_| QuoteError::InsufficientLiquidity)?;
        let tick = raw.clamp(
            uniswap_v3_math::tick_math::MIN_TICK,
            uniswap_v3_math::tick_math::MAX_TICK,
        );
        let price = uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(tick)
            .map_err(|_| QuoteError::Overflow)?;
        Ok(NextTick {
            tick,
            price,
            initialized,
        })
    }

    /// Advance the tick after a step:
    ///   reached the boundary  → cross it (apply net liquidity) and step the tick;
    ///   moved but not reached → recompute the tick from the new price;
    ///   unchanged             → leave it.
    fn advance_tick(
        &self,
        tick: i32,
        liquidity: u128,
        zero_for_one: bool,
        next: NextTick,
        price_start: U256,
        sqrt_now: U256,
    ) -> Result<(i32, u128), QuoteError> {
        match (sqrt_now == next.price, sqrt_now != price_start) {
            (true, _) => {
                let liquidity = match next.initialized {
                    true => {
                        let net =
                            tick_liquidity_net(&self.tick_data.ticks, next.tick, zero_for_one)
                                .ok_or(QuoteError::InsufficientLiquidity)?;
                        crossed_liquidity(liquidity, net).ok_or(QuoteError::Overflow)?
                    }
                    false => liquidity,
                };
                let tick = match zero_for_one {
                    true => next.tick.wrapping_sub(1),
                    false => next.tick,
                };
                Ok((tick, liquidity))
            }
            (false, true) => {
                let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(sqrt_now)
                    .map_err(|_| QuoteError::Overflow)?;
                Ok((tick, liquidity))
            }
            (false, false) => Ok((tick, liquidity)),
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

/// The extreme sqrt price a full-range swap runs toward (one unit inside the
/// valid range).
fn price_limit(zero_for_one: bool) -> U256 {
    match zero_for_one {
        true => uniswap_v3_math::tick_math::MIN_SQRT_RATIO + U256::from(1u64),
        false => uniswap_v3_math::tick_math::MAX_SQRT_RATIO - U256::from(1u64),
    }
}

/// Target price for one step: the closer of the next tick boundary and the limit.
/// `zero_for_one` prices fall (clamp up = `max`); otherwise they rise (clamp
/// down = `min`).
fn step_target(zero_for_one: bool, next_tick_price: U256, limit: U256) -> U256 {
    match zero_for_one {
        true => next_tick_price.max(limit),
        false => next_tick_price.min(limit),
    }
}

/// Net liquidity to apply when crossing `tick`, sign-adjusted for direction.
///
/// `None` when the tick is flagged initialized in the bitmap but absent from the
/// synced tick data: the quote fails rather than silently pricing with zero net
/// liquidity, which would misprice the swap.
fn tick_liquidity_net(
    ticks: &HashMap<i32, TickInfo>,
    tick: i32,
    zero_for_one: bool,
) -> Option<i128> {
    let net = ticks.get(&tick)?.liquidity_net;
    Some(match zero_for_one {
        true => -net,
        false => net,
    })
}

/// Liquidity after crossing an initialized tick — `None` on overflow/underflow.
fn crossed_liquidity(liquidity: u128, liquidity_net: i128) -> Option<u128> {
    match liquidity_net.is_negative() {
        true => liquidity.checked_sub(liquidity_net.unsigned_abs()),
        false => liquidity.checked_add(liquidity_net as u128),
    }
}

// ─── Pool + extension trait impls ───────────────────────────────────────────

impl Pool for UniswapV3Pool {
    fn id(&self) -> &PoolId {
        &self.id
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
        let zero_for_one = self.direction(&amount_in.asset, to)?;
        let spec = I256::from_raw(amount_in.raw);
        // A U256 with bit 255 set is not a representable positive input.
        match spec < I256::ZERO {
            true => Err(QuoteError::Overflow),
            false => {
                let outcome = self.simulate(zero_for_one, spec, price_limit(zero_for_one))?;
                // Nonzero input that yields nothing means the pool is dry.
                match !amount_in.raw.is_zero() && outcome.amount_out.is_zero() {
                    true => Err(QuoteError::InsufficientLiquidity),
                    false => Ok(AssetAmount::new(*to, outcome.amount_out)),
                }
            }
        }
    }
}

impl ExactOut for UniswapV3Pool {
    fn quote_exact_out(
        &self,
        amount_out: &AssetAmount,
        from: &AssetId,
    ) -> Result<AssetAmount, QuoteError> {
        let zero_for_one = self.direction(from, &amount_out.asset)?;
        let mag = I256::from_raw(amount_out.raw);
        match mag < I256::ZERO {
            true => Err(QuoteError::Overflow),
            false => {
                // Negative specified amount selects the exact-out path.
                let outcome =
                    self.simulate(zero_for_one, mag.wrapping_neg(), price_limit(zero_for_one))?;
                // A partial fill means the pool cannot source the full output.
                match outcome.limited {
                    true => Err(QuoteError::InsufficientLiquidity),
                    false => Ok(AssetAmount::new(*from, outcome.amount_in)),
                }
            }
        }
    }
}

impl Pricing for UniswapV3Pool {
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError> {
        let zero_for_one = self.direction(base, quote)?;
        // sqrtPriceX96 encodes token1 per token0 = sqrtP² / 2¹⁹².
        let token1_per_token0 = Ratio::from_q192_sqrt(self.sqrt_price_x96);
        let ratio = match zero_for_one {
            true => token1_per_token0,
            false => token1_per_token0
                .invert()
                .ok_or(QuoteError::InsufficientLiquidity)?,
        };
        Price::new(*base, *quote, ratio).ok_or(QuoteError::InsufficientLiquidity)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::ChainId;
    use alloy_primitives::B256;

    fn usdc() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x01]))
    }
    fn weth() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x02]))
    }
    fn dai() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x03]))
    }

    /// sqrtPriceX96 for tick 0 (price 1:1) = 2^96.
    const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;

    /// Set the initialization bit for `tick` in a V3 bitmap (test fixture).
    fn set_bit(bitmap: &mut HashMap<i16, U256>, tick: i32, spacing: i32) {
        let compressed = match tick < 0 && tick % spacing != 0 {
            true => (tick / spacing) - 1,
            false => tick / spacing,
        };
        let word = (compressed >> 8) as i16;
        let bit = (compressed % 256) as u8;
        *bitmap.entry(word).or_insert(U256::ZERO) |= U256::from(1u64) << bit;
    }

    /// A full-range USDC/WETH pool at tick 0 (1:1), 0.30% fee, 1e18 liquidity.
    fn full_range_pool() -> UniswapV3Pool {
        let (lower, upper) = (-887_220i32, 887_220i32);
        let liq: i128 = 1_000_000_000_000_000_000;
        let mut ticks = HashMap::new();
        let mut bitmap = HashMap::new();
        ticks.insert(
            lower,
            TickInfo {
                liquidity_net: liq,
                initialized: true,
            },
        );
        ticks.insert(
            upper,
            TickInfo {
                liquidity_net: -liq,
                initialized: true,
            },
        );
        set_bit(&mut bitmap, lower, 60);
        set_bit(&mut bitmap, upper, 60);
        UniswapV3Pool::new(
            PoolId::new("1:univ3:0xfull"),
            [usdc(), weth()],
            U256::from(SQRT_1_1),
            liq as u128,
            0,
            3000,
            TickData {
                ticks,
                bitmap,
                spacing: 60,
            },
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
            set_bit(&mut bitmap, t, 60);
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
        // At 1:1 with 0.30% fee, a small swap returns just under the input:
        // strictly less than input, and within the fee band (> input·996/1000).
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
    fn quote_both_directions_produce_output() {
        let pool = full_range_pool();
        let amount = U256::from(1_000_000_000u64);
        let a = pool
            .quote(&AssetAmount::new(usdc(), amount), &weth())
            .unwrap();
        let b = pool
            .quote(&AssetAmount::new(weth(), amount), &usdc())
            .unwrap();
        assert!(a.raw > U256::ZERO && a.asset == weth());
        assert!(b.raw > U256::ZERO && b.asset == usdc());
    }

    #[test]
    fn quote_crossing_a_tick_gets_a_worse_rate() {
        // A swap large enough to cross tick -60 drops active liquidity 1e18 → 1e17,
        // so its rate must be strictly worse than a tiny non-crossing swap.
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
        // The defining exact-out guarantee: feeding the computed input back through
        // exact-in must yield at least the requested output.
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
    fn fee_and_kind_introspection() {
        let pool = full_range_pool();
        assert_eq!(pool.fee_bps(&usdc(), &weth()), Some(Bps(30))); // 3000 pips
        assert_eq!(pool.fee_bps(&usdc(), &dai()), None); // not this pool's pair
        assert_eq!(pool.reserve(&usdc()), None); // no simple V3 reserve
        assert_eq!(pool.kind(), PoolKind::UniswapV3);
    }

    // ── swap-helper unit tests (tricky sign/clamp logic) ──────────────────────

    #[test]
    fn crossed_liquidity_adds_subtracts_and_underflows() {
        assert_eq!(crossed_liquidity(1_000, 300), Some(1_300));
        assert_eq!(crossed_liquidity(1_000, -300), Some(700));
        assert_eq!(crossed_liquidity(100, -300), None); // underflow
        // i128::MIN must not panic (a plain `-x` would overflow).
        assert_eq!(
            crossed_liquidity(u128::MAX, i128::MIN),
            Some(u128::MAX - (1u128 << 127))
        );
    }

    #[test]
    fn tick_liquidity_net_signs_by_direction_and_flags_missing() {
        let mut ticks = HashMap::new();
        ticks.insert(
            60,
            TickInfo {
                liquidity_net: 500,
                initialized: true,
            },
        );
        assert_eq!(tick_liquidity_net(&ticks, 60, false), Some(500));
        assert_eq!(tick_liquidity_net(&ticks, 60, true), Some(-500));
        // Initialized-but-missing must fail (never silently zero).
        assert_eq!(tick_liquidity_net(&ticks, 120, false), None);
    }

    #[test]
    fn step_target_picks_the_binding_bound() {
        let (lo, hi) = (U256::from(100u64), U256::from(200u64));
        // zero_for_one: price falls, limit is a floor → clamp UP (max)
        assert_eq!(step_target(true, lo, hi), hi);
        // one_for_zero: price rises, limit is a ceiling → clamp DOWN (min)
        assert_eq!(step_target(false, lo, hi), lo);
    }
}
