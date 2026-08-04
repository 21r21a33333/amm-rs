//! Uniswap V2 (and V2-fork) constant-product quoter.
//!
//! Exact-in:  `out = in·(10000−fee)·rOut / (rIn·10000 + in·(10000−fee))`
//! Exact-out: `in  = rIn·out·10000 / ((rOut−out)·(10000−fee)) + 1`
//!
//! Reserves and fee arithmetic operate on `U256` base units, matching
//! `UniswapV2Library.getAmountOut`/`getAmountIn` wei-for-wei.

use alloy_primitives::U256;

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::{PoolId, PoolKind};
use crate::primitives::price::Price;
use crate::primitives::ratio::{Bps, Ratio};
use crate::protocols::two_asset_direction;
use crate::traits::exact_out::ExactOut;
use crate::traits::introspect::Introspect;
use crate::traits::pool::Pool;
use crate::traits::pricing::Pricing;

/// One hundred percent, in basis points.
const BPS_ONE: u32 = 10_000;

/// A Uniswap V2 constant-product pool over two assets.
///
/// `assets` and `reserves` are index-aligned: `reserves[i]` is the on-chain
/// reserve of `assets[i]` in base units. `fee_bps` is the swap fee in basis
/// points (30 for the standard 0.3% pool).
#[derive(Clone, Debug)]
pub struct UniswapV2Pool {
    id: PoolId,
    assets: [AssetId; 2],
    reserves: [U256; 2],
    fee_bps: u32,
}

impl UniswapV2Pool {
    /// Construct a pool from a state snapshot. `reserves[i]` is the reserve of
    /// `assets[i]`.
    pub fn new(id: PoolId, assets: [AssetId; 2], reserves: [U256; 2], fee_bps: u32) -> Self {
        Self {
            id,
            assets,
            reserves,
            fee_bps,
        }
    }

    /// Resolve `(reserve_in, reserve_out)` for a `from -> to` swap, or
    /// [`QuoteError::AssetNotInPool`] if the pair is not this pool's.
    fn reserves_for(&self, from: &AssetId, to: &AssetId) -> Result<(U256, U256), QuoteError> {
        match two_asset_direction(&self.assets, from, to) {
            Some(true) => Ok((self.reserves[0], self.reserves[1])),
            Some(false) => Ok((self.reserves[1], self.reserves[0])),
            None => Err(QuoteError::AssetNotInPool {
                input: *from,
                output: *to,
            }),
        }
    }

    /// The fee multiplier `10000 - fee_bps`, or [`QuoteError::Overflow`] if the
    /// fee exceeds 100%.
    fn fee_factor(&self) -> Result<U256, QuoteError> {
        BPS_ONE
            .checked_sub(self.fee_bps)
            .map(U256::from)
            .ok_or(QuoteError::Overflow)
    }

    /// Constant-product exact-in: output for `amount_in`, rounded down (as the
    /// contract does). [`QuoteError::InsufficientLiquidity`] on a zero reserve.
    fn amount_out(
        &self,
        reserve_in: U256,
        reserve_out: U256,
        amount_in: U256,
    ) -> Result<U256, QuoteError> {
        match reserve_in.is_zero() || reserve_out.is_zero() {
            true => Err(QuoteError::InsufficientLiquidity),
            false => {
                let in_with_fee = amount_in
                    .checked_mul(self.fee_factor()?)
                    .ok_or(QuoteError::Overflow)?;
                let numerator = in_with_fee
                    .checked_mul(reserve_out)
                    .ok_or(QuoteError::Overflow)?;
                let denominator = reserve_in
                    .checked_mul(U256::from(BPS_ONE))
                    .ok_or(QuoteError::Overflow)?
                    .checked_add(in_with_fee)
                    .ok_or(QuoteError::Overflow)?;
                // denominator ≥ reserve_in·10000 > 0, so the division is safe.
                Ok(numerator / denominator)
            }
        }
    }

