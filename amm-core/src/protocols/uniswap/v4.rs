//! Uniswap V4 concentrated-liquidity quoter.
//!
//! V4's swap math is identical to V3, so this adapter feeds the shared
//! [`concentrated`](super::concentrated) engine. The two differences it models:
//! per-direction effective fees (the LP fee compounded with V4's per-direction
//! protocol fee), and hooks — a pool whose hook alters the curve or sets a
//! dynamic fee cannot be quoted statically and is refused.

use alloy_primitives::{B256, U256};

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::{PoolId, PoolKind};
use crate::primitives::price::Price;
use crate::primitives::ratio::Bps;
use crate::protocols::concentrated::{self, SwapState, TickData};
use crate::protocols::two_asset_direction;
use crate::traits::exact_out::ExactOut;
use crate::traits::introspect::Introspect;
use crate::traits::limits::{LimitedQuote, Limits};
use crate::traits::pool::Pool;
use crate::traits::pricing::Pricing;

pub use crate::protocols::concentrated::TickInfo;

/// How a V4 pool's hook affects quoting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hooks {
    /// No hook, or a hook that does not alter swap pricing — quotes are exact.
    None,
    /// A hook whose effect cannot be reproduced from static state (a custom
    /// curve or dynamic fee); every quote is refused with
    /// [`QuoteError::Unsupported`].
    Unsupported,
}

/// A Uniswap V4 concentrated-liquidity pool over two assets.
///
/// `assets[0]`/`assets[1]` are the pool's `token0`/`token1`. Fees are stored
/// per direction in pips (V4 protocol fees are per-direction), already compounded
/// with the LP fee via [`UniswapV4Pool::combined_fee`].
#[derive(Clone, Debug)]
pub struct UniswapV4Pool {
    id: PoolId,
    pool_id: B256,
    assets: [AssetId; 2],
    sqrt_price_x96: U256,
    liquidity: u128,
    tick: i32,
    fee_zero_for_one: u32,
    fee_one_for_zero: u32,
    tick_data: TickData,
    hooks: Hooks,
}

impl UniswapV4Pool {
    /// Construct a pool from a slot0 + liquidity + tick-state snapshot.
    ///
    /// `fee_zero_for_one`/`fee_one_for_zero` are the effective per-direction fees
    /// in pips (see [`UniswapV4Pool::combined_fee`]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PoolId,
        pool_id: B256,
        assets: [AssetId; 2],
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
        fee_zero_for_one: u32,
        fee_one_for_zero: u32,
        tick_data: TickData,
        hooks: Hooks,
    ) -> Self {
        Self {
            id,
            pool_id,
            assets,
            sqrt_price_x96,
            liquidity,
            tick,
            fee_zero_for_one,
            fee_one_for_zero,
            tick_data,
            hooks,
        }
    }

    /// V4's effective swap fee (pips): the protocol fee is taken first, then the
    /// LP fee on the remainder — `protocol + lp·(1e6 − protocol)/1e6`, matching
    /// v4-core. Both inputs are per-direction pip values.
    pub fn combined_fee(lp_fee: u32, protocol_fee: u32) -> u32 {
        let (lp, pf) = (lp_fee as u64, protocol_fee as u64);
        (pf + lp * (1_000_000 - pf) / 1_000_000) as u32
    }

    /// The 32-byte V4 pool id (hash of the pool key).
    pub fn pool_id(&self) -> B256 {
        self.pool_id
    }

    /// The effective fee (pips) for this swap direction.
    fn fee_pips(&self, zero_for_one: bool) -> u32 {
        match zero_for_one {
            true => self.fee_zero_for_one,
            false => self.fee_one_for_zero,
        }
    }

    /// The market snapshot handed to the shared engine, with the fee resolved for
    /// this swap direction.
    fn state(&self, zero_for_one: bool) -> SwapState<'_> {
        SwapState {
            sqrt_price_x96: self.sqrt_price_x96,
            tick: self.tick,
            liquidity: self.liquidity,
            fee_pips: self.fee_pips(zero_for_one),
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

    /// Refuse quotes on pools whose hook cannot be reproduced statically.
    fn ensure_supported(&self) -> Result<(), QuoteError> {
        match self.hooks {
            Hooks::None => Ok(()),
            Hooks::Unsupported => Err(QuoteError::Unsupported),
        }
    }
}

impl Pool for UniswapV4Pool {
    fn id(&self) -> &PoolId {
        &self.id
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
        self.ensure_supported()?;
        let zero_for_one = self.direction(&amount_in.asset, to)?;
        let out = concentrated::amount_out(&self.state(zero_for_one), zero_for_one, amount_in.raw)?;
        Ok(AssetAmount::new(*to, out))
    }
}

impl ExactOut for UniswapV4Pool {
    fn quote_exact_out(
        &self,
        amount_out: &AssetAmount,
        from: &AssetId,
    ) -> Result<AssetAmount, QuoteError> {
        self.ensure_supported()?;
        let zero_for_one = self.direction(from, &amount_out.asset)?;
        let needed =
            concentrated::amount_in(&self.state(zero_for_one), zero_for_one, amount_out.raw)?;
        Ok(AssetAmount::new(*from, needed))
    }
}

