//! Shared driver for SwapRouter02-family encoders (Uniswap V3, Aerodrome
//! Slipstream). Both wrap single-hop exactInput/OutputSingle calls with the same
//! native-ETH table and multicall assembly; they differ only in the params struct
//! and deadline placement, which each member supplies via [`SwapRouter02`].

use alloy::primitives::aliases::U160;
use alloy::primitives::{Address, Bytes, U256, address};
use alloy::{sol, sol_types::SolCall};
use amm_core::primitives::asset::AssetAmount;
use amm_core::traits::pool::Pool;

use crate::execution::{
    config::ChainConfig,
    error::BuildError,
    multicall::encode_multicall,
    options::ExecutionOptions,
    prepared::PreparedSwap,
    protocols::common,
    types::{Currency, CurrencyAmount},
};

/// Solidity `address(2)` — "keep funds in the router" so a following unwrapWETH9 forwards ETH.
pub(crate) const ADDRESS_THIS: Address = address!("0000000000000000000000000000000000000002");

sol! {
    /// Periphery calls shared verbatim across SwapRouter02-family routers.
    interface IPeripheryPayments {
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
        function refundETH() external payable;
    }
}

/// The resolved single-hop parameters a family member turns into inner calldata.
pub(crate) struct SingleParams {
    pub token_in: Address,
    pub token_out: Address,
    pub recipient: Address,
    /// amountIn (exact-in) or amountOut (exact-out).
    pub amount: U256,
    /// amountOutMinimum (exact-in) or amountInMaximum (exact-out).
    pub limit_amount: U256,
    pub sqrt_limit: U160,
    /// Absolute deadline; embedded in the struct by members that carry it there.
    /// V3 ignores this field (deadline goes in the multicall overload); Slipstream
    /// embeds it directly in the params struct.
    pub deadline: U256,
}

/// The per-protocol pieces of a SwapRouter02-family encoder. The driver owns
/// everything else (guards, native table, multicall, PreparedSwap assembly).
pub(crate) trait SwapRouter02 {
    /// The router this family member targets.
    fn router(&self, ctx: &ChainConfig) -> Result<Address, BuildError>;
    /// Encode the inner `exactInputSingle` call from resolved params.
    fn encode_exact_in(&self, p: &SingleParams) -> Result<Bytes, BuildError>;
    /// Encode the inner `exactOutputSingle` call from resolved params.
    fn encode_exact_out(&self, p: &SingleParams) -> Result<Bytes, BuildError>;
    /// `true` when the deadline must be enforced by the `multicall(deadline)` overload
    /// (V3 — no in-struct deadline); `false` when the struct carries it (Slipstream).
    fn deadline_in_multicall(&self) -> bool;
    /// The recipient that makes the router hold the swapped WETH for a following
    /// `unwrapWETH9` (native-out). SwapRouter02 resolves the `ADDRESS_THIS` sentinel
    /// to itself; classic routers (e.g. Slipstream) require their own address.
    fn custody_recipient(&self, router: Address) -> Address {
        let _ = router;
        ADDRESS_THIS
    }
}

/// Wrap the inner calls in the family's multicall, applying the deadline overload
/// only when the member has no in-struct deadline.
fn wrap(enc: &impl SwapRouter02, deadline: U256, calls: Vec<Bytes>) -> Result<Bytes, BuildError> {
    match enc.deadline_in_multicall() {
        true => {
            let secs = u64::try_from(deadline).map_err(|_| BuildError::Overflow)?;
            Ok(encode_multicall(Some(secs), calls))
        }
        false => Ok(encode_multicall(None, calls)),
    }
}

/// Exact-in build shared by the family. `pool` is the `&dyn Pool` for membership
/// checks; `enc` supplies the protocol-specific encoding.
pub(crate) fn build_exact_in(
    enc: &impl SwapRouter02,
    pool: &dyn Pool,
    ctx: &ChainConfig,
    amount_in: CurrencyAmount,
    to: Currency,
    quoted_out: &AssetAmount,
    opts: &ExecutionOptions,
) -> Result<PreparedSwap, BuildError> {
    let r = common::resolve_swap(ctx, pool, amount_in.currency, to, opts)?;
    // Single-hop build_exact_in always has 2 hops; multi-hop rejection lives in the planner.
    let sqrt_limit = common::resolve_sqrt_limit(pool.assets(), opts.price_limit.as_ref(), 2)?;

    let min = opts.slippage.min_amount_out(quoted_out);
    let router = enc.router(ctx)?;

    match (r.native_in, r.native_out) {
        // Native → Native is always a configuration error.
        (true, true) => Err(BuildError::NativeMismatch),

        // Native-in exact-in (ETH → token): tokenIn = WETH, recipient = user,
        // tx.value = amount_in.raw, no ERC-20 approval.
        (true, false) => {
            let p = SingleParams {
                token_in: common::evm_addr(&r.input),
                token_out: common::evm_addr(&r.output),
                recipient: r.recipient,
                amount: amount_in.raw,
                limit_amount: min.raw,
                sqrt_limit,
                deadline: r.deadline,
            };
            let inner = enc.encode_exact_in(&p)?;
            let data = wrap(enc, r.deadline, vec![inner])?;
            Ok(common::prepared(
                ctx,
                router,
                data,
                amount_in.raw,
                min,
                None,
                None,
            ))
        }

        // Native-out exact-in (token → ETH): swap recipient = custody_recipient so the
        // router holds the WETH, then unwrapWETH9 forwards it as ETH to the user.
        // SwapRouter02 uses the ADDRESS_THIS sentinel; classic routers (e.g. Slipstream)
        // require their own address.
        (false, true) => {
            let p = SingleParams {
                token_in: common::evm_addr(&r.input),
                token_out: common::evm_addr(&r.output),
                recipient: enc.custody_recipient(router),
                amount: amount_in.raw,
                limit_amount: min.raw,
                sqrt_limit,
                deadline: r.deadline,
            };
            let swap = enc.encode_exact_in(&p)?;
            let unwrap = IPeripheryPayments::unwrapWETH9Call {
                amountMinimum: min.raw,
                recipient: r.recipient,
            }
            .abi_encode()
            .into();
            let data = wrap(enc, r.deadline, vec![swap, unwrap])?;
            Ok(common::prepared(
                ctx,
                router,
                data,
                U256::ZERO,
                min,
                None,
                Some(common::erc20_approval(router, r.input, amount_in.raw)),
            ))
        }

        // ERC-20 → ERC-20: direct exactInputSingle, no wrap/unwrap.
        (false, false) => {
            let p = SingleParams {
                token_in: common::evm_addr(&r.input),
                token_out: common::evm_addr(&r.output),
                recipient: r.recipient,
                amount: amount_in.raw,
                limit_amount: min.raw,
                sqrt_limit,
                deadline: r.deadline,
            };
            let inner = enc.encode_exact_in(&p)?;
            let data = wrap(enc, r.deadline, vec![inner])?;
            Ok(common::prepared(
                ctx,
                router,
                data,
                U256::ZERO,
                min,
                None,
                Some(common::erc20_approval(router, r.input, amount_in.raw)),
            ))
        }
    }
}

