//! Aerodrome (Solidly) volatile quoter — constant product with the fee removed
//! from the input first: `amountIn -= amountIn·fee/10000`, then
//! `out = amountIn'·rOut / (rIn + amountIn')`, matching Aerodrome's `Pool.sol`.
//!
//! This fee-first truncation differs at the wei level from Uniswap V2's
//! combined `(10000 − fee)` factor, so it is a distinct quoter, not the V2 one.

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

/// One hundred percent, in basis points (Aerodrome's fee denominator).
const BPS_ONE: u32 = 10_000;

/// An Aerodrome volatile (constant-product) pool over two assets.
///
/// `assets` and `reserves` are index-aligned. `fee_bps` is out of 10 000
/// (Aerodrome's `factory.getFee`, e.g. 30 = 0.30%).
#[derive(Clone, Debug)]
pub struct AerodromeVolatilePool {
    id: PoolId,
    assets: [AssetId; 2],
    reserves: [U256; 2],
    fee_bps: u32,
}

impl AerodromeVolatilePool {
    /// Construct a pool from a reserve snapshot. `reserves[i]` is the reserve of
    /// `assets[i]`.
    pub fn new(id: PoolId, assets: [AssetId; 2], reserves: [U256; 2], fee_bps: u32) -> Self {
        Self {
            id,
            assets,
            reserves,
            fee_bps,
        }
    }

    /// Resolve `(reserve_in, reserve_out)` for a `from -> to` swap.
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

    /// Input remaining after Aerodrome's fee: `in − floor(in·fee/10000)`.
    fn after_fee(&self, amount_in: U256) -> Result<U256, QuoteError> {
        let fee = amount_in
            .checked_mul(U256::from(self.fee_bps))
            .ok_or(QuoteError::Overflow)?
            / U256::from(BPS_ONE);
        amount_in.checked_sub(fee).ok_or(QuoteError::Overflow)
    }

    /// Constant-product exact-in on the post-fee input, rounded down.
    fn amount_out(
        &self,
        reserve_in: U256,
        reserve_out: U256,
        amount_in: U256,
    ) -> Result<U256, QuoteError> {
        match reserve_in.is_zero() || reserve_out.is_zero() {
            true => Err(QuoteError::InsufficientLiquidity),
            false => {
                let net = self.after_fee(amount_in)?;
                let numerator = net.checked_mul(reserve_out).ok_or(QuoteError::Overflow)?;
                // reserve_in > 0, so the denominator is non-zero.
                let denominator = reserve_in.checked_add(net).ok_or(QuoteError::Overflow)?;
                Ok(numerator / denominator)
            }
        }
    }

    /// Closed-form exact-out inverse: the minimum input to receive at least
    /// `amount_out`. Rounds up at both the constant-product and fee-gross-up
    /// steps so the result always covers the target.
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
                    .ok_or(QuoteError::Overflow)?;
                let net_needed = (numerator / (reserve_out - amount_out))
                    .checked_add(U256::from(1u64))
                    .ok_or(QuoteError::Overflow)?;
                // Gross the post-fee input back up: in = ceil(net · 10000 / (10000 − fee)).
                let fee_factor = BPS_ONE
                    .checked_sub(self.fee_bps)
                    .ok_or(QuoteError::Overflow)?;
                match fee_factor == 0 {
                    true => Err(QuoteError::Overflow),
                    false => net_needed
                        .checked_mul(U256::from(BPS_ONE))
                        .ok_or(QuoteError::Overflow)?
                        .checked_div(U256::from(fee_factor))
                        .ok_or(QuoteError::Overflow)?
                        .checked_add(U256::from(1u64))
                        .ok_or(QuoteError::Overflow),
                }
            }
        }
    }
}