    /// Closed-form exact-out inverse: the minimum input to receive exactly
    /// `amount_out`, rounded up by `+1` so the pool always receives enough
    /// (matching `getAmountIn`). [`QuoteError::InsufficientLiquidity`] if the
    /// pool cannot cover `amount_out`.
    fn amount_in(
        &self,
        reserve_in: U256,
        reserve_out: U256,
        amount_out: U256,
    ) -> Result<U256, QuoteError> {
        match reserve_in.is_zero() || amount_out >= reserve_out {
            true => Err(QuoteError::InsufficientLiquidity),
            false => {
                let numerator = reserve_in
                    .checked_mul(amount_out)
                    .ok_or(QuoteError::Overflow)?
                    .checked_mul(U256::from(BPS_ONE))
                    .ok_or(QuoteError::Overflow)?;
                let denominator = (reserve_out - amount_out)
                    .checked_mul(self.fee_factor()?)
                    .ok_or(QuoteError::Overflow)?;
                match denominator.is_zero() {
                    true => Err(QuoteError::Overflow),
                    false => (numerator / denominator)
                        .checked_add(U256::from(1u64))
                        .ok_or(QuoteError::Overflow),
                }
            }
        }
    }
}

// ─── Pool + extension trait impls ───────────────────────────────────────────

impl Pool for UniswapV2Pool {
    fn id(&self) -> &PoolId {
        &self.id
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
        let (reserve_in, reserve_out) = self.reserves_for(&amount_in.asset, to)?;
        let out = self.amount_out(reserve_in, reserve_out, amount_in.raw)?;
        Ok(AssetAmount::new(*to, out))
    }
}

impl ExactOut for UniswapV2Pool {
    fn quote_exact_out(
        &self,
        amount_out: &AssetAmount,
        from: &AssetId,
    ) -> Result<AssetAmount, QuoteError> {
        let (reserve_in, reserve_out) = self.reserves_for(from, &amount_out.asset)?;
        let needed = self.amount_in(reserve_in, reserve_out, amount_out.raw)?;
        Ok(AssetAmount::new(*from, needed))
    }
}

impl Introspect for UniswapV2Pool {
    fn fee_bps(&self, source: &AssetId, destination: &AssetId) -> Option<Bps> {
        // A V2 pool charges the same fee in either direction; report it only for
        // the pair it actually trades.
        two_asset_direction(&self.assets, source, destination)
            .map(|_| Bps(u16::try_from(self.fee_bps).unwrap_or(u16::MAX)))
    }

    fn reserve(&self, asset: &AssetId) -> Option<AssetAmount> {
        self.assets
            .iter()
            .position(|a| a == asset)
            .map(|i| AssetAmount::new(*asset, self.reserves[i]))
    }

    fn kind(&self) -> PoolKind {
        PoolKind::UniswapV2
    }
}

