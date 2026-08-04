//! Aerodrome (Solidly) **stable** quoter — the `x³y + y³x` invariant.
//!
//! Stable pools hold near-parity assets, so instead of `x·y = k` they preserve
//! `k = x³y + y³x`. There is no closed form, so `getAmountOut` removes the fee,
//! scales both reserves to `1e18`, then Newton-iterates `get_y` for the new
//! opposite reserve that restores the invariant. It is bit-identical to
//! Aerodrome's on-chain `Pool.sol` (`_k`/`_f`/`_d`/`_get_y`) — same integer
//! truncations, same Newton edge cases — so a quote matches the chain to the wei.
//! Exact-out reuses the same solver on the symmetric invariant, rounding up so
//! the delivered output always covers the request.

use alloy_primitives::U256;

use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::{PoolId, PoolKind};
use crate::primitives::ratio::Bps;
use crate::protocols::two_asset_direction;
use crate::traits::exact_out::ExactOut;
use crate::traits::introspect::Introspect;
use crate::traits::pool::Pool;

/// One hundred percent, in basis points (Aerodrome's fee denominator).
const BPS_ONE: u32 = 10_000;

/// An Aerodrome stable (Solidly `x³y + y³x`) pool over two assets.
///
/// `assets`/`reserves` are index-aligned. `units[i]` is `10^decimals` for
/// `assets[i]` (the reserves are scaled to `1e18` by these before the solve).
/// `fee_bps` is out of 10 000.
///
/// Implements [`Pool`] + [`ExactOut`] + [`Introspect`]. A marginal
/// [`Pricing`](crate::traits::pricing::Pricing) spot price (the derivative of
/// the stable invariant) is intentionally deferred to its own task.
#[derive(Clone, Debug)]
pub struct AerodromeStablePool {
    id: PoolId,
    assets: [AssetId; 2],
    reserves: [U256; 2],
    units: [U256; 2],
    fee_bps: u32,
}

impl AerodromeStablePool {
    /// Construct from reserves, per-asset decimal counts, and the swap fee.
    ///
    /// `decimals[i]` is the decimal count (e.g. 6, 18); it is stored as
    /// `10^decimals` as the contract keeps it.
    pub fn new(
        id: PoolId,
        assets: [AssetId; 2],
        reserves: [U256; 2],
        decimals: [u8; 2],
        fee_bps: u32,
    ) -> Self {
        Self {
            id,
            assets,
            reserves,
            units: [pow10(decimals[0]), pow10(decimals[1])],
            fee_bps,
        }
    }

    /// `zero_for_one` for `from -> to`, plus the `(in, out)` reserve/unit pairs
    /// scaled to `1e18` for the solve.
    fn scaled(&self, from: &AssetId, to: &AssetId) -> Result<Scaled, QuoteError> {
        let zero_for_one =
            two_asset_direction(&self.assets, from, to).ok_or(QuoteError::AssetNotInPool {
                input: *from,
                output: *to,
            })?;
        let r0 = scale(self.reserves[0], self.units[0])?;
        let r1 = scale(self.reserves[1], self.units[1])?;
        Ok(match zero_for_one {
            true => Scaled {
                reserve_in: r0,
                reserve_out: r1,
                unit_in: self.units[0],
                unit_out: self.units[1],
            },
            false => Scaled {
                reserve_in: r1,
                reserve_out: r0,
                unit_in: self.units[1],
                unit_out: self.units[0],
            },
        })
    }

    /// `_k`: the invariant on raw reserves, each scaled to `1e18` by its unit.
    fn k(&self, x: U256, y: U256) -> Option<U256> {
        stable_invariant(scale(x, self.units[0]).ok()?, scale(y, self.units[1]).ok()?)
    }

