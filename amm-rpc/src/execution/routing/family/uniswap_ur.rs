//! Uniswap Universal Router span builders: exact-in and exact-out.
//!
//! ## Exact-in (`build_uniswap_span`)
//!
//! Turns one same-router [`RouterSpan`] of any mix of V2/V3/V4 pools into a single
//! `execute(commands, inputs, deadline)` call. Each hop becomes one UR command;
//! intermediates stay inside the router (`ADDRESS_THIS`) and every chained hop
//! draws its input from the router's own balance, so the whole span settles
//! atomically:
//!
//! - **hop 0** takes input from the user (`payerIsUser = true` for V2/V3,
//!   `SETTLE_ALL` for V4), amount = `amount_in`;
//! - **every later hop** takes input from the router balance (V2/V3
//!   `CONTRACT_BALANCE` with `payerIsUser = false`; V4 `SETTLE` from the router
//!   with `OPEN_DELTA`, and the swap consumes the full open input delta);
//! - **every non-last hop** delivers to `ADDRESS_THIS`; the **last hop** delivers
//!   to `recipient`;
//! - only the **last hop** carries `min_out`; earlier hops carry `0`.
//!
//! The single Permit2 approval is on the span's input token, sized to
//! `amount_in` (only hop 0 pulls from the user).
//!
//! ## Exact-out (`build_uniswap_span_exact_out`)
//!
//! Exact-out is fundamentally different: V2 and V3 execute the whole multi-hop
//! path as **one** command over a reversed path (the router walks backward,
//! pulling at most `amount_in_max` from the user). So a single-version V2 or V3
//! span → exactly ONE UR command, no `ADDRESS_THIS` chaining.
//!
//! A **mixed-version** span under exact-out is rejected with
//! [`BuildError::UnsupportedExactOut`] — chaining per-hop exact-out across
//! protocol boundaries would require intermediate accounting that the UR does not
//! natively support.
//!
//! Multi-hop V4 exact-out is also deferred: V4 has no single multi-hop exact-out
//! action; the `ExactOutPolicy::OrBetter` backward-solve path covers users who need it.
//! Returns `UnsupportedExactOut` with `kind: PoolKind::UniswapV4`.
//!
//! Native wrap/unwrap at span edges is handled via `native_in`/`native_out` flags
//! on both builders. Exact-out native edges are deferred; native-ETH exact-out lands
//! with on-chain fork proofs later.

use core::any::Any;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolValue;
use amm_core::primitives::asset::{AssetAmount, AssetId};
use amm_core::primitives::pool::PoolKind;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_core::protocols::uniswap::v3::UniswapV3Pool;
use amm_core::protocols::uniswap::v4::UniswapV4Pool;
use amm_core::traits::pool::Pool;

use crate::execution::config::ChainConfig;
use crate::execution::error::BuildError;
use crate::execution::prepared::PreparedSwap;
use crate::execution::protocols::common;
use crate::execution::protocols::uniswap_v4::{
    ACTIONS_EXACT_IN, ACTIONS_EXACT_OUT, ACTIONS_WRAP_EXACT_IN, ExactInputSingleParams,
    ExactOutputSingleParams, OPEN_DELTA, build_pool_key, build_v4_swap_input, settle_all_param,
    settle_from_router_param, take_param, unwrap_weth_input, wrap_eth_input,
};
use crate::execution::route_planner::{
    ADDRESS_THIS, CONTRACT_BALANCE, RoutePlanner, UNWRAP_WETH, V2_SWAP_EXACT_IN, V2_SWAP_EXACT_OUT,
    V3_SWAP_EXACT_IN, V3_SWAP_EXACT_OUT, V4_SWAP, WRAP_ETH, v2_swap_input, v3_path, v3_swap_input,
};
use crate::execution::routing::{Route, RouterSpan};
use crate::execution::types::UnsignedTx;

/// Downcast a `&dyn Pool` to a concrete pool type via trait upcasting.
///
/// `Pool: Any`, so upcasting to `&dyn Any` then `downcast_ref` recovers the
/// concrete type. Returns [`BuildError::UnsupportedProtocol`] when the pool is not
/// the expected concrete type (the `PoolKind` dispatch guarantees a match, so this
/// is a defensive fallthrough rather than an expected path).
fn downcast<T: Any>(pool: &dyn Pool) -> Result<&T, BuildError> {
    (pool as &dyn Any)
        .downcast_ref::<T>()
        .ok_or(BuildError::UnsupportedProtocol)
}

