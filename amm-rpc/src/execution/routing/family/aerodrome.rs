//! Aerodrome (Solidly-style) span builder: exact-in multi-hop → [`PreparedSwap`].
//!
//! The Aerodrome router is natively multi-hop: its `Route[]` struct carries one
//! entry per hop, so a 2+ pool span becomes a single router call with a longer
//! `Route[]`. This module turns a same-router [`RouterSpan`] of Aerodrome pools
//! into one encoded router transaction, mirroring how [`super::uniswap_ur`] handles
//! the Uniswap Universal Router.
//!
//! ## Encoding
//! - Each hop `i` in `span.pools` contributes one `Route { from: path[i], to:
//!   path[i+1], stable: <pool kind>, factory: ctx.aerodrome_factory() }`.
//! - `amountOutMin` guards only the **final** output; intermediate amounts are
//!   unconstrained (the span executes atomically, so only the terminal floor
//!   matters).
//! - Native dispatch:
//!   - `(false, false)` → `swapExactTokensForTokens`, `tx.value = 0`, ERC-20 approval on span input.
//!   - `(true, false)`  → `swapExactETHForTokens`, `tx.value = amount_in`, no approval.
//!   - `(false, true)`  → `swapExactTokensForETH`, `tx.value = 0`, ERC-20 approval on span input.
//!   - `(true, true)`   → [`BuildError::NativeMismatch`] (mirrors single-hop).
//!
//! ## Exact-out
//! The Solidly lineage has **no exact-out** entrypoint; [`build_aerodrome_span_exact_out`]
//! always returns [`BuildError::UnsupportedProtocol`].

use alloy::primitives::{Address, U256};
use alloy::{sol, sol_types::SolCall};
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::PoolKind;

use crate::execution::config::ChainConfig;
use crate::execution::error::BuildError;
use crate::execution::prepared::PreparedSwap;
use crate::execution::protocols::common;
use crate::execution::routing::{Route, RouterSpan};
use crate::execution::types::UnsignedTx;

sol! {
    /// Minimal ABI surface for the Aerodrome (Solidly) Router — redeclared here
    /// to decouple the span builder from the single-hop protocol module.
    ///
    /// Encoding is identical to the declaration in `protocols/aerodrome.rs`; the
    /// two `sol!` blocks produce distinct Rust types, so they cannot be unified
    /// without making one public-to-crate. Re-declaring here is the cleanest
    /// boundary: the span builder owns its own ABI surface.
    interface IAerodromeRouterSpan {
        /// One hop in a multi-step Aerodrome route.
        struct Route {
            address from;
            address to;
            bool stable;
            address factory;
        }

        /// Swap exact ERC-20 tokens for as many output tokens as possible.
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);

        /// Swap exact ETH for as many output tokens as possible (payable).
        function swapExactETHForTokens(
            uint256 amountOutMin,
            Route[] routes,
            address to,
            uint256 deadline
        ) external payable returns (uint256[] amounts);

        /// Swap exact ERC-20 tokens for as much ETH as possible.
        function swapExactTokensForETH(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);
    }
}

