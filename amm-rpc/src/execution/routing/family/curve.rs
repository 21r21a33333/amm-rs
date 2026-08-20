//! Curve multi-pool span builder.
//!
//! Encodes one ≤5-pool [`RouterSpan`] of consecutive Curve pools into a single
//! atomic `CurveRouterNG.exchange(...)` transaction. Longer consecutive-Curve
//! runs are split into multiple ≤5-pool spans by
//! [`partition`](crate::execution::routing::partition) (via `CURVE_MAX_HOPS`)
//! and chained sequentially by the executor — this builder only ever sees a
//! span of 1..=5 pools.
//!
//! ## CurveRouterNG `exchange`
//!
//! ```text
//! exchange(
//!     address[11]   _route,        // [token0, pool0, token1, pool1, …, tokenN]
//!     uint256[5][5] _swap_params,  // per hop: [i, j, swap_type, pool_type, n_coins]
//!     uint256       _amount,
//!     uint256       _min_dy,
//!     address[5]    _pools,        // zeros for swap_type 1
//!     address       _receiver,
//! ) payable returns (uint256)
//! ```
//!
//! Selector `0xc872a3c5`, verified on the Ethereum deployment
//! `0x45312ea0eFf7E09C83CBE249fa1d7598c4C8cd4e`.
//!
//! ## `_swap_params` per hop
//!
//! - `i`, `j` — input/output coin indices ([`CurvePool::coin_indices`]).
//! - `swap_type = 1` — direct pool `exchange`. `exchange_underlying` and zap
//!   types are not emitted (deferred, matching the single-pool encoder).
//! - `pool_type` — derived from the pool's [`CurveInterface`] and coin count,
//!   distinguishing classic from `-NG` pools:
//!
//!   | interface            | n_coins | pool_type |
//!   |----------------------|---------|-----------|
//!   | `StableI128`         | any     | 1         |
//!   | `StableI128Ng`       | any     | 10        |
//!   | `CryptoU256UseEth`   | 2       | 2         |
//!   | `CryptoU256UseEth`   | ≥3      | 3         |
//!   | `CryptoU256Receiver` | 2       | 20        |
//!   | `CryptoU256Receiver` | ≥3      | 30        |
//!
//! - `n_coins = pool.assets().len()`.
//!
//! ## Recipient & native ETH
//!
//! Delivery uses the `_receiver` argument, so a distinct recipient is honored
//! atomically even for receiver-less pools (the router receives and forwards) —
//! unlike the single-pool encoder, which must reject a distinct recipient on
//! receiver-less ABIs.
//!
//! Native ETH is honored at a span endpoint only when that endpoint pool's
//! interface is `CryptoU256UseEth` (the only Curve interface that natively
//! handles ETH, matching the single-pool encoder). The endpoint's `_route`
//! slot then carries the ETH sentinel `0xEeee…EEeE`; native-in sends
//! `tx.value = amount_in` with no approval; native-out has the router unwrap and
//! pay the receiver ETH. Native at any other endpoint interface is
//! [`BuildError::UnsupportedProtocol`].

use core::any::Any;

use alloy::primitives::{Address, B256, U256, address};
use alloy::{sol, sol_types::SolCall};
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::protocols::curve::interface::CurveInterface;
use amm_core::protocols::curve::pool::CurvePool;
use amm_core::traits::pool::Pool;

use crate::execution::config::ChainConfig;
use crate::execution::error::BuildError;
use crate::execution::prepared::PreparedSwap;
use crate::execution::protocols::common;
use crate::execution::routing::router_span::CURVE_MAX_HOPS;
use crate::execution::routing::{Route, RouterSpan};

sol! {
    /// CurveRouterNG's atomic multi-hop `exchange` (the `_receiver` overload).
    ///
    /// Only the argument **types** determine the selector, so the parameter
    /// names here are cosmetic; the layout matches the on-chain v1.2 ABI.
    interface ICurveRouterNG {
        function exchange(
            address[11] route,
            uint256[5][5] swap_params,
            uint256 amount,
            uint256 min_dy,
            address[5] pools,
            address receiver
        ) external payable returns (uint256);
    }
}

