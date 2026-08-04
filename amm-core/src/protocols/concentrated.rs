//! Shared Uniswap concentrated-liquidity tick engine (V3, V4, and Slipstream).
//!
//! Q64.96 arithmetic is delegated to the `uniswap_v3_math` crate; this module
//! owns the swap loop, direction/limit handling, and the price conversions that
//! every concentrated-liquidity quoter builds on. A caller resolves swap
//! direction and the effective per-swap fee, hands over a [`SwapState`], and
//! receives exact-in / exact-out / price-bounded results. A single signed engine
//! serves both directions: `compute_swap_step` selects exact-in vs exact-out
//! from the sign of the specified amount, so exact-out is just a negative input.

use std::collections::HashMap;

use alloy_primitives::{I256, U256};

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::price::Price;
use crate::primitives::ratio::Ratio;
use crate::traits::limits::LimitedQuote;

/// Liquidity bookkeeping for a single initialized tick.
#[derive(Clone, Debug)]
pub struct TickInfo {
    /// Net liquidity added when the tick is crossed left-to-right.
    pub liquidity_net: i128,
    /// Whether the tick is initialized (carries a position boundary).
    pub initialized: bool,
}

/// The tick state a swap traverses: per-tick liquidity, the initialization
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

impl TickData {
    /// Build tick data from a set of initialized ticks, computing the
    /// initialization bitmap. `spacing` is the pool's tick spacing; each tick is
    /// expected to be a multiple of it. This is how a state fetcher assembles the
    /// tick state a swap traverses.
    pub fn from_ticks(spacing: i32, ticks: impl IntoIterator<Item = (i32, TickInfo)>) -> TickData {
        let mut tick_map = HashMap::new();
        let mut bitmap: HashMap<i16, U256> = HashMap::new();
        for (tick, info) in ticks {
            set_bitmap_bit(&mut bitmap, tick, spacing);
            tick_map.insert(tick, info);
        }
        TickData {
            ticks: tick_map,
            bitmap,
            spacing,
        }
    }
}

/// Set the initialization bit for `tick` in a bitmap, using Uniswap's compressed
/// word/bit encoding.
fn set_bitmap_bit(bitmap: &mut HashMap<i16, U256>, tick: i32, spacing: i32) {
    let compressed = match tick < 0 && tick % spacing != 0 {
        true => (tick / spacing) - 1,
        false => tick / spacing,
    };
    let word = (compressed >> 8) as i16;
    let bit = (compressed % 256) as u8;
    *bitmap.entry(word).or_insert(U256::ZERO) |= U256::from(1u64) << bit;
}

/// A concentrated-liquidity market snapshot with the fee resolved for one swap
/// direction. Borrows the pool's [`TickData`]; cheap to build per quote.
pub(crate) struct SwapState<'a> {
    /// Current Q64.96 sqrt price.
    pub sqrt_price_x96: U256,
    /// Current tick.
    pub tick: i32,
    /// Active liquidity at the current price.
    pub liquidity: u128,
    /// Effective fee in pips for this swap direction.
    pub fee_pips: u32,
    /// The pool's tick state.
    pub ticks: &'a TickData,
}

/// The result of running the swap loop.
pub(crate) struct SwapOutcome {
    /// Total input consumed (including fees).
    pub amount_in: U256,
    /// Total output produced.
    pub amount_out: U256,
    /// `true` if the price limit was reached before the specified amount was
    /// fully consumed (a partial fill).
    pub limited: bool,
}

/// The next initialized-tick boundary reached in a step, with its price.
struct NextTick {
    tick: i32,
    price: U256,
    initialized: bool,
}

// ─── the engine ─────────────────────────────────────────────────────────────

