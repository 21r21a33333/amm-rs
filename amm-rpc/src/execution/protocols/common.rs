//! Shared building blocks for the protocol swap encoders: address/recipient/
//! deadline resolution, the common guard+resolve preamble, the concentrated
//! sqrt-price-limit resolver, and PreparedSwap/approval assembly.

use alloy::primitives::aliases::U160;
use alloy::primitives::{Address, Bytes, U256};
use amm_core::primitives::asset::{AssetAmount, AssetId};
use amm_core::primitives::price::Price;
use amm_core::traits::pool::Pool;

use crate::execution::{
    config::ChainConfig,
    error::BuildError,
    options::{Deadline, ExecutionOptions, Recipient},
    prepared::{ApprovalRequirement, PreparedSwap, Route},
    types::{Currency, TradeType, UnsignedTx},
};

/// Rightmost 20 bytes of an asset's 32-byte token id as an EVM address.
pub(crate) fn evm_addr(a: &AssetId) -> Address {
    Address::from_word(a.token)
}

/// Resolve a [`Recipient`] to a concrete EVM address.
///
/// # Errors
///
/// Returns [`BuildError::UnresolvedRecipient`] when the recipient is still the
/// `Sender` placeholder — the caller must have resolved it via
/// [`crate::execution::options::resolve`] first.
pub(crate) fn resolve_recipient(r: &Recipient) -> Result<Address, BuildError> {
    match r {
        Recipient::To(a) => Ok(*a),
        Recipient::Sender => Err(BuildError::UnresolvedRecipient),
    }
}

/// Resolve a [`Deadline`] to a `U256` Unix timestamp for on-chain use.
///
/// # Errors
///
/// - [`BuildError::UnresolvedDeadline`] — `FromNow` or `AtBlock`: must be
///   converted to an absolute timestamp by [`crate::execution::options::resolve`]
///   before the build layer is called. `AtBlock` is rejected because V2 bounds
///   by timestamp, so a block-number deadline cannot be converted here.
pub(crate) fn resolve_deadline(d: &Deadline) -> Result<U256, BuildError> {
    match d {
        Deadline::AtTimestamp(t) => Ok(U256::from(*t)),
        Deadline::FromNow(_) => Err(BuildError::UnresolvedDeadline),
        // V2 uses a Unix timestamp; a block number cannot be resolved here.
        Deadline::AtBlock(_) => Err(BuildError::UnresolvedDeadline),
    }
}

/// The validated, resolved inputs common to every single-hop swap encoder.
#[derive(Debug)]
pub(crate) struct ResolvedSwap {
    pub input: AssetId,
    pub output: AssetId,
    pub recipient: Address,
    pub deadline: U256,
    pub native_in: bool,
    pub native_out: bool,
}

/// Run the preamble shared by every encoder: trade-type guard, native↔native
/// rejection, input/output resolution (Native→WETH), pool-membership+distinctness
/// check, and recipient+deadline resolution.
pub(crate) fn resolve_swap(
    ctx: &ChainConfig,
    pool: &dyn Pool,
    in_currency: Currency,
    out_currency: Currency,
    route: &Route,
    opts: &ExecutionOptions,
    expected: TradeType,
) -> Result<ResolvedSwap, BuildError> {
    if route.trade_type != expected {
        return Err(BuildError::UnsupportedProtocol);
    }
    let native_in = in_currency.is_native();
    let native_out = out_currency.is_native();
    if native_in && native_out {
        return Err(BuildError::NativeMismatch);
    }
    let input = in_currency.resolve(ctx.weth);
    let output = out_currency.resolve(ctx.weth);
    let assets = pool.assets();
    if !(assets.contains(&input) && assets.contains(&output) && input != output) {
        return Err(BuildError::AssetNotInPool { input, output });
    }
    let recipient = resolve_recipient(&opts.recipient)?;
    let deadline = resolve_deadline(&opts.deadline)?;
    Ok(ResolvedSwap {
        input,
        output,
        recipient,
        deadline,
        native_in,
        native_out,
    })
}

/// Map the amm-core price converter's QuoteError into a BuildError.
pub(crate) fn map_price_limit_err(e: amm_core::error::QuoteError) -> BuildError {
    match e {
        amm_core::error::QuoteError::AssetNotInPool { input, output } => {
            BuildError::AssetNotInPool { input, output }
        }
        _ => BuildError::Overflow,
    }
}

/// Resolve an optional `price_limit` into a `sqrtPriceLimitX96` for single-hop
/// concentrated swaps: `U160::ZERO` when absent; multi-hop (`hops != 2`) + a limit
/// is `UnsupportedProtocol`.
pub(crate) fn resolve_sqrt_limit(
    assets: &[AssetId],
    price_limit: Option<&Price>,
    hops: usize,
) -> Result<U160, BuildError> {
    match price_limit {
        None => Ok(U160::ZERO),
        Some(limit) => {
            if hops != 2 {
                return Err(BuildError::UnsupportedProtocol);
            }
            let pair = [assets[0], assets[1]];
            let raw = amm_core::protocols::sqrt_price_limit_x96(&pair, limit)
                .map_err(map_price_limit_err)?;
            U160::checked_from_limbs_slice(raw.as_limbs()).ok_or(BuildError::Overflow)
        }
    }
}

/// The standard reset-free ERC-20 approval a token-input swap needs.
pub(crate) fn erc20_approval(
    spender: Address,
    token: AssetId,
    min_allowance: U256,
) -> ApprovalRequirement {
    ApprovalRequirement {
        spender,
        token,
        min_allowance,
        reset_first: false,
    }
}

