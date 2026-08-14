//! Aerodrome Slipstream swap encoder: exact-in and exact-out (ERC-20 and native ETH) → [`PreparedSwap`].
//!
//! Implements [`SwapRouter02`] for [`AerodromeSlipstreamPool`]. Slipstream is
//! Uniswap V3's twin with two key differences:
//!
//! - The ABI struct uses `int24 tickSpacing` where V3 uses `uint24 fee`.
//! - The deadline is embedded **inside** the params struct, so
//!   [`SwapRouter02::deadline_in_multicall`] returns `false`. Single-call
//!   ERC-20 swaps therefore produce **raw** `exactInputSingle` calldata (no
//!   multicall wrapper); two-call native-out paths produce
//!   `multicall(bytes[])` (selector `0xac9650d8`) without a deadline argument.
//!
//! The shared [`swaprouter02`] driver handles guards, native-ETH routing,
//! multicall assembly, and [`PreparedSwap`] construction; this module only
//! provides the ABI encoding pieces.

use alloy::primitives::aliases::I24;
use alloy::primitives::{Address, Bytes};
use alloy::{sol, sol_types::SolCall};
use amm_core::primitives::asset::AssetAmount;
use amm_core::protocols::aerodrome::slipstream::AerodromeSlipstreamPool;

use crate::execution::{
    config::ChainConfig,
    error::BuildError,
    executable::{Executable, Sealed},
    options::ExecutionOptions,
    prepared::PreparedSwap,
    protocols::swaprouter02::{self, SingleParams, SwapRouter02},
    types::{Currency, CurrencyAmount},
};

sol! {
    /// Minimal ABI surface for the Aerodrome Slipstream SwapRouter.
    ///
    /// Differs from Uniswap V3: `int24 tickSpacing` replaces `uint24 fee`, and
    /// `uint256 deadline` lives inside both params structs.
    interface ISlipstreamRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            int24 tickSpacing;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactOutputSingleParams {
            address tokenIn;
            address tokenOut;
            int24 tickSpacing;
            address recipient;
            uint256 deadline;
            uint256 amountOut;
            uint256 amountInMaximum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
        function exactOutputSingle(ExactOutputSingleParams params) external payable returns (uint256 amountIn);
    }
}

impl SwapRouter02 for AerodromeSlipstreamPool {
    fn router(&self, ctx: &ChainConfig) -> Result<Address, BuildError> {
        ctx.router_slipstream()
    }

    fn encode_exact_in(&self, p: &SingleParams) -> Result<Bytes, BuildError> {
        let tick_spacing = I24::try_from(self.tick_spacing()).map_err(|_| BuildError::Overflow)?;
        Ok(ISlipstreamRouter::exactInputSingleCall {
            params: ISlipstreamRouter::ExactInputSingleParams {
                tokenIn: p.token_in,
                tokenOut: p.token_out,
                tickSpacing: tick_spacing,
                recipient: p.recipient,
                deadline: p.deadline,
                amountIn: p.amount,
                amountOutMinimum: p.limit_amount,
                sqrtPriceLimitX96: p.sqrt_limit,
            },
        }
        .abi_encode()
        .into())
    }

    fn encode_exact_out(&self, p: &SingleParams) -> Result<Bytes, BuildError> {
        let tick_spacing = I24::try_from(self.tick_spacing()).map_err(|_| BuildError::Overflow)?;
        Ok(ISlipstreamRouter::exactOutputSingleCall {
            params: ISlipstreamRouter::ExactOutputSingleParams {
                tokenIn: p.token_in,
                tokenOut: p.token_out,
                tickSpacing: tick_spacing,
                recipient: p.recipient,
                deadline: p.deadline,
                amountOut: p.amount,
                amountInMaximum: p.limit_amount,
                sqrtPriceLimitX96: p.sqrt_limit,
            },
        }
        .abi_encode()
        .into())
    }

    /// `false` — the deadline is embedded in the params struct, not in a
    /// `multicall(uint256 deadline, bytes[])` overload.
    fn deadline_in_multicall(&self) -> bool {
        false
    }

    /// Classic SwapRouter (no ADDRESS_THIS sentinel) — the router must be passed
    /// its own address to retain the WETH for a following `unwrapWETH9`.
    fn custody_recipient(&self, router: Address) -> Address {
        router
    }
}

impl Sealed for AerodromeSlipstreamPool {}