    /// `getAmountOut`: exact-input output in base units. `None` on zero input, an
    /// empty reserve, overflow, or Newton non-convergence.
    fn amount_out(&self, amount_in: U256, s: &Scaled) -> Result<U256, QuoteError> {
        match amount_in.is_zero() || self.reserves[0].is_zero() || self.reserves[1].is_zero() {
            true => Err(QuoteError::InsufficientLiquidity),
            false => {
                let net = self.after_fee(amount_in)?;
                let xy = self.invariant()?;
                let amount_in_scaled = scale(net, s.unit_in)?;
                let y_new = self
                    .get_y(
                        amount_in_scaled
                            .checked_add(s.reserve_in)
                            .ok_or(QuoteError::Overflow)?,
                        xy,
                        s.reserve_out,
                    )
                    .ok_or(QuoteError::InsufficientLiquidity)?;
                let out_scaled = s
                    .reserve_out
                    .checked_sub(y_new)
                    .ok_or(QuoteError::Overflow)?;
                out_scaled
                    .checked_mul(s.unit_out)
                    .ok_or(QuoteError::Overflow)
                    .map(|v| v / e18())
            }
        }
    }

    /// Exact-out: the minimum input to receive at least `amount_out`. Solves the
    /// symmetric invariant for the new input reserve, then rounds up through the
    /// unit and fee conversions so the delivered output covers the request.
    fn amount_in(&self, amount_out: U256, s: &Scaled) -> Result<U256, QuoteError> {
        match amount_out.is_zero() || self.reserves[0].is_zero() || self.reserves[1].is_zero() {
            true => Err(QuoteError::InsufficientLiquidity),
            false => {
                let xy = self.invariant()?;
                let out_scaled = ceil_div(
                    amount_out.checked_mul(e18()).ok_or(QuoteError::Overflow)?,
                    s.unit_out,
                )?;
                let new_reserve_out = s
                    .reserve_out
                    .checked_sub(out_scaled)
                    .ok_or(QuoteError::InsufficientLiquidity)?;
                // Symmetric invariant: hold the (reduced) output reserve, solve for
                // the input reserve that restores k.
                let new_reserve_in = self
                    .get_y(new_reserve_out, xy, s.reserve_in)
                    .ok_or(QuoteError::InsufficientLiquidity)?;
                let in_scaled = new_reserve_in
                    .checked_sub(s.reserve_in)
                    .ok_or(QuoteError::InsufficientLiquidity)?;
                let net = ceil_div(
                    in_scaled
                        .checked_mul(s.unit_in)
                        .ok_or(QuoteError::Overflow)?,
                    e18(),
                )?;
                // Gross the post-fee input back up (round up). The two ceilings
                // bound the unit and fee conversion error, but not the Newton
                // solver's own residual (`get_y` returns an approximate root), so
                // add 1 wei to absorb it and keep the round-trip from under-delivering.
                let fee_factor = BPS_ONE
                    .checked_sub(self.fee_bps)
                    .ok_or(QuoteError::Overflow)?;
                match fee_factor == 0 {
                    true => Err(QuoteError::Overflow),
                    false => ceil_div(
                        net.checked_mul(U256::from(BPS_ONE))
                            .ok_or(QuoteError::Overflow)?,
                        U256::from(fee_factor),
                    )?
                    .checked_add(U256::from(1u64))
                    .ok_or(QuoteError::Overflow),
                }
            }
        }
    }

    /// The invariant `xy = _k(reserve0, reserve1)`.
    fn invariant(&self) -> Result<U256, QuoteError> {
        self.k(self.reserves[0], self.reserves[1])
            .ok_or(QuoteError::Overflow)
    }

    /// Input remaining after the fee: `in − floor(in·fee/10000)`.
    fn after_fee(&self, amount_in: U256) -> Result<U256, QuoteError> {
        let fee = amount_in
            .checked_mul(U256::from(self.fee_bps))
            .ok_or(QuoteError::Overflow)?
            / U256::from(BPS_ONE);
        amount_in.checked_sub(fee).ok_or(QuoteError::Overflow)
    }