impl Pricing for UniswapV4Pool {
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError> {
        self.ensure_supported()?;
        let zero_for_one = self.direction(base, quote)?;
        concentrated::spot_price(base, quote, self.sqrt_price_x96, zero_for_one)
    }
}

impl Introspect for UniswapV4Pool {
    fn fee_bps(&self, source: &AssetId, destination: &AssetId) -> Option<Bps> {
        // Per-direction effective fee in pips; 100 pips = 1 bp.
        two_asset_direction(&self.assets, source, destination)
            .map(|zero_for_one| Bps((self.fee_pips(zero_for_one) / 100) as u16))
    }

    fn reserve(&self, _asset: &AssetId) -> Option<AssetAmount> {
        None
    }

    fn kind(&self) -> PoolKind {
        PoolKind::UniswapV4
    }
}

impl Limits for UniswapV4Pool {
    fn max_amount_in(&self, from: &AssetId, to: &AssetId) -> Option<AssetAmount> {
        self.ensure_supported().ok()?;
        let zero_for_one = two_asset_direction(&self.assets, from, to)?;
        concentrated::max_amount_in(&self.state(zero_for_one), zero_for_one)
            .map(|raw| AssetAmount::new(*from, raw))
    }

    fn quote_with_limit(
        &self,
        amount_in: &AssetAmount,
        to: &AssetId,
        limit: Price,
    ) -> Result<LimitedQuote, QuoteError> {
        self.ensure_supported()?;
        let zero_for_one = self.direction(&amount_in.asset, to)?;
        concentrated::quote_with_limit(
            &self.state(zero_for_one),
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
    use alloy_primitives::B256;

    /// A full-range USDC/WETH V4 pool at tick 0, 1e18 liquidity, with the given
    /// per-direction fees (pips) and hook classification.
    fn full_range_pool(
        fee_zero_for_one: u32,
        fee_one_for_zero: u32,
        hooks: Hooks,
    ) -> UniswapV4Pool {
        let liq: i128 = 1_000_000_000_000_000_000;
        UniswapV4Pool::new(
            PoolId::new("1:univ4:0xfull"),
            B256::repeat_byte(0xAA),
            [usdc(), weth()],
            U256::from(SQRT_1_1),
            liq as u128,
            0,
            fee_zero_for_one,
            fee_one_for_zero,
            full_range_ticks(liq),
            hooks,
        )
    }

    #[test]
    fn combined_fee_compounds_lp_and_protocol() {
        // lp 500 + protocol 125 → ~624 pips, per v4-core.
        assert_eq!(UniswapV4Pool::combined_fee(500, 125), 624);
        // No protocol fee leaves the LP fee unchanged.
        assert_eq!(UniswapV4Pool::combined_fee(3000, 0), 3000);
    }

    #[test]
    fn quote_at_parity_is_input_minus_fee() {
        let pool = full_range_pool(3000, 3000, Hooks::None);
        let amount_in = U256::from(1_000_000_000u64);
        let out = pool
            .quote(&AssetAmount::new(usdc(), amount_in), &weth())
            .unwrap();
        assert!(out.raw < amount_in);
        assert!(out.raw > amount_in * U256::from(996u64) / U256::from(1000u64));
    }

    #[test]
    fn per_direction_fees_make_the_costlier_side_pay_more() {
        // Same symmetric pool; token1→token0 charges 1% vs token0→token1's 0.3%,
        // so the same input yields strictly less on the costlier side.
        let pool = full_range_pool(3000, 10_000, Hooks::None);
        let amount = U256::from(1_000_000_000u64);
        let cheap = pool
            .quote(&AssetAmount::new(usdc(), amount), &weth())
            .unwrap(); // 0.30%
        let dear = pool
            .quote(&AssetAmount::new(weth(), amount), &usdc())
            .unwrap(); // 1.00%
        assert!(cheap.raw > dear.raw);
    }

    #[test]
    fn introspection_reports_fee_reserve_and_kind() {
        let pool = full_range_pool(3000, 10_000, Hooks::None);
        assert_eq!(pool.fee_bps(&usdc(), &weth()), Some(Bps(30))); // 0.30% direction
        assert_eq!(pool.fee_bps(&weth(), &usdc()), Some(Bps(100))); // 1.00% direction
        assert_eq!(pool.reserve(&usdc()), None); // no simple V4 reserve
        assert_eq!(pool.kind(), PoolKind::UniswapV4);
    }

    #[test]
    fn unsupported_hooks_refuse_every_quote() {
        let pool = full_range_pool(3000, 3000, Hooks::Unsupported);
        assert_eq!(
            pool.quote(&AssetAmount::new(usdc(), U256::from(1_000u64)), &weth()),
            Err(QuoteError::Unsupported)
        );
        assert_eq!(
            pool.quote_exact_out(&AssetAmount::new(weth(), U256::from(1_000u64)), &usdc()),
            Err(QuoteError::Unsupported)
        );
        assert!(pool.max_amount_in(&usdc(), &weth()).is_none());
    }
}
