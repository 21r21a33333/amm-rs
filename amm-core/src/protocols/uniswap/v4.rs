//! Uniswap V4 concentrated-liquidity quoter.
//!
//! V4's swap math is identical to V3, so this adapter feeds the shared
//! [`concentrated`](super::concentrated) engine. The two differences it models:
//! per-direction effective fees (the LP fee compounded with V4's per-direction
//! protocol fee), and hooks — a pool whose hook alters the curve or sets a
//! dynamic fee cannot be quoted statically and is refused.

use alloy_primitives::{Address, B256, U256};

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

/// How a V4 pool's hook affects quoting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
///
/// The last three fields are the pool's on-chain **identity** — the `PoolKey`
/// fields (`currency0`/`currency1` from `assets`, plus `key_fee`, `tick_spacing`,
/// `hooks_address`) whose `abi.encode` hashes to `pool_id`. Quoting never needs
/// them, but building a swap does: a V4 swap must reconstruct the full `PoolKey`.
/// `key_fee` is the *static* pool-key fee (the value in the key), distinct from
/// the live per-direction fees above — for a dynamic-fee pool they differ, and
/// only the static one hashes to `pool_id`. `hooks_address` is kept separate from
/// the `hooks` quoting flag: a `Hooks::None` pool can still carry a non-zero,
/// price-neutral hook address that the `PoolKey` must include.
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
    key_fee: u32,
    tick_spacing: i32,
    hooks_address: Address,
}

impl UniswapV4Pool {
    /// Construct a pool from a slot0 + liquidity + tick-state snapshot.
    ///
    /// `fee_zero_for_one`/`fee_one_for_zero` are the effective per-direction fees
    /// in pips (see [`UniswapV4Pool::combined_fee`]). `key_fee`, `tick_spacing`,
    /// and `hooks_address` are the `PoolKey` identity fields a swap needs (see the
    /// struct docs); they do not affect quoting.
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
        key_fee: u32,
        tick_spacing: i32,
        hooks_address: Address,
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
            key_fee,
            tick_spacing,
            hooks_address,
        }
    }

    /// V4's effective swap fee (pips): the protocol fee is taken first, then the
    /// LP fee on the remainder, matching v4-core's
    /// `ProtocolFeeLibrary.calculateSwapFee`:
    ///
    /// `protocol + lp − ⌊protocol·lp / 1e6⌋`
    ///
    /// The cross-term must be *floored* (not the algebraically equal
    /// `protocol + lp·(1e6 − protocol)/1e6`, which floors a different term and
    /// yields a fee one pip lower). Both inputs are per-direction pip values;
    /// `protocol == 0` collapses to the LP fee.
    pub fn combined_fee(lp_fee: u32, protocol_fee: u32) -> u32 {
        let (lp, pf) = (lp_fee as u64, protocol_fee as u64);
        (pf + lp - pf * lp / 1_000_000) as u32
    }

    /// The 32-byte V4 pool id (hash of the pool key).
    pub fn pool_id(&self) -> B256 {
        self.pool_id
    }

    /// The static pool-key fee (pips) — the `PoolKey.fee` value that hashes to
    /// [`pool_id`](Self::pool_id), not the live effective fee used for quoting.
    pub fn key_fee(&self) -> u32 {
        self.key_fee
    }

    /// The pool's tick spacing (`PoolKey.tickSpacing`).
    pub fn tick_spacing(&self) -> i32 {
        self.tick_spacing
    }

    /// The pool's hook contract address (`PoolKey.hooks`; zero if none).
    pub fn hooks_address(&self) -> Address {
        self.hooks_address
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

    fn as_exact_out(&self) -> Option<&dyn ExactOut> {
        Some(self)
    }
    fn as_pricing(&self) -> Option<&dyn Pricing> {
        Some(self)
    }
    fn as_introspect(&self) -> Option<&dyn Introspect> {
        Some(self)
    }
    fn as_limits(&self) -> Option<&dyn Limits> {
        Some(self)
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
        two_asset_direction(&self.assets, source, destination).map(|zero_for_one| {
            Bps(u16::try_from(self.fee_pips(zero_for_one) / 100).unwrap_or(u16::MAX))
        })
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
            3000,
            60,
            Address::ZERO,
        )
    }

    #[test]
    fn identity_accessors_expose_the_pool_key_fields() {
        let pool = full_range_pool(3000, 3000, Hooks::None);
        assert_eq!(pool.key_fee(), 3000);
        assert_eq!(pool.tick_spacing(), 60);
        assert_eq!(pool.hooks_address(), Address::ZERO);
    }

    #[test]
    fn combined_fee_compounds_lp_and_protocol() {
        // lp 500 + protocol 125: 125 + 500 − ⌊125·500/1e6⌋ = 625, per v4-core
        // `calculateSwapFee`. Flooring the other cross-term would give 624.
        assert_eq!(UniswapV4Pool::combined_fee(500, 125), 625);
        // A pair whose cross-term forces the rounding boundary (lp 500,
        // protocol 253): 253 + 500 − ⌊253·500/1e6⌋ = 753, where the other-term
        // form ⌊500·(1e6−253)/1e6⌋ + 253 = 752.
        assert_eq!(UniswapV4Pool::combined_fee(500, 253), 753);
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

    #[test]
    fn capability_accessors_expose_extension_traits_via_dyn_pool() {
        let pool = full_range_pool(3000, 3000, Hooks::None);
        let dynamic: &dyn Pool = &pool;
        // V4 implements every extension trait, each reachable from a `&dyn Pool`.
        assert!(dynamic.as_exact_out().is_some());
        assert!(dynamic.as_pricing().is_some());
        assert!(dynamic.as_introspect().is_some());
        assert!(dynamic.as_limits().is_some());
        // The handle is the real thing: exact-out via the accessor equals the
        // direct call, wei-for-wei.
        let want = AssetAmount::new(weth(), U256::from(1_000_000u64));
        assert_eq!(
            dynamic
                .as_exact_out()
                .unwrap()
                .quote_exact_out(&want, &usdc()),
            pool.quote_exact_out(&want, &usdc()),
        );
    }
}
