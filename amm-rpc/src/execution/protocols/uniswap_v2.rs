//! Uniswap V2 swap encoder: exact-in and exact-out (ERC-20 and native ETH) → [`PreparedSwap`].
//!
//! [`build_swap`] dispatches over `(input native?, output native?)` to encode one of
//! three Router02 exact-in entrypoints. [`build_swap_exact_out`] mirrors it for the three
//! exact-out entrypoints.
//! The `sol!` macro derives ABI selectors and parameter layouts from the interface.

use alloy::primitives::U256;
use alloy::{sol, sol_types::SolCall};
use amm_core::primitives::asset::AssetAmount;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;

use crate::execution::{
    config::ChainConfig,
    error::BuildError,
    executable::{Executable, Sealed},
    options::ExecutionOptions,
    prepared::{PreparedSwap, Route},
    protocols::common,
    types::{Currency, CurrencyAmount, TradeType},
};

sol! {
    /// Minimal ABI surface for the Uniswap V2 Router02.
    interface IUniswapV2Router02 {
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] path,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);

        function swapExactETHForTokens(
            uint256 amountOutMin,
            address[] path,
            address to,
            uint256 deadline
        ) external payable returns (uint256[] amounts);

        function swapExactTokensForETH(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] path,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);

        function swapTokensForExactTokens(
            uint256 amountOut,
            uint256 amountInMax,
            address[] path,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);

        function swapETHForExactTokens(
            uint256 amountOut,
            address[] path,
            address to,
            uint256 deadline
        ) external payable returns (uint256[] amounts);

        function swapTokensForExactETH(
            uint256 amountOut,
            uint256 amountInMax,
            address[] path,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);
    }
}

impl Sealed for UniswapV2Pool {}

impl Executable for UniswapV2Pool {
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        route: &Route,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        // V2 has no sqrtPriceLimit — reject any price limit up front.
        if opts.price_limit.is_some() {
            return Err(BuildError::UnsupportedProtocol);
        }

        let r = common::resolve_swap(
            ctx,
            self,
            amount_in.currency,
            to,
            route,
            opts,
            TradeType::ExactIn,
        )?;
        let min = opts.slippage.min_amount_out(quoted_out);
        let router = ctx.router_v2()?;
        // resolve() already maps Native → WETH, so the path is uniform across all arms.
        let path = vec![common::evm_addr(&r.input), common::evm_addr(&r.output)];

