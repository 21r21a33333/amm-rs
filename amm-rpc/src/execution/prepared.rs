//! Quote-to-build bridge types: [`Route`], [`PreparedSwap`], and
//! [`ApprovalRequirement`].
//!
//! These types carry the output of the build layer back to callers, expressing
//! exactly what the signer and submitter need — the transaction envelope, the
//! expected output floor, an optional input ceiling (exact-out), and any ERC-20
//! approval that must be sent first.

use alloy::primitives::{Address, U256};
use amm_core::primitives::asset::{AssetAmount, AssetId};
use amm_core::primitives::ratio::Bps;

use crate::execution::types::{TradeType, UnsignedTx};

/// A route through one or more liquidity pools, expressed as an ordered list of
/// asset hops and their corresponding fee tiers.
///
/// `hops` always has one more entry than `fee_tiers`: for a single-hop swap
/// `A → B`, `hops = [A, B]` and `fee_tiers` carries the pool fee (or is empty
/// when the fee is embedded in the pool identifier and need not be passed
/// separately).
///
/// This is a v1 single-hop shape; the `hops`/`fee_tiers` pair generalises
/// naturally to multi-hop paths without breaking the type.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// Ordered token sequence: `[in, hop₁, …, out]`. Always at least two
    /// entries.
    pub hops: Vec<AssetId>,
    /// Per-pool fee tiers (hundredths of a basis point, Uniswap convention).
    /// Empty when fees are carried implicitly by the pool key.
    pub fee_tiers: Vec<u32>,
    /// Whether the exact constraint is on the input or output side.
    pub trade_type: TradeType,
}

impl Route {
    /// Construct a single-hop route `input → output` with no explicit fee tier.
    ///
    /// `hops` is set to `[input, output]`; `fee_tiers` is empty. The caller may
    /// push a fee tier into `fee_tiers` after construction when the router
    /// requires it.
    pub fn new_single_hop(input: AssetId, output: AssetId, trade_type: TradeType) -> Route {
        Route {
            hops: vec![input, output],
            fee_tiers: vec![],
            trade_type,
        }
    }
}

/// The ERC-20 allowance a swap requires before the transaction can be sent.
///
/// Some tokens (USDT, KNC, and similar non-standard ERC-20s) implement the
/// `require(allowance == 0 || amount == 0)` guard, which means an existing
/// non-zero allowance must be reset to zero before a new non-zero approval can
/// be set. `reset_first` signals this requirement to the caller.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequirement {
    /// The contract that will spend the tokens (router / Permit2 entry-point).
    pub spender: Address,
    /// The token that needs to be approved.
    pub token: AssetId,
    /// The minimum allowance the spender must have; callers may grant more.
    pub min_allowance: U256,
    /// When `true`, the caller must first send `approve(spender, 0)` before
    /// granting `min_allowance` (USDT/KNC-class reset-first requirement).
    pub reset_first: bool,
}

/// The fully-built output of the swap builder: everything a signer and
/// submitter need to execute a trade.
///
/// `approval` is `None` when no ERC-20 approval is required — for example,
/// when the input token is the chain's native asset (ETH, MATIC, …). When
/// present, it must be submitted (and confirmed) before `tx` is sent.
///
/// `max_spent` is `Some` only on exact-out trades, where the input amount is a
/// ceiling rather than an exact value. On exact-in trades it is `None`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSwap {
    /// The signer-ready transaction to submit.
    pub tx: UnsignedTx,
    /// The minimum output the caller will receive (exact-in: exact target;
    /// exact-out: the floor after applying slippage).
    pub min_received: AssetAmount,
    /// The maximum input the caller will spend (exact-out trades only).
    pub max_spent: Option<AssetAmount>,
    /// ERC-20 approval required before `tx` is sent. `None` for native-input
    /// swaps or when an existing sufficient allowance is already in place.
    pub approval: Option<ApprovalRequirement>,
    /// Estimated price impact in basis points. `None` when the pool state
    /// needed for the estimate is unavailable.
    pub price_impact: Option<Bps>,
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;
    use amm_core::primitives::asset::ChainId;

    use super::*;

    fn asset(chain: u64, byte: u8) -> AssetId {
        AssetId::new(ChainId(chain), B256::left_padding_from(&[byte]))
    }

    #[test]
    fn new_single_hop_exact_in_yields_two_hops_no_fee_tiers() {
        let a = asset(1, 0xAA);
        let b = asset(1, 0xBB);
        let route = Route::new_single_hop(a, b, TradeType::ExactIn);

        assert_eq!(route.hops, vec![a, b], "hops should be [input, output]");
        assert!(
            route.fee_tiers.is_empty(),
            "fee_tiers should be empty for new_single_hop"
        );
        assert_eq!(route.trade_type, TradeType::ExactIn);
    }

    #[test]
    fn new_single_hop_exact_out_preserves_trade_type() {
        let a = asset(1, 0x01);
        let b = asset(1, 0x02);
        let route = Route::new_single_hop(a, b, TradeType::ExactOut);

        assert_eq!(route.hops, vec![a, b]);
        assert!(route.fee_tiers.is_empty());
        assert_eq!(route.trade_type, TradeType::ExactOut);
    }
}
