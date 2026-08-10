//! Uniswap V3 swap encoder: exact-in and exact-out (ERC-20 and native ETH) → [`PreparedSwap`].
//!
//! [`build_swap`] encodes a `SwapRouter02` `exactInputSingle` call for the
//! exact-in path, and [`build_swap_exact_out`] mirrors it for `exactOutputSingle`.
//! Every V3 call is wrapped via `encode_multicall(Some(deadline_secs), vec![…])` so the
//! deadline is enforced by the `multicall(uint256 deadline, bytes[])` overload — V3 structs
//! have **no** `deadline` field.
//!
//! Native-ETH paths are handled via the `unwrapWETH9` / `refundETH` helpers baked into the
//! same multicall: native-in sets `tx.value`; native-out routes the swap to `ADDRESS_THIS`
//! (`address(2)`) then appends `unwrapWETH9(min, user)` so the router unwraps WETH back to
//! ETH before forwarding it.

use alloy::primitives::aliases::U24;
use alloy::primitives::{Address, U256, address};
use alloy::{sol, sol_types::SolCall};
use amm_core::primitives::asset::AssetAmount;
use amm_core::protocols::uniswap::v3::UniswapV3Pool;
use amm_core::traits::pool::Pool;

use crate::execution::{
    config::ChainConfig,
    error::BuildError,
    executable::{Executable, Sealed},
    multicall::encode_multicall,
    options::ExecutionOptions,
    prepared::{PreparedSwap, Route},
    protocols::common,
    types::{Currency, CurrencyAmount, TradeType},
};

/// Solidity `address(2)` — SwapRouter02 uses this sentinel to mean "keep funds
/// in the router" so a subsequent `unwrapWETH9` call can forward them as ETH.
const ADDRESS_THIS: Address = address!("0000000000000000000000000000000000000002");

sol! {
    /// Minimal ABI surface for the Uniswap V3 SwapRouter02.
    interface ISwapRouter02 {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactOutputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountOut;
            uint256 amountInMaximum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
        function exactOutputSingle(ExactOutputSingleParams params) external payable returns (uint256 amountIn);
        /// Unwrap at least `amountMinimum` WETH held by the router and send ETH to `recipient`.
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
        /// Refund any unspent ETH (msg.value surplus) back to msg.sender.
        function refundETH() external payable;
    }
}

impl Sealed for UniswapV3Pool {}

impl Executable for UniswapV3Pool {
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        route: &Route,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        let r = common::resolve_swap(
            ctx,
            self,
            amount_in.currency,
            to,
            route,
            opts,
            TradeType::ExactIn,
        )?;
        let sqrt_limit =
            common::resolve_sqrt_limit(self.assets(), opts.price_limit.as_ref(), route.hops.len())?;

        let min = opts.slippage.min_amount_out(quoted_out);
        let router = ctx.router_v3()?;
        let deadline_secs = u64::try_from(r.deadline).map_err(|_| BuildError::Overflow)?;
        let fee = U24::try_from(self.fee_pips()).map_err(|_| BuildError::Overflow)?;