        match (r.native_in, r.native_out) {
            // Native → Native is always a configuration error.
            (true, true) => Err(BuildError::NativeMismatch),

            // Native-in (ETH → token): use swapExactETHForTokens; value = amountIn; no approval.
            (true, false) => {
                let call = IUniswapV2Router02::swapExactETHForTokensCall {
                    amountOutMin: min.raw,
                    path,
                    to: r.recipient,
                    deadline: r.deadline,
                };
                Ok(common::prepared(
                    ctx,
                    router,
                    call.abi_encode().into(),
                    // Attach the ETH amount as msg.value; no ERC-20 transfer needed.
                    amount_in.raw,
                    min,
                    None,
                    // Native ETH is transferred as msg.value — no ERC-20 approval required.
                    None,
                ))
            }

            // Native-out (token → ETH): use swapExactTokensForETH; value = 0; approval required.
            (false, true) => {
                let call = IUniswapV2Router02::swapExactTokensForETHCall {
                    amountIn: amount_in.raw,
                    amountOutMin: min.raw,
                    path,
                    to: r.recipient,
                    deadline: r.deadline,
                };
                Ok(common::prepared(
                    ctx,
                    router,
                    call.abi_encode().into(),
                    U256::ZERO,
                    min,
                    None,
                    Some(common::erc20_approval(router, r.input, amount_in.raw)),
                ))
            }

            // ERC-20 → ERC-20: use swapExactTokensForTokens; value = 0; approval required.
            (false, false) => {
                let call = IUniswapV2Router02::swapExactTokensForTokensCall {
                    amountIn: amount_in.raw,
                    amountOutMin: min.raw,
                    path,
                    to: r.recipient,
                    deadline: r.deadline,
                };
                Ok(common::prepared(
                    ctx,
                    router,
                    call.abi_encode().into(),
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
        // V2 has no sqrtPriceLimit — reject any price limit up front.
        if opts.price_limit.is_some() {
            return Err(BuildError::UnsupportedProtocol);
        }

        let r = common::resolve_swap(
            ctx,
            self,
            from,
            amount_out.currency,
            route,
            opts,
            TradeType::ExactOut,
        )?;
        // Slippage ceiling on the input side; router refunds any surplus ETH.
        let max = opts.slippage.max_amount_in(quoted_in);
        let router = ctx.router_v2()?;
        // resolve() already maps Native → WETH, so the path is uniform across all arms.
        let path = vec![common::evm_addr(&r.input), common::evm_addr(&r.output)];
        let out = amount_out.raw;

        match (r.native_in, r.native_out) {
            // Native → Native is always a configuration error.
            (true, true) => Err(BuildError::NativeMismatch),

            // Native-in (ETH → token): use swapETHForExactTokens; value = max input;
            // the router refunds any surplus ETH to msg.sender; no ERC-20 approval.
            (true, false) => {
                let call = IUniswapV2Router02::swapETHForExactTokensCall {
                    amountOut: out,
                    path,
                    to: r.recipient,
                    deadline: r.deadline,
                };
                Ok(common::prepared(
                    ctx,
                    router,
                    call.abi_encode().into(),
                    // Attach the ETH ceiling as msg.value; router refunds the surplus.
                    max.raw,
                    AssetAmount::new(r.output, out),
                    Some(AssetAmount::new(r.input, max.raw)),
                    // Native ETH is transferred as msg.value — no ERC-20 approval required.
                    None,
                ))
            }

            // Native-out (token → ETH): use swapTokensForExactETH; value = 0; approval required.
            (false, true) => {
                let call = IUniswapV2Router02::swapTokensForExactETHCall {
                    amountOut: out,
                    amountInMax: max.raw,
                    path,
                    to: r.recipient,
                    deadline: r.deadline,
                };
                Ok(common::prepared(
                    ctx,
                    router,
                    call.abi_encode().into(),
                    U256::ZERO,
                    AssetAmount::new(r.output, out),
                    Some(AssetAmount::new(r.input, max.raw)),
                    Some(common::erc20_approval(router, r.input, max.raw)),
                ))
            }

            // ERC-20 → ERC-20: use swapTokensForExactTokens; value = 0; approval required.
            (false, false) => {
                let call = IUniswapV2Router02::swapTokensForExactTokensCall {
                    amountOut: out,
                    amountInMax: max.raw,
                    path,
                    to: r.recipient,
                    deadline: r.deadline,
                };
                Ok(common::prepared(
                    ctx,
                    router,
                    call.abi_encode().into(),
                    U256::ZERO,
                    AssetAmount::new(r.output, out),
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
    use amm_core::protocols::uniswap::v2::UniswapV2Pool;
    use amm_core::slippage::Slippage;

    use super::IUniswapV2Router02;
    use crate::execution::{
        config::{ChainConfig, Routers},
        error::BuildError,
        executable::Executable,
        options::{Deadline, ExecutionOptions, Recipient},
        prepared::Route,
        types::{Currency, CurrencyAmount, TradeType},
    };

    /// A fixed router address used across tests.
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

    /// Minimal chain config: chain 1, WETH, V2 router set.
    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth()).with_routers(Routers {
            v2: Some(router()),
            ..Default::default()
        })
    }

    /// Build a pool over `a` and `b` with unit reserves.
    fn pool(a: AssetId, b: AssetId) -> UniswapV2Pool {
        UniswapV2Pool::new(PoolId::new("1:univ2:0x"), [a, b], [U256::from(1u64); 2], 30)
    }

    /// Resolved execution options: explicit recipient + absolute timestamp.
    fn opts_resolved(to: Address, deadline_ts: u64, slippage_bps: u16) -> ExecutionOptions {
        ExecutionOptions::new(Slippage::from_bps(Bps(slippage_bps)))
            .with_recipient(Recipient::To(to))
            .with_deadline(Deadline::AtTimestamp(deadline_ts))
    }

    // ─── exact-in success path ───────────────────────────────────────────────

    #[test]
    fn exact_in_erc20_encodes_correctly() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x55);
        let deadline_ts = 1_700_000_000u64;
        // 100 bps slippage: quoted_out 1000 → min_received 990
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

        // Decode the calldata back to verify field values.
        let decoded =
            IUniswapV2Router02::swapExactTokensForTokensCall::abi_decode(&prepared.tx.data)
                .expect("calldata must round-trip through abi_decode");

        // amountIn round-trips
        assert_eq!(
            decoded.amountIn,
            U256::from(500u64),
            "amountIn must equal input raw"
        );

        // amountOutMin == min_amount_out(quoted_out) at 100 bps = floor(1000 * 9900/10000) = 990
        assert_eq!(
            decoded.amountOutMin,
            U256::from(990u64),
            "amountOutMin must be 990 at 100bps"
        );

        // path == [input, output]
        assert_eq!(
            decoded.path,
            vec![
                crate::execution::protocols::common::evm_addr(&a),
                crate::execution::protocols::common::evm_addr(&b)
            ],
            "path must be [input, output]"
        );

        // tx.value == 0 (ERC-20, no native transfer)
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "value must be 0 for ERC-20 swap"
        );

        // tx.chain == ctx.chain
        assert_eq!(prepared.tx.chain, c.chain, "tx.chain must match ctx.chain");

        // tx.to == router
        assert_eq!(prepared.tx.to, router(), "tx.to must be the V2 router");

        // deadline passes through
        assert_eq!(
            decoded.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );

        // approval == Some { spender: router, token: input, min_allowance: amount_in.raw, reset_first: false }
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender,
            router(),
            "approval.spender must be the V2 router"
        );
        assert_eq!(approval.token, a, "approval.token must be the input asset");
        assert_eq!(
            approval.min_allowance,
            U256::from(500u64),
            "approval.min_allowance must be amount_in.raw"
        );
        assert!(
            !approval.reset_first,
            "reset_first must be false (no token metadata)"
        );

        // min_received == 990 of asset b
        assert_eq!(prepared.min_received.raw, U256::from(990u64));
        assert_eq!(prepared.min_received.asset, b);

        // max_spent == None (exact-in)
        assert!(
            prepared.max_spent.is_none(),
            "max_spent must be None for exact-in"
        );
    }

    // ─── price_limit → UnsupportedProtocol ──────────────────────────────────

    #[test]
    fn price_limit_some_returns_unsupported_protocol() {
        use alloy::primitives::B256;
        use amm_core::primitives::asset::ChainId;
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = pool(a, b);
        let c = ctx();

        let base = AssetId::new(ChainId(1), B256::left_padding_from(&[0xaa]));
        let quote = AssetId::new(ChainId(1), B256::left_padding_from(&[0xbb]));
        let ratio = Ratio::new(U256::from(3u64), U256::from(1u64)).unwrap();
        let price_limit = Price::new(base, quote, ratio).unwrap();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50)
            .with_price_limit(Some(price_limit));

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
            .expect_err("price_limit Some must return an error");
        assert_eq!(
            err,
            BuildError::UnsupportedProtocol,
            "must be UnsupportedProtocol"
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

        // recipient left as Sender (unresolved)
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

    // ─── AtBlock deadline → UnresolvedDeadline ──────────────────────────────

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
            .expect_err("AtBlock deadline must fail for V2");
        assert_eq!(err, BuildError::UnresolvedDeadline);
    }

    // ─── build_swap_exact_out trade-type mismatch → UnsupportedProtocol ────

    #[test]
    fn build_swap_exact_out_exact_in_route_returns_unsupported_protocol() {
        // Passing an ExactIn route to build_swap_exact_out must be rejected.
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
            .expect_err(
                "ExactIn route passed to build_swap_exact_out must return UnsupportedProtocol",
            );
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ─── exact-out ERC-20 success path ──────────────────────────────────────

    #[test]
    fn exact_out_erc20_encodes_correctly() {
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

        // Decode calldata back to verify field values.
        let decoded =
            IUniswapV2Router02::swapTokensForExactTokensCall::abi_decode(&prepared.tx.data)
                .expect("calldata must round-trip through abi_decode");

        // amountOut round-trips
        assert_eq!(
            decoded.amountOut, out_raw,
            "amountOut must equal amount_out.raw"
        );

        // amountInMax == max_amount_in(quoted_in) at 100 bps = ceil(500 * 10100/10000) = 505
        assert_eq!(
            decoded.amountInMax,
            U256::from(505u64),
            "amountInMax must be 505 at 100bps"
        );

        // path == [input, output]
        assert_eq!(
            decoded.path,
            vec![
                crate::execution::protocols::common::evm_addr(&a),
                crate::execution::protocols::common::evm_addr(&b)
            ],
            "path must be [input, output]"
        );

        // tx.value == 0 (ERC-20, no native transfer)
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "value must be 0 for ERC-20 swap"
        );

        // tx.chain == ctx.chain
        assert_eq!(prepared.tx.chain, c.chain, "tx.chain must match ctx.chain");

        // tx.to == router
        assert_eq!(prepared.tx.to, router(), "tx.to must be the V2 router");

        // deadline passes through
        assert_eq!(
            decoded.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );

        // min_received == exact out target in asset b
        assert_eq!(
            prepared.min_received.raw, out_raw,
            "min_received.raw must equal amount_out.raw"
        );
        assert_eq!(
            prepared.min_received.asset, b,
            "min_received.asset must be output asset"
        );

        // max_spent == Some(input asset, max_amount_in)
        let max_spent = prepared
            .max_spent
            .expect("max_spent must be Some for exact-out");
        assert_eq!(
            max_spent.raw,
            U256::from(505u64),
            "max_spent.raw must be the slippage ceiling"
        );
        assert_eq!(
            max_spent.asset, a,
            "max_spent.asset must be the input asset"
        );

        // approval == Some { spender: router, token: input, min_allowance: max_amount_in }
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender,
            router(),
            "approval.spender must be the V2 router"
        );
        assert_eq!(approval.token, a, "approval.token must be the input asset");
        assert_eq!(
            approval.min_allowance,
            U256::from(505u64),
            "approval.min_allowance must be max_amount_in"
        );
        assert!(!approval.reset_first, "reset_first must be false");
    }

    // ─── native-in exact-out (ETH → token) ──────────────────────────────────

    #[test]
    fn native_in_exact_out_encodes_swap_eth_for_exact_tokens() {
        // Pool over WETH (0xC0) and token b (0x02); input is Native (ETH).
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
            .expect("native-in build_swap_exact_out must succeed");

        // tx.value must be max_amount_in (the ETH ceiling the router may consume).
        assert_eq!(
            prepared.tx.value,
            U256::from(505u64),
            "tx.value must equal max_amount_in for native-in exact-out"
        );

        // No ERC-20 approval needed when paying with native ETH.
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in swap"
        );

        // Decode calldata and verify it is swapETHForExactTokens.
        let decoded = IUniswapV2Router02::swapETHForExactTokensCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as swapETHForExactTokens");

        assert_eq!(decoded.amountOut, out_raw, "amountOut must round-trip");
        // path[0] must be WETH (resolved from Native).
        assert_eq!(
            decoded.path,
            vec![
                crate::execution::protocols::common::evm_addr(&w),
                crate::execution::protocols::common::evm_addr(&b)
            ],
            "path must be [weth, output]"
        );
        assert_eq!(decoded.to, recipient, "recipient must round-trip");
        assert_eq!(
            decoded.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
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
    }

    // ─── native-in (ETH → token) ─────────────────────────────────────────────

    #[test]
    fn native_in_erc20_out_encodes_swap_exact_eth_for_tokens() {
        // Pool over WETH (0xC0) and token b (0x02); input is Native (ETH).
        let b = asset(0x02);
        let w = weth();
        let p = pool(w, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x77);
        let deadline_ts = 1_700_000_000u64;
        // 200 bps slippage: quoted_out 1000 → min_received 980
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
            .expect("native-in build_swap must succeed");

        // tx.value must carry the ETH amount.
        assert_eq!(
            prepared.tx.value, amount_in_raw,
            "tx.value must equal amount_in.raw for native-in"
        );

        // No ERC-20 approval needed when paying with native ETH.
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in swap"
        );

        // Decode calldata and verify it is swapExactETHForTokens.
        let decoded = IUniswapV2Router02::swapExactETHForTokensCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as swapExactETHForTokens");

        // path[0] must be WETH (resolved from Native).
        assert_eq!(
            decoded.path,
            vec![
                crate::execution::protocols::common::evm_addr(&w),
                crate::execution::protocols::common::evm_addr(&b)
            ],
            "path must be [weth, output]"
        );

        // amountOutMin == floor(1000 * 9800/10000) == 980
        assert_eq!(
            decoded.amountOutMin,
            U256::from(980u64),
            "amountOutMin must be 980 at 200bps"
        );

        assert_eq!(decoded.to, recipient, "recipient must round-trip");
        assert_eq!(
            decoded.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );
    }