/// Exact-out build shared by the family.
pub(crate) fn build_exact_out(
    enc: &impl SwapRouter02,
    pool: &dyn Pool,
    ctx: &ChainConfig,
    amount_out: CurrencyAmount,
    from: Currency,
    quoted_in: &AssetAmount,
    opts: &ExecutionOptions,
) -> Result<PreparedSwap, BuildError> {
    let r = common::resolve_swap(ctx, pool, from, amount_out.currency, opts)?;
    // Single-hop build_exact_out always has 2 hops; multi-hop rejection lives in the planner.
    let sqrt_limit = common::resolve_sqrt_limit(pool.assets(), opts.price_limit.as_ref(), 2)?;

    let max = opts.slippage.max_amount_in(quoted_in);
    let router = enc.router(ctx)?;

    match (r.native_in, r.native_out) {
        // Native → Native: already rejected above; unreachable in practice.
        (true, true) => Err(BuildError::NativeMismatch),

        // Native-in exact-out (ETH → token): tx.value = max input ceiling; router
        // refunds any unspent ETH via refundETH().
        (true, false) => {
            let p = SingleParams {
                token_in: common::evm_addr(&r.input),
                token_out: common::evm_addr(&r.output),
                recipient: r.recipient,
                amount: amount_out.raw,
                limit_amount: max.raw,
                sqrt_limit,
                deadline: r.deadline,
            };
            let swap = enc.encode_exact_out(&p)?;
            let refund = IPeripheryPayments::refundETHCall {}.abi_encode().into();
            let data = wrap(enc, r.deadline, vec![swap, refund])?;
            Ok(common::prepared(
                ctx,
                router,
                data,
                max.raw,
                AssetAmount::new(r.output, amount_out.raw),
                Some(AssetAmount::new(r.input, max.raw)),
                None,
            ))
        }

        // Native-out exact-out (token → ETH): swap recipient = custody_recipient so the
        // router holds the WETH, then unwrapWETH9 forwards the exact target amount as ETH.
        // SwapRouter02 uses the ADDRESS_THIS sentinel; classic routers (e.g. Slipstream)
        // require their own address.
        (false, true) => {
            let p = SingleParams {
                token_in: common::evm_addr(&r.input),
                token_out: common::evm_addr(&r.output),
                recipient: enc.custody_recipient(router),
                amount: amount_out.raw,
                limit_amount: max.raw,
                sqrt_limit,
                deadline: r.deadline,
            };
            let swap = enc.encode_exact_out(&p)?;
            // Unwrap exactly the target amount (not the min — this is exact-out).
            let unwrap = IPeripheryPayments::unwrapWETH9Call {
                amountMinimum: amount_out.raw,
                recipient: r.recipient,
            }
            .abi_encode()
            .into();
            let data = wrap(enc, r.deadline, vec![swap, unwrap])?;
            Ok(common::prepared(
                ctx,
                router,
                data,
                U256::ZERO,
                AssetAmount::new(r.output, amount_out.raw),
                Some(AssetAmount::new(r.input, max.raw)),
                Some(common::erc20_approval(router, r.input, max.raw)),
            ))
        }

        // ERC-20 → ERC-20: direct exactOutputSingle, no wrap/unwrap.
        (false, false) => {
            let p = SingleParams {
                token_in: common::evm_addr(&r.input),
                token_out: common::evm_addr(&r.output),
                recipient: r.recipient,
                amount: amount_out.raw,
                limit_amount: max.raw,
                sqrt_limit,
                deadline: r.deadline,
            };
            let inner = enc.encode_exact_out(&p)?;
            let data = wrap(enc, r.deadline, vec![inner])?;
            Ok(common::prepared(
                ctx,
                router,
                data,
                U256::ZERO,
                AssetAmount::new(r.output, amount_out.raw),
                Some(AssetAmount::new(r.input, max.raw)),
                Some(common::erc20_approval(router, r.input, max.raw)),
            ))
        }
    }
}