        match (r.native_in, r.native_out) {
            // Native → Native is always a configuration error.
            (true, true) => Err(BuildError::NativeMismatch),

            // Native-in exact-in (ETH → token): tokenIn = WETH, recipient = user,
            // tx.value = amount_in.raw, no ERC-20 approval.
            (true, false) => {
                let params = ISwapRouter02::ExactInputSingleParams {
                    tokenIn: common::evm_addr(&r.input),
                    tokenOut: common::evm_addr(&r.output),
                    fee,
                    recipient: r.recipient,
                    amountIn: amount_in.raw,
                    amountOutMinimum: min.raw,
                    sqrtPriceLimitX96: sqrt_limit,
                };
                let inner = ISwapRouter02::exactInputSingleCall { params }
                    .abi_encode()
                    .into();
                let data = encode_multicall(Some(deadline_secs), vec![inner]);
                Ok(common::prepared(
                    ctx,
                    router,
                    data,
                    // ETH is attached as msg.value; no ERC-20 transfer.
                    amount_in.raw,
                    min,
                    None,
                    // Native ETH sent as msg.value — no ERC-20 approval required.
                    None,
                ))
            }

            // Native-out exact-in (token → ETH): swap recipient = ADDRESS_THIS so the router
            // holds the WETH, then unwrapWETH9 forwards it as ETH to the user.
            (false, true) => {
                let params = ISwapRouter02::ExactInputSingleParams {
                    tokenIn: common::evm_addr(&r.input),
                    tokenOut: common::evm_addr(&r.output),
                    fee,
                    // Route output to the router itself so unwrapWETH9 can act on it.
                    recipient: ADDRESS_THIS,
                    amountIn: amount_in.raw,
                    amountOutMinimum: min.raw,
                    sqrtPriceLimitX96: sqrt_limit,
                };
                let swap_call = ISwapRouter02::exactInputSingleCall { params }
                    .abi_encode()
                    .into();
                let unwrap_call = ISwapRouter02::unwrapWETH9Call {
                    amountMinimum: min.raw,
                    recipient: r.recipient,
                }
                .abi_encode()
                .into();
                let data = encode_multicall(Some(deadline_secs), vec![swap_call, unwrap_call]);
                // min_received.asset is WETH; the user actually receives unwrapped ETH —
                // this matches the PreparedSwap doc (asset represents the on-chain token).
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
                let params = ISwapRouter02::ExactInputSingleParams {
                    tokenIn: common::evm_addr(&r.input),
                    tokenOut: common::evm_addr(&r.output),
                    fee,
                    recipient: r.recipient,
                    amountIn: amount_in.raw,
                    amountOutMinimum: min.raw,
                    sqrtPriceLimitX96: sqrt_limit,
                };
                let inner = ISwapRouter02::exactInputSingleCall { params }
                    .abi_encode()
                    .into();
                let data = encode_multicall(Some(deadline_secs), vec![inner]);
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

    fn build_swap_exact_out(
        &self,
        ctx: &ChainConfig,
        amount_out: CurrencyAmount,
        from: Currency,
        route: &Route,
        quoted_in: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        let r = common::resolve_swap(
            ctx,
            self,
            from,
            amount_out.currency,
            route,
            opts,
            TradeType::ExactOut,
        )?;
        let sqrt_limit =
            common::resolve_sqrt_limit(self.assets(), opts.price_limit.as_ref(), route.hops.len())?;

        let max = opts.slippage.max_amount_in(quoted_in);
        let router = ctx.router_v3()?;
        let deadline_secs = u64::try_from(r.deadline).map_err(|_| BuildError::Overflow)?;
        let fee = U24::try_from(self.fee_pips()).map_err(|_| BuildError::Overflow)?;

        match (r.native_in, r.native_out) {
            // Native → Native: already rejected above; unreachable in practice.
            (true, true) => Err(BuildError::NativeMismatch),

            // Native-in exact-out (ETH → token): tx.value = max input ceiling; router
            // refunds any unspent ETH via refundETH().
            (true, false) => {
                let params = ISwapRouter02::ExactOutputSingleParams {
                    tokenIn: common::evm_addr(&r.input),
                    tokenOut: common::evm_addr(&r.output),
                    fee,
                    recipient: r.recipient,
                    amountOut: amount_out.raw,
                    amountInMaximum: max.raw,
                    sqrtPriceLimitX96: sqrt_limit,
                };
                let swap_call = ISwapRouter02::exactOutputSingleCall { params }
                    .abi_encode()
                    .into();
                let refund_call = ISwapRouter02::refundETHCall {}.abi_encode().into();
                let data = encode_multicall(Some(deadline_secs), vec![swap_call, refund_call]);
                Ok(common::prepared(
                    ctx,
                    router,
                    data,
                    // Attach the ETH ceiling as msg.value; refundETH returns any surplus.
                    max.raw,
                    AssetAmount::new(r.output, amount_out.raw),
                    Some(AssetAmount::new(r.input, max.raw)),
                    // Native ETH sent as msg.value — no ERC-20 approval required.
                    None,
                ))
            }

            // Native-out exact-out (token → ETH): swap recipient = ADDRESS_THIS so the
            // router holds the WETH, then unwrapWETH9 forwards the exact target amount as ETH.
            (false, true) => {
                let params = ISwapRouter02::ExactOutputSingleParams {
                    tokenIn: common::evm_addr(&r.input),
                    tokenOut: common::evm_addr(&r.output),
                    fee,
                    // Route output to the router itself so unwrapWETH9 can act on it.
                    recipient: ADDRESS_THIS,
                    amountOut: amount_out.raw,
                    amountInMaximum: max.raw,
                    sqrtPriceLimitX96: sqrt_limit,
                };
                let swap_call = ISwapRouter02::exactOutputSingleCall { params }
                    .abi_encode()
                    .into();
                // Unwrap exactly the target amount (not the min — this is exact-out).
                let unwrap_call = ISwapRouter02::unwrapWETH9Call {
                    amountMinimum: amount_out.raw,
                    recipient: r.recipient,
                }
                .abi_encode()
                .into();
                let data = encode_multicall(Some(deadline_secs), vec![swap_call, unwrap_call]);
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
                let params = ISwapRouter02::ExactOutputSingleParams {
                    tokenIn: common::evm_addr(&r.input),
                    tokenOut: common::evm_addr(&r.output),
                    fee,
                    recipient: r.recipient,
                    amountOut: amount_out.raw,
                    amountInMaximum: max.raw,
                    sqrtPriceLimitX96: sqrt_limit,
                };
                let inner = ISwapRouter02::exactOutputSingleCall { params }
                    .abi_encode()
                    .into();
                let data = encode_multicall(Some(deadline_secs), vec![inner]);
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
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, U256};
    use alloy::sol_types::SolCall;
    use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::primitives::ratio::Bps;
    use amm_core::protocols::uniswap::v3::{TickData, UniswapV3Pool};
    use amm_core::slippage::Slippage;

    use super::{ADDRESS_THIS, ISwapRouter02};
    use crate::execution::multicall::IMulticall;
    use crate::execution::{
        config::{ChainConfig, Routers},
        error::BuildError,
        executable::Executable,
        options::{Deadline, ExecutionOptions, Recipient},
        prepared::Route,
        types::{Currency, CurrencyAmount, TradeType},
    };

    fn router() -> Address {
        Address::repeat_byte(0xAB)
    }

    fn chain_id() -> ChainId {
        ChainId(1)
    }

    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    fn weth() -> AssetId {
        asset(0xC0)
    }

    /// Minimal chain config: chain 1, WETH, V3 router set.
    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth()).with_routers(Routers {
            v3: Some(router()),
            ..Default::default()
        })
    }

    /// Build a minimal V3 pool over `a` and `b` with 3000 pips fee.
    fn pool(a: AssetId, b: AssetId) -> UniswapV3Pool {
        // sqrtPriceX96 = 1.0, no ticks needed for encoder tests.
        UniswapV3Pool::new(
            PoolId::new("1:univ3:0x"),
            [a, b],
            U256::from(1u64) << 96,
            0,
            0,
            3000,
            TickData::from_ticks(60, vec![]),
        )
    }

    /// Resolved execution options.
    fn opts_resolved(to: Address, deadline_ts: u64, slippage_bps: u16) -> ExecutionOptions {
        ExecutionOptions::new(Slippage::from_bps(Bps(slippage_bps)))
            .with_recipient(Recipient::To(to))
            .with_deadline(Deadline::AtTimestamp(deadline_ts))
    }

    // ─── selector sanity ────────────────────────────────────────────────────

    #[test]
    fn selectors_are_correct() {
        assert_eq!(
            ISwapRouter02::exactInputSingleCall::SELECTOR,
            [0x04, 0xe4, 0x5a, 0xaf],
            "exactInputSingle selector must be 0x04e45aaf"
        );
        assert_eq!(
            ISwapRouter02::exactOutputSingleCall::SELECTOR,
            [0x50, 0x23, 0xb4, 0xdf],
            "exactOutputSingle selector must be 0x5023b4df"
        );
        assert_eq!(
            ISwapRouter02::unwrapWETH9Call::SELECTOR,
            [0x49, 0x40, 0x4b, 0x7c],
            "unwrapWETH9 selector must be 0x49404b7c"
        );
        assert_eq!(
            ISwapRouter02::refundETHCall::SELECTOR,
            [0x12, 0x21, 0x0e, 0x8a],
            "refundETH selector must be 0x12210e8a"
        );
    }

    // ─── exact-in ERC-20 success path ───────────────────────────────────────

    #[test]
    fn exact_in_erc20_encodes_exact_input_single() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x55);
        let deadline_ts = 1_700_000_000u64;
        // 100 bps slippage: quoted_out 1000 → min_received = floor(1000 * 9900/10000) = 990
        let opts = opts_resolved(recipient, deadline_ts, 100);

        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(500u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(1000u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("build_swap must succeed for ERC-20 exact-in");

        // tx.to == router, tx.value == 0, tx.chain matches
        assert_eq!(prepared.tx.to, router(), "tx.to must be the V3 router");
        assert_eq!(prepared.tx.value, U256::ZERO, "value must be 0 for ERC-20");
        assert_eq!(prepared.tx.chain, c.chain, "tx.chain must match ctx.chain");

        // Outer calldata is multicall(uint256 deadline, bytes[])
        assert_eq!(
            &prepared.tx.data[..4],
            &[0x5a, 0xe4, 0x01, 0xdc],
            "outer selector must be multicall(uint256,bytes[])"
        );
        let outer = IMulticall::multicall_1Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_1Call");
        assert_eq!(
            outer.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip through multicall"
        );
        assert_eq!(outer.data.len(), 1, "must have exactly one inner call");

        // Inner call is exactInputSingle
        let inner = ISwapRouter02::exactInputSingleCall::abi_decode(&outer.data[0])
            .expect("inner must decode as exactInputSingle");
        assert_eq!(
            inner.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&a),
            "tokenIn must be input asset address"
        );
        assert_eq!(
            inner.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&b),
            "tokenOut must be output asset address"
        );
        assert_eq!(
            inner.params.fee.to::<u32>(),
            3000u32,
            "fee must match pool.fee_pips()"
        );
        assert_eq!(
            inner.params.recipient, recipient,
            "recipient must round-trip"
        );
        assert_eq!(
            inner.params.amountIn,
            U256::from(500u64),
            "amountIn must equal input raw"
        );
        // amountOutMinimum == floor(1000 * 9900/10000) = 990
        assert_eq!(
            inner.params.amountOutMinimum,
            U256::from(990u64),
            "amountOutMinimum must be 990 at 100bps"
        );
        assert_eq!(
            inner.params.sqrtPriceLimitX96,
            alloy::primitives::aliases::U160::ZERO,
            "sqrtPriceLimitX96 must be zero"
        );

        // min_received == 990 of asset b
        assert_eq!(prepared.min_received.raw, U256::from(990u64));
        assert_eq!(prepared.min_received.asset, b);

        // max_spent is None for exact-in
        assert!(
            prepared.max_spent.is_none(),
            "max_spent must be None for exact-in"
        );

        // approval == Some { spender: router, token: input, min_allowance: amountIn, reset_first: false }
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender,
            router(),
            "approval.spender must be the V3 router"
        );
        assert_eq!(approval.token, a, "approval.token must be the input asset");
        assert_eq!(
            approval.min_allowance,
            U256::from(500u64),
            "approval.min_allowance must be amount_in.raw"
        );
        assert!(!approval.reset_first, "reset_first must be false");
    }

    // ─── exact-out ERC-20 success path ──────────────────────────────────────

    #[test]
    fn exact_out_erc20_encodes_exact_output_single() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x55);
        let deadline_ts = 1_700_000_000u64;
        // 100 bps slippage: quoted_in 500 → max_amount_in = ceil(500 * 10100/10000) = 505
        let opts = opts_resolved(recipient, deadline_ts, 100);

        let out_raw = U256::from(400u64);
        let amount_out = CurrencyAmount {
            currency: Currency::Token(b),
            raw: out_raw,
        };
        let quoted_in = AssetAmount::new(a, U256::from(500u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactOut);

        let prepared = p
            .build_swap_exact_out(
                &c,
                amount_out,
                Currency::Token(a),
                &route,
                &quoted_in,
                &opts,
            )
            .expect("build_swap_exact_out must succeed for ERC-20 exact-out");

        // tx fields
        assert_eq!(prepared.tx.to, router(), "tx.to must be the V3 router");
        assert_eq!(prepared.tx.value, U256::ZERO, "value must be 0");
        assert_eq!(prepared.tx.chain, c.chain, "tx.chain must match ctx.chain");

        // Outer is multicall(uint256 deadline, bytes[])
        assert_eq!(
            &prepared.tx.data[..4],
            &[0x5a, 0xe4, 0x01, 0xdc],
            "outer selector must be multicall(uint256,bytes[])"
        );
        let outer = IMulticall::multicall_1Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_1Call");
        assert_eq!(
            outer.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip through multicall"
        );
        assert_eq!(outer.data.len(), 1, "must have exactly one inner call");

        // Inner is exactOutputSingle
        let inner = ISwapRouter02::exactOutputSingleCall::abi_decode(&outer.data[0])
            .expect("inner must decode as exactOutputSingle");
        assert_eq!(
            inner.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&a),
            "tokenIn must be input asset address"
        );
        assert_eq!(
            inner.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&b),
            "tokenOut must be output asset address"
        );
        assert_eq!(
            inner.params.fee.to::<u32>(),
            3000u32,
            "fee must match pool.fee_pips()"
        );
        assert_eq!(
            inner.params.recipient, recipient,
            "recipient must round-trip"
        );
        assert_eq!(
            inner.params.amountOut, out_raw,
            "amountOut must equal amount_out.raw"
        );
        // amountInMaximum == ceil(500 * 10100/10000) = 505
        assert_eq!(
            inner.params.amountInMaximum,
            U256::from(505u64),
            "amountInMaximum must be 505 at 100bps"
        );
        assert_eq!(
            inner.params.sqrtPriceLimitX96,
            alloy::primitives::aliases::U160::ZERO,
            "sqrtPriceLimitX96 must be zero"
        );

        // min_received == exact output target in asset b
        assert_eq!(prepared.min_received.raw, out_raw);
        assert_eq!(prepared.min_received.asset, b);

        // max_spent == Some(input asset, max_amount_in)
        let max_spent = prepared
            .max_spent
            .expect("max_spent must be Some for exact-out");
        assert_eq!(
            max_spent.raw,
            U256::from(505u64),
            "max_spent.raw must be slippage ceiling"
        );
        assert_eq!(
            max_spent.asset, a,
            "max_spent.asset must be the input asset"
        );

        // approval references input with max_amount_in as min_allowance
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender,
            router(),
            "approval.spender must be the V3 router"
        );
        assert_eq!(approval.token, a, "approval.token must be the input asset");
        assert_eq!(
            approval.min_allowance,
            U256::from(505u64),
            "approval.min_allowance must be max_amount_in"
        );
        assert!(!approval.reset_first, "reset_first must be false");
    }

    // ─── native-in exact-in (ETH → token) ───────────────────────────────────

    #[test]
    fn native_in_exact_in_encodes_exact_input_single_with_value() {
        let b = asset(0x02);
        let w = weth();
        let p = pool(w, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x77);
        let deadline_ts = 1_700_000_000u64;
        // 200 bps slippage: quoted_out 1000 → min_received = floor(1000 * 9800/10000) = 980
        let opts = opts_resolved(recipient, deadline_ts, 200);

        let amount_in_raw = U256::from(500u64);
        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: amount_in_raw,
        };
        let quoted_out = AssetAmount::new(b, U256::from(1000u64));
        let route = Route::new_single_hop(w, b, TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("native-in exact-in must succeed");

        // tx.value must carry the ETH amount.
        assert_eq!(
            prepared.tx.value, amount_in_raw,
            "tx.value must equal amount_in.raw for native-in"
        );

        // No ERC-20 approval — ETH is attached as msg.value.
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in"
        );

        // Outer multicall with deadline.
        let outer = IMulticall::multicall_1Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_1Call");
        assert_eq!(
            outer.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );
        assert_eq!(outer.data.len(), 1, "native-in exact-in: one inner call");

        // Inner is exactInputSingle with tokenIn = WETH and recipient = user.
        let inner = ISwapRouter02::exactInputSingleCall::abi_decode(&outer.data[0])
            .expect("inner must decode as exactInputSingle");
        assert_eq!(
            inner.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&w),
            "tokenIn must be WETH (resolved from Native)"
        );
        assert_eq!(
            inner.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&b),
            "tokenOut must be output token"
        );
        assert_eq!(
            inner.params.recipient, recipient,
            "recipient must be user address"
        );
        assert_eq!(
            inner.params.amountIn, amount_in_raw,
            "amountIn must round-trip"
        );
        // amountOutMinimum == floor(1000 * 9800/10000) = 980
        assert_eq!(
            inner.params.amountOutMinimum,
            U256::from(980u64),
            "amountOutMinimum must be 980 at 200bps"
        );

        // min_received == 980 of token b
        assert_eq!(prepared.min_received.raw, U256::from(980u64));
        assert_eq!(prepared.min_received.asset, b);
        assert!(prepared.max_spent.is_none());
    }

    // ─── native-out exact-in (token → ETH) ──────────────────────────────────

    #[test]
    fn native_out_exact_in_encodes_exact_input_single_plus_unwrap() {
        let a = asset(0x01);
        let w = weth();
        let p = pool(a, w);
        let c = ctx();

        let recipient = Address::repeat_byte(0x88);
        let deadline_ts = 1_800_000_000u64;
        // 50 bps slippage: quoted_out 1000 → min_received = floor(1000 * 9950/10000) = 995
        let opts = opts_resolved(recipient, deadline_ts, 50);

        let amount_in_raw = U256::from(300u64);
        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: amount_in_raw,
        };
        let quoted_out = AssetAmount::new(w, U256::from(1000u64));
        let route = Route::new_single_hop(a, w, TradeType::ExactIn);

        let prepared = p
            .build_swap(&c, amount_in, Currency::Native, &route, &quoted_out, &opts)
            .expect("native-out exact-in must succeed");

        // tx.value must be zero (ERC-20 input).
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for native-out"
        );

        // Approval must be present on the input token.
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(approval.token, a, "approval.token must be the input asset");
        assert_eq!(
            approval.spender,
            router(),
            "approval.spender must be the router"
        );
        assert_eq!(
            approval.min_allowance, amount_in_raw,
            "approval.min_allowance must equal amount_in.raw"
        );

        // Outer multicall with deadline.
        let outer = IMulticall::multicall_1Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_1Call");
        assert_eq!(
            outer.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );
        assert_eq!(outer.data.len(), 2, "native-out exact-in: two inner calls");

        // First inner: exactInputSingle with recipient = ADDRESS_THIS.
        let swap = ISwapRouter02::exactInputSingleCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactInputSingle");
        assert_eq!(
            swap.params.recipient, ADDRESS_THIS,
            "swap recipient must be ADDRESS_THIS (address(2))"
        );
        assert_eq!(
            swap.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&a),
            "tokenIn must be input"
        );
        assert_eq!(
            swap.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&w),
            "tokenOut must be WETH"
        );
        assert_eq!(
            swap.params.amountIn, amount_in_raw,
            "amountIn must round-trip"
        );
        // amountOutMinimum == floor(1000 * 9950/10000) = 995
        assert_eq!(
            swap.params.amountOutMinimum,
            U256::from(995u64),
            "amountOutMinimum must be 995 at 50bps"
        );

        // Second inner: unwrapWETH9(995, user).
        let unwrap = ISwapRouter02::unwrapWETH9Call::abi_decode(&outer.data[1])
            .expect("second inner must decode as unwrapWETH9");
        assert_eq!(
            unwrap.amountMinimum,
            U256::from(995u64),
            "unwrapWETH9.amountMinimum must equal min_received.raw"
        );
        assert_eq!(
            unwrap.recipient, recipient,
            "unwrapWETH9.recipient must be the user address"
        );

        // min_received.asset is WETH (on-chain token the router holds before unwrap).
        assert_eq!(prepared.min_received.raw, U256::from(995u64));
        assert_eq!(prepared.min_received.asset, w);
        assert!(prepared.max_spent.is_none());
    }

    // ─── native-in exact-out (ETH → token) ──────────────────────────────────

    #[test]
    fn native_in_exact_out_encodes_exact_output_single_plus_refund() {
        let b = asset(0x02);
        let w = weth();
        let p = pool(w, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x77);
        let deadline_ts = 1_700_000_000u64;
        // 100 bps slippage: quoted_in 500 → max_amount_in = ceil(500 * 10100/10000) = 505
        let opts = opts_resolved(recipient, deadline_ts, 100);

        let out_raw = U256::from(400u64);
        let amount_out = CurrencyAmount {
            currency: Currency::Token(b),
            raw: out_raw,
        };
        let quoted_in = AssetAmount::new(w, U256::from(500u64));
        let route = Route::new_single_hop(w, b, TradeType::ExactOut);

        let prepared = p
            .build_swap_exact_out(&c, amount_out, Currency::Native, &route, &quoted_in, &opts)
            .expect("native-in exact-out must succeed");

        // tx.value must be max_amount_in (the ETH ceiling the router may consume).
        assert_eq!(
            prepared.tx.value,
            U256::from(505u64),
            "tx.value must equal max_amount_in for native-in exact-out"
        );

        // No ERC-20 approval — ETH sent as msg.value.
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in"
        );

        // Outer multicall with deadline.
        let outer = IMulticall::multicall_1Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_1Call");
        assert_eq!(
            outer.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );
        assert_eq!(outer.data.len(), 2, "native-in exact-out: two inner calls");

        // First inner: exactOutputSingle with recipient = user.
        let swap = ISwapRouter02::exactOutputSingleCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactOutputSingle");
        assert_eq!(
            swap.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&w),
            "tokenIn must be WETH"
        );
        assert_eq!(
            swap.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&b),
            "tokenOut must be output token"
        );
        assert_eq!(
            swap.params.recipient, recipient,
            "swap recipient must be user address for native-in"
        );
        assert_eq!(swap.params.amountOut, out_raw, "amountOut must round-trip");
        assert_eq!(
            swap.params.amountInMaximum,
            U256::from(505u64),
            "amountInMaximum must be slippage ceiling"
        );

        // Second inner: refundETH() — no args.
        assert_eq!(
            &outer.data[1][..4],
            &ISwapRouter02::refundETHCall::SELECTOR,
            "second inner selector must be refundETH()"
        );

        // max_spent == Some(weth, max_amount_in)
        let max_spent = prepared
            .max_spent
            .expect("max_spent must be Some for exact-out");
        assert_eq!(
            max_spent.raw,
            U256::from(505u64),
            "max_spent.raw must be the slippage ceiling"
        );
        assert_eq!(max_spent.asset, w, "max_spent.asset must be WETH");
    }

    // ─── native-out exact-out (token → ETH) ─────────────────────────────────

    #[test]
    fn native_out_exact_out_encodes_exact_output_single_plus_unwrap() {
        let a = asset(0x01);
        let w = weth();
        let p = pool(a, w);
        let c = ctx();

        let recipient = Address::repeat_byte(0x99);
        let deadline_ts = 1_900_000_000u64;
        // 100 bps slippage: quoted_in 500 → max_amount_in = ceil(500 * 10100/10000) = 505
        let opts = opts_resolved(recipient, deadline_ts, 100);

        let out_raw = U256::from(400u64);
        let amount_out = CurrencyAmount {
            currency: Currency::Native,
            raw: out_raw,
        };
        let quoted_in = AssetAmount::new(a, U256::from(500u64));
        let route = Route::new_single_hop(a, w, TradeType::ExactOut);

        let prepared = p
            .build_swap_exact_out(
                &c,
                amount_out,
                Currency::Token(a),
                &route,
                &quoted_in,
                &opts,
            )
            .expect("native-out exact-out must succeed");

        // tx.value must be zero (ERC-20 input).
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for native-out"
        );

        // Approval must be present on the input token.
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(approval.token, a, "approval.token must be the input asset");
        assert_eq!(
            approval.spender,
            router(),
            "approval.spender must be the router"
        );
        assert_eq!(
            approval.min_allowance,
            U256::from(505u64),
            "approval.min_allowance must be max_amount_in"
        );

        // Outer multicall with deadline.
        let outer = IMulticall::multicall_1Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_1Call");
        assert_eq!(
            outer.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );
        assert_eq!(outer.data.len(), 2, "native-out exact-out: two inner calls");

        // First inner: exactOutputSingle with recipient = ADDRESS_THIS.
        let swap = ISwapRouter02::exactOutputSingleCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactOutputSingle");
        assert_eq!(
            swap.params.recipient, ADDRESS_THIS,
            "swap recipient must be ADDRESS_THIS (address(2))"
        );
        assert_eq!(
            swap.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&a),
            "tokenIn must be input"
        );
        assert_eq!(
            swap.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&w),
            "tokenOut must be WETH"
        );
        assert_eq!(swap.params.amountOut, out_raw, "amountOut must round-trip");
        assert_eq!(
            swap.params.amountInMaximum,
            U256::from(505u64),
            "amountInMaximum must be slippage ceiling"
        );

        // Second inner: unwrapWETH9(amount_out.raw, user) — exact target amount.
        let unwrap = ISwapRouter02::unwrapWETH9Call::abi_decode(&outer.data[1])
            .expect("second inner must decode as unwrapWETH9");
        assert_eq!(
            unwrap.amountMinimum, out_raw,
            "unwrapWETH9.amountMinimum must equal amount_out.raw (exact target)"
        );
        assert_eq!(
            unwrap.recipient, recipient,
            "unwrapWETH9.recipient must be the user address"
        );

        // max_spent == Some(input asset, max_amount_in)
        let max_spent = prepared
            .max_spent
            .expect("max_spent must be Some for exact-out");
        assert_eq!(max_spent.raw, U256::from(505u64));
        assert_eq!(max_spent.asset, a);
        // min_received carries the exact output in WETH asset.
        assert_eq!(prepared.min_received.raw, out_raw);
        assert_eq!(prepared.min_received.asset, w);
    }

    // ─── both native → NativeMismatch ────────────────────────────────────────

    #[test]
    fn native_in_native_out_returns_native_mismatch() {
        let w = weth();
        let b = asset(0x02);
        let p = pool(w, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(w, U256::from(100u64));
        let route = Route::new_single_hop(w, b, TradeType::ExactIn);

        let err = p
            .build_swap(&c, amount_in, Currency::Native, &route, &quoted_out, &opts)
            .expect_err("native→native must return NativeMismatch");
        assert_eq!(err, BuildError::NativeMismatch);
    }

    #[test]
    fn native_in_native_out_exact_out_returns_native_mismatch() {
        let w = weth();
        let b = asset(0x02);
        let p = pool(w, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_out = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_in = AssetAmount::new(w, U256::from(100u64));
        let route = Route::new_single_hop(w, b, TradeType::ExactOut);

        let err = p
            .build_swap_exact_out(&c, amount_out, Currency::Native, &route, &quoted_in, &opts)
            .expect_err("native→native exact-out must return NativeMismatch");
        assert_eq!(err, BuildError::NativeMismatch);
    }

    // ─── single-hop price_limit encodes a non-zero sqrtPriceLimitX96 ────────

    #[test]
    fn single_hop_price_limit_encodes_nonzero_sqrt_limit() {
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let ratio = Ratio::new(U256::from(2u64), U256::from(1u64)).unwrap();
        let price_limit = Price::new(a, b, ratio).unwrap();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50)
            .with_price_limit(Some(price_limit.clone()));

        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("single-hop price_limit must succeed for exact-in");

        let outer =
            crate::execution::multicall::IMulticall::multicall_1Call::abi_decode(&prepared.tx.data)
                .expect("outer must decode as multicall_1Call");
        let inner = ISwapRouter02::exactInputSingleCall::abi_decode(&outer.data[0])
            .expect("inner must decode as exactInputSingle");

        let expected_raw =
            amm_core::protocols::sqrt_price_limit_x96(&[a, b], &price_limit).unwrap();
        let expected =
            alloy::primitives::aliases::U160::checked_from_limbs_slice(expected_raw.as_limbs())
                .unwrap();

        assert_ne!(
            inner.params.sqrtPriceLimitX96,
            alloy::primitives::aliases::U160::ZERO,
            "sqrtPriceLimitX96 must be non-zero when price_limit is set"
        );
        assert_eq!(
            inner.params.sqrtPriceLimitX96, expected,
            "sqrtPriceLimitX96 must match amm_core::protocols::sqrt_price_limit_x96"
        );
    }

    // ─── single-hop price_limit (exact-out) encodes a non-zero sqrtPriceLimitX96

    #[test]
    fn single_hop_price_limit_encodes_nonzero_sqrt_limit_exact_out() {
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let ratio = Ratio::new(U256::from(2u64), U256::from(1u64)).unwrap();
        let price_limit = Price::new(a, b, ratio).unwrap();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50)
            .with_price_limit(Some(price_limit.clone()));

        let amount_out = CurrencyAmount {
            currency: Currency::Token(b),
            raw: U256::from(100u64),
        };
        let quoted_in = AssetAmount::new(a, U256::from(100u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactOut);

        let prepared = p
            .build_swap_exact_out(
                &c,
                amount_out,
                Currency::Token(a),
                &route,
                &quoted_in,
                &opts,
            )
            .expect("single-hop price_limit must succeed for exact-out");

        let outer =
            crate::execution::multicall::IMulticall::multicall_1Call::abi_decode(&prepared.tx.data)
                .expect("outer must decode as multicall_1Call");
        let inner = ISwapRouter02::exactOutputSingleCall::abi_decode(&outer.data[0])
            .expect("inner must decode as exactOutputSingle");

        let expected_raw =
            amm_core::protocols::sqrt_price_limit_x96(&[a, b], &price_limit).unwrap();
        let expected =
            alloy::primitives::aliases::U160::checked_from_limbs_slice(expected_raw.as_limbs())
                .unwrap();

        assert_ne!(
            inner.params.sqrtPriceLimitX96,
            alloy::primitives::aliases::U160::ZERO,
            "sqrtPriceLimitX96 must be non-zero when price_limit is set"
        );
        assert_eq!(
            inner.params.sqrtPriceLimitX96, expected,
            "sqrtPriceLimitX96 must match amm_core::protocols::sqrt_price_limit_x96"
        );
    }

    // ─── multi-hop + price_limit → UnsupportedProtocol ──────────────────────

    #[test]
    fn multi_hop_price_limit_returns_unsupported_protocol() {
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let a = asset(0x01);
        let mid = asset(0x05);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let ratio = Ratio::new(U256::from(2u64), U256::from(1u64)).unwrap();
        let price_limit = Price::new(a, b, ratio).unwrap();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50)
            .with_price_limit(Some(price_limit));

        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));
        // Build a 3-hop route [a, mid, b] by pushing an intermediate hop.
        let mut route = Route::new_single_hop(a, b, TradeType::ExactIn);
        route.hops.insert(1, mid);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("multi-hop + price_limit must return UnsupportedProtocol");
        assert_eq!(
            err,
            BuildError::UnsupportedProtocol,
            "must be UnsupportedProtocol for multi-hop with price_limit"
        );
    }

    // ─── AtBlock deadline → UnresolvedDeadline (folded-in E1.2 coverage) ────

    #[test]
    fn at_block_deadline_returns_unresolved_deadline() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
            .with_recipient(Recipient::To(Address::repeat_byte(0x55)))
            .with_deadline(Deadline::AtBlock(1234));

        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("AtBlock deadline must return UnresolvedDeadline for V3");
        assert_eq!(
            err,
            BuildError::UnresolvedDeadline,
            "must be UnresolvedDeadline"
        );
    }

    // ─── wrong assets → AssetNotInPool ──────────────────────────────────────

    #[test]
    fn wrong_assets_returns_asset_not_in_pool() {
        let a = asset(0x01);
        let b = asset(0x02);
        let c_asset = asset(0x03); // not in pool
        let p = pool(a, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(c_asset, U256::from(100u64));
        let route = Route::new_single_hop(a, c_asset, TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(c_asset),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("unknown output must fail");
        assert!(
            matches!(err, BuildError::AssetNotInPool { .. }),
            "expected AssetNotInPool, got: {err:?}"
        );
    }

    // ─── Recipient::Sender → UnresolvedRecipient ────────────────────────────

    #[test]
    fn unresolved_sender_returns_unresolved_recipient() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
            .with_deadline(Deadline::AtTimestamp(9_999_999));

        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("Sender recipient must fail");
        assert_eq!(err, BuildError::UnresolvedRecipient);
    }

    // ─── Deadline::FromNow → UnresolvedDeadline ─────────────────────────────

    #[test]
    fn from_now_deadline_returns_unresolved_deadline() {
        use std::time::Duration;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
            .with_recipient(Recipient::To(Address::repeat_byte(0x55)))
            .with_deadline(Deadline::FromNow(Duration::from_secs(300)));

        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("FromNow deadline must fail");
        assert_eq!(err, BuildError::UnresolvedDeadline);
    }

    // ─── wrong trade type → UnsupportedProtocol ─────────────────────────────

    #[test]
    fn exact_out_route_in_build_swap_returns_unsupported_protocol() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));
        // Intentionally wrong trade type: ExactOut passed to build_swap.
        let route = Route::new_single_hop(a, b, TradeType::ExactOut);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("ExactOut route in build_swap must return UnsupportedProtocol");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    #[test]
    fn exact_in_route_in_build_swap_exact_out_returns_unsupported_protocol() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_out = CurrencyAmount {
            currency: Currency::Token(b),
            raw: U256::from(100u64),
        };
        let quoted_in = AssetAmount::new(a, U256::from(100u64));
        // Intentionally wrong trade type: ExactIn passed to build_swap_exact_out.
        let route = Route::new_single_hop(a, b, TradeType::ExactIn);

        let err = p
            .build_swap_exact_out(
                &c,
                amount_out,
                Currency::Token(a),
                &route,
                &quoted_in,
                &opts,
            )
            .expect_err("ExactIn route in build_swap_exact_out must return UnsupportedProtocol");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }
}