    // ─── native-out (token → ETH) ─────────────────────────────────────────────

    #[test]
    fn erc20_in_native_out_encodes_swap_exact_tokens_for_eth() {
        // Pool over token a (0x01) and WETH (0xC0); output is Native (ETH).
        let a = asset(0x01);
        let w = weth();
        let p = pool(a, w);
        let c = ctx();

        let recipient = Address::repeat_byte(0x88);
        let deadline_ts = 1_800_000_000u64;
        // 50 bps slippage: quoted_out 1000 → min_received 995
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
            .expect("native-out build_swap must succeed");

        // tx.value must be zero (ERC-20 input; ETH comes out, not in).
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be zero for native-out swap"
        );

        // Approval must be present and reference the input token.
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.token, a,
            "approval.token must be the ERC-20 input asset"
        );
        assert_eq!(
            approval.spender,
            router(),
            "approval.spender must be the V2 router"
        );
        assert_eq!(
            approval.min_allowance, amount_in_raw,
            "approval.min_allowance must equal amount_in.raw"
        );

        // Decode calldata and verify it is swapExactTokensForETH.
        let decoded = IUniswapV2Router02::swapExactTokensForETHCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as swapExactTokensForETH");

        assert_eq!(decoded.amountIn, amount_in_raw, "amountIn must round-trip");
        // amountOutMin == floor(1000 * 9950/10000) == 995
        assert_eq!(
            decoded.amountOutMin,
            U256::from(995u64),
            "amountOutMin must be 995 at 50bps"
        );
        // path[last] must be WETH (resolved from Native).
        assert_eq!(
            decoded.path,
            vec![
                crate::execution::protocols::common::evm_addr(&a),
                crate::execution::protocols::common::evm_addr(&w)
            ],
            "path must be [input, weth]"
        );
        assert_eq!(decoded.to, recipient, "recipient must round-trip");
        assert_eq!(
            decoded.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );
    }

    // ─── native-in + native-out → NativeMismatch ────────────────────────────

    #[test]
    fn native_in_native_out_returns_native_mismatch() {
        // Pool must contain WETH on both sides — use weth() for both just to pass
        // the asset-membership check.  The NativeMismatch guard fires first in
        // the match, but the pool needs to be valid to reach it.
        let w = weth();
        // Build a pool with the same asset on both sides would fail the `input != output`
        // check; use weth() as input and any other token as the second slot but pass
        // Native for both currencies so the guard fires before the pool check.
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
        assert_eq!(
            err,
            BuildError::NativeMismatch,
            "must be NativeMismatch, got: {err:?}"
        );
    }
}