impl Executable for AerodromeSlipstreamPool {
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        swaprouter02::build_exact_in(self, self, ctx, amount_in, to, quoted_out, opts)
    }

    fn build_swap_exact_out(
        &self,
        ctx: &ChainConfig,
        amount_out: CurrencyAmount,
        from: Currency,
        quoted_in: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        swaprouter02::build_exact_out(self, self, ctx, amount_out, from, quoted_in, opts)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::aliases::I24;
    use alloy::primitives::{Address, B256, U256, address};
    use alloy::sol_types::SolCall;
    use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::primitives::ratio::Bps;
    use amm_core::protocols::aerodrome::slipstream::{AerodromeSlipstreamPool, TickData};
    use amm_core::slippage::Slippage;

    use super::ISlipstreamRouter;
    use crate::execution::multicall::IMulticall;
    use crate::execution::{
        config::{ChainConfig, Routers},
        error::BuildError,
        executable::Executable,
        options::{Deadline, ExecutionOptions, Recipient},
        types::{Currency, CurrencyAmount},
    };

    // ─── test fixtures ───────────────────────────────────────────────────────

    const SPACING: i32 = 100;

    fn router() -> Address {
        address!("0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5")
    }

    fn chain_id() -> ChainId {
        ChainId(8453)
    }

    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    fn weth_base() -> AssetId {
        // 0x4200000000000000000000000000000000000006
        AssetId::new(
            chain_id(),
            B256::left_padding_from(&[
                0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x06,
            ]),
        )
    }

    /// Minimal Base chain config: chain 8453, Base WETH, Slipstream router set.
    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth_base()).with_routers(Routers {
            slipstream: Some(router()),
            ..Default::default()
        })
    }

    /// Build a minimal Slipstream pool over `a` and `b` with tick spacing 100.
    fn pool(a: AssetId, b: AssetId) -> AerodromeSlipstreamPool {
        AerodromeSlipstreamPool::new(
            PoolId::new("8453:slipstream:0x"),
            [a, b],
            U256::from(1u64) << 96,
            0,
            0,
            400,
            TickData::from_ticks(SPACING, vec![]),
        )
    }

    fn opts_resolved(to: Address, deadline_ts: u64, slippage_bps: u16) -> ExecutionOptions {
        ExecutionOptions::new(Slippage::from_bps(Bps(slippage_bps)))
            .with_recipient(Recipient::To(to))
            .with_deadline(Deadline::AtTimestamp(deadline_ts))
    }

    // ─── selector sanity ────────────────────────────────────────────────────

    /// Slipstream's struct layout (int24 tickSpacing, in-struct deadline) must
    /// produce different selectors from Uniswap V3's (uint24 fee, no deadline).
    #[test]
    fn selectors_differ_from_uniswap_v3() {
        // Uniswap V3 exactInputSingle selector (from v3 test suite)
        let v3_selector = [0x04u8, 0xe4, 0x5a, 0xaf];
        assert_ne!(
            ISlipstreamRouter::exactInputSingleCall::SELECTOR,
            v3_selector,
            "Slipstream exactInputSingle selector must differ from V3 (int24 tickSpacing + deadline change the sig)"
        );
    }

    // ─── exact-in ERC-20 success path ───────────────────────────────────────

    /// ERC-20 exact-in: tx.data decodes DIRECTLY as exactInputSingle (no multicall
    /// wrapper — deadline_in_multicall() == false and single call passes through raw).
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
        let prepared = p
            .build_swap(&c, amount_in, Currency::Token(b), &quoted_out, &opts)
            .expect("build_swap must succeed for ERC-20 exact-in");

        // tx.to == slipstream router, tx.value == 0
        assert_eq!(
            prepared.tx.to,
            router(),
            "tx.to must be the Slipstream router"
        );
        assert_eq!(prepared.tx.value, U256::ZERO, "value must be 0 for ERC-20");
        assert_eq!(prepared.tx.chain, c.chain, "tx.chain must match ctx.chain");

        // ERC-20 single call: raw exactInputSingle (NO multicall wrapper)
        let inner = ISlipstreamRouter::exactInputSingleCall::abi_decode(&prepared.tx.data)
            .expect("tx.data must decode directly as exactInputSingle (no multicall wrapper)");

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
            inner.params.tickSpacing,
            I24::try_from(SPACING).unwrap(),
            "tickSpacing must equal pool.tick_spacing()"
        );
        assert_eq!(
            inner.params.recipient, recipient,
            "recipient must round-trip"
        );
        assert_eq!(
            inner.params.deadline,
            U256::from(deadline_ts),
            "deadline must be embedded in the struct"
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

        // approval == Some { spender: router, token: input, min_allowance: amountIn }
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender,
            router(),
            "approval.spender must be the Slipstream router"
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

    /// ERC-20 exact-out: tx.data decodes DIRECTLY as exactOutputSingle (no multicall wrapper).
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
        let prepared = p
            .build_swap_exact_out(&c, amount_out, Currency::Token(a), &quoted_in, &opts)
            .expect("build_swap_exact_out must succeed for ERC-20 exact-out");

        assert_eq!(
            prepared.tx.to,
            router(),
            "tx.to must be the Slipstream router"
        );
        assert_eq!(prepared.tx.value, U256::ZERO, "value must be 0");

        // Raw exactOutputSingle (no multicall wrapper)
        let inner = ISlipstreamRouter::exactOutputSingleCall::abi_decode(&prepared.tx.data)
            .expect("tx.data must decode directly as exactOutputSingle (no multicall wrapper)");

        assert_eq!(
            inner.params.tickSpacing,
            I24::try_from(SPACING).unwrap(),
            "tickSpacing must equal pool.tick_spacing()"
        );
        assert_eq!(
            inner.params.deadline,
            U256::from(deadline_ts),
            "deadline must be embedded in the struct"
        );
        assert_eq!(
            inner.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&a)
        );
        assert_eq!(
            inner.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&b)
        );
        assert_eq!(inner.params.recipient, recipient);
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
            alloy::primitives::aliases::U160::ZERO
        );

        // max_spent == Some(input, max)
        let max_spent = prepared
            .max_spent
            .expect("max_spent must be Some for exact-out");
        assert_eq!(max_spent.raw, U256::from(505u64));
        assert_eq!(max_spent.asset, a);

        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(approval.spender, router());
        assert_eq!(approval.token, a);
        assert_eq!(approval.min_allowance, U256::from(505u64));
        assert!(!approval.reset_first);
    }

    // ─── native-in exact-in (ETH → token) ───────────────────────────────────

    /// Native-in exact-in: single raw exactInputSingle call, tx.value set, no approval.
    #[test]
    fn native_in_exact_in_encodes_exact_input_single_with_value() {
        let b = asset(0x02);
        let w = weth_base();
        let p = pool(w, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x77);
        let deadline_ts = 1_700_000_000u64;
        let opts = opts_resolved(recipient, deadline_ts, 200);

        let amount_in_raw = U256::from(500u64);
        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: amount_in_raw,
        };
        let quoted_out = AssetAmount::new(b, U256::from(1000u64));
        let prepared = p
            .build_swap(&c, amount_in, Currency::Token(b), &quoted_out, &opts)
            .expect("native-in exact-in must succeed");

        assert_eq!(
            prepared.tx.value, amount_in_raw,
            "tx.value must carry the ETH amount"
        );
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in"
        );

        // Single call — raw exactInputSingle (no multicall wrapper for Slipstream)
        let inner = ISlipstreamRouter::exactInputSingleCall::abi_decode(&prepared.tx.data)
            .expect("tx.data must decode as raw exactInputSingle for native-in");

        assert_eq!(
            inner.params.tickSpacing,
            I24::try_from(SPACING).unwrap(),
            "tickSpacing must equal pool.tick_spacing()"
        );
        assert_eq!(
            inner.params.deadline,
            U256::from(deadline_ts),
            "deadline must be embedded in struct"
        );
        assert_eq!(
            inner.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&w),
            "tokenIn must be WETH (resolved from Native)"
        );
        assert_eq!(
            inner.params.recipient, recipient,
            "recipient must be user address"
        );
        assert_eq!(inner.params.amountIn, amount_in_raw);
        // 200 bps: floor(1000 * 9800/10000) = 980
        assert_eq!(inner.params.amountOutMinimum, U256::from(980u64));
    }

    // ─── native-out exact-in (token → ETH) ──────────────────────────────────

    /// Native-out exact-in: multicall(bytes[]) (selector 0xac9650d8) wrapping
    /// [exactInputSingle(recipient=router), unwrapWETH9(min, user)].
    /// Slipstream has no ADDRESS_THIS sentinel; the router's own address retains WETH.
    #[test]
    fn native_out_exact_in_encodes_multicall_with_swap_and_unwrap() {
        let a = asset(0x01);
        let w = weth_base();
        let p = pool(a, w);
        let c = ctx();

        let recipient = Address::repeat_byte(0x88);
        let deadline_ts = 1_800_000_000u64;
        let opts = opts_resolved(recipient, deadline_ts, 50);

        let amount_in_raw = U256::from(300u64);
        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: amount_in_raw,
        };
        let quoted_out = AssetAmount::new(w, U256::from(1000u64));
        let prepared = p
            .build_swap(&c, amount_in, Currency::Native, &quoted_out, &opts)
            .expect("native-out exact-in must succeed");

        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for ERC-20 input"
        );
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(approval.token, a);
        assert_eq!(approval.spender, router());
        assert_eq!(approval.min_allowance, amount_in_raw);

        // Outer is multicall(bytes[]) — selector 0xac9650d8 (no deadline)
        assert_eq!(
            &prepared.tx.data[..4],
            &[0xac, 0x96, 0x50, 0xd8],
            "outer selector must be multicall(bytes[]) for Slipstream native-out"
        );
        let outer = IMulticall::multicall_0Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_0Call");
        assert_eq!(outer.data.len(), 2, "native-out exact-in: two inner calls");

        // First inner: exactInputSingle with recipient = router (Slipstream has no ADDRESS_THIS
        // sentinel; the router's own address must be used to retain WETH for unwrapWETH9).
        let swap = ISlipstreamRouter::exactInputSingleCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactInputSingle");
        assert_eq!(
            swap.params.recipient,
            router(),
            "swap recipient must be the router address (Slipstream has no ADDRESS_THIS sentinel)"
        );
        assert_eq!(
            swap.params.tickSpacing,
            I24::try_from(SPACING).unwrap(),
            "tickSpacing must be present in inner call"
        );
        assert_eq!(
            swap.params.deadline,
            U256::from(deadline_ts),
            "deadline in struct"
        );
        assert_eq!(
            swap.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&a)
        );
        assert_eq!(
            swap.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&w)
        );
        assert_eq!(swap.params.amountIn, amount_in_raw);
        // 50 bps: floor(1000 * 9950/10000) = 995
        assert_eq!(swap.params.amountOutMinimum, U256::from(995u64));

        // Second inner: unwrapWETH9(min, user)
        use crate::execution::protocols::swaprouter02::IPeripheryPayments;
        let unwrap = IPeripheryPayments::unwrapWETH9Call::abi_decode(&outer.data[1])
            .expect("second inner must decode as unwrapWETH9");
        assert_eq!(unwrap.amountMinimum, U256::from(995u64));
        assert_eq!(unwrap.recipient, recipient);

        assert_eq!(prepared.min_received.raw, U256::from(995u64));
        assert_eq!(prepared.min_received.asset, w);
        assert!(prepared.max_spent.is_none());
    }

    // ─── native-in exact-out (ETH → token) ──────────────────────────────────

    /// Native-in exact-out: multicall(bytes[]) wrapping
    /// [exactOutputSingle(recipient=user), refundETH()], tx.value = max_amount_in.
    #[test]
    fn native_in_exact_out_encodes_multicall_with_swap_and_refund() {
        let b = asset(0x02);
        let w = weth_base();
        let p = pool(w, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x77);
        let deadline_ts = 1_700_000_000u64;
        let opts = opts_resolved(recipient, deadline_ts, 100);

        let out_raw = U256::from(400u64);
        let amount_out = CurrencyAmount {
            currency: Currency::Token(b),
            raw: out_raw,
        };
        let quoted_in = AssetAmount::new(w, U256::from(500u64));
        let prepared = p
            .build_swap_exact_out(&c, amount_out, Currency::Native, &quoted_in, &opts)
            .expect("native-in exact-out must succeed");

        // tx.value = max_amount_in = ceil(500 * 10100/10000) = 505
        assert_eq!(
            prepared.tx.value,
            U256::from(505u64),
            "tx.value must equal max_amount_in"
        );
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in"
        );

        // Outer: multicall(bytes[]) — selector 0xac9650d8
        assert_eq!(
            &prepared.tx.data[..4],
            &[0xac, 0x96, 0x50, 0xd8],
            "outer selector must be multicall(bytes[]) for Slipstream native-in exact-out"
        );
        let outer = IMulticall::multicall_0Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_0Call");
        assert_eq!(outer.data.len(), 2, "native-in exact-out: two inner calls");

        // First inner: exactOutputSingle with recipient = user
        let swap = ISlipstreamRouter::exactOutputSingleCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactOutputSingle");
        assert_eq!(
            swap.params.tickSpacing,
            I24::try_from(SPACING).unwrap(),
            "tickSpacing must be present"
        );
        assert_eq!(
            swap.params.deadline,
            U256::from(deadline_ts),
            "deadline in struct"
        );
        assert_eq!(
            swap.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&w)
        );
        assert_eq!(
            swap.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&b)
        );
        assert_eq!(
            swap.params.recipient, recipient,
            "recipient must be user address for native-in"
        );
        assert_eq!(swap.params.amountOut, out_raw);
        assert_eq!(swap.params.amountInMaximum, U256::from(505u64));

        // Second inner: refundETH()
        use crate::execution::protocols::swaprouter02::IPeripheryPayments;
        assert_eq!(
            &outer.data[1][..4],
            &IPeripheryPayments::refundETHCall::SELECTOR,
            "second inner selector must be refundETH()"
        );

        let max_spent = prepared
            .max_spent
            .expect("max_spent must be Some for exact-out");
        assert_eq!(max_spent.raw, U256::from(505u64));
        assert_eq!(max_spent.asset, w);
    }

    // ─── native-out exact-out (token → ETH) ─────────────────────────────────

    /// Native-out exact-out: multicall(bytes[]) wrapping
    /// [exactOutputSingle(recipient=router), unwrapWETH9(amount_out, user)].
    /// Slipstream has no ADDRESS_THIS sentinel; the router's own address retains WETH.
    #[test]
    fn native_out_exact_out_encodes_multicall_with_swap_and_unwrap() {
        let a = asset(0x01);
        let w = weth_base();
        let p = pool(a, w);
        let c = ctx();

        let recipient = Address::repeat_byte(0x99);
        let deadline_ts = 1_900_000_000u64;
        let opts = opts_resolved(recipient, deadline_ts, 100);

        let out_raw = U256::from(400u64);
        let amount_out = CurrencyAmount {
            currency: Currency::Native,
            raw: out_raw,
        };
        let quoted_in = AssetAmount::new(a, U256::from(500u64));
        let prepared = p
            .build_swap_exact_out(&c, amount_out, Currency::Token(a), &quoted_in, &opts)
            .expect("native-out exact-out must succeed");

        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for ERC-20 input"
        );
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(approval.token, a);
        assert_eq!(approval.spender, router());
        // max_amount_in = ceil(500 * 10100/10000) = 505
        assert_eq!(approval.min_allowance, U256::from(505u64));

        // Outer: multicall(bytes[]) — selector 0xac9650d8
        assert_eq!(
            &prepared.tx.data[..4],
            &[0xac, 0x96, 0x50, 0xd8],
            "outer selector must be multicall(bytes[]) for Slipstream native-out exact-out"
        );
        let outer = IMulticall::multicall_0Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_0Call");
        assert_eq!(outer.data.len(), 2, "native-out exact-out: two inner calls");

        // First inner: exactOutputSingle with recipient = router (Slipstream has no ADDRESS_THIS
        // sentinel; the router's own address must be used to retain WETH for unwrapWETH9).
        let swap = ISlipstreamRouter::exactOutputSingleCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactOutputSingle");
        assert_eq!(
            swap.params.recipient,
            router(),
            "swap recipient must be the router address (Slipstream has no ADDRESS_THIS sentinel)"
        );
        assert_eq!(
            swap.params.tickSpacing,
            I24::try_from(SPACING).unwrap(),
            "tickSpacing must be present"
        );
        assert_eq!(
            swap.params.deadline,
            U256::from(deadline_ts),
            "deadline in struct"
        );
        assert_eq!(
            swap.params.tokenIn,
            crate::execution::protocols::common::evm_addr(&a)
        );
        assert_eq!(
            swap.params.tokenOut,
            crate::execution::protocols::common::evm_addr(&w)
        );
        assert_eq!(swap.params.amountOut, out_raw);
        assert_eq!(swap.params.amountInMaximum, U256::from(505u64));

        // Second inner: unwrapWETH9(amount_out_raw, user) — exact target
        use crate::execution::protocols::swaprouter02::IPeripheryPayments;
        let unwrap = IPeripheryPayments::unwrapWETH9Call::abi_decode(&outer.data[1])
            .expect("second inner must decode as unwrapWETH9");
        assert_eq!(
            unwrap.amountMinimum, out_raw,
            "unwrapWETH9.amountMinimum must be exact target"
        );
        assert_eq!(unwrap.recipient, recipient);

        let max_spent = prepared.max_spent.expect("max_spent must be Some");
        assert_eq!(max_spent.raw, U256::from(505u64));
        assert_eq!(max_spent.asset, a);
        assert_eq!(prepared.min_received.raw, out_raw);
        assert_eq!(prepared.min_received.asset, w);
    }

    // ─── both native → NativeMismatch ────────────────────────────────────────

    #[test]
    fn native_in_native_out_returns_native_mismatch() {
        let w = weth_base();
        let b = asset(0x02);
        let p = pool(w, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(w, U256::from(100u64));
        let err = p
            .build_swap(&c, amount_in, Currency::Native, &quoted_out, &opts)
            .expect_err("native→native must return NativeMismatch");
        assert_eq!(err, BuildError::NativeMismatch);
    }

    #[test]
    fn native_in_native_out_exact_out_returns_native_mismatch() {
        let w = weth_base();
        let b = asset(0x02);
        let p = pool(w, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_out = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_in = AssetAmount::new(w, U256::from(100u64));
        let err = p
            .build_swap_exact_out(&c, amount_out, Currency::Native, &quoted_in, &opts)
            .expect_err("native→native exact-out must return NativeMismatch");
        assert_eq!(err, BuildError::NativeMismatch);
    }

    // ─── guard: Recipient::Sender → UnresolvedRecipient ─────────────────────

    #[test]
    fn unresolved_sender_returns_unresolved_recipient() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        // Default Recipient is Sender (unresolved)
        let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
            .with_deadline(Deadline::AtTimestamp(9_999_999));

        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));
        let err = p
            .build_swap(&c, amount_in, Currency::Token(b), &quoted_out, &opts)
            .expect_err("Sender recipient must fail");
        assert_eq!(err, BuildError::UnresolvedRecipient);
    }

    // ─── guard: Deadline::FromNow → UnresolvedDeadline ──────────────────────

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
        let err = p
            .build_swap(&c, amount_in, Currency::Token(b), &quoted_out, &opts)
            .expect_err("FromNow deadline must fail");
        assert_eq!(err, BuildError::UnresolvedDeadline);
    }

    // ─── guard: native→native → NativeMismatch ──────────────────────────────

    #[test]
    fn both_native_in_build_swap_returns_native_mismatch() {
        let w = weth_base();
        let b = asset(0x02);
        let p = pool(w, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));

        let err = p
            .build_swap(&c, amount_in, Currency::Native, &quoted_out, &opts)
            .expect_err("native→native in build_swap must return NativeMismatch");
        assert_eq!(err, BuildError::NativeMismatch);
    }

    #[test]
    fn both_native_in_build_swap_exact_out_returns_native_mismatch() {
        let w = weth_base();
        let b = asset(0x02);
        let p = pool(w, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_out = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_in = AssetAmount::new(w, U256::from(100u64));

        let err = p
            .build_swap_exact_out(&c, amount_out, Currency::Native, &quoted_in, &opts)
            .expect_err("native→native in build_swap_exact_out must return NativeMismatch");
        assert_eq!(err, BuildError::NativeMismatch);
    }

    // ─── single-hop price_limit encodes non-zero sqrtPriceLimitX96 ──────────

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
        let prepared = p
            .build_swap(&c, amount_in, Currency::Token(b), &quoted_out, &opts)
            .expect("single-hop price_limit must succeed");

        // ERC-20 single call: raw exactInputSingle
        let inner = ISlipstreamRouter::exactInputSingleCall::abi_decode(&prepared.tx.data)
            .expect("tx.data must decode as raw exactInputSingle");

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

    // ─── single-hop price_limit succeeds (multi-hop rejection is planner-side) ─

    #[test]
    fn multi_hop_price_limit_returns_unsupported_protocol() {
        // Single-hop build always passes hops=2 to resolve_sqrt_limit; the
        // multi-hop + price_limit rejection is a planner concern, not encoder-side.
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let a = asset(0x01);
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

        // Single-hop + price_limit succeeds; hops=2 is always below the 3-hop gate.
        p.build_swap(&c, amount_in, Currency::Token(b), &quoted_out, &opts)
            .expect("single-hop + price_limit must succeed for Slipstream");
    }

    // ─── as_executable dispatch ──────────────────────────────────────────────

    /// `as_executable` must return `Some` for a Slipstream pool once the arm is added.
    #[test]
    fn as_executable_is_some_for_slipstream_pool() {
        use crate::execution::executable::as_executable;
        use amm_core::traits::pool::Pool;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        assert!(as_executable(&p as &dyn Pool).is_some());
    }
}
