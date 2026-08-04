//! Exact-rational arithmetic ([`Ratio`]), rounding modes ([`Rounding`]), and
//! typed basis points ([`Bps`]).
//!
//! `Ratio` is the price *lingua franca*: a reduced, non-negative rational built
//! on `num_rational::BigRational`. Arbitrary precision is required because a
//! Uniswap V3 price is `sqrtPriceX96² / 2¹⁹²`, whose numerator (~2³²¹) exceeds
//! `U256`. Using `num-rational` keeps this correct and small; the reduction,
//! comparison, and rounding logic are the crate's, not ours.

use alloy_primitives::U256;
use num_bigint::{BigInt, Sign};
use num_rational::BigRational;
use num_traits::Zero;

/// Rounding mode, applied only at value-producing boundaries (never silently).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// Toward zero (floor for non-negative values).
    Down,
    /// Away from zero (ceil for non-negative values).
    Up,
    /// To nearest; ties round away from zero (up, for non-negative values).
    HalfUp,
}

/// Typed basis points (1 bp = 0.01%). Replaces bare `u32` bps parameters.
///
/// `u16` is sufficient: fees and slippage are well under 100%, and a price
/// impact fraction is in `[0, 1]` (≤ 10 000 bps).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Bps(pub u16);

/// A reduced, non-negative exact rational.
///
/// Backed by `num_rational::BigRational`, so construction is always reduced and
/// arithmetic never overflows. Structural equality is value equality.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ratio(BigRational);

impl Ratio {
    /// Build from `U256` parts. `None` if `den == 0`.
    pub fn new(num: U256, den: U256) -> Option<Ratio> {
        match den.is_zero() {
            true => None,
            false => Some(Ratio(BigRational::new(to_bigint(num), to_bigint(den)))),
        }
    }

    /// Build a Uniswap V3 price `sqrtPriceX96² / 2¹⁹²`. Infallible: arbitrary
    /// precision means the squared numerator cannot overflow.
    pub fn from_q192_sqrt(sqrt_price_x96: U256) -> Ratio {
        let s = to_bigint(sqrt_price_x96);
        let num = &s * &s;
        let den = BigInt::from(2u8).pow(192);
        Ratio(BigRational::new(num, den))
    }

    /// The Uniswap V3 sqrt price whose square is this ratio:
    /// `floor(sqrt(self · 2¹⁹²))`. The exact inverse of [`Ratio::from_q192_sqrt`]
    /// for perfect squares, flooring otherwise. `None` only if the result exceeds
    /// `U256` (unreachable for any in-range price).
    pub fn to_q192_sqrt(&self) -> Option<U256> {
        // sqrtPriceX96 = floor( sqrt( numer · 2¹⁹² / denom ) ). numer ≥ 0 and
        // denom > 0 (a non-negative reduced rational), so the root is well-defined.
        let shifted = (self.0.numer().clone() << 192usize) / self.0.denom().clone();
        from_bigint(&shifted.sqrt())
    }

    /// The reciprocal. `None` if the ratio is zero.
    pub fn invert(self) -> Option<Ratio> {
        match self.0.is_zero() {
            true => None,
            false => Some(Ratio(self.0.recip())),
        }
    }

    /// Apply the ratio to a raw amount: `x * num / den`, rounded. `None` if the
    /// result exceeds `U256`.
    pub(crate) fn apply(&self, x: U256, rounding: Rounding) -> Option<U256> {
        let scaled = &self.0 * BigRational::from_integer(to_bigint(x));
        let integer = match rounding {
            Rounding::Down => scaled.floor(),
            Rounding::Up => scaled.ceil(),
            Rounding::HalfUp => scaled.round(),
        }
        .to_integer();
        from_bigint(&integer)
    }

    /// Whether the ratio equals zero.
    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }
}

impl core::ops::Mul for Ratio {
    type Output = Ratio;