/// Build one Aerodrome router transaction for a same-router Aerodrome span (exact-in).
///
/// Turns every pool index in `span.pools` into one `Route` entry, producing a
/// single `swapExact*For*` call with a multi-element `Route[]`. The Aerodrome
/// router handles the chaining natively — no intermediate transfers are needed.
///
/// `amount_in` is the raw amount of the span's input token (or ETH). `min_out`
/// bounds only the final output (`amountOutMin` in the ABI); intermediate
/// amounts are unconstrained because the whole span is atomic.
///
/// `native_in`/`native_out` select among the three Solidly entrypoints;
/// `(true, true)` returns [`BuildError::NativeMismatch`].
///
/// `deadline` is the absolute Unix timestamp passed to the router. `is_final`
/// is accepted for signature parity with sibling builders but is not branched on
/// (delivery is fully determined by `recipient`).
///
/// # Errors
/// - [`BuildError::NativeMismatch`]  — both `native_in` and `native_out` are `true`.
/// - [`BuildError::MissingChainConfig`] — Aerodrome router or factory not set.
/// - [`BuildError::UnsupportedProtocol`] — a pool along the span has no `Introspect`.
//
// `dead_code`: entry point used by the plan dispatcher. Exercised by this
// module's tests.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_aerodrome_span(
    ctx: &ChainConfig,
    route: &Route<'_>,
    span: &RouterSpan,
    amount_in: U256,
    min_out: U256,
    recipient: Address,
    native_in: bool,
    native_out: bool,
    deadline: u64,
    _is_final: bool,
) -> Result<PreparedSwap, BuildError> {
    // Solidly has no native→native path.
    if native_in && native_out {
        return Err(BuildError::NativeMismatch);
    }

    let start = span.pools.start;
    let end = span.pools.end;

    // partition() guarantees well-formed spans; assert the trust boundary.
    debug_assert!(span.pools.end <= route.pools.len() && span.pools.end < route.path.len());

    let router = ctx.router_aerodrome()?;
    let factory = ctx.aerodrome_factory()?;

    // Build one Route per hop. `stable` is derived from the pool's PoolKind;
    // `factory` is uniform across all Aerodrome pools on a given chain.
    let routes: Vec<IAerodromeRouterSpan::Route> = span
        .pools
        .clone()
        .map(|i| {
            let pool = route.pools[i];
            let stable = pool
                .as_introspect()
                .ok_or(BuildError::UnsupportedProtocol)?
                .kind()
                == PoolKind::AerodromeStable;
            Ok(IAerodromeRouterSpan::Route {
                from: common::evm_addr(&route.path[i]),
                to: common::evm_addr(&route.path[i + 1]),
                stable,
                factory,
            })
        })
        .collect::<Result<_, BuildError>>()?;

    // The span's input asset feeds the approval (ERC-20 input only).
    let seg_input = route.path[start];
    // The span's output asset labels `min_received`.
    let seg_output = route.path[end];

    let deadline_u256 = U256::from(deadline);

    match (native_in, native_out) {
        // Both native is a config error — caught above; this arm is unreachable.
        (true, true) => Err(BuildError::NativeMismatch),

        // Native-in (ETH → token): value = amount_in, no ERC-20 approval.
        (true, false) => {
            let call = IAerodromeRouterSpan::swapExactETHForTokensCall {
                amountOutMin: min_out,
                routes,
                to: recipient,
                deadline: deadline_u256,
            };
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data: call.abi_encode().into(),
                value: amount_in,
            };
            Ok(PreparedSwap {
                tx,
                min_received: AssetAmount::new(seg_output, min_out),
                max_spent: None,
                approval: None,
                price_impact: None,
            })
        }

        // Native-out (token → ETH): value = 0, ERC-20 approval on span input.
        (false, true) => {
            let call = IAerodromeRouterSpan::swapExactTokensForETHCall {
                amountIn: amount_in,
                amountOutMin: min_out,
                routes,
                to: recipient,
                deadline: deadline_u256,
            };
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data: call.abi_encode().into(),
                value: U256::ZERO,
            };
            Ok(PreparedSwap {
                tx,
                // native-out: report min_received as the chain native asset (B256::ZERO).
                min_received: AssetAmount::new(
                    AssetId::new(ChainId(ctx.chain.0), alloy::primitives::B256::ZERO),
                    min_out,
                ),
                max_spent: None,
                approval: Some(common::erc20_approval(router, seg_input, amount_in)),
                price_impact: None,
            })
        }

        // ERC-20 → ERC-20: value = 0, ERC-20 approval on span input.
        (false, false) => {
            let call = IAerodromeRouterSpan::swapExactTokensForTokensCall {
                amountIn: amount_in,
                amountOutMin: min_out,
                routes,
                to: recipient,
                deadline: deadline_u256,
            };
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data: call.abi_encode().into(),
                value: U256::ZERO,
            };
            Ok(PreparedSwap {
                tx,
                min_received: AssetAmount::new(seg_output, min_out),
                max_spent: None,
                approval: Some(common::erc20_approval(router, seg_input, amount_in)),
                price_impact: None,
            })
        }
    }
}

