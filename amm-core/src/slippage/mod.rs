//! Slippage tolerance ([`Slippage`]): turns a quoted amount into an execution
//! guard (minimum output / maximum input), and compounds across a path.

use alloy_primitives::U256;

use crate::primitives::asset::AssetAmount;
use crate::primitives::ratio::{Bps, Ratio, Rounding};

/// One hundred percent, in basis points.
const BPS_ONE: u64 = 10_000;

/// A slippage tolerance, expressed in basis points.
///
/// Guards round *against* the trader: [`Slippage::min_amount_out`] rounds down,
/// [`Slippage::max_amount_in`] rounds up, so a guard never lies about what will
/// be accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slippage(Bps);

impl Slippage {
    /// A tolerance of `bps` basis points.
    pub fn from_bps(bps: Bps) -> Self {
        Self(bps)
    }

    /// The tolerance in basis points.
    pub fn bps(&self) -> Bps {
        self.0
    }

    /// The minimum output to accept for a quoted output: `quoted * (1 - tol)`,
    /// rounded **down**.
    #[must_use]
    pub fn min_amount_out(&self, quoted_out: &AssetAmount) -> AssetAmount {
        let Bps(tol) = self.0;
        let num = BPS_ONE.saturating_sub(u64::from(tol));
        let factor = Ratio::new(U256::from(num), U256::from(BPS_ONE)).expect("denominator != 0");
        let raw = factor
            .apply(quoted_out.raw, Rounding::Down)
            .unwrap_or(U256::ZERO);
        AssetAmount::new(quoted_out.asset, raw)
    }

    /// The maximum input to spend for a quoted input: `quoted * (1 + tol)`,
    /// rounded **up** (saturating at `U256::MAX`).
    #[must_use]
    pub fn max_amount_in(&self, quoted_in: &AssetAmount) -> AssetAmount {
        let Bps(tol) = self.0;
        let num = BPS_ONE + u64::from(tol);
        let factor = Ratio::new(U256::from(num), U256::from(BPS_ONE)).expect("denominator != 0");
        let raw = factor
            .apply(quoted_in.raw, Rounding::Up)
            .unwrap_or(U256::MAX);
        AssetAmount::new(quoted_in.asset, raw)
    }

    /// Compound this per-hop tolerance across `hops`: `1 - (1 - tol)^hops`,
    /// floored to whole basis points.
    #[must_use]
    pub fn compound(self, hops: usize) -> Slippage {
        let Bps(tol) = self.0;
        let one_minus_t = Ratio::new(
            U256::from(BPS_ONE.saturating_sub(u64::from(tol))),
            U256::from(BPS_ONE),
        )
        .expect("denominator != 0");
        let mut kept = Ratio::new(U256::from(1u64), U256::from(1u64)).expect("denominator != 0");
        for _ in 0..hops {
            kept = kept * one_minus_t.clone();
        }
        // compounded = floor(10_000 * (1 - kept)) = 10_000 - ceil(10_000 * kept)
        let kept_bps = kept
            .apply(U256::from(BPS_ONE), Rounding::Up)
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(BPS_ONE);
        Slippage(Bps(BPS_ONE.saturating_sub(kept_bps) as u16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::{AssetId, ChainId};
    use alloy_primitives::B256;

    fn amt(raw: u64) -> AssetAmount {
        let asset = AssetId::new(ChainId(1), B256::left_padding_from(&[0xaa]));
        AssetAmount::new(asset, U256::from(raw))
    }

    #[test]
    fn min_out_rounds_down_on_remainder() {
        // 33 bps off 100 = 100 * 9967/10000 = 99.67 -> floor 99
        assert_eq!(
            Slippage::from_bps(Bps(33)).min_amount_out(&amt(100)).raw,
            U256::from(99u64)
        );
    }

    #[test]
    fn max_in_rounds_up_on_remainder() {
        // 33 bps over 100 = 100 * 10033/10000 = 100.33 -> ceil 101
        assert_eq!(
            Slippage::from_bps(Bps(33)).max_amount_in(&amt(100)).raw,
            U256::from(101u64)
        );
    }

    #[test]
    fn compound_two_legs_50bps_floors_to_99() {
        // 1 - (1 - 0.005)^2 = 0.009975 -> 99.75 bps -> floor 99
        assert_eq!(Slippage::from_bps(Bps(50)).compound(2).bps(), Bps(99));
    }

    #[test]
    fn compound_one_leg_is_identity_zero_legs_is_none() {
        assert_eq!(Slippage::from_bps(Bps(50)).compound(1).bps(), Bps(50));
        assert_eq!(Slippage::from_bps(Bps(50)).compound(0).bps(), Bps(0));
    }

    use proptest::prelude::*;

    proptest! {
        /// The guards round against the trader for every quote and tolerance:
        /// min-out is the floor of `q·(1−tol)` and never exceeds `q`; max-in is
        /// the ceil of `q·(1+tol)` and never falls below `q`.
        #[test]
        fn guards_round_against_the_trader(q in 0u64.., tol in 0u16..=10_000) {
            let (qq, den) = (U256::from(q), U256::from(BPS_ONE));
            let s = Slippage::from_bps(Bps(tol));
            let min_out = s.min_amount_out(&amt(q)).raw;
            let max_in = s.max_amount_in(&amt(q)).raw;

            let num_out = U256::from(BPS_ONE - u64::from(tol));
            let num_in = U256::from(BPS_ONE + u64::from(tol));
            prop_assert_eq!(min_out, qq * num_out / den); // floor
            prop_assert_eq!(max_in, (qq * num_in + den - U256::from(1u64)) / den); // ceil
            prop_assert!(min_out <= qq);
            prop_assert!(max_in >= qq);
        }

        /// A wider tolerance only ever loosens the guard.
        #[test]
        fn wider_tolerance_loosens_the_guard(q in 1u64.., t0 in 0u16..5_000, bump in 1u16..5_000) {
            let (lo, hi) = (Slippage::from_bps(Bps(t0)), Slippage::from_bps(Bps(t0 + bump)));
            prop_assert!(hi.min_amount_out(&amt(q)).raw <= lo.min_amount_out(&amt(q)).raw);
            prop_assert!(hi.max_amount_in(&amt(q)).raw >= lo.max_amount_in(&amt(q)).raw);
        }

        /// Compounding over more hops never reduces the effective tolerance.
        #[test]
        fn compound_is_monotone_in_hops(tol in 0u16..=1_000, hops in 0usize..8) {
            let s = Slippage::from_bps(Bps(tol));
            prop_assert!(s.compound(hops + 1).bps().0 >= s.compound(hops).bps().0);
        }
    }
}