    /// Multiply two ratios (reduced). Infallible with arbitrary precision.
    fn mul(self, rhs: Ratio) -> Ratio {
        Ratio(self.0 * rhs.0)
    }
}

fn to_bigint(x: U256) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &x.to_be_bytes::<32>())
}

fn from_bigint(x: &BigInt) -> Option<U256> {
    let (sign, bytes) = x.to_bytes_be();
    match sign == Sign::Minus || bytes.len() > 32 {
        true => None,
        false => Some(U256::from_be_slice(&bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(n: u64, d: u64) -> Ratio {
        Ratio::new(U256::from(n), U256::from(d)).unwrap()
    }

    #[test]
    fn reduces_on_construction() {
        assert_eq!(r(6, 4), r(3, 2));
    }

    #[test]
    fn zero_denominator_is_none() {
        assert!(Ratio::new(U256::from(1u64), U256::ZERO).is_none());
    }

    #[test]
    fn invert_roundtrips() {
        assert_eq!(r(3, 2).invert().unwrap().invert().unwrap(), r(3, 2));
    }

    #[test]
    fn invert_zero_is_none() {
        assert!(r(0, 5).invert().is_none());
    }

    #[test]
    fn ord_compares_values() {
        assert!(r(1, 3) < r(1, 2));
        assert!(r(2, 3) > r(1, 2));
        assert_eq!(r(2, 4), r(1, 2));
    }

    #[test]
    fn apply_rounds_each_way() {
        assert_eq!(
            r(1, 3).apply(U256::from(10u64), Rounding::Down).unwrap(),
            U256::from(3u64)
        );
        assert_eq!(
            r(1, 3).apply(U256::from(10u64), Rounding::Up).unwrap(),
            U256::from(4u64)
        );
        assert_eq!(
            r(1, 2).apply(U256::from(5u64), Rounding::HalfUp).unwrap(),
            U256::from(3u64)
        );
    }

    #[test]
    fn mul_reduces() {
        // 2/3 * 3/4 = 1/2
        assert_eq!(r(2, 3) * r(3, 4), r(1, 2));
    }

    #[test]
    fn from_q192_sqrt_price_one() {
        // sqrtP = 2^96 → price = (2^96)^2 / 2^192 = 1
        let sqrt = U256::from(1u64) << 96;
        assert_eq!(Ratio::from_q192_sqrt(sqrt), r(1, 1));
    }

    #[test]
    fn to_q192_sqrt_inverts_from_q192_sqrt_on_perfect_squares() {
        let two_96 = U256::from(1u64) << 96; // sqrtP for price 1
        assert_eq!(Ratio::from_q192_sqrt(two_96).to_q192_sqrt(), Some(two_96));
        let two_97 = U256::from(1u64) << 97; // sqrtP for price 4
        assert_eq!(r(4, 1).to_q192_sqrt(), Some(two_97));
        assert_eq!(Ratio::from_q192_sqrt(two_97), r(4, 1));
    }

    #[test]
    fn to_q192_sqrt_is_the_floor_of_the_exact_root() {
        // price 2 has an irrational sqrt; `sp` must be the largest value whose
        // square (as a price) does not exceed 2.
        let two = r(2, 1);
        let sp = two.to_q192_sqrt().unwrap();
        assert!(Ratio::from_q192_sqrt(sp) <= two);
        assert!(Ratio::from_q192_sqrt(sp + U256::from(1u64)) > two);
    }

    #[test]
    fn from_q192_sqrt_holds_large_numerator() {
        // sqrtP = 2^160 → price = 2^320 / 2^192 = 2^128, exact (no overflow).
        let sqrt = U256::from(1u64) << 160;
        let ratio = Ratio::from_q192_sqrt(sqrt);
        let expected = Ratio::new(U256::from(1u64) << 128, U256::from(1u64)).unwrap();
        assert_eq!(ratio, expected);
    }
}