impl Pool for AerodromeVolatilePool {
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

impl ExactOut for AerodromeVolatilePool {
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

impl Pricing for AerodromeVolatilePool {
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError> {
        // Marginal (mid) price of `quote` per `base` is the reserve ratio,
        // fee-exclusive: `reserve_quote / reserve_base`.
        let (reserve_base, reserve_quote) = self.reserves_for(base, quote)?;
        let ratio =
            Ratio::new(reserve_quote, reserve_base).ok_or(QuoteError::InsufficientLiquidity)?;
        Price::new(*base, *quote, ratio).ok_or(QuoteError::InsufficientLiquidity)
    }
}

impl Introspect for AerodromeVolatilePool {
    fn fee_bps(&self, source: &AssetId, destination: &AssetId) -> Option<Bps> {
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
        PoolKind::AerodromeVolatile
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

    fn pool(reserves: [U256; 2], fee_bps: u32) -> AerodromeVolatilePool {
        AerodromeVolatilePool::new(
            PoolId::new("1:aero-v:0x0"),
            [usdc(), weth()],
            reserves,
            fee_bps,
        )
    }

    #[test]
    fn quote_matches_fee_first_hand_computed_value() {
        // reserves (1e6, 1e6), 30 bps, in 1000: fee = floor(1000·30/10000) = 3,
        // net = 997, out = floor(997·1e6 / (1e6 + 997)) = 996.
        let out = pool([U256::from(1_000_000u64), U256::from(1_000_000u64)], 30)
            .quote(&AssetAmount::new(usdc(), U256::from(1000u64)), &weth())
            .unwrap();
        assert_eq!(out.raw, U256::from(996u64));
    }

    #[test]
    fn small_input_fee_floors_to_zero_unlike_v2() {
        // in 303, 30 bps: fee = floor(303·30/10000) = floor(0.909) = 0, so NO fee is
        // taken; net = 303, out = floor(303·1e6/(1e6+303)) = 302. Uniswap V2's
        // combined (10000−fee) factor never floors the fee away and yields 301 —
        // the wei-level divergence this fee-first formula exists to capture.
        let out = pool([U256::from(1_000_000u64), U256::from(1_000_000u64)], 30)
            .quote(&AssetAmount::new(usdc(), U256::from(303u64)), &weth())
            .unwrap();
        assert_eq!(out.raw, U256::from(302u64));
    }

    #[test]
    fn exact_out_input_covers_the_target() {
        let p = pool(
            [
                U256::from(1_000_000_000_000u64),
                U256::from(2_000_000_000_000u64),
            ],
            30,
        );
        let want = U256::from(1_000_000u64);
        let needed = p
            .quote_exact_out(&AssetAmount::new(weth(), want), &usdc())
            .unwrap();
        assert_eq!(needed.asset, usdc());
        let delivered = p
            .quote(&AssetAmount::new(usdc(), needed.raw), &weth())
            .unwrap();
        assert!(
            delivered.raw >= want,
            "exact-out input must cover the target"
        );
    }

    #[test]
    fn exact_out_beyond_reserve_is_insufficient_liquidity() {
        let p = pool([U256::from(1_000_000u64), U256::from(1_000_000u64)], 30);
        assert_eq!(
            p.quote_exact_out(&AssetAmount::new(weth(), U256::from(1_000_000u64)), &usdc()),
            Err(QuoteError::InsufficientLiquidity)
        );
    }

    #[test]
    fn zero_reserves_and_unknown_pair_error() {
        assert_eq!(
            pool([U256::ZERO, U256::ZERO], 30)
                .quote(&AssetAmount::new(usdc(), U256::from(100u64)), &weth()),
            Err(QuoteError::InsufficientLiquidity)
        );
        assert!(matches!(
            pool([U256::from(1u64), U256::from(1u64)], 30)
                .quote(&AssetAmount::new(dai(), U256::from(1u64)), &weth()),
            Err(QuoteError::AssetNotInPool { .. })
        ));
    }

    #[test]
    fn introspection_reports_fee_reserve_and_kind() {
        let p = pool([U256::from(10u64), U256::from(20u64)], 30);
        assert_eq!(p.fee_bps(&usdc(), &weth()), Some(Bps(30)));
        assert_eq!(p.fee_bps(&usdc(), &dai()), None);
        assert_eq!(
            p.reserve(&weth()),
            Some(AssetAmount::new(weth(), U256::from(20u64)))
        );
        assert_eq!(p.reserve(&dai()), None);
        assert_eq!(p.kind(), PoolKind::AerodromeVolatile);
        // spot price = reserve_weth / reserve_usdc = 20/10 = 2.
        let two = Ratio::new(U256::from(2u64), U256::from(1u64)).unwrap();
        assert_eq!(p.spot_price(&usdc(), &weth()).unwrap().ratio(), &two);
    }
}