/// Native-ETH sentinel used by CurveRouterNG in the `_route` array.
const ETH_SENTINEL: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

/// Downcast a `&dyn Pool` to `&CurvePool`.
///
/// The partition layer guarantees every pool in a `RouterKind::Curve` span is a
/// `CurvePool`, so failure here is a routing-invariant violation rather than
/// user error.
fn downcast_curve(pool: &dyn Pool) -> Result<&CurvePool, BuildError> {
    (pool as &dyn Any)
        .downcast_ref::<CurvePool>()
        .ok_or(BuildError::UnsupportedProtocol)
}

/// Map a Curve pool's `(interface, n_coins)` to the CurveRouterNG `pool_type`
/// code. See the module table.
///
/// `CurveInterface` is `#[non_exhaustive]`; an interface family this builder
/// does not know how to classify returns [`BuildError::UnsupportedProtocol`]
/// rather than guessing a `pool_type` (which would mis-route or revert).
fn curve_pool_type(interface: CurveInterface, n_coins: usize) -> Result<U256, BuildError> {
    Ok(match interface {
        CurveInterface::StableI128 => U256::from(1),
        CurveInterface::StableI128Ng => U256::from(10),
        CurveInterface::CryptoU256UseEth => U256::from(if n_coins == 2 { 2 } else { 3 }),
        CurveInterface::CryptoU256Receiver => U256::from(if n_coins == 2 { 20 } else { 30 }),
        _ => return Err(BuildError::UnsupportedProtocol),
    })
}

/// Require that a native-ETH endpoint pool is `CryptoU256UseEth` — the only
/// Curve interface that natively handles ETH through the router.
fn ensure_use_eth_endpoint(pool: &dyn Pool) -> Result<(), BuildError> {
    match downcast_curve(pool)?.interface() {
        Some(CurveInterface::CryptoU256UseEth) => Ok(()),
        _ => Err(BuildError::UnsupportedProtocol),
    }
}

/// The chain's native asset id (`token == B256::ZERO`), used to label
/// `min_received` on native-out.
fn native_asset(ctx: &ChainConfig) -> AssetId {
    AssetId::new(ChainId(ctx.chain.0), B256::ZERO)
}