/// Build one Universal Router transaction for a same-router Uniswap span
/// (exact-in). Dispatches each pool in `span.pools` by [`PoolKind`] and appends one
/// UR command per hop; see the module docs for the chaining contract.
///
/// `recipient` is where the span's final output lands; `min_out` is the
/// slippage floor on that final output (earlier hops are unbounded — the span
/// is atomic, so only the terminal amount matters). `deadline` is the absolute
/// Unix timestamp threaded into the UR `execute` call. `is_final` is accepted for
/// signature parity with sibling family builders; exact-in delivery is fully
/// determined by `recipient`, so it is not branched on here.
///
/// `native_in`: when `true`, the span's input is native ETH. A `WRAP_ETH`
/// command is prepended so the router holds WETH before the first hop, and every
/// hop draws from the router balance (`payer_is_user=false` / `CONTRACT_BALANCE` /
/// V4 `SETTLE`-from-router). `tx.value` is set to `amount_in`; `approval` is `None`.
///
/// `native_out`: when `true`, the span's output is native ETH. The last hop
/// delivers to `ADDRESS_THIS` and `UNWRAP_WETH` is appended. `min_received` is
/// reported as the chain native asset (`AssetId(chain, B256::ZERO)`).
///
/// Native edges wrap/unwrap ETH but do NOT rewrite path currencies: the caller must
/// supply the pool-currency (WETH) address in `route.path` at the span boundary,
/// not `address(0)`. The builder wraps `amount_in` ETH to WETH (`native_in`) /
/// unwraps the final WETH to ETH (`native_out`).
///
/// Both flags may be set simultaneously: wrap + all-hops-from-router + last-hop-to-
/// router + unwrap; `tx.value = amount_in`; `approval = None`.
///
/// # Errors
/// - [`BuildError::UnsupportedProtocol`] — a pool's kind is not V2/V3/V4, or a
///   downcast to the concrete pool type fails.
/// - [`BuildError::Overflow`] — a V4 `amountOutMinimum` exceeds `u128`.
/// - Router/Permit2 address lookups may return [`BuildError::MissingChainConfig`].
//
// `dead_code`: this is the span-dispatch entry point; the multi-span
// composer that calls it lands in a later task. Exercised now by this module's
// tests. The allow keeps `downcast` (only reachable through here) live too.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_uniswap_span(
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
    let start = span.pools.start;
    let end = span.pools.end;
    let last = end - 1;

    // partition() guarantees well-formed spans; assert the trust boundary.
    debug_assert!(span.pools.end <= route.pools.len() && span.pools.end < route.path.len());

    // The span's input asset is the token entering hop `start`; the output is
    // the token leaving the last hop. These bound the approval and min_received.
    let seg_input = route.path[start];
    let seg_output = route.path[end];

    let mut planner = RoutePlanner::new();

    // native_in: wrap `amount_in` ETH to WETH held by the router before any swap.
    // Every hop then draws from the router balance rather than from the user.
    if native_in {
        planner.add(WRAP_ETH, wrap_eth_input(amount_in));
    }

    for i in span.pools.clone() {
        let pool = route.pools[i];
        let in_asset = route.path[i];
        let out_asset = route.path[i + 1];
        let in_addr = common::evm_addr(&in_asset);
        let out_addr = common::evm_addr(&out_asset);

        let is_first = i == start;
        let is_last = i == last;

        // Under native_in the router already holds the input WETH after WRAP_ETH,
        // so every hop (including the first) draws from the router balance.
        // Without native_in only hop 0 pulls from the user.
        let payer_is_user = is_first && !native_in;

        // Non-last hops deliver the intermediate to the router; the last hop pays
        // the caller-supplied recipient — or ADDRESS_THIS when native_out so the
        // router holds the WETH that UNWRAP_WETH will convert.
        let hop_recipient = match (is_last, native_out) {
            (true, true) => ADDRESS_THIS,
            (true, false) => recipient,
            (false, _) => ADDRESS_THIS,
        };
        // Only the terminal hop enforces the slippage floor; earlier hops are 0.
        let hop_min = match is_last {
            true => min_out,
            false => U256::ZERO,
        };
        // V2/V3 amount sentinel: exact `amount_in` from the user on hop 0 (ERC-20
        // path), else the router's full balance of the upstream intermediate.
        let hop_amount = match payer_is_user {
            true => amount_in,
            false => CONTRACT_BALANCE,
        };

        let kind = pool
            .as_introspect()
            .ok_or(BuildError::UnsupportedProtocol)?
            .kind();
        match kind {
            PoolKind::UniswapV2 => {
                let _p = downcast::<UniswapV2Pool>(pool)?;
                // V2 path is just the ordered token pair; the pair fee is implicit
                // in the pool, so the UR needs only the addresses.
                let input = v2_swap_input(
                    true,
                    hop_recipient,
                    hop_amount,
                    hop_min,
                    vec![in_addr, out_addr],
                    payer_is_user,
                );
                planner.add(V2_SWAP_EXACT_IN, input);
            }
            PoolKind::UniswapV3 => {
                let p = downcast::<UniswapV3Pool>(pool)?;
                // Single-hop V3 path `tokenIn ‖ fee(u24) ‖ tokenOut` (forward).
                let path = v3_path(&[in_addr, out_addr], &[p.fee_pips()], false);
                let input = v3_swap_input(
                    true,
                    hop_recipient,
                    hop_amount,
                    hop_min,
                    path,
                    payer_is_user,
                );
                planner.add(V3_SWAP_EXACT_IN, input);
            }
            PoolKind::UniswapV4 => {
                let p = downcast::<UniswapV4Pool>(pool)?;
                let (pool_key, c0, _c1) = build_pool_key(p)?;
                // currency0 is the numerically-smaller address; zeroForOne is true
                // when the hop's input is currency0.
                let zero_for_one = in_addr == c0;
                // V4 swap amounts are u128. When payer_is_user (ERC-20 first hop)
                // the swap carries the exact amount_in; otherwise OPEN_DELTA (0)
                // so the pool consumes the full open input delta from the router.
                let swap_amount_in = match payer_is_user {
                    true => u128::try_from(amount_in).map_err(|_| BuildError::Overflow)?,
                    false => u128::try_from(OPEN_DELTA).map_err(|_| BuildError::Overflow)?,
                };
                let min_u128 = u128::try_from(hop_min).map_err(|_| BuildError::Overflow)?;

                let swap_params = ExactInputSingleParams {
                    poolKey: pool_key,
                    zeroForOne: zero_for_one,
                    amountIn: swap_amount_in,
                    amountOutMinimum: min_u128,
                    hookData: alloy::primitives::Bytes::new(),
                };
                let p_swap = swap_params.abi_encode().into();
                let p_take = take_param(out_addr, hop_recipient);

                // When payer_is_user (ERC-20 first hop): SETTLE_ALL pulls from the
                // user. Otherwise (chained or native_in first hop): SETTLE from the
                // router's own balance (payerIsUser=false), mirroring V2/V3
                // CONTRACT_BALANCE draw.
                let (actions, p_settle): (&[u8], _) = match payer_is_user {
                    true => (&ACTIONS_EXACT_IN, settle_all_param(in_addr, amount_in)),
                    false => (&ACTIONS_WRAP_EXACT_IN, settle_from_router_param(in_addr)),
                };
                let v4_input = build_v4_swap_input(actions, vec![p_swap, p_settle, p_take]);
                planner.add(V4_SWAP, v4_input);
            }
            // Partition groups only V2/V3/V4 under the Universal Router; any
            // other kind reaching here is a routing invariant violation.
            _ => return Err(BuildError::UnsupportedProtocol),
        }
    }

    // native_out: the last hop already delivered WETH to ADDRESS_THIS; now unwrap
    // it to ETH and deliver it to the recipient, enforcing the slippage floor.
    if native_out {
        planner.add(UNWRAP_WETH, unwrap_weth_input(recipient, min_out));
    }

    let data = planner.encode(deadline);
    let value = match native_in {
        true => amount_in,
        false => U256::ZERO,
    };
    let tx = UnsignedTx {
        chain: ctx.chain,
        to: ctx.router_universal()?,
        data,
        value,
    };

    // native_in: the user sends ETH as tx.value; no Permit2 approval needed.
    // ERC-20 input: only hop 0 pulls from the user, so the Permit2 approval
    // covers the span input token sized to `amount_in`.
    let approval = match native_in {
        true => None,
        false => Some(common::erc20_approval(ctx.permit2()?, seg_input, amount_in)),
    };

    // native_out: min_received is the chain native asset (address(0) on the chain).
    // ERC-20 out: min_received is the span output token.
    let min_received_asset = match native_out {
        true => AssetId::new(ctx.chain, B256::ZERO),
        false => seg_output,
    };

    Ok(PreparedSwap {
        tx,
        min_received: AssetAmount::new(min_received_asset, min_out),
        max_spent: None,
        approval,
        price_impact: None,
    })
}

