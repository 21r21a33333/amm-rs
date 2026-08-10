//! Aerodrome (Solidly-style) swap encoder: exact-in only → [`PreparedSwap`].
//!
//! Both [`AerodromeStablePool`] and [`AerodromeVolatilePool`] share a single
//! private encoder [`build_solidly`] that dispatches over
//! `(native_in, native_out)` to one of three Aerodrome router entrypoints:
//!
//! - `swapExactETHForTokens` — native-in (ETH → token)
//! - `swapExactTokensForETH` — native-out (token → ETH)
//! - `swapExactTokensForTokens` — ERC-20 to ERC-20
//!
//! The Solidly lineage has **no exact-out** entrypoint, so
//! [`build_swap_exact_out`] returns [`BuildError::UnsupportedProtocol`] for
//! both pool types.
//!
//! Each swap encodes a single-element `Route[]` carrying the `stable` flag
//! derived from the concrete pool type (`true` for stable, `false` for
//! volatile) and the configured factory address.

use alloy::primitives::U256;
use alloy::{sol, sol_types::SolCall};
use amm_core::primitives::asset::AssetAmount;
use amm_core::protocols::aerodrome::stable::AerodromeStablePool;
use amm_core::protocols::aerodrome::volatile::AerodromeVolatilePool;
use amm_core::traits::pool::Pool;

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
    /// Minimal ABI surface for the Aerodrome (Solidly) Router.
    ///
    /// Uses a `Route[]` struct instead of a plain address path. The `stable`
    /// flag selects the pool invariant; `factory` is the Aerodrome pool factory.
    interface IAerodromeRouter {
        /// A single hop in the Aerodrome route: token pair, curve type, factory.
        struct Route {
            address from;
            address to;
            bool stable;
            address factory;
        }

        /// Swap an exact amount of ERC-20 tokens for as many output tokens as possible.
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);

        /// Swap an exact amount of ETH for as many output tokens as possible.
        function swapExactETHForTokens(
            uint256 amountOutMin,
            Route[] routes,
            address to,
            uint256 deadline
        ) external payable returns (uint256[] amounts);

        /// Swap an exact amount of ERC-20 tokens for as much ETH as possible.
        function swapExactTokensForETH(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);
    }
}