/// Build the single atomic transaction for a multi-pool Curve span.
///
/// `deadline` and `is_final` are accepted for signature parity with the sibling
/// span builders; CurveRouterNG has no deadline argument and delivery is fully
/// determined by `_receiver`, so neither is branched on.
///
/// # Errors
/// - [`BuildError::NativeMismatch`] — both `native_in` and `native_out` are `true`.
/// - [`BuildError::MissingChainConfig`] — the Curve router address is unset.
/// - [`BuildError::UnsupportedProtocol`] — a pool in the span is not a
///   `CurvePool`, lacks an interface/address, or native ETH is requested at an
///   endpoint whose pool is not `CryptoU256UseEth`.
/// - [`BuildError::AssetNotInPool`] — a hop's coins are not both in its pool.
//
// `dead_code`: entry point used by the plan dispatcher; exercised by this
// module's tests and the fork matrix.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_curve_span(
    ctx: &ChainConfig,
    route: &Route<'_>,
    span: &RouterSpan,
    amount_in: U256,
    min_out: U256,
    recipient: Address,
    native_in: bool,
    native_out: bool,
    _deadline: u64,
    _is_final: bool,
) -> Result<PreparedSwap, BuildError> {
    if native_in && native_out {
        return Err(BuildError::NativeMismatch);
    }

    let start = span.pools.start;
    let end = span.pools.end;

    // partition() guarantees well-formed spans capped at CURVE_MAX_HOPS.
    debug_assert!(end <= route.pools.len() && end < route.path.len());
    let n_hops = end - start;
    debug_assert!((1..=CURVE_MAX_HOPS).contains(&n_hops));

    let router = ctx.router_curve()?;

    // Native ETH is only supported at an endpoint whose pool is CryptoU256UseEth.
    if native_in {
        ensure_use_eth_endpoint(route.pools[start])?;
    }
    if native_out {
        ensure_use_eth_endpoint(route.pools[end - 1])?;
    }

    // Assemble the fixed-size router arrays. `_pools` stays all-zero (swap_type 1).
    let mut route_arr = [Address::ZERO; 11];
    let mut swap_params = [[U256::ZERO; 5]; 5];

    for (k, pool_idx) in span.pools.clone().enumerate() {
        let pool = downcast_curve(route.pools[pool_idx])?;
        let interface = pool.interface().ok_or(BuildError::UnsupportedProtocol)?;
        let pool_addr = pool.pool_address().ok_or(BuildError::UnsupportedProtocol)?;

        let in_asset = route.path[start + k];
        let out_asset = route.path[start + k + 1];
        let (i, j) =
            pool.coin_indices(&in_asset, &out_asset)
                .ok_or(BuildError::AssetNotInPool {
                    input: in_asset,
                    output: out_asset,
                })?;
        let n_coins = pool.assets().len();

        // Endpoint slots carry the ETH sentinel when that side is native.
        let in_native = k == 0 && native_in;
        let out_native = k == n_hops - 1 && native_out;
        route_arr[2 * k] = endpoint_addr(in_native, &in_asset);
        route_arr[2 * k + 1] = pool_addr;
        route_arr[2 * k + 2] = endpoint_addr(out_native, &out_asset);

        swap_params[k] = [
            U256::from(i),
            U256::from(j),
            U256::from(1), // swap_type: direct `exchange`
            curve_pool_type(interface, n_coins)?,
            U256::from(n_coins),
        ];
    }

    let call = ICurveRouterNG::exchangeCall {
        route: route_arr,
        swap_params,
        amount: amount_in,
        min_dy: min_out,
        pools: [Address::ZERO; 5],
        receiver: recipient,
    };
    let data = call.abi_encode();

    // Input side: native-in sends value with no approval; otherwise the router
    // pulls the span input token via transferFrom under an ERC-20 approval.
    let seg_input = route.path[start];
    let (value, approval) = match native_in {
        true => (amount_in, None),
        false => (
            U256::ZERO,
            Some(common::erc20_approval(router, seg_input, amount_in)),
        ),
    };

    let min_received = match native_out {
        true => AssetAmount::new(native_asset(ctx), min_out),
        false => AssetAmount::new(route.path[end], min_out),
    };

    Ok(common::prepared(
        ctx,
        router,
        data.into(),
        value,
        min_received,
        None,
        approval,
    ))
}

/// Endpoint address for the `_route` array: the ETH sentinel when this side is
/// native, otherwise the asset's EVM address.
fn endpoint_addr(is_native: bool, asset: &AssetId) -> Address {
    match is_native {
        true => ETH_SENTINEL,
        false => common::evm_addr(asset),
    }
}