    /// `_get_y`: Newton-iterate for the opposite reserve satisfying `f(x0,y)=xy`.
    /// Reproduces Aerodrome's edge cases exactly (including its `_k(x0, y+1)` call,
    /// which rescales the already-scaled `x0` by decimals). `None` mirrors the
    /// contract's `revert("!y")` non-convergence.
    fn get_y(&self, x0: U256, xy: U256, mut y: U256) -> Option<U256> {
        let one = U256::from(1u8);
        for _ in 0..255 {
            let k = f(x0, y)?;
            match k < xy {
                true => {
                    let derivative = d(x0, y)?;
                    if derivative.is_zero() {
                        return None;
                    }
                    let mut dy = (xy - k).checked_mul(e18())? / derivative;
                    if dy.is_zero() {
                        if k == xy {
                            return Some(y);
                        }
                        if self.k(x0, y.checked_add(one)?)? > xy {
                            return Some(y + one);
                        }
                        dy = one;
                    }
                    y = y.checked_add(dy)?;
                }
                false => {
                    let derivative = d(x0, y)?;
                    if derivative.is_zero() {
                        return None;
                    }
                    let mut dy = (k - xy).checked_mul(e18())? / derivative;
                    if dy.is_zero() {
                        if k == xy || f(x0, y.checked_sub(one)?)? < xy {
                            return Some(y);
                        }
                        dy = one;
                    }
                    y = y.checked_sub(dy)?;
                }
            }
        }
        None
    }
}

/// The `(in, out)` reserves and units for one swap direction, reserves scaled to `1e18`.
struct Scaled {
    reserve_in: U256,
    reserve_out: U256,
    unit_in: U256,
    unit_out: U256,
}

/// `10^decimals`.
fn pow10(decimals: u8) -> U256 {
    U256::from(10u128.pow(decimals as u32))
}

/// The fixed-point unit `1e18`.
fn e18() -> U256 {
    U256::from(1_000_000_000_000_000_000u128)
}

/// Scale a raw amount of a token with unit `10^decimals` to `1e18` fixed point.
fn scale(x: U256, unit: U256) -> Result<U256, QuoteError> {
    x.checked_mul(e18())
        .ok_or(QuoteError::Overflow)
        .map(|v| v / unit)
}

/// `ceil(a / b)`; `Err` if `b == 0`.
fn ceil_div(a: U256, b: U256) -> Result<U256, QuoteError> {
    match b.is_zero() {
        true => Err(QuoteError::Overflow),
        false => Ok(a
            .checked_add(b - U256::from(1u64))
            .ok_or(QuoteError::Overflow)?
            / b),
    }
}

/// The invariant `x³y + y³x` in `1e18` fixed point: `(xy/1e18)·((x²+y²)/1e18)/1e18`.
fn stable_invariant(x: U256, y: U256) -> Option<U256> {
    let a = x.checked_mul(y)? / e18();
    let b = (x.checked_mul(x)? / e18()).checked_add(y.checked_mul(y)? / e18())?;
    Some(a.checked_mul(b)? / e18())
}

/// `_f`: the invariant at already-scaled `(x0, y)`.
fn f(x0: U256, y: U256) -> Option<U256> {
    stable_invariant(x0, y)
}

/// `_d`: `∂f/∂y = 3·x0·y²/1e18² + x0³/1e18²`, the Newton step denominator.
fn d(x0: U256, y: U256) -> Option<U256> {
    let three = U256::from(3u8);
    let term1 = three
        .checked_mul(x0)?
        .checked_mul(y.checked_mul(y)? / e18())?
        / e18();
    let term2 = (x0.checked_mul(x0)? / e18()).checked_mul(x0)? / e18();
    term1.checked_add(term2)
}

impl Pool for AerodromeStablePool {
    fn id(&self) -> &PoolId {
        &self.id
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
        let s = self.scaled(&amount_in.asset, to)?;
        let out = self.amount_out(amount_in.raw, &s)?;
        Ok(AssetAmount::new(*to, out))
    }
}

impl ExactOut for AerodromeStablePool {
    fn quote_exact_out(
        &self,
        amount_out: &AssetAmount,
        from: &AssetId,
    ) -> Result<AssetAmount, QuoteError> {
        let s = self.scaled(from, &amount_out.asset)?;
        let needed = self.amount_in(amount_out.raw, &s)?;
        Ok(AssetAmount::new(*from, needed))
    }
}

