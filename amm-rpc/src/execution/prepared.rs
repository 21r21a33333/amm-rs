//! Build-layer output types: [`PreparedSwap`] and [`ApprovalRequirement`].
//!
//! These types carry the output of the build layer back to callers, expressing
//! exactly what the signer and submitter need — the transaction envelope, the
//! expected output floor, an optional input ceiling (exact-out), and any ERC-20
//! approval that must be sent first.

use alloy::primitives::{Address, U256};
use amm_core::primitives::asset::{AssetAmount, AssetId};
use amm_core::primitives::ratio::Bps;

use crate::execution::types::UnsignedTx;

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
    /// The output the caller is guaranteed to receive.
    ///
    /// - **exact-in:** the slippage floor (`slippage.min_amount_out(quoted)`).
    /// - **exact-out:** the exact target output (slippage bounds the *input*
    ///   instead — see [`max_spent`](Self::max_spent)).
    ///
    /// For native-output swaps `.asset` is the wrapped identity (WETH); the
    /// recipient receives the same amount of unwrapped native ETH.
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