/// Check whether every pool in `span.pools` belongs to the same [`PoolKind`].
///
/// Returns `Some(kind)` when all pools agree on `kind`; `None` when the span
/// mixes Uniswap versions (or a pool has no introspection data). Mixed spans
/// cannot be encoded as a single exact-out command — the caller must reject them
/// with [`BuildError::UnsupportedExactOut`].
fn span_is_single_version(route: &Route<'_>, span: &RouterSpan) -> Option<PoolKind> {
    let mut pools = span.pools.clone().map(|i| route.pools[i]);
    // Require the first pool to fix the expected kind; then check the rest.
    let first_kind = pools.next()?.as_introspect()?.kind();
    for pool in pools {
        let kind = pool.as_introspect()?.kind();
        if kind != first_kind {
            return None;
        }
    }
    Some(first_kind)
}

/// Build one Universal Router transaction for a same-router Uniswap span
/// (exact-out, single-version only, ERC-20 edges only).
///
/// Exact-out differs from exact-in: V3 and V2 execute the whole multi-hop path as
/// **one** UR command over a reversed path, so chaining is neither needed nor
/// possible. Only single-version spans are supported — a mixed V2/V3/V4 span
/// returns [`BuildError::UnsupportedExactOut`] immediately.
///
/// ## Dispatch
/// - **All-V3 (any hop count):** one `V3_SWAP_EXACT_OUT` with the reversed v3_path.
/// - **All-V2 (any hop count):** one `V2_SWAP_EXACT_OUT` with the forward token path
///   (`[in, …, out]`); the UR reverses internally.
/// - **All-V4, single-hop:** the standard V4 exact-out stream
///   (`SWAP_EXACT_OUT_SINGLE`, `SETTLE_ALL`, `TAKE`).
/// - **All-V4, multi-hop:** deferred — V4 has no single multi-hop exact-out action;
///   `ExactOutPolicy::OrBetter` backward-solve covers users who need it.
///   Returns `UnsupportedExactOut { kind: UniswapV4 }`.
/// - **Mixed versions:** `UnsupportedExactOut { kind: <first pool's PoolKind> }`.
///
/// `amount_out` is the exact target output of the span. `amount_in_max` is the
/// slippage-bounded maximum input. `recipient` is where the span output lands.
/// `native_in`/`native_out`: native-ETH exact-out edges are deferred; native-ETH
/// exact-out lands with on-chain fork proofs later. When either flag is `true` this
/// function returns `Err(BuildError::UnsupportedProtocol)` rather than silently
/// emitting calldata we cannot yet verify.
/// `is_final` is accepted for signature parity with the exact-in builder; delivery
/// is determined by `recipient`, so it is not branched on here.
///
/// # Errors
/// - [`BuildError::UnsupportedProtocol`] — `native_in` or `native_out` is `true`
///   (deferred; native-ETH exact-out lands with on-chain fork proofs later), or pool
///   kind is not V2/V3/V4, or downcast to the concrete pool type fails.
/// - [`BuildError::UnsupportedExactOut`] — mixed-version span or multi-hop V4.
/// - [`BuildError::Overflow`] — a V4 amount exceeds `u128`.
/// - Router/Permit2 address lookups may return [`BuildError::MissingChainConfig`].
//
// `dead_code`: entry point used by the multi-span composer (later task).
// Exercised by this module's tests. Allow keeps `span_is_single_version` live.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_uniswap_span_exact_out(
    ctx: &ChainConfig,
    route: &Route<'_>,
    span: &RouterSpan,
    amount_out: U256,
    amount_in_max: U256,
    recipient: Address,
    native_in: bool,
    native_out: bool,
    deadline: u64,
    _is_final: bool,
) -> Result<PreparedSwap, BuildError> {
    // Native-ETH exact-out edges are deferred; native-ETH exact-out lands with
    // on-chain fork proofs later. Reject early rather than silently producing
    // calldata we cannot verify.
    if native_in || native_out {
        return Err(BuildError::UnsupportedProtocol);
    }
    let start = span.pools.start;
    let end = span.pools.end;

    // partition() guarantees well-formed spans; assert the trust boundary.
    debug_assert!(span.pools.end <= route.pools.len() && span.pools.end < route.path.len());

    // RouterSpan input/output assets for approval and PreparedSwap fields.
    let seg_input = route.path[start];
    let seg_output = route.path[end];

    // Reject mixed-version spans: exact-out across protocol boundaries cannot
    // be expressed as a single UR command sequence.
    let version = match span_is_single_version(route, span) {
        Some(v) => v,
        None => {
            // Report the first pool's kind as the culprit.
            let first_kind = route.pools[start]
                .as_introspect()
                .ok_or(BuildError::UnsupportedProtocol)?
                .kind();
            return Err(BuildError::UnsupportedExactOut { kind: first_kind });
        }
    };

    // Collect the span token path [in, …, out] and per-hop fees (V3 only).
    let tokens: Vec<Address> = (start..=end)
        .map(|i| common::evm_addr(&route.path[i]))
        .collect();

    let mut planner = RoutePlanner::new();

    match version {
        PoolKind::UniswapV3 => {
            // Collect V3 fee tiers in forward order; reversed path is built below.
            let fees: Vec<u32> = span
                .pools
                .clone()
                .map(|i| downcast::<UniswapV3Pool>(route.pools[i]).map(|p| p.fee_pips()))
                .collect::<Result<_, _>>()?;

            // Exact-out requires the reversed path: [out, fee_last, …, fee_first, in].
            let path = v3_path(&tokens, &fees, true);
            // For exact-out: `amount` = amountOut (exact target), `limit` = amountInMaximum.
            // `payer_is_user = true` — the router pulls the input from the user up front.
            let input = v3_swap_input(false, recipient, amount_out, amount_in_max, path, true);
            planner.add(V3_SWAP_EXACT_OUT, input);
        }
        PoolKind::UniswapV2 => {
            // V2 exact-out (`swapTokensForExactTokens`) takes the forward token path
            // [tokenIn, …, tokenOut]; the V2 router reverses internally when computing
            // the required input amounts.
            let _: Vec<_> = span
                .pools
                .clone()
                .map(|i| downcast::<UniswapV2Pool>(route.pools[i]))
                .collect::<Result<_, _>>()?;
            // `amount` = amountOut (exact target), `limit` = amountInMaximum.
            // `payer_is_user = true` — the user funds this single command.
            let input = v2_swap_input(false, recipient, amount_out, amount_in_max, tokens, true);
            planner.add(V2_SWAP_EXACT_OUT, input);
        }
        PoolKind::UniswapV4 => {
            // V4 has no single multi-hop exact-out action. Chaining per-hop
            // `SWAP_EXACT_OUT_SINGLE` backward through the router is not supported;
            // `ExactOutPolicy::OrBetter` backward-solve covers users who need multi-hop
            // V4 exact-out. This is an honest, documented limitation — not a silent gap.
            if span.pools.len() > 1 {
                return Err(BuildError::UnsupportedExactOut {
                    kind: PoolKind::UniswapV4,
                });
            }
            // Single-hop V4 exact-out: reuse the existing ExactOutputSingleParams +
            // ACTIONS_EXACT_OUT stream (same encoding as uniswap_v4.rs build_swap_exact_out).
            let pool_idx = span.pools.start;
            let pool = downcast::<UniswapV4Pool>(route.pools[pool_idx])?;
            let (pool_key, c0, _c1) = build_pool_key(pool)?;
            let in_addr = common::evm_addr(&route.path[pool_idx]);
            let out_addr = common::evm_addr(&route.path[pool_idx + 1]);
            let zero_for_one = in_addr == c0;

            let out_u128 = u128::try_from(amount_out).map_err(|_| BuildError::Overflow)?;
            let max_u128 = u128::try_from(amount_in_max).map_err(|_| BuildError::Overflow)?;

            let swap_params = ExactOutputSingleParams {
                poolKey: pool_key,
                zeroForOne: zero_for_one,
                amountOut: out_u128,
                amountInMaximum: max_u128,
                hookData: alloy::primitives::Bytes::new(),
            };
            let p_swap = swap_params.abi_encode().into();
            let p_settle = settle_all_param(in_addr, amount_in_max);
            let p_take = take_param(out_addr, recipient);
            let v4_input = build_v4_swap_input(&ACTIONS_EXACT_OUT, vec![p_swap, p_settle, p_take]);
            planner.add(V4_SWAP, v4_input);
        }
        // Partition groups only V2/V3/V4 under the Universal Router.
        _ => return Err(BuildError::UnsupportedProtocol),
    }

    let data = planner.encode(deadline);
    let tx = UnsignedTx {
        chain: ctx.chain,
        to: ctx.router_universal()?,
        data,
        value: U256::ZERO,
    };

    // Permit2 approval on the span input token, sized to amount_in_max (the
    // router pulls at most this much from the user for the whole span).
    let approval = Some(common::erc20_approval(
        ctx.permit2()?,
        seg_input,
        amount_in_max,
    ));

    Ok(PreparedSwap {
        tx,
        // Exact-out guarantees the output amount exactly.
        min_received: AssetAmount::new(seg_output, amount_out),
        // max_spent is the slippage-bounded input ceiling.
        max_spent: Some(AssetAmount::new(seg_input, amount_in_max)),
        approval,
        price_impact: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy::primitives::{B256, Bytes};
    use alloy::sol_types::{SolCall, SolValue};
    use amm_core::primitives::asset::{AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::protocols::uniswap::v3::TickData;
    use amm_core::protocols::uniswap::v4::{Hooks, TickInfo, UniswapV4Pool};

    use crate::execution::config::{ChainConfig, Routers};
    use crate::execution::route_planner::IUniversalRouter;
    use crate::execution::types::TradeType;

    // ── Fixtures ─────────────────────────────────────────────────────────────

    fn chain_id() -> ChainId {
        ChainId(1)
    }

    /// AssetId from a single discriminant byte (address = right-padded byte).
    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    fn universal_router() -> Address {
        Address::repeat_byte(0xAA)
    }

    fn permit2() -> Address {
        Address::repeat_byte(0xBB)
    }

    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), asset(0x02)).with_routers(Routers {
            universal: Some(universal_router()),
            permit2: Some(permit2()),
            ..Default::default()
        })
    }

    /// sqrtPriceX96 for tick 0 (price 1:1) = 2^96.
    const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;

    /// Full-range tick data `[-887220, 887220]` at spacing 60 with `liq` liquidity.
    fn full_range_ticks(liq: i128) -> TickData {
        TickData::from_ticks(
            60,
            vec![
                (
                    -887_220,
                    TickInfo {
                        liquidity_net: liq,
                        initialized: true,
                    },
                ),
                (
                    887_220,
                    TickInfo {
                        liquidity_net: -liq,
                        initialized: true,
                    },
                ),
            ],
        )
    }

    /// A V3 pool over the (address-sorted) asset pair with the given fee pips.
    fn v3_pool(a: AssetId, b: AssetId, fee_pips: u32) -> UniswapV3Pool {
        let (lo, hi) = order(a, b);
        UniswapV3Pool::new(
            PoolId::new("1:univ3:test"),
            [lo, hi],
            U256::from(SQRT_1_1),
            1_000_000_000_000_000_000u128,
            0,
            fee_pips,
            full_range_ticks(1_000_000_000_000_000_000i128),
        )
    }

    /// A full-range V4 pool over the (address-sorted) asset pair.
    fn v4_pool(a: AssetId, b: AssetId) -> UniswapV4Pool {
        let (lo, hi) = order(a, b);
        let liq: i128 = 1_000_000_000_000_000_000;
        UniswapV4Pool::new(
            PoolId::new("1:univ4:test"),
            B256::repeat_byte(0xCC),
            [lo, hi],
            U256::from(SQRT_1_1),
            liq as u128,
            0,
            3000,
            3000,
            full_range_ticks(liq),
            Hooks::None,
            3000,
            60,
            Address::ZERO,
        )
    }

    /// Order two assets by their EVM address so `assets[0] == currency0`.
    fn order(a: AssetId, b: AssetId) -> (AssetId, AssetId) {
        match common::evm_addr(&a) < common::evm_addr(&b) {
            true => (a, b),
            false => (b, a),
        }
    }

    // ── Decode helpers ───────────────────────────────────────────────────────

    /// Decode the outer `execute(bytes, bytes[], uint256)` calldata.
    fn decode_outer(data: &[u8]) -> (Bytes, Vec<Bytes>, U256) {
        let d =
            IUniversalRouter::executeCall::abi_decode(data).expect("outer must decode as execute");
        (d.commands, d.inputs, d.deadline)
    }

    /// Decode a V3 swap input `(recipient, amount, limit, path, payerIsUser)`.
    fn decode_v3(input: &Bytes) -> (Address, U256, U256, Bytes, bool) {
        <(Address, U256, U256, Bytes, bool)>::abi_decode_params(input)
            .expect("V3 input must decode")
    }

    /// Decode a V4 swap input `(bytes actions, bytes[] params)`.
    fn decode_v4(input: &Bytes) -> (Bytes, Vec<Bytes>) {
        <(Bytes, Vec<Bytes>)>::abi_decode_params(input).expect("V4 input must decode")
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    /// A mixed V3→V4 two-hop span: hop-1 (V3) takes from the user and delivers
    /// the intermediate to the router; hop-2 (V4) takes from the router and
    /// delivers the output to `recipient`.
    #[test]
    fn v3_then_v4_two_hop_chains_through_router() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 3000);
        let p2 = v4_pool(b, c);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x55);
        let amount_in = U256::from(1_000u64);
        let min_out = U256::from(900u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_uniswap_span(
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
        .expect("mixed V3→V4 span must build");

        // tx envelope: Universal Router, zero value, chain matches.
        assert_eq!(prepared.tx.to, universal_router(), "tx.to must be UR");
        assert_eq!(prepared.tx.value, U256::ZERO, "exact-in has no value");
        assert_eq!(prepared.tx.chain, chain_id());

        let (commands, inputs, dl) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[V3_SWAP_EXACT_IN, V4_SWAP],
            "commands must be [V3_SWAP_EXACT_IN, V4_SWAP]"
        );
        assert_eq!(inputs.len(), 2, "two hops → two inputs");
        assert_eq!(dl, U256::from(deadline), "deadline round-trips");

        // Hop 1 (V3): input from user, amount == amount_in, min 0 (not last),
        // recipient == ADDRESS_THIS.
        let (v3_recip, v3_amt, v3_limit, _path, v3_payer) = decode_v3(&inputs[0]);
        assert_eq!(v3_recip, ADDRESS_THIS, "hop1 delivers to router");
        assert_eq!(v3_amt, amount_in, "hop1 amount is amount_in");
        assert_eq!(v3_limit, U256::ZERO, "hop1 (non-last) carries no min");
        assert!(v3_payer, "hop1 payer is the user");

        // Hop 2 (V4): chained SETTLE from router, delivers to recipient, min_out.
        let (actions, params) = decode_v4(&inputs[1]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_WRAP_EXACT_IN[..],
            "chained V4 hop uses [SWAP, SETTLE, TAKE]"
        );
        assert_eq!(params.len(), 3, "V4 hop has 3 params");

        // params[0]: swap with amountIn == OPEN_DELTA (0) and amountOutMin == min_out.
        let swap = ExactInputSingleParams::abi_decode(&params[0])
            .expect("params[0] must decode as ExactInputSingleParams");
        assert_eq!(swap.amountIn, 0u128, "chained V4 swaps the full open delta");
        assert_eq!(
            swap.amountOutMinimum,
            u128::try_from(min_out).unwrap(),
            "last hop carries min_out"
        );

        // params[1]: SETTLE from router — (currency=b, OPEN_DELTA, payerIsUser=false).
        let (settle_cur, settle_amt, settle_payer) =
            <(Address, U256, bool)>::abi_decode_params(&params[1])
                .expect("params[1] must decode as chained SETTLE");
        assert_eq!(
            settle_cur,
            common::evm_addr(&b),
            "settle currency is intermediate"
        );
        assert_eq!(settle_amt, U256::ZERO, "settle uses OPEN_DELTA");
        assert!(!settle_payer, "chained hop does not pay from user");

        // params[2]: TAKE output c to the recipient.
        let (take_cur, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2])
                .expect("params[2] must decode as TAKE");
        assert_eq!(take_cur, common::evm_addr(&c), "take output currency");
        assert_eq!(take_to, recipient, "last V4 hop delivers to recipient");
        assert_eq!(take_amt, U256::ZERO, "take uses OPEN_DELTA");

        // Approval: Permit2 on the span input, sized to amount_in.
        let approval = prepared.approval.expect("exact-in ERC-20 needs approval");
        assert_eq!(approval.spender, permit2(), "spender is Permit2");
        assert_eq!(approval.token, a, "approval token is span input");
        assert_eq!(approval.min_allowance, amount_in, "allowance is amount_in");

        assert!(prepared.max_spent.is_none(), "exact-in has no max_spent");
        assert_eq!(
            prepared.min_received.asset, c,
            "min_received asset is span output"
        );
        assert_eq!(
            prepared.min_received.raw, min_out,
            "min_received is min_out"
        );
    }

    /// An all-V3 three-hop span: hop 0 pulls from the user; hops 1 and 2 draw
    /// the router balance (`CONTRACT_BALANCE`); the last hop delivers to recipient.
    #[test]
    fn all_v3_three_hop_uses_contract_balance_for_chained_hops() {
        let (a, b, c, d) = (asset(0x11), asset(0x22), asset(0x33), asset(0x44));
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        let p3 = v3_pool(c, d, 10_000);
        let route = Route {
            pools: vec![&p1, &p2, &p3],
            path: vec![a, b, c, d],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..3,
        };

        let recipient = Address::repeat_byte(0x77);
        let amount_in = U256::from(5_000u64);
        let min_out = U256::from(4_200u64);
        let deadline = 42u64;

        let prepared = build_uniswap_span(
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
        .expect("all-V3 3-hop span must build");

        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[V3_SWAP_EXACT_IN, V3_SWAP_EXACT_IN, V3_SWAP_EXACT_IN],
            "commands must be three V3 exact-in swaps"
        );
        assert_eq!(inputs.len(), 3, "three hops → three inputs");

        // Hop 0: from user, amount_in, ADDRESS_THIS, min 0.
        let (r0, amt0, lim0, _p0, payer0) = decode_v3(&inputs[0]);
        assert_eq!(r0, ADDRESS_THIS, "hop0 delivers to router");
        assert_eq!(amt0, amount_in, "hop0 amount is amount_in");
        assert_eq!(lim0, U256::ZERO, "hop0 carries no min");
        assert!(payer0, "hop0 payer is user");

        // Hop 1 (middle): CONTRACT_BALANCE, payer=false, ADDRESS_THIS, min 0.
        let (r1, amt1, lim1, _p1, payer1) = decode_v3(&inputs[1]);
        assert_eq!(r1, ADDRESS_THIS, "hop1 delivers to router");
        assert_eq!(amt1, CONTRACT_BALANCE, "hop1 spends router balance");
        assert_eq!(lim1, U256::ZERO, "hop1 carries no min");
        assert!(!payer1, "hop1 payer is the router");

        // Hop 2 (last): CONTRACT_BALANCE, payer=false, recipient, min_out.
        let (r2, amt2, lim2, _p2, payer2) = decode_v3(&inputs[2]);
        assert_eq!(r2, recipient, "last hop delivers to recipient");
        assert_eq!(amt2, CONTRACT_BALANCE, "last hop spends router balance");
        assert_eq!(lim2, min_out, "last hop carries min_out");
        assert!(!payer2, "last hop payer is the router");

        // RouterSpan-level outputs.
        assert_eq!(prepared.min_received.asset, d, "min_received asset is d");
        assert_eq!(prepared.min_received.raw, min_out);
        let approval = prepared.approval.expect("needs approval");
        assert_eq!(approval.token, a, "approval token is span input a");
        assert_eq!(approval.min_allowance, amount_in);
    }

    // ── Exact-out tests ──────────────────────────────────────────────────────

    /// All-V3 two-hop exact-out: a single `V3_SWAP_EXACT_OUT` command with the
    /// **reversed** v3 path (last token first), `amount == amount_out`,
    /// `limit == amount_in_max`, `recipient == recipient`, `payerIsUser == true`.
    #[test]
    fn all_v3_exact_out_two_hop_uses_single_reversed_command() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        // Two V3 hops with distinct fees so path-encoding can be verified.
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x55);
        let amount_out = U256::from(800u64);
        let amount_in_max = U256::from(1_200u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_uniswap_span_exact_out(
            &ctx(),
            &route,
            &span,
            amount_out,
            amount_in_max,
            recipient,
            false,
            false,
            deadline,
            true,
        )
        .expect("all-V3 2-hop exact-out must build");

        // tx envelope checks.
        assert_eq!(prepared.tx.to, universal_router(), "tx.to must be UR");
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "exact-out ERC-20 has no value"
        );
        assert_eq!(prepared.tx.chain, chain_id());

        // Outer: single command [V3_SWAP_EXACT_OUT].
        let (commands, inputs, dl) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[V3_SWAP_EXACT_OUT],
            "commands must be [V3_SWAP_EXACT_OUT]"
        );
        assert_eq!(inputs.len(), 1, "single V3 exact-out command → one input");
        assert_eq!(dl, U256::from(deadline), "deadline round-trips");

        // Decode the single V3 input tuple (recipient, amount, limit, path, payerIsUser).
        let (v3_recip, v3_amt, v3_limit, path, v3_payer) = decode_v3(&inputs[0]);
        assert_eq!(
            v3_recip, recipient,
            "recipient must be the caller-supplied address"
        );
        assert_eq!(
            v3_amt, amount_out,
            "amount must be amount_out (exact target)"
        );
        assert_eq!(v3_limit, amount_in_max, "limit must be amount_in_max");
        assert!(v3_payer, "payerIsUser must be true");

        // Verify the path is reversed: [c ‖ fee(3000) ‖ b ‖ fee(500) ‖ a].
        // The addresses are right-padded from their discriminant byte, so we can
        // compare the 20-byte address slices at known offsets.
        // Reversed path layout: token(20) | fee(3) | token(20) | fee(3) | token(20) = 66 bytes.
        assert_eq!(path.len(), 66, "two-hop reversed path must be 66 bytes");
        let c_addr = common::evm_addr(&c);
        let b_addr = common::evm_addr(&b);
        let a_addr = common::evm_addr(&a);
        assert_eq!(
            &path[0..20],
            c_addr.as_slice(),
            "first token in reversed path is c"
        );
        // fee between c and b in the reversed path is the fee of the c→b pool (hop p2, fee 3000).
        assert_eq!(&path[20..23], &[0x00, 0x0b, 0xb8], "fee c→b is 3000");
        assert_eq!(&path[23..43], b_addr.as_slice(), "second token is b");
        // fee between b and a in the reversed path is the fee of the b→a pool (hop p1, fee 500).
        assert_eq!(&path[43..46], &[0x00, 0x01, 0xf4], "fee b→a is 500");
        assert_eq!(
            &path[46..66],
            a_addr.as_slice(),
            "last token in reversed path is a"
        );

        // PreparedSwap field assertions.
        assert_eq!(
            prepared.min_received.asset, c,
            "min_received asset is span output c"
        );
        assert_eq!(
            prepared.min_received.raw, amount_out,
            "min_received is amount_out"
        );
        let max_spent = prepared.max_spent.expect("exact-out must have max_spent");
        assert_eq!(max_spent.asset, a, "max_spent asset is span input a");
        assert_eq!(max_spent.raw, amount_in_max, "max_spent is amount_in_max");
        let approval = prepared.approval.expect("exact-out ERC-20 needs approval");
        assert_eq!(approval.spender, permit2(), "spender is Permit2");
        assert_eq!(approval.token, a, "approval token is span input a");
        assert_eq!(
            approval.min_allowance, amount_in_max,
            "allowance is amount_in_max"
        );
    }

    /// Mixed V3+V4 exact-out span → `Err(UnsupportedExactOut { kind })`.
    /// The first pool's kind is reported as the culprit.
    #[test]
    fn mixed_v3_v4_exact_out_returns_unsupported_exact_out() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 3000);
        let p2 = v4_pool(b, c);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let err = build_uniswap_span_exact_out(
            &ctx(),
            &route,
            &span,
            U256::from(100u64),
            U256::from(200u64),
            Address::repeat_byte(0x55),
            false,
            false,
            1_700_000_000u64,
            true,
        )
        .expect_err("mixed V3+V4 exact-out must fail");

        assert!(
            matches!(
                err,
                BuildError::UnsupportedExactOut {
                    kind: PoolKind::UniswapV3
                }
            ),
            "expected UnsupportedExactOut {{ kind: UniswapV3 }}, got {err:?}"
        );
    }

    /// All-V2 exact-out: a single `V2_SWAP_EXACT_OUT` command with the **forward**
    /// token path `[in, ..., out]` (the UR reverses internally for exact-out).
    /// `amount == amount_out`, `limit == amount_in_max`, `payerIsUser == true`.
    #[test]
    fn all_v2_exact_out_uses_single_forward_path_command() {
        use amm_core::protocols::uniswap::v2::UniswapV2Pool;

        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = UniswapV2Pool::new(
            PoolId::new("1:univ2:test-ab"),
            [a, b],
            [U256::from(1_000_000u64); 2],
            30,
        );
        let p2 = UniswapV2Pool::new(
            PoolId::new("1:univ2:test-bc"),
            [b, c],
            [U256::from(1_000_000u64); 2],
            30,
        );
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x66);
        let amount_out = U256::from(500u64);
        let amount_in_max = U256::from(700u64);
        let deadline = 99u64;

        let prepared = build_uniswap_span_exact_out(
            &ctx(),
            &route,
            &span,
            amount_out,
            amount_in_max,
            recipient,
            false,
            false,
            deadline,
            true,
        )
        .expect("all-V2 2-hop exact-out must build");

        assert_eq!(prepared.tx.to, universal_router(), "tx.to must be UR");
        assert_eq!(prepared.tx.value, U256::ZERO);

        let (commands, inputs, dl) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[V2_SWAP_EXACT_OUT],
            "commands must be [V2_SWAP_EXACT_OUT]"
        );
        assert_eq!(inputs.len(), 1);
        assert_eq!(dl, U256::from(deadline));

        // Decode V2 input: (recipient, amount, limit, address[] path, payerIsUser).
        let (v2_recip, v2_amt, v2_limit, v2_path, v2_payer) =
            <(Address, U256, U256, Vec<Address>, bool)>::abi_decode_params(&inputs[0])
                .expect("V2 exact-out input must decode");

        assert_eq!(v2_recip, recipient);
        assert_eq!(v2_amt, amount_out, "amount must be amount_out");
        assert_eq!(v2_limit, amount_in_max, "limit must be amount_in_max");
        assert!(v2_payer, "payerIsUser must be true");
        // Forward path: [a, b, c] — the UR router reverses internally.
        assert_eq!(
            v2_path,
            vec![
                common::evm_addr(&a),
                common::evm_addr(&b),
                common::evm_addr(&c)
            ],
            "V2 exact-out path must be forward [a, b, c]"
        );

        assert_eq!(prepared.min_received.asset, c);
        assert_eq!(prepared.min_received.raw, amount_out);
        let max_spent = prepared.max_spent.expect("must have max_spent");
        assert_eq!(max_spent.asset, a);
        assert_eq!(max_spent.raw, amount_in_max);
        let approval = prepared.approval.expect("needs approval");
        assert_eq!(approval.token, a);
        assert_eq!(approval.min_allowance, amount_in_max);
    }

    /// Single-hop all-V4 exact-out: the happy path that was untested.
    ///
    /// Asserts the full encode round-trip for `build_uniswap_span_exact_out` on a
    /// one-pool V4 span:
    /// - UR command stream is `[V4_SWAP]`
    /// - V4 actions are `ACTIONS_EXACT_OUT` (`[SWAP_EXACT_OUT_SINGLE, SETTLE_ALL, TAKE]`)
    /// - `ExactOutputSingleParams` carries the correct `amountOut` and `amountInMaximum`
    /// - SETTLE_ALL settles the span input; TAKE delivers to `recipient`
    /// - `PreparedSwap`: `tx.to == router_universal`, `tx.value == 0`,
    ///   `max_spent == Some(amount_in_max on input)`, `min_received == amount_out on output`,
    ///   `approval` is `Some` Permit2 on the span input
    #[test]
    fn all_v4_single_hop_exact_out_encodes_correct_structure() {
        let (a, b) = (asset(0x11), asset(0x22));
        let pool = v4_pool(a, b);
        let route = Route {
            pools: vec![&pool],
            path: vec![a, b],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..1,
        };

        let recipient = Address::repeat_byte(0x55);
        let amount_out = U256::from(800u64);
        let amount_in_max = U256::from(1_200u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_uniswap_span_exact_out(
            &ctx(),
            &route,
            &span,
            amount_out,
            amount_in_max,
            recipient,
            false,
            false,
            deadline,
            true,
        )
        .expect("single-hop all-V4 exact-out must build");

        // tx envelope: Universal Router, zero value, matching chain.
        assert_eq!(prepared.tx.to, universal_router(), "tx.to must be UR");
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "exact-out ERC-20 has no value"
        );
        assert_eq!(prepared.tx.chain, chain_id());

        // Outer: single command [V4_SWAP], one input, deadline round-trips.
        let (commands, inputs, dl) = decode_outer(&prepared.tx.data);
        assert_eq!(commands.as_ref(), &[V4_SWAP], "commands must be [V4_SWAP]");
        assert_eq!(inputs.len(), 1, "single V4 exact-out command → one input");
        assert_eq!(dl, U256::from(deadline), "deadline round-trips");

        // Inner V4_SWAP input: actions == ACTIONS_EXACT_OUT, three params.
        let (actions, params) = decode_v4(&inputs[0]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_EXACT_OUT[..],
            "actions must be ACTIONS_EXACT_OUT [SWAP_EXACT_OUT_SINGLE, SETTLE_ALL, TAKE]"
        );
        assert_eq!(params.len(), 3, "V4 exact-out has 3 params");

        // params[0]: ExactOutputSingleParams — amountOut and amountInMaximum.
        let swap = ExactOutputSingleParams::abi_decode(&params[0])
            .expect("params[0] must decode as ExactOutputSingleParams");
        assert_eq!(
            swap.amountOut,
            u128::try_from(amount_out).unwrap(),
            "amountOut must equal amount_out"
        );
        assert_eq!(
            swap.amountInMaximum,
            u128::try_from(amount_in_max).unwrap(),
            "amountInMaximum must equal amount_in_max"
        );

        // params[1]: SETTLE_ALL (input currency, amount_in_max as U256).
        let (settle_cur, settle_amt) = <(Address, U256)>::abi_decode_params(&params[1])
            .expect("params[1] must decode as SETTLE_ALL (Address, U256)");
        assert_eq!(
            settle_cur,
            common::evm_addr(&a),
            "SETTLE_ALL currency must be the span input"
        );
        assert_eq!(
            settle_amt, amount_in_max,
            "SETTLE_ALL amount must be amount_in_max"
        );

        // params[2]: TAKE (output currency, recipient, OPEN_DELTA).
        let (take_cur, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2])
                .expect("params[2] must decode as TAKE (Address, Address, U256)");
        assert_eq!(
            take_cur,
            common::evm_addr(&b),
            "TAKE currency must be the span output"
        );
        assert_eq!(take_to, recipient, "TAKE must deliver to recipient");
        assert_eq!(take_amt, U256::ZERO, "TAKE amount must be OPEN_DELTA");

        // PreparedSwap field assertions.
        assert_eq!(
            prepared.min_received.asset, b,
            "min_received asset is span output"
        );
        assert_eq!(
            prepared.min_received.raw, amount_out,
            "min_received is amount_out"
        );
        let max_spent = prepared.max_spent.expect("exact-out must have max_spent");
        assert_eq!(max_spent.asset, a, "max_spent asset is span input");
        assert_eq!(max_spent.raw, amount_in_max, "max_spent is amount_in_max");
        let approval = prepared.approval.expect("exact-out ERC-20 needs approval");
        assert_eq!(approval.spender, permit2(), "approval spender is Permit2");
        assert_eq!(approval.token, a, "approval token is span input");
        assert_eq!(
            approval.min_allowance, amount_in_max,
            "approval allowance is amount_in_max"
        );
    }

    /// Multi-hop all-V4 exact-out → `Err(UnsupportedExactOut { kind: UniswapV4 })`.
    /// V4 multi-hop exact-out is deferred; `ExactOutPolicy::OrBetter` covers it later.
    #[test]
    fn all_v4_multi_hop_exact_out_returns_unsupported() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v4_pool(a, b);
        let p2 = v4_pool(b, c);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let err = build_uniswap_span_exact_out(
            &ctx(),
            &route,
            &span,
            U256::from(100u64),
            U256::from(200u64),
            Address::repeat_byte(0x77),
            false,
            false,
            1_700_000_000u64,
            true,
        )
        .expect_err("multi-hop V4 exact-out must return UnsupportedExactOut");

        assert!(
            matches!(
                err,
                BuildError::UnsupportedExactOut {
                    kind: PoolKind::UniswapV4
                }
            ),
            "expected UnsupportedExactOut {{ kind: UniswapV4 }}, got {err:?}"
        );
    }

    // ── Native wrap/unwrap at span edges ─────────────────────────────────

    /// native_in exact-in (all-V3 two-hop): WRAP_ETH is the first command;
    /// every hop draws from the router (payer_is_user=false, amount=CONTRACT_BALANCE);
    /// tx.value == amount_in; approval == None.
    #[test]
    fn native_in_exact_in_wraps_and_all_hops_draw_from_router() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x55);
        let amount_in = U256::from(1_000u64);
        let min_out = U256::from(900u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_uniswap_span(
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
        .expect("native-in V3 2-hop must build");

        // tx.value == amount_in (ETH sent as value); no Permit2 approval.
        assert_eq!(
            prepared.tx.value, amount_in,
            "tx.value must equal amount_in"
        );
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in"
        );

        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        // First command must be WRAP_ETH, then two V3 swaps.
        assert_eq!(
            commands.as_ref(),
            &[
                crate::execution::route_planner::WRAP_ETH,
                V3_SWAP_EXACT_IN,
                V3_SWAP_EXACT_IN,
            ],
            "commands must be [WRAP_ETH, V3_SWAP_EXACT_IN, V3_SWAP_EXACT_IN]"
        );
        assert_eq!(inputs.len(), 3, "WRAP_ETH + two hops = three inputs");

        // WRAP_ETH input: (ADDRESS_THIS, amount_in).
        let (wrap_recip, wrap_amt) = <(Address, U256)>::abi_decode_params(&inputs[0])
            .expect("WRAP_ETH input must decode as (Address, U256)");
        assert_eq!(
            wrap_recip, ADDRESS_THIS,
            "WRAP_ETH recipient must be ADDRESS_THIS"
        );
        assert_eq!(wrap_amt, amount_in, "WRAP_ETH amount must be amount_in");

        // Hop 0 (first V3 swap): payer_is_user=false, amount=CONTRACT_BALANCE.
        let (r0, amt0, lim0, _path0, payer0) = decode_v3(&inputs[1]);
        assert_eq!(r0, ADDRESS_THIS, "hop0 delivers to router (not last)");
        assert_eq!(amt0, CONTRACT_BALANCE, "hop0 draws from router (native_in)");
        assert_eq!(lim0, U256::ZERO, "hop0 carries no min (not last)");
        assert!(!payer0, "hop0 payer is the router, not the user");

        // Hop 1 (last V3 swap): payer_is_user=false, amount=CONTRACT_BALANCE, delivers to recipient.
        let (r1, amt1, lim1, _path1, payer1) = decode_v3(&inputs[2]);
        assert_eq!(r1, recipient, "last hop delivers to recipient");
        assert_eq!(
            amt1, CONTRACT_BALANCE,
            "last hop draws from router (native_in)"
        );
        assert_eq!(lim1, min_out, "last hop carries min_out");
        assert!(!payer1, "last hop payer is the router");
    }

    /// native_out exact-in (all-V3 two-hop): last command is UNWRAP_WETH → (recipient, min_out);
    /// last swap's recipient is ADDRESS_THIS; tx.value == 0; approval is Some (ERC-20 input).
    #[test]
    fn native_out_exact_in_last_hop_to_router_then_unwraps() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x55);
        let amount_in = U256::from(1_000u64);
        let min_out = U256::from(900u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_uniswap_span(
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
        .expect("native-out V3 2-hop must build");

        // tx.value == 0 (ERC-20 input); Permit2 approval is Some.
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for native-out"
        );
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(approval.spender, permit2(), "spender is Permit2");
        assert_eq!(approval.token, a, "approval token is span input");
        assert_eq!(approval.min_allowance, amount_in, "allowance is amount_in");

        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        // Two V3 swaps then UNWRAP_WETH.
        assert_eq!(
            commands.as_ref(),
            &[
                V3_SWAP_EXACT_IN,
                V3_SWAP_EXACT_IN,
                crate::execution::route_planner::UNWRAP_WETH,
            ],
            "commands must be [V3_SWAP_EXACT_IN, V3_SWAP_EXACT_IN, UNWRAP_WETH]"
        );
        assert_eq!(inputs.len(), 3, "two swaps + UNWRAP_WETH = three inputs");

        // Hop 0 (first): payer_is_user=true, amount=amount_in, recipient=ADDRESS_THIS.
        let (r0, amt0, _lim0, _path0, payer0) = decode_v3(&inputs[0]);
        assert_eq!(r0, ADDRESS_THIS, "hop0 delivers to router");
        assert_eq!(amt0, amount_in, "hop0 amount is amount_in");
        assert!(payer0, "hop0 payer is user");

        // Hop 1 (last): recipient is ADDRESS_THIS (so UNWRAP_WETH can access WETH), carries min_out.
        let (r1, _amt1, lim1, _path1, _payer1) = decode_v3(&inputs[1]);
        assert_eq!(
            r1, ADDRESS_THIS,
            "last hop recipient is ADDRESS_THIS (native_out)"
        );
        assert_eq!(lim1, min_out, "last hop carries min_out");

        // UNWRAP_WETH input: (recipient, min_out).
        let (unwrap_recip, unwrap_min) = <(Address, U256)>::abi_decode_params(&inputs[2])
            .expect("UNWRAP_WETH input must decode as (Address, U256)");
        assert_eq!(
            unwrap_recip, recipient,
            "UNWRAP_WETH recipient must be caller-supplied recipient"
        );
        assert_eq!(unwrap_min, min_out, "UNWRAP_WETH min must be min_out");

        // min_received is the chain native asset (address(0) on the chain).
        assert_eq!(
            prepared.min_received.asset,
            AssetId::new(chain_id(), B256::ZERO),
            "min_received asset must be chain native (B256::ZERO)"
        );
        assert_eq!(
            prepared.min_received.raw, min_out,
            "min_received raw is min_out"
        );
    }

    /// exact-out with native_in=true → Err(UnsupportedProtocol).
    /// Native-ETH exact-out edges are deferred; they land with on-chain fork proofs later.
    #[test]
    fn exact_out_native_in_returns_unsupported_protocol() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let err = build_uniswap_span_exact_out(
            &ctx(),
            &route,
            &span,
            U256::from(800u64),
            U256::from(1_200u64),
            Address::repeat_byte(0x55),
            true,  // native_in
            false, // native_out
            1_700_000_000u64,
            true,
        )
        .expect_err("exact-out native_in must return Err");

        assert_eq!(
            err,
            BuildError::UnsupportedProtocol,
            "native_in exact-out must yield UnsupportedProtocol"
        );
    }

    /// exact-out with native_out=true → Err(UnsupportedProtocol).
    /// Native-ETH exact-out edges are deferred; they land with on-chain fork proofs later.
    #[test]
    fn exact_out_native_out_returns_unsupported_protocol() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: crate::execution::routing::RouterKind::UniswapUniversal,
            pools: 0..2,
        };

        let err = build_uniswap_span_exact_out(
            &ctx(),
            &route,
            &span,
            U256::from(800u64),
            U256::from(1_200u64),
            Address::repeat_byte(0x55),
            false, // native_in
            true,  // native_out
            1_700_000_000u64,
            true,
        )
        .expect_err("exact-out native_out must return Err");

        assert_eq!(
            err,
            BuildError::UnsupportedProtocol,
            "native_out exact-out must yield UnsupportedProtocol"
        );
    }
}