/// Shared Aerodrome/Solidly encoder for both stable and volatile pools.
///
/// The `stable` parameter distinguishes the two invariants (`true` = x³y+y³x,
/// `false` = constant product) and is encoded directly into the `Route` struct
/// passed to the router.
///
/// # Errors
///
/// - [`BuildError::UnsupportedProtocol`] — `opts.price_limit` is `Some`
///   (Solidly has no `sqrtPriceLimit` bound) or `route.trade_type` is
///   `ExactOut`.
/// - [`BuildError::NativeMismatch`] — both input and output are native.
/// - [`BuildError::MissingChainConfig`] — Aerodrome router or factory not
///   configured on `ctx`.
/// - Propagated from [`common::resolve_swap`]: [`BuildError::UnresolvedRecipient`],
///   [`BuildError::UnresolvedDeadline`], [`BuildError::AssetNotInPool`].
#[allow(clippy::too_many_arguments)]
fn build_solidly(
    pool: &dyn Pool,
    stable: bool,
    ctx: &ChainConfig,
    amount_in: CurrencyAmount,
    to: Currency,
    route: &Route,
    quoted_out: &AssetAmount,
    opts: &ExecutionOptions,
) -> Result<PreparedSwap, BuildError> {
    // Solidly has no price-bound mechanism.
    if opts.price_limit.is_some() {
        return Err(BuildError::UnsupportedProtocol);
    }

    let r = common::resolve_swap(
        ctx,
        pool,
        amount_in.currency,
        to,
        route,
        opts,
        TradeType::ExactIn,
    )?;
    let router = ctx.router_aerodrome()?;
    let factory = ctx.aerodrome_factory()?;
    let min = opts.slippage.min_amount_out(quoted_out);

    let routes = vec![IAerodromeRouter::Route {
        from: common::evm_addr(&r.input),
        to: common::evm_addr(&r.output),
        stable,
        factory,
    }];

    match (r.native_in, r.native_out) {
        // Native → Native is always a configuration error.
        (true, true) => Err(BuildError::NativeMismatch),

        // Native-in (ETH → token): value = amount_in.raw, no ERC-20 approval.
        (true, false) => {
            let call = IAerodromeRouter::swapExactETHForTokensCall {
                amountOutMin: min.raw,
                routes,
                to: r.recipient,
                deadline: r.deadline,
            };
            Ok(common::prepared(
                ctx,
                router,
                call.abi_encode().into(),
                amount_in.raw,
                min,
                None,
                None,
            ))
        }

        // Native-out (token → ETH): value = 0, ERC-20 approval on input.
        (false, true) => {
            let call = IAerodromeRouter::swapExactTokensForETHCall {
                amountIn: amount_in.raw,
                amountOutMin: min.raw,
                routes,
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

        // ERC-20 → ERC-20: value = 0, ERC-20 approval on input.
        (false, false) => {
            let call = IAerodromeRouter::swapExactTokensForTokensCall {
                amountIn: amount_in.raw,
                amountOutMin: min.raw,
                routes,
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

impl Sealed for AerodromeStablePool {}

impl Executable for AerodromeStablePool {
    /// Build an exact-in swap through the Aerodrome stable pool (`stable = true`).
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        route: &Route,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        build_solidly(self, true, ctx, amount_in, to, route, quoted_out, opts)
    }

    /// Solidly has no exact-out entrypoint — always returns
    /// [`BuildError::UnsupportedProtocol`].
    fn build_swap_exact_out(
        &self,
        _ctx: &ChainConfig,
        _amount_out: CurrencyAmount,
        _from: Currency,
        _route: &Route,
        _quoted_in: &AssetAmount,
        _opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        Err(BuildError::UnsupportedProtocol)
    }
}

impl Sealed for AerodromeVolatilePool {}

impl Executable for AerodromeVolatilePool {
    /// Build an exact-in swap through the Aerodrome volatile pool (`stable = false`).
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        route: &Route,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        build_solidly(self, false, ctx, amount_in, to, route, quoted_out, opts)
    }

    /// Solidly has no exact-out entrypoint — always returns
    /// [`BuildError::UnsupportedProtocol`].
    fn build_swap_exact_out(
        &self,
        _ctx: &ChainConfig,
        _amount_out: CurrencyAmount,
        _from: Currency,
        _route: &Route,
        _quoted_in: &AssetAmount,
        _opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        Err(BuildError::UnsupportedProtocol)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, U256, address};
    use alloy::sol_types::SolCall;
    use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::primitives::ratio::Bps;
    use amm_core::protocols::aerodrome::BASE_POOL_FACTORY;
    use amm_core::protocols::aerodrome::stable::AerodromeStablePool;
    use amm_core::protocols::aerodrome::volatile::AerodromeVolatilePool;
    use amm_core::slippage::Slippage;
    use amm_core::traits::pool::Pool;

    use super::IAerodromeRouter;
    use crate::execution::{
        config::{ChainConfig, Routers},
        error::BuildError,
        executable::Executable,
        options::{Deadline, ExecutionOptions, Recipient},
        prepared::Route,
        types::{Currency, CurrencyAmount, TradeType},
    };

    // ─── test fixtures ───────────────────────────────────────────────────────

    /// Canonical Aerodrome router address on Base.
    const ROUTER: Address = address!("0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");
    /// Re-export the shared factory constant for assertions.
    const FACTORY: Address = BASE_POOL_FACTORY;

    fn chain_id() -> ChainId {
        ChainId(8453)
    }

    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    /// WETH on Base: 0x4200000000000000000000000000000000000006
    fn weth_base() -> AssetId {
        AssetId::new(
            chain_id(),
            B256::left_padding_from(&[
                0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x06,
            ]),
        )
    }

    /// Minimal Base chain config with Aerodrome router and factory set.
    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth_base()).with_routers(Routers {
            aerodrome: Some(ROUTER),
            aerodrome_factory: Some(FACTORY),
            ..Default::default()
        })
    }

    /// Build an Aerodrome stable pool over `a` and `b`.
    fn stable_pool(a: AssetId, b: AssetId) -> AerodromeStablePool {
        AerodromeStablePool::new(
            PoolId::new("8453:aerodrome-stable:0x"),
            [a, b],
            [U256::from(1_000_000u64); 2],
            [6, 18],
            5,
        )
    }

    /// Build an Aerodrome volatile pool over `a` and `b`.
    fn volatile_pool(a: AssetId, b: AssetId) -> AerodromeVolatilePool {
        AerodromeVolatilePool::new(
            PoolId::new("8453:aerodrome-volatile:0x"),
            [a, b],
            [U256::from(1_000_000u64); 2],
            30,
        )
    }

    /// Fully-resolved execution options.
    fn opts_resolved(to: Address, deadline_ts: u64, slippage_bps: u16) -> ExecutionOptions {
        ExecutionOptions::new(Slippage::from_bps(Bps(slippage_bps)))
            .with_recipient(Recipient::To(to))
            .with_deadline(Deadline::AtTimestamp(deadline_ts))
    }

    // ─── shared helper: ERC-20 exact-in assertions ───────────────────────────

    /// Run an ERC-20 exact-in build and decode the calldata, asserting all
    /// fields are correct for the given `stable_expected` flag.
    fn check_exact_in(pool: &dyn Executable, stable_expected: bool) {
        let a = asset(0x01);
        let b = asset(0x02);
        let c = ctx();

        let recipient = Address::repeat_byte(0x55);
        let deadline_ts = 1_700_000_000u64;
        // 100 bps slippage: quoted_out 1000 → min = floor(1000 * 9900/10000) = 990
        let opts = opts_resolved(recipient, deadline_ts, 100);

        let amount_in_raw = U256::from(500u64);
        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: amount_in_raw,
        };
        let quoted_out = AssetAmount::new(b, U256::from(1000u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactIn);

        let prepared = pool
            .build_swap(
                &c,
                amount_in,
                Currency::Token(b),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("build_swap must succeed for ERC-20 exact-in");

        // tx fields
        assert_eq!(prepared.tx.to, ROUTER, "tx.to must be Aerodrome router");
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for ERC-20"
        );
        assert_eq!(prepared.tx.chain, c.chain, "tx.chain must match ctx.chain");

        // Decode calldata as swapExactTokensForTokens
        let decoded = IAerodromeRouter::swapExactTokensForTokensCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as swapExactTokensForTokens");

        assert_eq!(
            decoded.amountIn, amount_in_raw,
            "amountIn must equal amount_in.raw"
        );
        // amountOutMin == floor(1000 * 9900/10000) = 990
        assert_eq!(
            decoded.amountOutMin,
            U256::from(990u64),
            "amountOutMin must be 990 at 100bps"
        );
        assert_eq!(decoded.to, recipient, "recipient must round-trip");
        assert_eq!(
            decoded.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );

        // Route assertions
        assert_eq!(decoded.routes.len(), 1, "routes.len() must be 1");
        let r = &decoded.routes[0];
        assert_eq!(
            r.stable, stable_expected,
            "routes[0].stable must match pool type"
        );
        assert_eq!(
            r.factory, FACTORY,
            "routes[0].factory must be the configured factory"
        );
        assert_eq!(
            r.from,
            crate::execution::protocols::common::evm_addr(&a),
            "routes[0].from must be input asset address"
        );
        assert_eq!(
            r.to,
            crate::execution::protocols::common::evm_addr(&b),
            "routes[0].to must be output asset address"
        );

        // min_received
        assert_eq!(prepared.min_received.raw, U256::from(990u64));
        assert_eq!(prepared.min_received.asset, b);

        // max_spent == None (exact-in)
        assert!(
            prepared.max_spent.is_none(),
            "max_spent must be None for exact-in"
        );

        // Approval
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender, ROUTER,
            "approval.spender must be the router"
        );
        assert_eq!(approval.token, a, "approval.token must be the input asset");
        assert_eq!(
            approval.min_allowance, amount_in_raw,
            "approval.min_allowance must equal amount_in.raw"
        );
        assert!(!approval.reset_first, "reset_first must be false");
    }

    // ─── ERC-20 exact-in: stable pool (stable = true) ───────────────────────

    #[test]
    fn stable_exact_in_erc20_encodes_correctly() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = stable_pool(a, b);
        check_exact_in(&p, true);
    }

    // ─── ERC-20 exact-in: volatile pool (stable = false) ────────────────────

    #[test]
    fn volatile_exact_in_erc20_encodes_correctly() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = volatile_pool(a, b);
        check_exact_in(&p, false);
    }

    // ─── native-in (ETH → token): stable pool ───────────────────────────────

    #[test]
    fn stable_native_in_erc20_out_encodes_swap_exact_eth_for_tokens() {
        let b = asset(0x02);
        let w = weth_base();
        let p = stable_pool(w, b);
        let c = ctx();

        let recipient = Address::repeat_byte(0x77);
        let deadline_ts = 1_700_000_000u64;
        // 200 bps: quoted_out 1000 → min = floor(1000 * 9800/10000) = 980
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

        // tx.value carries the ETH amount
        assert_eq!(
            prepared.tx.value, amount_in_raw,
            "tx.value must equal amount_in.raw for native-in"
        );

        // No ERC-20 approval for native-in
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in"
        );

        // Decode calldata as swapExactETHForTokens
        let decoded = IAerodromeRouter::swapExactETHForTokensCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as swapExactETHForTokens");

        // amountOutMin == floor(1000 * 9800/10000) = 980
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

        // Route
        assert_eq!(decoded.routes.len(), 1);
        let r = &decoded.routes[0];
        assert!(r.stable, "route must be stable for AerodromeStablePool");
        assert_eq!(r.factory, FACTORY);
        assert_eq!(r.from, crate::execution::protocols::common::evm_addr(&w));
        assert_eq!(r.to, crate::execution::protocols::common::evm_addr(&b));
    }

    // ─── native-out (token → ETH): volatile pool ────────────────────────────

    #[test]
    fn volatile_erc20_in_native_out_encodes_swap_exact_tokens_for_eth() {
        let a = asset(0x01);
        let w = weth_base();
        let p = volatile_pool(a, w);
        let c = ctx();

        let recipient = Address::repeat_byte(0x88);
        let deadline_ts = 1_800_000_000u64;
        // 50 bps: quoted_out 1000 → min = floor(1000 * 9950/10000) = 995
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

        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for token-in"
        );

        // Approval required for ERC-20 input
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(approval.spender, ROUTER);
        assert_eq!(approval.token, a);
        assert_eq!(approval.min_allowance, amount_in_raw);

        // Decode calldata as swapExactTokensForETH
        let decoded = IAerodromeRouter::swapExactTokensForETHCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as swapExactTokensForETH");

        assert_eq!(decoded.amountIn, amount_in_raw, "amountIn must round-trip");
        // amountOutMin == floor(1000 * 9950/10000) = 995
        assert_eq!(
            decoded.amountOutMin,
            U256::from(995u64),
            "amountOutMin must be 995 at 50bps"
        );
        assert_eq!(decoded.to, recipient, "recipient must round-trip");
        assert_eq!(
            decoded.deadline,
            U256::from(deadline_ts),
            "deadline must round-trip"
        );

        // Route
        assert_eq!(decoded.routes.len(), 1);
        let r = &decoded.routes[0];
        assert!(
            !r.stable,
            "route must be volatile for AerodromeVolatilePool"
        );
        assert_eq!(r.factory, FACTORY);
        assert_eq!(r.from, crate::execution::protocols::common::evm_addr(&a));
        assert_eq!(r.to, crate::execution::protocols::common::evm_addr(&w));
    }

    // ─── exact_out returns UnsupportedProtocol (both pool types) ────────────

    #[test]
    fn stable_exact_out_returns_unsupported_protocol() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = stable_pool(a, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_out = CurrencyAmount {
            currency: Currency::Token(b),
            raw: U256::from(100u64),
        };
        let quoted_in = AssetAmount::new(a, U256::from(100u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactOut);

        let err = p
            .build_swap_exact_out(
                &c,
                amount_out,
                Currency::Token(a),
                &route,
                &quoted_in,
                &opts,
            )
            .expect_err("stable build_swap_exact_out must return error");
        assert_eq!(
            err,
            BuildError::UnsupportedProtocol,
            "must be UnsupportedProtocol"
        );
    }

    #[test]
    fn volatile_exact_out_returns_unsupported_protocol() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = volatile_pool(a, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_out = CurrencyAmount {
            currency: Currency::Token(b),
            raw: U256::from(100u64),
        };
        let quoted_in = AssetAmount::new(a, U256::from(100u64));
        let route = Route::new_single_hop(a, b, TradeType::ExactOut);

        let err = p
            .build_swap_exact_out(
                &c,
                amount_out,
                Currency::Token(a),
                &route,
                &quoted_in,
                &opts,
            )
            .expect_err("volatile build_swap_exact_out must return error");
        assert_eq!(
            err,
            BuildError::UnsupportedProtocol,
            "must be UnsupportedProtocol"
        );
    }

    // ─── selector sanity ────────────────────────────────────────────────────

    /// Verify the `swapExactTokensForTokens` selector against the known value.
    #[test]
    fn selectors_are_correct() {
        assert_eq!(
            IAerodromeRouter::swapExactTokensForTokensCall::SELECTOR,
            [0xca, 0xc8, 0x8e, 0xa9],
            "swapExactTokensForTokens selector must be 0xcac88ea9"
        );
    }

    // ─── guard: price_limit → UnsupportedProtocol ───────────────────────────

    #[test]
    fn price_limit_some_returns_unsupported_protocol() {
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = stable_pool(a, b);
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
            .expect_err("price_limit Some must return error");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ─── guard: Recipient::Sender → UnresolvedRecipient ─────────────────────

    #[test]
    fn unresolved_sender_returns_unresolved_recipient() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = volatile_pool(a, b);
        let c = ctx();

        // Default Recipient is Sender (unresolved)
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

    // ─── guard: Deadline::FromNow → UnresolvedDeadline ──────────────────────

    #[test]
    fn from_now_deadline_returns_unresolved_deadline() {
        use std::time::Duration;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = stable_pool(a, b);
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

    // ─── guard: wrong trade type → UnsupportedProtocol ──────────────────────

    #[test]
    fn exact_out_route_in_build_swap_returns_unsupported_protocol() {
        let a = asset(0x01);
        let b = asset(0x02);
        let p = volatile_pool(a, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(b, U256::from(100u64));
        // Wrong trade type: ExactOut passed to build_swap
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

    // ─── guard: both native → NativeMismatch ────────────────────────────────

    #[test]
    fn native_in_native_out_returns_native_mismatch() {
        let w = weth_base();
        let b = asset(0x02);
        let p = volatile_pool(w, b);
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

    // ─── guard: wrong assets → AssetNotInPool ───────────────────────────────

    #[test]
    fn wrong_assets_returns_asset_not_in_pool() {
        let a = asset(0x01);
        let b = asset(0x02);
        let unknown = asset(0x99);
        let p = stable_pool(a, b);
        let c = ctx();

        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);
        let amount_in = CurrencyAmount {
            currency: Currency::Token(a),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(unknown, U256::from(100u64));
        let route = Route::new_single_hop(a, unknown, TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(unknown),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("unknown output must fail");
        assert!(matches!(err, BuildError::AssetNotInPool { .. }));
    }

    // ─── as_executable dispatch ──────────────────────────────────────────────

    /// `as_executable` must return `Some` for both Aerodrome pool types.
    #[test]
    fn as_executable_is_some_for_stable_pool() {
        use crate::execution::executable::as_executable;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = stable_pool(a, b);
        assert!(as_executable(&p as &dyn Pool).is_some());
    }

    #[test]
    fn as_executable_is_some_for_volatile_pool() {
        use crate::execution::executable::as_executable;

        let a = asset(0x01);
        let b = asset(0x02);
        let p = volatile_pool(a, b);
        assert!(as_executable(&p as &dyn Pool).is_some());
    }
}
