//! Sealed [`Executable`] trait and [`as_executable`] dispatch.
//!
//! Only in-crate pool types may implement [`Executable`]; the [`private::Sealed`]
//! bound enforces this at compile time. [`as_executable`] is the single dispatch
//! point: it tries a `downcast_ref` for each known concrete pool type and returns
//! `None` for pools with no encoder yet registered.

use core::any::Any;

use amm_core::primitives::asset::AssetAmount;
use amm_core::traits::pool::Pool;

use crate::execution::{
    config::ChainConfig,
    error::BuildError,
    options::ExecutionOptions,
    prepared::{PreparedSwap, Route},
    types::{Currency, CurrencyAmount},
};

mod private {
    /// Sealing supertrait — only types in this crate can name it.
    pub trait Sealed {}
}

/// Re-export so downstream modules in this crate can write `impl Sealed for …`
/// without reaching into the private module directly.
pub(crate) use private::Sealed;

/// A pool that can encode an on-chain swap transaction.
///
/// Sealed: only in-crate pool types may implement this trait, so
/// [`as_executable`] remains the single source of dispatch truth. Downstream
/// callers obtain an `&dyn Executable` exclusively through [`as_executable`].
pub trait Executable: private::Sealed {
    /// Build an exact-in swap transaction: spend `amount_in` to receive as much
    /// of `to` as possible, with output floor given by `quoted_out`.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the pool state, chain config, or options are
    /// insufficient to produce a valid transaction.
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        route: &Route,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError>;

    /// Build an exact-out swap transaction: receive exactly `amount_out` of
    /// `from`, spending at most the ceiling given by `quoted_in`.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the pool state, chain config, or options are
    /// insufficient to produce a valid transaction.
    fn build_swap_exact_out(
        &self,
        ctx: &ChainConfig,
        amount_out: CurrencyAmount,
        from: Currency,
        route: &Route,
        quoted_in: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError>;
}

/// Recover a pool's swap encoder, if one is registered.
///
/// Tries each known concrete pool type via `downcast_ref` (TypeId is the
/// safety check — no `PoolKind` gate needed). Returns `None` for pools whose
/// `Executable` impl has not yet landed.
pub fn as_executable(pool: &dyn Pool) -> Option<&dyn Executable> {
    let any: &dyn Any = pool;
    if let Some(p) = any.downcast_ref::<amm_core::protocols::uniswap::v2::UniswapV2Pool>() {
        return Some(p);
    }
    if let Some(p) = any.downcast_ref::<amm_core::protocols::uniswap::v3::UniswapV3Pool>() {
        return Some(p);
    }
    if let Some(p) =
        any.downcast_ref::<amm_core::protocols::aerodrome::slipstream::AerodromeSlipstreamPool>()
    {
        return Some(p);
    }
    if let Some(p) =
        any.downcast_ref::<amm_core::protocols::aerodrome::stable::AerodromeStablePool>()
    {
        return Some(p);
    }
    if let Some(p) =
        any.downcast_ref::<amm_core::protocols::aerodrome::volatile::AerodromeVolatilePool>()
    {
        return Some(p);
    }
    #[cfg(feature = "curve")]
    if let Some(p) = any.downcast_ref::<amm_core::protocols::curve::pool::CurvePool>() {
        return Some(p);
    }
    if let Some(p) = any.downcast_ref::<amm_core::protocols::uniswap::v4::UniswapV4Pool>() {
        return Some(p);
    }
    None
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{B256, U256};
    use amm_core::primitives::asset::{AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::protocols::uniswap::v2::UniswapV2Pool;
    use amm_core::traits::pool::Pool;

    use super::as_executable;

    /// `as_executable` returns `Some` for a `UniswapV2Pool` — the dispatch arm
    /// is active once `impl Executable for UniswapV2Pool` lands.
    #[test]
    fn as_executable_is_some_for_v2_pool() {
        let a = AssetId::new(ChainId(1), B256::left_padding_from(&[1]));
        let b = AssetId::new(ChainId(1), B256::left_padding_from(&[2]));
        let pool = UniswapV2Pool::new(PoolId::new("1:univ2:0x"), [a, b], [U256::from(1u64); 2], 30);
        assert!(as_executable(&pool as &dyn Pool).is_some());
    }
}