impl Pricing for UniswapV2Pool {
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError> {
        // Marginal (mid) price of `quote` per `base` is the reserve ratio,
        // fee-exclusive: `reserve_quote / reserve_base`.
        let (reserve_base, reserve_quote) = self.reserves_for(base, quote)?;
        let ratio =
            Ratio::new(reserve_quote, reserve_base).ok_or(QuoteError::InsufficientLiquidity)?;
        Price::new(*base, *quote, ratio).ok_or(QuoteError::InsufficientLiquidity)
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

    /// Canonical USDC/WETH pool: 1M USDC (6-dec) and 500 WETH (18-dec), 30 bps.
    /// The pinned outputs are wei-exact against `UniswapV2Library.getAmountOut`.
    fn usdc_weth() -> UniswapV2Pool {
        UniswapV2Pool::new(
            PoolId::new("1:univ2:0xusdcweth"),
            [usdc(), weth()],
            [
                U256::from(1_000_000_000_000u64),            // 1M USDC
                U256::from(500_000_000_000_000_000_000u128), // 500 WETH
            ],
            30,
        )
    }

    #[test]
    fn quote_usdc_to_weth_is_wei_exact() {
        let out = usdc_weth()
            .quote(
                &AssetAmount::new(usdc(), U256::from(1_000_000_000u64)),
                &weth(),
            )
            .unwrap();
        assert_eq!(out.asset, weth());
        // (1e9·9970·500e18) / (1e12·10000 + 1e9·9970)
        assert_eq!(out.raw, U256::from(498_003_490_519_951_608u64));
    }

    #[test]
    fn quote_weth_to_usdc_reverse_direction_is_wei_exact() {
        let out = usdc_weth()
            .quote(
                &AssetAmount::new(weth(), U256::from(1_000_000_000_000_000_000u64)),
                &usdc(),
            )
            .unwrap();
        assert_eq!(out.asset, usdc());
        // (1e18·9970·1e12) / (500e18·10000 + 1e18·9970)
        assert_eq!(out.raw, U256::from(1_990_031_876u64));
    }

    #[test]
    fn quote_balanced_pool_matches_hand_computed_formula() {
        // reserves (1M, 1M), 30 bps, in 1000:
        // out = 1000·9970·1_000_000 / (1_000_000·10000 + 1000·9970) = floor(996.007) = 996
        let pool = UniswapV2Pool::new(
            PoolId::new("1:univ2:0xbalanced"),
            [usdc(), weth()],
            [U256::from(1_000_000u64), U256::from(1_000_000u64)],
            30,
        );
        let out = pool
            .quote(&AssetAmount::new(usdc(), U256::from(1000u64)), &weth())
            .unwrap();
        assert_eq!(out.raw, U256::from(996u64));
    }

    #[test]
    fn quote_unknown_pair_errors() {
        assert!(matches!(
            usdc_weth().quote(&AssetAmount::new(dai(), U256::from(1u64)), &weth()),
            Err(QuoteError::AssetNotInPool { .. })
        ));
    }

    #[test]
    fn quote_zero_reserves_is_insufficient_liquidity() {
        let empty = UniswapV2Pool::new(
            PoolId::new("1:univ2:0xempty"),
            [usdc(), weth()],
            [U256::ZERO, U256::ZERO],
            30,
        );
        assert_eq!(
            empty.quote(&AssetAmount::new(usdc(), U256::from(100u64)), &weth()),
            Err(QuoteError::InsufficientLiquidity)
        );
    }

    #[test]
    fn exact_out_is_closed_form_inverse_of_exact_in() {
        // On the balanced pool, 1000 in → 996 out; asking for exactly 996 out must
        // require 1000 in (getAmountIn's +1 ceiling round-trips here).
        let pool = UniswapV2Pool::new(
            PoolId::new("1:univ2:0xbalanced"),
            [usdc(), weth()],
            [U256::from(1_000_000u64), U256::from(1_000_000u64)],
            30,
        );
        let needed = pool
            .quote_exact_out(&AssetAmount::new(weth(), U256::from(996u64)), &usdc())
            .unwrap();
        assert_eq!(needed.asset, usdc());
        assert_eq!(needed.raw, U256::from(1000u64));
    }

    #[test]
    fn exact_out_beyond_reserve_is_insufficient_liquidity() {
        // Cannot withdraw the entire (or more than the) output reserve.
        let out_reserve = U256::from(500_000_000_000_000_000_000u128);
        assert_eq!(
            usdc_weth().quote_exact_out(&AssetAmount::new(weth(), out_reserve), &usdc()),
            Err(QuoteError::InsufficientLiquidity)
        );
    }

    #[test]
    fn spot_price_is_reserve_ratio() {
        // quote/base = reserve_weth / reserve_usdc = 500e18 / 1e12.
        let price = usdc_weth().spot_price(&usdc(), &weth()).unwrap();
        let expected = Ratio::new(
            U256::from(500_000_000_000_000_000_000u128),
            U256::from(1_000_000_000_000u64),
        )
        .unwrap();
        assert_eq!(price.ratio(), &expected);
    }

    #[test]
    fn price_impact_captures_fee_plus_slippage() {
        // Balanced pool, spot = 1:1 → spot-implied out for 1000 in is 1000; actual
        // is 996, so impact = (1000-996)/1000 = 40 bps (30 fee + ~10 slippage).
        let pool = UniswapV2Pool::new(
            PoolId::new("1:univ2:0xbalanced"),
            [usdc(), weth()],
            [U256::from(1_000_000u64), U256::from(1_000_000u64)],
            30,
        );
        let impact = pool
            .price_impact(&AssetAmount::new(usdc(), U256::from(1000u64)), &weth())
            .unwrap();
        assert_eq!(impact, Bps(40));
    }
}