/// CurveRouterNG has no exact-out entrypoint — always
/// [`BuildError::UnsupportedProtocol`]. Accepted for signature parity with the
/// sibling `_exact_out` builders; the executor's Strict gate rejects Curve spans
/// before dispatch, and OrBetter degrades to exact-in.
//
// `dead_code`: present for API completeness; the executor never routes Curve
// spans through an exact-out builder.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_curve_span_exact_out(
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, address};
    use amm_core::primitives::asset::ChainId;
    use amm_core::primitives::pool::PoolId;
    use amm_core::protocols::curve::pool::CurvePool;
    use curve_math::Pool as CurveMathPool;

    use crate::execution::config::{ChainConfig, Routers};
    use crate::execution::types::TradeType;

    // ── constants ─────────────────────────────────────────────────────────────

    const ROUTER: Address = address!("45312ea0eFf7E09C83CBE249fa1d7598c4C8cd4e");
    const POOL_A: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"); // 3pool
    const POOL_B: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"); // tricrypto2

    fn chain() -> ChainId {
        ChainId(1)
    }
    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain(), B256::left_padding_from(&[byte]))
    }
    fn dai() -> AssetId {
        asset(0x01)
    }
    fn usdc() -> AssetId {
        asset(0x02)
    }
    fn usdt() -> AssetId {
        asset(0x03)
    }
    fn wbtc() -> AssetId {
        asset(0x04)
    }
    fn weth() -> AssetId {
        asset(0xC0)
    }

    // ── pool fixtures ─────────────────────────────────────────────────────────

    /// StableI128 3-coin pool DAI/USDC/USDT → pool_type 1.
    fn stable_pool() -> CurvePool {
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let bal = e18 * U256::from(1_000_000u64);
        let inner = CurveMathPool::StableSwapV1 {
            balances: vec![bal, bal, bal],
            rates: vec![e18, e18, e18],
            amp: U256::from(2_000u64),
            fee: U256::from(1_000_000u64),
        };
        CurvePool::new(PoolId::new("1:curve:A"), vec![dai(), usdc(), usdt()], inner)
            .with_execution(POOL_A, CurveInterface::StableI128)
    }

    /// CryptoU256UseEth 3-coin pool USDT/WBTC/WETH → pool_type 3, native-capable.
    fn crypto_pool() -> CurvePool {
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let inner = CurveMathPool::TwoCryptoStable {
            balances: [e18 * U256::from(1_000_000u64), e18 * U256::from(100u64)],
            precisions: [U256::from(1u64), U256::from(1u64)],
            price_scale: e18 * U256::from(20_000u64),
            d: e18 * U256::from(2_000_000u64),
            ann: U256::from(400_000u64),
            mid_fee: U256::from(3_000_000u64),
            out_fee: U256::from(30_000_000u64),
            fee_gamma: U256::from(230_000_000_000_000u64),
        };
        CurvePool::new(
            PoolId::new("1:curve:B"),
            vec![usdt(), wbtc(), weth()],
            inner,
        )
        .with_execution(POOL_B, CurveInterface::CryptoU256UseEth)
    }

    fn ctx_with_router() -> ChainConfig {
        ChainConfig::new(chain(), weth()).with_routers(Routers {
            curve: Some(ROUTER),
            ..Default::default()
        })
    }

    /// Build a 2-hop Curve route USDC →(A)→ USDT →(B)→ `out` over the two pools.
    fn two_hop_route<'a>(a: &'a CurvePool, b: &'a CurvePool, out: AssetId) -> Route<'a> {
        Route {
            pools: vec![a as &dyn Pool, b as &dyn Pool],
            path: vec![usdc(), usdt(), out],
            trade_type: TradeType::ExactIn,
        }
    }

    fn span_0_2() -> RouterSpan {
        RouterSpan {
            kind: crate::execution::routing::RouterKind::Curve,
            pools: 0..2,
        }
    }

    // ── selector ──────────────────────────────────────────────────────────────

    /// The `exchange` type signature must yield the on-chain selector 0xc872a3c5
    /// (verified against `0x45312ea0…` deployment). This guards the ABI layout.
    #[test]
    fn exchange_selector_matches_onchain() {
        assert_eq!(
            ICurveRouterNG::exchangeCall::SELECTOR,
            [0xc8, 0x72, 0xa3, 0xc5],
            "exchange(address[11],uint256[5][5],uint256,uint256,address[5],address) selector"
        );
    }

    // ── pool_type classifier ──────────────────────────────────────────────────

    #[test]
    fn pool_type_table_is_ng_aware() {
        assert_eq!(
            curve_pool_type(CurveInterface::StableI128, 3).unwrap(),
            U256::from(1)
        );
        assert_eq!(
            curve_pool_type(CurveInterface::StableI128Ng, 2).unwrap(),
            U256::from(10)
        );
        assert_eq!(
            curve_pool_type(CurveInterface::CryptoU256UseEth, 2).unwrap(),
            U256::from(2)
        );
        assert_eq!(
            curve_pool_type(CurveInterface::CryptoU256UseEth, 3).unwrap(),
            U256::from(3)
        );
        assert_eq!(
            curve_pool_type(CurveInterface::CryptoU256Receiver, 2).unwrap(),
            U256::from(20)
        );
        assert_eq!(
            curve_pool_type(CurveInterface::CryptoU256Receiver, 3).unwrap(),
            U256::from(30)
        );
    }

    // ── 2-hop encode round-trip ───────────────────────────────────────────────

    /// A 2-hop ERC-20 span encodes route/swap_params/amount/min_dy/receiver and
    /// targets the router with an approval on the span input token.
    #[test]
    fn two_hop_erc20_encodes_route_and_params() {
        let a = stable_pool();
        let b = crypto_pool();
        let route = two_hop_route(&a, &b, weth());
        let span = span_0_2();
        let recipient = Address::repeat_byte(0x55);

        let prepared = build_curve_span(
            &ctx_with_router(),
            &route,
            &span,
            U256::from(1_000_000_000u64), // 1000 USDC (6dp)
            U256::from(490_000_000_000_000_000u64),
            recipient,
            false,
            false,
            9_999_999,
            true,
        )
        .expect("build_curve_span must succeed");

        assert_eq!(prepared.tx.to, ROUTER, "tx.to must be the Curve router");
        assert_eq!(prepared.tx.value, U256::ZERO, "ERC-20 span is not payable");

        let d = ICurveRouterNG::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as exchange");

        // route: [USDC, A, USDT, B, WETH, 0, 0, …]
        assert_eq!(d.route[0], common::evm_addr(&usdc()));
        assert_eq!(d.route[1], POOL_A);
        assert_eq!(d.route[2], common::evm_addr(&usdt()));
        assert_eq!(d.route[3], POOL_B);
        assert_eq!(d.route[4], common::evm_addr(&weth()));
        assert_eq!(d.route[5], Address::ZERO, "trailing route slots stay zero");

        // hop 0 (A, stable, USDC=1 → USDT=2): [1, 2, 1, 1, 3]
        assert_eq!(
            d.swap_params[0],
            [
                U256::from(1),
                U256::from(2),
                U256::from(1),
                U256::from(1),
                U256::from(3)
            ]
        );
        // hop 1 (B, crypto 3-coin, USDT=0 → WETH=2): [0, 2, 1, 3, 3]
        assert_eq!(
            d.swap_params[1],
            [
                U256::from(0),
                U256::from(2),
                U256::from(1),
                U256::from(3),
                U256::from(3)
            ]
        );
        // trailing swap_params rows stay zero
        assert_eq!(d.swap_params[2], [U256::ZERO; 5]);

        assert_eq!(d.amount, U256::from(1_000_000_000u64));
        assert_eq!(d.min_dy, U256::from(490_000_000_000_000_000u64));
        assert_eq!(d.receiver, recipient, "receiver must be the recipient");
        assert_eq!(d.pools, [Address::ZERO; 5], "_pools empty for swap_type 1");

        // approval targets the router with the span input token (USDC).
        let approval = prepared.approval.expect("ERC-20 input needs approval");
        assert_eq!(approval.spender, ROUTER);
        assert_eq!(approval.token, usdc());
        assert_eq!(approval.min_allowance, U256::from(1_000_000_000u64));

        assert_eq!(prepared.min_received.asset, weth());
        assert_eq!(
            prepared.min_received.raw,
            U256::from(490_000_000_000_000_000u64)
        );
        assert!(prepared.max_spent.is_none());
    }

    // ── native-out ────────────────────────────────────────────────────────────

    /// Native-out on a CryptoU256UseEth endpoint: last route slot = ETH sentinel,
    /// value stays 0 (ERC-20 input), approval present, min_received is native.
    #[test]
    fn native_out_uses_eth_sentinel() {
        let a = stable_pool();
        let b = crypto_pool();
        // out asset is weth() at the boundary (quote path holds WETH), native flag on.
        let route = two_hop_route(&a, &b, weth());
        let span = span_0_2();
        let recipient = Address::repeat_byte(0x66);

        let prepared = build_curve_span(
            &ctx_with_router(),
            &route,
            &span,
            U256::from(1_000_000_000u64),
            U256::from(1u64),
            recipient,
            false,
            true,
            9_999_999,
            true,
        )
        .expect("native-out must succeed on a use_eth endpoint");

        assert_eq!(prepared.tx.value, U256::ZERO, "native-out input is ERC-20");
        let d = ICurveRouterNG::exchangeCall::abi_decode(&prepared.tx.data).unwrap();
        assert_eq!(
            d.route[4], ETH_SENTINEL,
            "native-out endpoint = ETH sentinel"
        );
        assert_eq!(d.receiver, recipient);
        assert_eq!(
            prepared.min_received.asset,
            native_asset(&ctx_with_router()),
            "native-out min_received is the chain native asset"
        );
        assert!(prepared.approval.is_some(), "ERC-20 input still approved");
    }

    /// Native-in on a CryptoU256UseEth endpoint: first route slot = ETH sentinel,
    /// tx.value carries the amount, no approval.
    #[test]
    fn native_in_sets_value_and_sentinel_no_approval() {
        // Single-pool span on the crypto pool: ETH → WBTC.
        let b = crypto_pool();
        let route = Route {
            pools: vec![&b as &dyn Pool],
            path: vec![weth(), wbtc()],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::Curve,
            pools: 0..1,
        };
        let dx = U256::from(1_000_000_000_000_000_000u64); // 1 ETH

        let prepared = build_curve_span(
            &ctx_with_router(),
            &route,
            &span,
            dx,
            U256::from(1u64),
            Address::repeat_byte(0x77),
            true,
            false,
            9_999_999,
            true,
        )
        .expect("native-in must succeed on a use_eth endpoint");

        assert_eq!(prepared.tx.value, dx, "native-in carries tx.value");
        assert!(prepared.approval.is_none(), "native-in has no approval");
        let d = ICurveRouterNG::exchangeCall::abi_decode(&prepared.tx.data).unwrap();
        assert_eq!(
            d.route[0], ETH_SENTINEL,
            "native-in endpoint = ETH sentinel"
        );
        assert_eq!(d.route[2], common::evm_addr(&wbtc()));
    }

    // ── guards ────────────────────────────────────────────────────────────────

    /// Native on a non-`CryptoU256UseEth` endpoint (stable pool) is rejected.
    #[test]
    fn native_on_stable_endpoint_is_rejected() {
        let a = stable_pool();
        let route = Route {
            pools: vec![&a as &dyn Pool],
            path: vec![weth(), usdc()], // pretend native-in; stable can't
            trade_type: TradeType::ExactIn,
        };
        // stable pool has no weth coin, but the native guard fires first.
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::Curve,
            pools: 0..1,
        };
        let err = build_curve_span(
            &ctx_with_router(),
            &route,
            &span,
            U256::from(1u64),
            U256::from(1u64),
            Address::repeat_byte(0x11),
            true,
            false,
            9_999_999,
            true,
        )
        .expect_err("native on a stable endpoint must be rejected");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    /// Native on both sides is a config error.
    #[test]
    fn native_both_sides_is_native_mismatch() {
        let a = stable_pool();
        let b = crypto_pool();
        let route = two_hop_route(&a, &b, weth());
        let err = build_curve_span(
            &ctx_with_router(),
            &route,
            &span_0_2(),
            U256::from(1u64),
            U256::from(1u64),
            Address::repeat_byte(0x11),
            true,
            true,
            9_999_999,
            true,
        )
        .expect_err("native on both sides must be NativeMismatch");
        assert_eq!(err, BuildError::NativeMismatch);
    }

    /// An unconfigured router yields a typed MissingChainConfig error.
    #[test]
    fn missing_router_is_typed_error() {
        let a = stable_pool();
        let b = crypto_pool();
        let route = two_hop_route(&a, &b, weth());
        let ctx = ChainConfig::new(chain(), weth()); // no routers
        let err = build_curve_span(
            &ctx,
            &route,
            &span_0_2(),
            U256::from(1u64),
            U256::from(1u64),
            Address::repeat_byte(0x11),
            false,
            false,
            9_999_999,
            true,
        )
        .expect_err("missing router must error");
        assert!(matches!(
            err,
            BuildError::MissingChainConfig {
                what: crate::execution::error::MissingAddr::CurveRouter,
                ..
            }
        ));
    }

    /// Exact-out through the Curve span builder is always unsupported.
    #[test]
    fn exact_out_is_unsupported() {
        let a = stable_pool();
        let b = crypto_pool();
        let route = two_hop_route(&a, &b, weth());
        let err = build_curve_span_exact_out(
            &ctx_with_router(),
            &route,
            &span_0_2(),
            U256::from(1u64),
            U256::from(1u64),
            Address::repeat_byte(0x11),
            false,
            false,
            9_999_999,
            true,
        )
        .expect_err("exact-out must be unsupported");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }
}