impl Introspect for AerodromeStablePool {
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
        PoolKind::AerodromeStable
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
    fn dai() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x02]))
    }
    fn weth() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x03]))
    }

    const E18: u128 = 1_000_000_000_000_000_000;

    /// A value-balanced USDC(6)/DAI(18) stable pool: 1M of each, 0.05% fee.
    fn usdc_dai_pool() -> AerodromeStablePool {
        AerodromeStablePool::new(
            PoolId::new("1:aero-s:0x0"),
            [usdc(), dai()],
            [
                U256::from(1_000_000u128 * 1_000_000),
                U256::from(1_000_000u128 * E18),
            ],
            [6, 18],
            5,
        )
    }

    #[test]
    fn balanced_pool_swaps_near_parity() {
        // 1 USDC (1e6) → just under 1 DAI (1e18): below parity, above 0.99 after fee.
        let out = usdc_dai_pool()
            .quote(&AssetAmount::new(usdc(), U256::from(1_000_000u64)), &dai())
            .unwrap();
        assert_eq!(out.asset, dai());
        assert!(out.raw < U256::from(E18), "out {} must be < 1 DAI", out.raw);
        assert!(
            out.raw > U256::from(990_000_000_000_000_000u128),
            "out {} > 0.99 DAI",
            out.raw
        );
    }

    #[test]
    fn deep_pool_1000_in_is_near_1000_out() {
        // 1000 USDC into a 1M/1M stable pool: ~1000 DAI, just under, after fee+slippage.
        let out = usdc_dai_pool()
            .quote(
                &AssetAmount::new(usdc(), U256::from(1_000_000_000u64)),
                &dai(),
            )
            .unwrap();
        assert!(out.raw > U256::from(998u128 * E18));
        assert!(out.raw < U256::from(1000u128 * E18));
    }

    #[test]
    fn symmetric_pool_quotes_equally_both_directions() {
        // 18/18 equal reserves, no fee: identical output in either direction.
        let r = U256::from(1_000_000u128 * E18);
        let pool = AerodromeStablePool::new(
            PoolId::new("1:aero-s:0xsym"),
            [dai(), usdc()],
            [r, r],
            [18, 18],
            0,
        );
        let amt = U256::from(E18);
        let a = pool.quote(&AssetAmount::new(dai(), amt), &usdc()).unwrap();
        let b = pool.quote(&AssetAmount::new(usdc(), amt), &dai()).unwrap();
        assert_eq!(a.raw, b.raw);
    }

    #[test]
    fn exact_out_input_covers_the_target() {
        // Round-trip: the reverse-solved input must deliver at least the request.
        let pool = usdc_dai_pool();
        let want = U256::from(500_000_000_000_000_000u128); // 0.5 DAI
        let needed = pool
            .quote_exact_out(&AssetAmount::new(dai(), want), &usdc())
            .unwrap();
        assert_eq!(needed.asset, usdc());
        let delivered = pool
            .quote(&AssetAmount::new(usdc(), needed.raw), &dai())
            .unwrap();
        assert!(
            delivered.raw >= want,
            "exact-out input {} delivered {} < target {want}",
            needed.raw,
            delivered.raw
        );
    }

    #[test]
    fn zero_input_empty_reserves_and_unknown_pair_error() {
        let pool = usdc_dai_pool();
        assert_eq!(
            pool.quote(&AssetAmount::new(usdc(), U256::ZERO), &dai()),
            Err(QuoteError::InsufficientLiquidity)
        );
        assert!(matches!(
            pool.quote(&AssetAmount::new(weth(), U256::from(1u64)), &dai()),
            Err(QuoteError::AssetNotInPool { .. })
        ));
    }

    #[test]
    fn introspection_reports_fee_reserve_and_kind() {
        let pool = usdc_dai_pool();
        assert_eq!(pool.fee_bps(&usdc(), &dai()), Some(Bps(5)));
        assert_eq!(pool.fee_bps(&usdc(), &weth()), None);
        assert_eq!(
            pool.reserve(&usdc()),
            Some(AssetAmount::new(
                usdc(),
                U256::from(1_000_000u128 * 1_000_000)
            ))
        );
        assert_eq!(pool.kind(), PoolKind::AerodromeStable);
    }
}