/// Solidly has no exact-out entrypoint; always returns [`BuildError::UnsupportedProtocol`].
///
/// Accepted for signature parity with [`super::uniswap_ur::build_uniswap_span_exact_out`];
/// callers that support exact-out must not route Aerodrome spans through this builder.
//
// `dead_code`: present for API completeness; plan.rs exact-out is stubbed (Task 9).
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_aerodrome_span_exact_out(
    _ctx: &ChainConfig,
    _route: &Route<'_>,
    _span: &RouterSpan,
    _amount_out: U256,
    _amount_in_max: U256,
    _recipient: Address,
    _native_in: bool,
    _native_out: bool,
    _deadline: u64,
    _is_final: bool,
) -> Result<PreparedSwap, BuildError> {
    Err(BuildError::UnsupportedProtocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy::primitives::{Address, B256, U256, address};
    use alloy::sol_types::SolCall;
    use amm_core::primitives::asset::{AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::protocols::aerodrome::BASE_POOL_FACTORY;
    use amm_core::protocols::aerodrome::stable::AerodromeStablePool;
    use amm_core::protocols::aerodrome::volatile::AerodromeVolatilePool;

    use crate::execution::config::{ChainConfig, Routers};
    use crate::execution::routing::RouterKind;
    use crate::execution::types::TradeType;

    // ── Fixtures ─────────────────────────────────────────────────────────────

    /// Canonical Aerodrome router address on Base.
    const ROUTER: Address = address!("0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");
    /// Aerodrome factory (BASE_POOL_FACTORY).
    const FACTORY: Address = BASE_POOL_FACTORY;

    fn chain_id() -> ChainId {
        ChainId(8453)
    }

    /// AssetId from a single discriminant byte (address = left-padded).
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

    /// Minimal Base chain config with Aerodrome router and factory.
    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth_base()).with_routers(Routers {
            aerodrome: Some(ROUTER),
            aerodrome_factory: Some(FACTORY),
            ..Default::default()
        })
    }

    /// Volatile Aerodrome pool over `a` and `b`, fee 30 bps.
    fn vol_pool(a: AssetId, b: AssetId) -> AerodromeVolatilePool {
        // Pool assets must be address-sorted; the AerodromeVolatilePool constructor
        // stores them in the order given, so we sort here.
        let (lo, hi) = if common::evm_addr(&a) < common::evm_addr(&b) {
            (a, b)
        } else {
            (b, a)
        };
        AerodromeVolatilePool::new(
            PoolId::new("8453:aerodrome-volatile:0x"),
            [lo, hi],
            [U256::from(1_000_000_000_000_000_000u128); 2],
            30,
        )
    }

    /// Stable Aerodrome pool over `a` and `b`.
    fn stable_pool(a: AssetId, b: AssetId) -> AerodromeStablePool {
        let (lo, hi) = if common::evm_addr(&a) < common::evm_addr(&b) {
            (a, b)
        } else {
            (b, a)
        };
        AerodromeStablePool::new(
            PoolId::new("8453:aerodrome-stable:0x"),
            [lo, hi],
            [U256::from(1_000_000u64); 2],
            [6, 18],
            5,
        )
    }

    // ── Test: 2-hop volatile route → swapExactTokensForTokens ───────────────

    /// A 2-hop all-volatile Aerodrome span: decode `swapExactTokensForTokens`,
    /// assert `Route[]` has 2 entries with correct fields, `amountOutMin == min_out`,
    /// `to == recipient`, `tx.to == router`.
    #[test]
    fn two_hop_volatile_encodes_swap_exact_tokens_for_tokens() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = vol_pool(a, b);
        let p2 = vol_pool(b, c);
        let route = Route {
            pools: vec![&p1 as &dyn amm_core::traits::pool::Pool, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Aerodrome,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x55);
        let amount_in = U256::from(1_000u64);
        let min_out = U256::from(900u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_aerodrome_span(
            &ctx(),
            &route,
            &span,
            amount_in,
            min_out,
            recipient,
            false,
            false,
            deadline,
            true,
        )
        .expect("2-hop volatile span must build");

        // Transaction envelope.
        assert_eq!(prepared.tx.to, ROUTER, "tx.to must be Aerodrome router");
        assert_eq!(prepared.tx.value, U256::ZERO, "ERC-20 path has no value");
        assert_eq!(prepared.tx.chain, chain_id());

        // Decode calldata as swapExactTokensForTokens.
        let decoded =
            IAerodromeRouterSpan::swapExactTokensForTokensCall::abi_decode(&prepared.tx.data)
                .expect("calldata must decode as swapExactTokensForTokens");

        assert_eq!(decoded.amountIn, amount_in, "amountIn must round-trip");
        assert_eq!(
            decoded.amountOutMin, min_out,
            "amountOutMin must be min_out"
        );
        assert_eq!(decoded.to, recipient, "recipient must round-trip");
        assert_eq!(
            decoded.deadline,
            U256::from(deadline),
            "deadline must round-trip"
        );

        // Route[] must have 2 entries.
        assert_eq!(decoded.routes.len(), 2, "Route[] must have 2 entries");

        let r0 = &decoded.routes[0];
        assert_eq!(
            r0.from,
            common::evm_addr(&a),
            "routes[0].from must be asset a"
        );
        assert_eq!(r0.to, common::evm_addr(&b), "routes[0].to must be asset b");
        assert!(!r0.stable, "volatile pool → routes[0].stable must be false");
        assert_eq!(r0.factory, FACTORY, "routes[0].factory must match");

        let r1 = &decoded.routes[1];
        assert_eq!(
            r1.from,
            common::evm_addr(&b),
            "routes[1].from must be asset b"
        );
        assert_eq!(r1.to, common::evm_addr(&c), "routes[1].to must be asset c");
        assert!(!r1.stable, "volatile pool → routes[1].stable must be false");
        assert_eq!(r1.factory, FACTORY, "routes[1].factory must match");

        // PreparedSwap fields.
        assert_eq!(
            prepared.min_received.raw, min_out,
            "min_received.raw == min_out"
        );
        assert_eq!(
            prepared.min_received.asset, c,
            "min_received.asset is output"
        );
        assert!(prepared.max_spent.is_none(), "exact-in has no max_spent");

        // ERC-20 approval on span input (token a) for the router.
        let approval = prepared.approval.expect("ERC-20 input must have approval");
        assert_eq!(approval.spender, ROUTER, "spender must be the router");
        assert_eq!(approval.token, a, "approval.token must be span input a");
        assert_eq!(
            approval.min_allowance, amount_in,
            "allowance must equal amount_in"
        );
        assert!(!approval.reset_first, "reset_first must be false");
    }

    // ── Test: 3-hop mixed stable/volatile → Route[] has 3 entries ───────────

    /// A 3-hop span mixing stable and volatile pools: asserts all 3 Route entries
    /// have the correct `stable` flag derived from each pool's kind.
    #[test]
    fn three_hop_mixed_stable_volatile_encodes_correct_stable_flags() {
        // Use addresses that sort predictably: 0x11 < 0x22 < 0x33 < 0x44.
        let (a, b, c, d) = (asset(0x11), asset(0x22), asset(0x33), asset(0x44));
        let p1 = vol_pool(a, b); // volatile
        let p2 = stable_pool(b, c); // stable
        let p3 = vol_pool(c, d); // volatile
        let route = Route {
            pools: vec![&p1 as &dyn amm_core::traits::pool::Pool, &p2, &p3],
            path: vec![a, b, c, d],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Aerodrome,
            pools: 0..3,
        };

        let recipient = Address::repeat_byte(0x77);
        let amount_in = U256::from(5_000u64);
        let min_out = U256::from(4_200u64);
        let deadline = 42u64;

        let prepared = build_aerodrome_span(
            &ctx(),
            &route,
            &span,
            amount_in,
            min_out,
            recipient,
            false,
            false,
            deadline,
            true,
        )
        .expect("3-hop mixed span must build");

        let decoded =
            IAerodromeRouterSpan::swapExactTokensForTokensCall::abi_decode(&prepared.tx.data)
                .expect("calldata must decode as swapExactTokensForTokens");

        assert_eq!(decoded.routes.len(), 3, "Route[] must have 3 entries");
        assert!(
            !decoded.routes[0].stable,
            "hop 0 (volatile) → stable must be false"
        );
        assert!(
            decoded.routes[1].stable,
            "hop 1 (stable) → stable must be true"
        );
        assert!(
            !decoded.routes[2].stable,
            "hop 2 (volatile) → stable must be false"
        );

        // factory is uniform across all hops.
        for (i, r) in decoded.routes.iter().enumerate() {
            assert_eq!(r.factory, FACTORY, "routes[{i}].factory must match");
        }

        assert_eq!(
            decoded.amountOutMin, min_out,
            "amountOutMin bounds only the final output"
        );
        assert_eq!(decoded.to, recipient, "recipient must round-trip");
    }

    // ── Test: native-in (ETH → token) → swapExactETHForTokens ───────────────

    /// A native-in 2-hop span: `tx.value == amount_in`, `approval == None`, calldata
    /// decodes as `swapExactETHForTokens` with correct Route[] and `amountOutMin`.
    #[test]
    fn native_in_encodes_swap_exact_eth_for_tokens() {
        let w = weth_base();
        let b = asset(0x22);
        let c = asset(0x33);
        let p1 = vol_pool(w, b);
        let p2 = vol_pool(b, c);
        let route = Route {
            pools: vec![&p1 as &dyn amm_core::traits::pool::Pool, &p2],
            path: vec![w, b, c],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Aerodrome,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x88);
        let amount_in = U256::from(2_000u64);
        let min_out = U256::from(1_800u64);
        let deadline = 9_999_999u64;

        let prepared = build_aerodrome_span(
            &ctx(),
            &route,
            &span,
            amount_in,
            min_out,
            recipient,
            true,  // native_in
            false, // native_out
            deadline,
            true,
        )
        .expect("native-in span must build");

        // ETH attached as tx.value; no ERC-20 approval.
        assert_eq!(
            prepared.tx.value, amount_in,
            "tx.value must equal amount_in for native-in"
        );
        assert!(
            prepared.approval.is_none(),
            "native-in must have no approval"
        );

        // Decode as swapExactETHForTokens.
        let decoded =
            IAerodromeRouterSpan::swapExactETHForTokensCall::abi_decode(&prepared.tx.data)
                .expect("calldata must decode as swapExactETHForTokens");

        assert_eq!(
            decoded.amountOutMin, min_out,
            "amountOutMin must be min_out"
        );
        assert_eq!(decoded.to, recipient, "recipient must round-trip");
        assert_eq!(
            decoded.deadline,
            U256::from(deadline),
            "deadline must round-trip"
        );
        assert_eq!(decoded.routes.len(), 2, "Route[] must have 2 entries");

        assert_eq!(decoded.routes[0].from, common::evm_addr(&w));
        assert_eq!(decoded.routes[0].to, common::evm_addr(&b));
        assert_eq!(decoded.routes[1].from, common::evm_addr(&b));
        assert_eq!(decoded.routes[1].to, common::evm_addr(&c));
    }

    // ── Test: native-out (token → ETH) → swapExactTokensForETH ──────────────

    /// A native-out 2-hop span: `tx.value == 0`, `approval == Some` on span input,
    /// calldata decodes as `swapExactTokensForETH`.
    #[test]
    fn native_out_encodes_swap_exact_tokens_for_eth() {
        let a = asset(0x11);
        let b = asset(0x22);
        let w = weth_base();
        let p1 = vol_pool(a, b);
        let p2 = vol_pool(b, w);
        let route = Route {
            pools: vec![&p1 as &dyn amm_core::traits::pool::Pool, &p2],
            path: vec![a, b, w],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Aerodrome,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x99);
        let amount_in = U256::from(3_000u64);
        let min_out = U256::from(2_700u64);
        let deadline = 1_800_000_000u64;

        let prepared = build_aerodrome_span(
            &ctx(),
            &route,
            &span,
            amount_in,
            min_out,
            recipient,
            false, // native_in
            true,  // native_out
            deadline,
            true,
        )
        .expect("native-out span must build");

        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "token-in path has no ETH value"
        );

        // ERC-20 approval on span input token `a`.
        let approval = prepared
            .approval
            .expect("native-out must have approval on span input");
        assert_eq!(approval.spender, ROUTER, "spender must be the router");
        assert_eq!(approval.token, a, "approval.token must be span input a");
        assert_eq!(approval.min_allowance, amount_in, "allowance == amount_in");

        // Decode as swapExactTokensForETH.
        let decoded =
            IAerodromeRouterSpan::swapExactTokensForETHCall::abi_decode(&prepared.tx.data)
                .expect("calldata must decode as swapExactTokensForETH");

        assert_eq!(decoded.amountIn, amount_in, "amountIn must round-trip");
        assert_eq!(
            decoded.amountOutMin, min_out,
            "amountOutMin must be min_out"
        );
        assert_eq!(decoded.to, recipient, "recipient must round-trip");
        assert_eq!(
            decoded.deadline,
            U256::from(deadline),
            "deadline must round-trip"
        );
        assert_eq!(decoded.routes.len(), 2, "Route[] must have 2 entries");
    }

    // ── Test: (true, true) → NativeMismatch ─────────────────────────────────

    /// Both `native_in` and `native_out` → `BuildError::NativeMismatch`.
    #[test]
    fn native_in_and_native_out_returns_native_mismatch() {
        let w = weth_base();
        let b = asset(0x22);
        let p = vol_pool(w, b);
        let route = Route {
            pools: vec![&p as &dyn amm_core::traits::pool::Pool],
            path: vec![w, b],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Aerodrome,
            pools: 0..1,
        };

        let err = build_aerodrome_span(
            &ctx(),
            &route,
            &span,
            U256::from(100u64),
            U256::from(90u64),
            Address::repeat_byte(0x55),
            true,
            true,
            9_999_999u64,
            true,
        )
        .expect_err("native→native must return NativeMismatch");

        assert_eq!(err, BuildError::NativeMismatch);
    }

    // ── Test: exact-out → UnsupportedProtocol ───────────────────────────────

    /// `build_aerodrome_span_exact_out` always returns `UnsupportedProtocol`.
    #[test]
    fn exact_out_returns_unsupported_protocol() {
        let a = asset(0x11);
        let b = asset(0x22);
        let p = vol_pool(a, b);
        let route = Route {
            pools: vec![&p as &dyn amm_core::traits::pool::Pool],
            path: vec![a, b],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: RouterKind::Aerodrome,
            pools: 0..1,
        };

        let err = build_aerodrome_span_exact_out(
            &ctx(),
            &route,
            &span,
            U256::from(100u64),
            U256::from(120u64),
            Address::repeat_byte(0x55),
            false,
            false,
            9_999_999u64,
            true,
        )
        .expect_err("exact-out must return UnsupportedProtocol");

        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ── Test: selector sanity ────────────────────────────────────────────────

    /// Verify the `swapExactTokensForTokens` selector matches the expected 4-byte sig.
    #[test]
    fn selector_swap_exact_tokens_for_tokens_is_correct() {
        assert_eq!(
            IAerodromeRouterSpan::swapExactTokensForTokensCall::SELECTOR,
            [0xca, 0xc8, 0x8e, 0xa9],
            "swapExactTokensForTokens selector must be 0xcac88ea9"
        );
    }
}
