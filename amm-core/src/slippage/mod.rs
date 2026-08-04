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
}