/// Assemble a [`PreparedSwap`] from encoded calldata + resolved amounts. `price_impact`
/// is always `None` until an estimator lands.
pub(crate) fn prepared(
    ctx: &ChainConfig,
    router: Address,
    data: Bytes,
    value: U256,
    min_received: AssetAmount,
    max_spent: Option<AssetAmount>,
    approval: Option<ApprovalRequirement>,
) -> PreparedSwap {
    PreparedSwap {
        tx: UnsignedTx {
            chain: ctx.chain,
            to: router,
            data,
            value,
        },
        min_received,
        max_spent,
        approval,
        price_impact: None,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, U256};
    use amm_core::primitives::asset::{AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::primitives::ratio::Bps;
    use amm_core::protocols::uniswap::v2::UniswapV2Pool;
    use amm_core::slippage::Slippage;

    use super::*;
    use crate::execution::{
        config::{ChainConfig, Routers},
        options::{Deadline, ExecutionOptions, Recipient},
        prepared::Route,
        types::{Currency, TradeType},
    };

    fn chain_id() -> ChainId {
        ChainId(1)
    }

    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    fn weth() -> AssetId {
        asset(0xC0)
    }

    fn router() -> Address {
        Address::repeat_byte(0xAB)
    }

    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth()).with_routers(Routers {
            v2: Some(router()),
            ..Default::default()
        })
    }

    fn pool(a: AssetId, b: AssetId) -> UniswapV2Pool {
        UniswapV2Pool::new(PoolId::new("1:univ2:0x"), [a, b], [U256::from(1u64); 2], 30)
    }

    fn opts_resolved(to: Address, deadline_ts: u64, slippage_bps: u16) -> ExecutionOptions {
        ExecutionOptions::new(Slippage::from_bps(Bps(slippage_bps)))
            .with_recipient(Recipient::To(to))
            .with_deadline(Deadline::AtTimestamp(deadline_ts))
    }

    // ─── resolve_swap ────────────────────────────────────────────────────────

    #[test]
    fn resolve_swap_rejects_wrong_trade_type() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let route = Route::new_single_hop(a, b, TradeType::ExactOut);

        let err = resolve_swap(
            &c,
            &p,
            Currency::Token(a),
            Currency::Token(b),
            &route,
            &opts,
            TradeType::ExactIn,
        )
        .expect_err("wrong trade type must fail");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    #[test]
    fn resolve_swap_rejects_native_native() {
        let w = weth();
        let b = asset(0x02);
        let p = pool(w, b);
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let route = Route::new_single_hop(w, b, TradeType::ExactIn);

        let err = resolve_swap(
            &c,
            &p,
            Currency::Native,
            Currency::Native,
            &route,
            &opts,
            TradeType::ExactIn,
        )
        .expect_err("native→native must fail");
        assert_eq!(err, BuildError::NativeMismatch);
    }

    #[test]
    fn resolve_swap_rejects_unknown_asset() {
        let a = asset(0x01);
        let b = asset(0x02);
        let unknown = asset(0x99);
        let p = pool(a, b);
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let route = Route::new_single_hop(a, unknown, TradeType::ExactIn);

        let err = resolve_swap(
            &c,
            &p,
            Currency::Token(a),
            Currency::Token(unknown),
            &route,
            &opts,
            TradeType::ExactIn,
        )
        .expect_err("unknown asset must fail");
        assert!(matches!(err, BuildError::AssetNotInPool { .. }));
    }

    #[test]
    fn resolve_swap_resolves_native_in_to_weth_and_sets_flags() {
        let w = weth();
        let b = asset(0x02);
        let p = pool(w, b);
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let route = Route::new_single_hop(w, b, TradeType::ExactIn);

        let r = resolve_swap(
            &c,
            &p,
            Currency::Native,
            Currency::Token(b),
            &route,
            &opts,
            TradeType::ExactIn,
        )
        .expect("native-in resolve must succeed");

        assert_eq!(r.input, w, "Native input must resolve to WETH");
        assert!(r.native_in, "native_in must be true");
        assert!(!r.native_out, "native_out must be false");
    }

    // ─── resolve_sqrt_limit ──────────────────────────────────────────────────

    #[test]
    fn resolve_sqrt_limit_none_is_zero() {
        let a = asset(0x01);
        let b = asset(0x02);
        let assets = [a, b];
        let result = resolve_sqrt_limit(&assets, None, 2).expect("None limit must succeed");
        assert_eq!(result, U160::ZERO, "absent limit must yield U160::ZERO");
    }

    #[test]
    fn resolve_sqrt_limit_multi_hop_with_limit_is_unsupported() {
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let a = asset(0x01);
        let b = asset(0x02);
        let assets = [a, b];

        let ratio = Ratio::new(U256::from(2u64), U256::from(1u64)).unwrap();
        let price_limit = Price::new(a, b, ratio).unwrap();

        // hops = 3 simulates a multi-hop route (not exactly 2).
        let err = resolve_sqrt_limit(&assets, Some(&price_limit), 3)
            .expect_err("multi-hop + price_limit must fail");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ─── erc20_approval ─────────────────────────────────────────────────────

    #[test]
    fn erc20_approval_sets_reset_first_false() {
        let a = asset(0x01);
        let spender = Address::repeat_byte(0xAB);
        let allowance = U256::from(1000u64);

        let req = erc20_approval(spender, a, allowance);

        assert_eq!(req.spender, spender);
        assert_eq!(req.token, a);
        assert_eq!(req.min_allowance, allowance);
        assert!(!req.reset_first, "reset_first must be false");
    }
}