/// Run the tick-crossing swap loop toward `limit`.
///
/// `amount_specified` is signed: positive is exact-in (drains toward zero as
/// input is consumed), negative is exact-out (rises toward zero as output is
/// produced).
pub(crate) fn simulate(
    state: &SwapState<'_>,
    zero_for_one: bool,
    amount_specified: I256,
    limit: U256,
) -> Result<SwapOutcome, QuoteError> {
    // A snapshot with no tick data cannot be priced.
    if state.ticks.ticks.is_empty() || state.ticks.bitmap.is_empty() {
        return Err(QuoteError::InsufficientLiquidity);
    }

    let exact_in = amount_specified >= I256::ZERO;
    let mut remaining = amount_specified;
    let mut sqrt = state.sqrt_price_x96;
    let mut tick = state.tick;
    let mut liquidity = state.liquidity;
    let mut amount_in = U256::ZERO;
    let mut amount_out = U256::ZERO;

    while remaining != I256::ZERO && sqrt != limit {
        let next = next_tick(state.ticks, tick, zero_for_one)?;
        let target = step_target(zero_for_one, next.price, limit);

        let (next_sqrt, step_in, step_out, step_fee) =
            uniswap_v3_math::swap_math::compute_swap_step(
                sqrt,
                target,
                liquidity,
                remaining,
                state.fee_pips,
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
        (tick, liquidity) = advance_tick(
            state.ticks,
            tick,
            liquidity,
            zero_for_one,
            next,
            price_start,
            sqrt,
        )?;
    }

    Ok(SwapOutcome {
        amount_in,
        amount_out,
        limited: remaining != I256::ZERO,
    })
}

/// Resolve the next initialized-tick boundary from the bitmap (clamped to the
/// valid tick range), with its sqrt price.
fn next_tick(ticks: &TickData, tick: i32, zero_for_one: bool) -> Result<NextTick, QuoteError> {
    let (raw, initialized) = uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
        &ticks.bitmap,
        tick,
        ticks.spacing,
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
    ticks: &TickData,
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
                    let net = tick_liquidity_net(&ticks.ticks, next.tick, zero_for_one)
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
///
/// Hand-rolled rather than `uniswap_v3_math::liquidity_math::add_delta`: that
/// helper negates its `i128` argument, which panics in debug builds on
/// `i128::MIN`; `unsigned_abs()` here stays correct across the full `i128` range.
fn crossed_liquidity(liquidity: u128, liquidity_net: i128) -> Option<u128> {
    match liquidity_net.is_negative() {
        true => liquidity.checked_sub(liquidity_net.unsigned_abs()),
        false => liquidity.checked_add(liquidity_net as u128),
    }
}

// ─── shared quote entry points (used by V3, V4) ──────────────────────────────

/// A `U256` reinterpreted as a positive `I256`, or [`QuoteError::Overflow`] if
/// bit 255 is set (not a representable positive amount).
fn positive_i256(x: U256) -> Result<I256, QuoteError> {
    let v = I256::from_raw(x);
    match v < I256::ZERO {
        true => Err(QuoteError::Overflow),
        false => Ok(v),
    }
}

/// Exact-in output for a resolved direction. `InsufficientLiquidity` if the input
/// exceeds the pool's total liquidity — a partial fill is rejected rather than
/// silently returning a short output (symmetric with [`amount_in`]).
pub(crate) fn amount_out(
    state: &SwapState<'_>,
    zero_for_one: bool,
    amount_in: U256,
) -> Result<U256, QuoteError> {
    let spec = positive_i256(amount_in)?;
    let outcome = simulate(state, zero_for_one, spec, price_limit(zero_for_one))?;
    match outcome.limited {
        true => Err(QuoteError::InsufficientLiquidity),
        false => Ok(outcome.amount_out),
    }
}

/// Exact-out input for a resolved direction. A partial fill means the pool cannot
/// source the full output.
pub(crate) fn amount_in(
    state: &SwapState<'_>,
    zero_for_one: bool,
    amount_out: U256,
) -> Result<U256, QuoteError> {
    let mag = positive_i256(amount_out)?;
    let outcome = simulate(
        state,
        zero_for_one,
        mag.wrapping_neg(),
        price_limit(zero_for_one),
    )?;
    match outcome.limited {
        true => Err(QuoteError::InsufficientLiquidity),
        false => Ok(outcome.amount_in),
    }
}

/// The maximum input absorbable before the price reaches its extreme. `I256::MAX`
/// exceeds any pool's absorbable input, so the swap halts at the extreme and the
/// consumed input is the bound (`compute_swap_step`'s internal `mul_div` is
/// 512-bit, so `I256::MAX` cannot overflow it).
pub(crate) fn max_amount_in(state: &SwapState<'_>, zero_for_one: bool) -> Option<U256> {
    simulate(state, zero_for_one, I256::MAX, price_limit(zero_for_one))
        .ok()
        .map(|o| o.amount_in)
}

/// The marginal (mid) price of `quote` per `base` from a sqrt price.
pub(crate) fn spot_price(
    base: &AssetId,
    quote: &AssetId,
    sqrt_price_x96: U256,
    zero_for_one: bool,
) -> Result<Price, QuoteError> {
    // sqrtPriceX96 encodes token1 per token0 = sqrtP² / 2¹⁹².
    let token1_per_token0 = Ratio::from_q192_sqrt(sqrt_price_x96);
    let ratio = match zero_for_one {
        true => token1_per_token0,
        false => token1_per_token0
            .invert()
            .ok_or(QuoteError::InsufficientLiquidity)?,
    };
    Price::new(*base, *quote, ratio).ok_or(QuoteError::InsufficientLiquidity)
}

/// Price-bounded (partial-fill) quote for a resolved direction.
pub(crate) fn quote_with_limit(
    state: &SwapState<'_>,
    assets: &[AssetId; 2],
    zero_for_one: bool,
    amount_in: &AssetAmount,
    to: &AssetId,
    limit: &Price,
) -> Result<LimitedQuote, QuoteError> {
    let spec = positive_i256(amount_in.raw)?;
    let sqrt_limit = clamped_sqrt_limit(assets, limit)?;
    // If the price is already past the bound, nothing swaps toward it.
    let outcome = match limit_already_reached(state.sqrt_price_x96, zero_for_one, sqrt_limit) {
        true => SwapOutcome {
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            limited: true,
        },
        false => simulate(state, zero_for_one, spec, sqrt_limit)?,
    };
    Ok(LimitedQuote {
        amount_in: AssetAmount::new(amount_in.asset, outcome.amount_in),
        amount_out: AssetAmount::new(*to, outcome.amount_out),
        limited: outcome.limited,
    })
}

/// Convert a caller `Price` bound into a `sqrtPriceX96` in the pool's
/// `token1/token0` orientation (accepting either input orientation), clamped
/// into the valid engine range. `Err(AssetNotInPool)` if the price is about
/// assets this pool does not trade.
fn clamped_sqrt_limit(assets: &[AssetId; 2], limit: &Price) -> Result<U256, QuoteError> {
    let (token0, token1) = (assets[0], assets[1]);
    let ratio_10 = match (
        limit.base() == token0 && limit.quote() == token1,
        limit.base() == token1 && limit.quote() == token0,
    ) {
        (true, _) => limit.ratio().clone(),
        (_, true) => limit
            .ratio()
            .clone()
            .invert()
            .ok_or(QuoteError::InsufficientLiquidity)?,
        _ => {
            return Err(QuoteError::AssetNotInPool {
                input: limit.base(),
                output: limit.quote(),
            });
        }
    };
    let raw = ratio_10.to_q192_sqrt().ok_or(QuoteError::Overflow)?;
    let lo = uniswap_v3_math::tick_math::MIN_SQRT_RATIO + U256::from(1u64);
    let hi = uniswap_v3_math::tick_math::MAX_SQRT_RATIO - U256::from(1u64);
    Ok(raw.clamp(lo, hi))
}

/// Whether the price is already at or past `sqrt_limit` for this direction, so no
/// swap toward it is possible.
fn limit_already_reached(sqrt_price_x96: U256, zero_for_one: bool, sqrt_limit: U256) -> bool {
    match zero_for_one {
        true => sqrt_limit >= sqrt_price_x96, // price falls; limit is below
        false => sqrt_limit <= sqrt_price_x96, // price rises; limit is above
    }
}

#[cfg(test)]
pub(crate) fn set_tick_bit(bitmap: &mut HashMap<i16, U256>, tick: i32, spacing: i32) {
    set_bitmap_bit(bitmap, tick, spacing);
}

/// Shared fixtures for the V3/V4 quoter test modules. Which helpers are used
/// depends on the enabled feature set, so unused ones are allowed.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod fixtures {
    use super::{TickData, TickInfo, set_tick_bit};
    use crate::primitives::asset::{AssetId, ChainId};
    use alloy_primitives::B256;
    use std::collections::HashMap;

    /// sqrtPriceX96 for tick 0 (price 1:1) = 2^96.
    pub(crate) const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;

    pub(crate) fn usdc() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x01]))
    }
    pub(crate) fn weth() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x02]))
    }
    pub(crate) fn dai() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x03]))
    }

    /// A full-range position `[-887220, 887220]` carrying `liq`, at spacing 60.
    pub(crate) fn full_range_ticks(liq: i128) -> TickData {
        let (lower, upper) = (-887_220i32, 887_220i32);
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
        set_tick_bit(&mut bitmap, lower, 60);
        set_tick_bit(&mut bitmap, upper, 60);
        TickData {
            ticks,
            bitmap,
            spacing: 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn limit_already_reached_checks_the_correct_side() {
        let cur = U256::from(1_000u64);
        // zero_for_one (price falls): a limit at/above current is already passed.
        assert!(limit_already_reached(cur, true, U256::from(1_000u64)));
        assert!(limit_already_reached(cur, true, U256::from(1_001u64)));
        assert!(!limit_already_reached(cur, true, U256::from(999u64)));
        // one_for_zero (price rises): a limit at/below current is already passed.
        assert!(limit_already_reached(cur, false, U256::from(1_000u64)));
        assert!(!limit_already_reached(cur, false, U256::from(1_001u64)));
    }
}
