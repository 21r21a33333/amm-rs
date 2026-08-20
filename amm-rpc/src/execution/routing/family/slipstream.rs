//! Aerodrome Slipstream multi-hop span builder: exact-in and exact-out
//! (`build_slipstream_span` / `build_slipstream_span_exact_out`) with a
//! path-encoded router ABI.
//!
//! ## Path encoding
//! Slipstream's multi-hop router uses the same packed-bytes path convention as
//! Uniswap V3, but the per-hop field is `int24 tickSpacing` (3 bytes big-endian)
//! instead of `uint24 fee`. `slipstream_path` encodes:
//!
//! ```text
//! token(20) ‖ tickSpacing(int24, 3 bytes BE) ‖ token(20) ‖ …
//! ```
//!
//! For a positive `i32` tick spacing, the 3 bytes are the low 3 bytes of its
//! two's-complement representation: `(ts as u32).to_be_bytes()[1..4]`.
//! Negative tick spacings (pathological — never occur on a real Slipstream
//! deployment) are encoded in two's-complement the same way, matching how the
//! router's Solidity decoder interprets `int24`.
//!
//! ## Exact-in native table (mirrors single-hop `swaprouter02.rs`)
//! - **ERC-20 both:** `exactInput(ExactInputParams{ … })`, `tx.value = 0`,
//!   ERC-20 approval on span input to `ctx.router_slipstream()`.
//! - **native-in:** raw `exactInput`, `tx.value = amount_in`, no approval.
//! - **native-out:** `multicall([exactInput(recipient=ADDRESS_THIS),
//!   unwrapWETH9(min_out, recipient)])`, `tx.value = 0`, ERC-20 approval.
//!
//! `ADDRESS_THIS = address(2)` — the same sentinel used by `swaprouter02.rs`.
//! Slipstream's path-encoded router *does* resolve this sentinel so the router
//! holds the WETH between the swap and the `unwrapWETH9` call.  (The single-hop
//! router did not; see the `custody_recipient` override in
//! `protocols/slipstream.rs`.  The path-encoded router is a different ABI
//! surface and does support `ADDRESS_THIS`.)
//!
//! ## Exact-out
//! Slipstream CL supports exact-out: `exactOutput` with the reversed path
//! `[out ‖ ts_last ‖ … ‖ ts_first ‖ in]`. Native edges follow the same table
//! as the Uniswap V3 span builder (native-in: `multicall([exactOutput, refundETH])`;
//! native-out: `multicall([exactOutput(recipient=ADDRESS_THIS), unwrapWETH9])`).

use core::any::Any;

use alloy::primitives::{Address, Bytes, U256};
use alloy::{sol, sol_types::SolCall};
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::protocols::aerodrome::slipstream::AerodromeSlipstreamPool;
use amm_core::traits::pool::Pool;

use crate::execution::config::ChainConfig;
use crate::execution::error::BuildError;
use crate::execution::multicall::encode_multicall;
use crate::execution::prepared::PreparedSwap;
use crate::execution::protocols::common;
use crate::execution::routing::{Route, RouterSpan};
use crate::execution::types::UnsignedTx;

sol! {
    /// Minimal path-encoded ABI surface for the Aerodrome Slipstream router.
    ///
    /// These function signatures are **not** present in the existing single-hop
    /// Slipstream ABI (`protocols/slipstream.rs`). The multi-hop path-encoded
    /// surface is declared here for the span builder; it is scoped to this module
    /// to avoid collisions with the single-hop `ISlipstreamRouter`.
    ///
    /// Struct layout mirrors Uniswap V3's `ISwapRouter.ExactInputParams` /
    /// `ExactOutputParams`, with `deadline` embedded (as in Slipstream's
    /// single-hop ABI — the router does not use a separate multicall deadline
    /// overload for these calls).
    interface ISlipstreamRouterPath {
        struct ExactInputParams {
            bytes path;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
        }
        struct ExactOutputParams {
            bytes path;
            address recipient;
            uint256 deadline;
            uint256 amountOut;
            uint256 amountInMaximum;
        }
        function exactInput(ExactInputParams params) external payable returns (uint256 amountOut);
        function exactOutput(ExactOutputParams params) external payable returns (uint256 amountIn);

        // Periphery helpers shared with the single-hop router — redeclared here
        // so this module is self-contained and imports nothing from
        // `protocols/slipstream.rs`.
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
        function refundETH() external payable;
    }
}

/// Sentinel: `address(2)` — instructs the router to hold funds for a following
/// `unwrapWETH9`. The path-encoded Slipstream router resolves this sentinel
/// (unlike the single-hop router, which requires its own address instead).
const ADDRESS_THIS: Address = Address::with_last_byte(2);

// ─── Path encoder ─────────────────────────────────────────────────────────────

/// Encode a Slipstream multi-hop path: `token(20) ‖ tickSpacing(int24, 3 bytes BE) ‖ token(20) ‖ …`.
///
/// `tick_spacings.len() == tokens.len() - 1` (one spacing per hop). When
/// `reversed` is `true` the path is emitted back-to-front with the tick spacings
/// reversed too (exact-out requires the reversed path, mirroring `v3_path`).
///
/// The int24 encoding: a positive tick spacing `ts` is packed as the low 3 bytes
/// of `(ts as u32).to_be_bytes()`, i.e. the byte at index 1 through 3. Negative
/// values are encoded in two's-complement the same way — `(ts as u32)` reinterprets
/// the bits, then the low 3 bytes are taken. This matches how Solidity's `int24`
/// is stored in memory.
pub(crate) fn slipstream_path(tokens: &[Address], tick_spacings: &[i32], reversed: bool) -> Bytes {
    debug_assert_eq!(
        tick_spacings.len(),
        tokens.len().saturating_sub(1),
        "tick_spacings.len() must be tokens.len() - 1"
    );

    let mut out = Vec::with_capacity(tokens.len() * 20 + tick_spacings.len() * 3);

    // Helper: push one 20-byte address, then optionally the 3-byte int24 spacing.
    let push = |buf: &mut Vec<u8>, tok: &Address, ts: Option<i32>| {
        buf.extend_from_slice(tok.as_slice());
        if let Some(t) = ts {
            // int24 packed as low 3 bytes of two's-complement u32 big-endian.
            // `t as u32` reinterprets the bits; [1..4] gives bytes 1, 2, 3 of
            // the big-endian u32 (the low 24 bits).
            buf.extend_from_slice(&(t as u32).to_be_bytes()[1..4]);
        }
    };

    match reversed {
        false => {
            // Forward: token₀ ‖ ts₀ ‖ token₁ ‖ ts₁ ‖ … ‖ tokenₙ (no trailing ts).
            for (i, tok) in tokens.iter().enumerate() {
                push(&mut out, tok, tick_spacings.get(i).copied());
            }
        }
        true => {
            // Reversed: tokenₙ ‖ tsₙ₋₁ ‖ … ‖ ts₀ ‖ token₀ (no trailing ts).
            // Iterating in reverse: at index `i` (from the back), the spacing
            // between token[i] and token[i-1] is tick_spacings[i-1].
            for (i, tok) in tokens.iter().enumerate().rev() {
                // The spacing *before* token[i] in the forward direction is
                // tick_spacings[i-1]. In reversed order this becomes the spacing
                // *after* token[i], so we emit it immediately after the token.
                // For i==0 there is no preceding spacing — it is the final token.
                let ts = i.checked_sub(1).and_then(|j| tick_spacings.get(j).copied());
                push(&mut out, tok, ts);
            }
        }
    }

    out.into()
}

// ─── Downcast helper ──────────────────────────────────────────────────────────

/// Downcast a `&dyn Pool` to `&AerodromeSlipstreamPool`.
///
/// Mirrors the `downcast` helper in `family/uniswap_ur.rs`: the partition layer
/// guarantees every pool in a `RouterKind::Slipstream` span is a Slipstream pool,
/// so failure here indicates a routing invariant violation rather than user error.
fn downcast_slipstream(pool: &dyn Pool) -> Result<&AerodromeSlipstreamPool, BuildError> {
    (pool as &dyn Any)
        .downcast_ref::<AerodromeSlipstreamPool>()
        .ok_or(BuildError::UnsupportedProtocol)
}

// ─── Exact-in span builder ─────────────────────────────────────────────────────

/// Build one Slipstream router transaction for a same-router Slipstream span (exact-in).
///
/// Collects the token path and per-hop tick spacings from the span, encodes the
/// Slipstream path bytes, and dispatches to the native-ETH table:
///
/// - **ERC-20 → ERC-20:** `exactInput(ExactInputParams{ path, recipient, deadline,
///   amountIn, amountOutMinimum })`, `tx.value = 0`, ERC-20 approval on span input.
/// - **native-in (ETH → token):** raw `exactInput` with `tx.value = amount_in`,
///   no approval.
/// - **native-out (token → ETH):** `multicall([exactInput(recipient=ADDRESS_THIS),
///   unwrapWETH9(min_out, recipient)])`, `tx.value = 0`, ERC-20 approval.
/// - **native → native:** [`BuildError::NativeMismatch`].
///
/// `tx.to = ctx.router_slipstream()`. The deadline is embedded inside the
/// `ExactInputParams` struct (matching the single-hop router's convention; the
/// path-encoded router uses the same in-struct deadline, no multicall deadline
/// overload is used).
///
/// `is_final` is accepted for signature parity with sibling builders but is not
/// branched on — delivery is determined by `recipient`.
///
/// # Errors
/// - [`BuildError::NativeMismatch`] — both `native_in` and `native_out` are `true`.
/// - [`BuildError::MissingChainConfig`] — Slipstream router not configured.
/// - [`BuildError::UnsupportedProtocol`] — a pool in the span cannot be downcast
///   to `AerodromeSlipstreamPool` (routing invariant violation).
//
// `dead_code`: entry point called by the plan dispatcher. Exercised by this
// module's tests.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_slipstream_span(
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
    // Mirror the Aerodrome builder: reject native→native early.
    if native_in && native_out {
        return Err(BuildError::NativeMismatch);
    }

    let start = span.pools.start;
    let end = span.pools.end;

    // partition() guarantees well-formed spans; assert the trust boundary.
    debug_assert!(span.pools.end <= route.pools.len() && span.pools.end < route.path.len());

    let router = ctx.router_slipstream()?;

    // Collect the token path and per-hop tick spacings.
    // `route.path[start..=end]` has `end - start + 1` entries; there are
    // `end - start` pools (= hops), each contributing one tick spacing.
    let tokens: Vec<Address> = (start..=end)
        .map(|i| common::evm_addr(&route.path[i]))
        .collect();
    let tick_spacings: Vec<i32> = span
        .pools
        .clone()
        .map(|i| Ok(downcast_slipstream(route.pools[i])?.tick_spacing()))
        .collect::<Result<_, BuildError>>()?;

    let path = slipstream_path(&tokens, &tick_spacings, false);

    // The span's input and output assets for approval and min_received labelling.
    let seg_input = route.path[start];
    let seg_output = route.path[end];

    let deadline_u256 = U256::from(deadline);

    match (native_in, native_out) {
        // Both native is a configuration error — caught above; unreachable here.
        (true, true) => Err(BuildError::NativeMismatch),

        // Native-in (ETH → token): tx.value = amount_in, no ERC-20 approval.
        // The recipient in the call is the final user recipient directly.
        (true, false) => {
            let call = ISlipstreamRouterPath::exactInputCall {
                params: ISlipstreamRouterPath::ExactInputParams {
                    path,
                    recipient,
                    deadline: deadline_u256,
                    amountIn: amount_in,
                    amountOutMinimum: min_out,
                },
            };
            // Single call: encode_multicall with no deadline passes a single call through raw.
            let data = encode_multicall(None, vec![call.abi_encode().into()]);
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data,
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

        // Native-out (token → ETH): swap to ADDRESS_THIS, then unwrapWETH9 to user.
        // The path-encoded Slipstream router supports the ADDRESS_THIS sentinel.
        (false, true) => {
            let swap_call = ISlipstreamRouterPath::exactInputCall {
                params: ISlipstreamRouterPath::ExactInputParams {
                    path,
                    recipient: ADDRESS_THIS,
                    deadline: deadline_u256,
                    amountIn: amount_in,
                    amountOutMinimum: min_out,
                },
            };
            let unwrap_call = ISlipstreamRouterPath::unwrapWETH9Call {
                amountMinimum: min_out,
                recipient,
            };
            // Two inner calls: multicall(bytes[]) — no deadline overload (in-struct).
            let data = encode_multicall(
                None,
                vec![
                    swap_call.abi_encode().into(),
                    unwrap_call.abi_encode().into(),
                ],
            );
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data,
                value: U256::ZERO,
            };
            // native-out: report min_received as the chain native asset (B256::ZERO).
            Ok(PreparedSwap {
                tx,
                min_received: AssetAmount::new(
                    AssetId::new(ChainId(ctx.chain.0), alloy::primitives::B256::ZERO),
                    min_out,
                ),
                max_spent: None,
                approval: Some(common::erc20_approval(router, seg_input, amount_in)),
                price_impact: None,
            })
        }

        // ERC-20 → ERC-20: single exactInput call, no wrap/unwrap.
        (false, false) => {
            let call = ISlipstreamRouterPath::exactInputCall {
                params: ISlipstreamRouterPath::ExactInputParams {
                    path,
                    recipient,
                    deadline: deadline_u256,
                    amountIn: amount_in,
                    amountOutMinimum: min_out,
                },
            };
            let data = encode_multicall(None, vec![call.abi_encode().into()]);
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data,
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

// ─── Exact-out span builder ────────────────────────────────────────────────────

/// Build one Slipstream router transaction for a same-router Slipstream span (exact-out).
///
/// Slipstream CL supports exact-out via `exactOutput`, which takes the **reversed**
/// path `[out ‖ ts_last ‖ … ‖ ts_first ‖ in]`. The router walks backward,
/// pulling at most `amount_in_max` from the user.
///
/// Native-ETH table (mirrors the Uniswap V3 span builder's convention):
/// - **ERC-20 → ERC-20:** `exactOutput(ExactOutputParams{ path_reversed, recipient,
///   deadline, amountOut, amountInMaximum })`, `tx.value = 0`, ERC-20 approval.
/// - **native-in (ETH → token):** `multicall([exactOutput(recipient=user),
///   refundETH()])`, `tx.value = amount_in_max`, no approval.
/// - **native-out (token → ETH):** `multicall([exactOutput(recipient=ADDRESS_THIS),
///   unwrapWETH9(amount_out, recipient)])`, `tx.value = 0`, ERC-20 approval.
/// - **native → native:** [`BuildError::NativeMismatch`].
///
/// `is_final` is accepted for signature parity with the exact-in builder.
///
/// # Errors
/// - [`BuildError::NativeMismatch`] — both `native_in` and `native_out` are `true`.
/// - [`BuildError::MissingChainConfig`] — Slipstream router not configured.
/// - [`BuildError::UnsupportedProtocol`] — a pool in the span cannot be downcast.
//
// `dead_code`: reached only through the executor's Strict exact-out dispatch;
// exercised directly by this module's tests.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_slipstream_span_exact_out(
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
    if native_in && native_out {
        return Err(BuildError::NativeMismatch);
    }

    let start = span.pools.start;
    let end = span.pools.end;

    debug_assert!(span.pools.end <= route.pools.len() && span.pools.end < route.path.len());

    let router = ctx.router_slipstream()?;

    // Collect the forward token path and tick spacings, then reverse for exact-out.
    let tokens: Vec<Address> = (start..=end)
        .map(|i| common::evm_addr(&route.path[i]))
        .collect();
    let tick_spacings: Vec<i32> = span
        .pools
        .clone()
        .map(|i| Ok(downcast_slipstream(route.pools[i])?.tick_spacing()))
        .collect::<Result<_, BuildError>>()?;

    // Exact-out uses the reversed path: [out ‖ ts_last ‖ … ‖ ts_first ‖ in].
    let path_reversed = slipstream_path(&tokens, &tick_spacings, true);

    let seg_input = route.path[start];
    let seg_output = route.path[end];

    let deadline_u256 = U256::from(deadline);

    match (native_in, native_out) {
        (true, true) => Err(BuildError::NativeMismatch),

        // Native-in exact-out (ETH → token): tx.value = amount_in_max;
        // refundETH returns any unspent ETH.
        (true, false) => {
            let swap_call = ISlipstreamRouterPath::exactOutputCall {
                params: ISlipstreamRouterPath::ExactOutputParams {
                    path: path_reversed,
                    recipient,
                    deadline: deadline_u256,
                    amountOut: amount_out,
                    amountInMaximum: amount_in_max,
                },
            };
            let refund_call = ISlipstreamRouterPath::refundETHCall {};
            let data = encode_multicall(
                None,
                vec![
                    swap_call.abi_encode().into(),
                    refund_call.abi_encode().into(),
                ],
            );
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data,
                value: amount_in_max,
            };
            Ok(PreparedSwap {
                tx,
                min_received: AssetAmount::new(seg_output, amount_out),
                max_spent: Some(AssetAmount::new(
                    // native-in: the input asset is WETH (same as ctx.weth); we
                    // surface max_spent against the WETH asset since seg_input
                    // resolves to WETH in the caller's path.
                    seg_input,
                    amount_in_max,
                )),
                approval: None,
                price_impact: None,
            })
        }

        // Native-out exact-out (token → ETH): swap to ADDRESS_THIS so router
        // holds the WETH, then unwrapWETH9 forwards exactly amount_out as ETH.
        (false, true) => {
            let swap_call = ISlipstreamRouterPath::exactOutputCall {
                params: ISlipstreamRouterPath::ExactOutputParams {
                    path: path_reversed,
                    recipient: ADDRESS_THIS,
                    deadline: deadline_u256,
                    amountOut: amount_out,
                    amountInMaximum: amount_in_max,
                },
            };
            // Unwrap exactly the target amount (not the min — this is exact-out).
            let unwrap_call = ISlipstreamRouterPath::unwrapWETH9Call {
                amountMinimum: amount_out,
                recipient,
            };
            let data = encode_multicall(
                None,
                vec![
                    swap_call.abi_encode().into(),
                    unwrap_call.abi_encode().into(),
                ],
            );
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data,
                value: U256::ZERO,
            };
            Ok(PreparedSwap {
                tx,
                // native-out: native asset is address(0) on the chain.
                min_received: AssetAmount::new(
                    AssetId::new(ChainId(ctx.chain.0), alloy::primitives::B256::ZERO),
                    amount_out,
                ),
                max_spent: Some(AssetAmount::new(seg_input, amount_in_max)),
                approval: Some(common::erc20_approval(router, seg_input, amount_in_max)),
                price_impact: None,
            })
        }

        // ERC-20 → ERC-20 exact-out: single exactOutput call, no wrap/unwrap.
        (false, false) => {
            let call = ISlipstreamRouterPath::exactOutputCall {
                params: ISlipstreamRouterPath::ExactOutputParams {
                    path: path_reversed,
                    recipient,
                    deadline: deadline_u256,
                    amountOut: amount_out,
                    amountInMaximum: amount_in_max,
                },
            };
            let data = encode_multicall(None, vec![call.abi_encode().into()]);
            let tx = UnsignedTx {
                chain: ctx.chain,
                to: router,
                data,
                value: U256::ZERO,
            };
            Ok(PreparedSwap {
                tx,
                min_received: AssetAmount::new(seg_output, amount_out),
                max_spent: Some(AssetAmount::new(seg_input, amount_in_max)),
                approval: Some(common::erc20_approval(router, seg_input, amount_in_max)),
                price_impact: None,
            })
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use alloy::primitives::{Address, B256, U256, address};
    use alloy::sol_types::SolCall;
    use amm_core::primitives::asset::{AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::protocols::aerodrome::slipstream::{AerodromeSlipstreamPool, TickData};

    use crate::execution::config::{ChainConfig, Routers};
    use crate::execution::multicall::IMulticall;
    use crate::execution::routing::RouterKind;
    use crate::execution::types::TradeType;

    // ── Fixtures ─────────────────────────────────────────────────────────────

    const ROUTER: Address = address!("0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5");

    fn chain_id() -> ChainId {
        ChainId(8453)
    }

    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    fn weth_base() -> AssetId {
        AssetId::new(
            chain_id(),
            B256::left_padding_from(&[
                0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x06,
            ]),
        )
    }

    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth_base()).with_routers(Routers {
            slipstream: Some(ROUTER),
            ..Default::default()
        })
    }

    /// Build a minimal Slipstream pool over `a` and `b` with the given tick spacing.
    fn pool(a: AssetId, b: AssetId, tick_spacing: i32) -> AerodromeSlipstreamPool {
        AerodromeSlipstreamPool::new(
            PoolId::new("8453:slipstream:0x"),
            [a, b],
            U256::from(1u64) << 96,
            0,
            0,
            400,
            TickData::from_ticks(tick_spacing, vec![]),
        )
    }

    // ── slipstream_path ───────────────────────────────────────────────────────

    /// Forward path: `token₀ ‖ ts₀(int24 BE) ‖ token₁ ‖ ts₁ ‖ token₂`.
    /// spacing 100 = 0x000064, spacing 200 = 0x0000c8.
    #[test]
    fn slipstream_path_forward_two_hop() {
        let t0 = Address::with_last_byte(0xA0);
        let t1 = Address::with_last_byte(0xB1);
        let t2 = Address::with_last_byte(0xC2);

        let fwd = slipstream_path(&[t0, t1, t2], &[100, 200], false);
        // 2 tokens × 20 + 2 spacings × 3 + 1 trailing token × 20 = 66 bytes.
        assert_eq!(fwd.len(), 66, "2-hop forward path must be 66 bytes");

        assert_eq!(&fwd[0..20], t0.as_slice(), "first token must be t0");
        assert_eq!(&fwd[20..23], &[0x00, 0x00, 0x64], "spacing 100 = 0x000064");
        assert_eq!(&fwd[23..43], t1.as_slice(), "second token must be t1");
        assert_eq!(&fwd[43..46], &[0x00, 0x00, 0xc8], "spacing 200 = 0x0000c8");
        assert_eq!(&fwd[46..66], t2.as_slice(), "last token must be t2");
    }

    /// Reversed path (exact-out): `token₂ ‖ ts₁ ‖ token₁ ‖ ts₀ ‖ token₀`.
    #[test]
    fn slipstream_path_reversed_two_hop() {
        let t0 = Address::with_last_byte(0xA0);
        let t1 = Address::with_last_byte(0xB1);
        let t2 = Address::with_last_byte(0xC2);

        let rev = slipstream_path(&[t0, t1, t2], &[100, 200], true);
        assert_eq!(rev.len(), 66, "2-hop reversed path must be 66 bytes");

        assert_eq!(
            &rev[0..20],
            t2.as_slice(),
            "reversed first token must be t2"
        );
        assert_eq!(
            &rev[20..23],
            &[0x00, 0x00, 0xc8],
            "reversed ts[1] = 200 = 0x0000c8"
        );
        assert_eq!(
            &rev[23..43],
            t1.as_slice(),
            "reversed second token must be t1"
        );
        assert_eq!(
            &rev[43..46],
            &[0x00, 0x00, 0x64],
            "reversed ts[0] = 100 = 0x000064"
        );
        assert_eq!(
            &rev[46..66],
            t0.as_slice(),
            "reversed last token must be t0"
        );
    }

    /// Single-hop forward path: `token₀ ‖ ts₀ ‖ token₁` (43 bytes).
    #[test]
    fn slipstream_path_single_hop_forward() {
        let t0 = Address::with_last_byte(0x10);
        let t1 = Address::with_last_byte(0x20);

        let fwd = slipstream_path(&[t0, t1], &[200], false);
        assert_eq!(fwd.len(), 43, "1-hop path must be 43 bytes");
        assert_eq!(&fwd[0..20], t0.as_slice());
        assert_eq!(&fwd[20..23], &[0x00, 0x00, 0xc8], "spacing 200 = 0x0000c8");
        assert_eq!(&fwd[23..43], t1.as_slice());
    }

    /// Tick spacing 1 encodes as `0x000001`.
    #[test]
    fn slipstream_path_tick_spacing_one() {
        let t0 = Address::with_last_byte(0x01);
        let t1 = Address::with_last_byte(0x02);

        let fwd = slipstream_path(&[t0, t1], &[1], false);
        assert_eq!(&fwd[20..23], &[0x00, 0x00, 0x01], "spacing 1 = 0x000001");
    }

    // ── 2-hop exact-in ERC-20 ─────────────────────────────────────────────────

    /// A 2-hop Slipstream exact-in route: decode `exactInput`, assert path bytes
    /// (token‖ts‖token‖ts‖token), `amountIn`, `amountOutMinimum == min_out`,
    /// `recipient`, `tx.to == router`.
    #[test]
    fn two_hop_exact_in_erc20_encodes_exact_input() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = pool(a, b, 100);
        let p2 = pool(b, c, 200);
        let route = Route {
            pools: vec![&p1 as &dyn Pool, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Slipstream,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x55);
        let amount_in = U256::from(1_000u64);
        let min_out = U256::from(900u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_slipstream_span(
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
        .expect("2-hop ERC-20 exact-in must build");

        // Transaction envelope.
        assert_eq!(
            prepared.tx.to, ROUTER,
            "tx.to must be the Slipstream router"
        );
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "ERC-20 path has no ETH value"
        );
        assert_eq!(prepared.tx.chain, chain_id());

        // Decode the single exactInput call (no multicall wrapper for ERC-20).
        let inner = ISlipstreamRouterPath::exactInputCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as exactInput");

        // Path: token a ‖ ts(100) ‖ token b ‖ ts(200) ‖ token c.
        let path = &inner.params.path;
        assert_eq!(path.len(), 66, "2-hop path must be 66 bytes");
        assert_eq!(
            &path[0..20],
            common::evm_addr(&a).as_slice(),
            "first token is a"
        );
        assert_eq!(&path[20..23], &[0x00, 0x00, 0x64], "ts₀ = 100 = 0x000064");
        assert_eq!(
            &path[23..43],
            common::evm_addr(&b).as_slice(),
            "middle token is b"
        );
        assert_eq!(&path[43..46], &[0x00, 0x00, 0xc8], "ts₁ = 200 = 0x0000c8");
        assert_eq!(
            &path[46..66],
            common::evm_addr(&c).as_slice(),
            "last token is c"
        );

        assert_eq!(
            inner.params.recipient, recipient,
            "recipient must round-trip"
        );
        assert_eq!(
            inner.params.deadline,
            U256::from(deadline),
            "deadline round-trips"
        );
        assert_eq!(
            inner.params.amountIn, amount_in,
            "amountIn must equal amount_in"
        );
        assert_eq!(
            inner.params.amountOutMinimum, min_out,
            "amountOutMinimum must be min_out"
        );

        // PreparedSwap fields.
        assert_eq!(
            prepared.min_received.raw, min_out,
            "min_received.raw == min_out"
        );
        assert_eq!(
            prepared.min_received.asset, c,
            "min_received.asset is span output c"
        );
        assert!(prepared.max_spent.is_none(), "exact-in has no max_spent");

        // ERC-20 approval on span input `a` for the router.
        let approval = prepared.approval.expect("ERC-20 input must have approval");
        assert_eq!(
            approval.spender, ROUTER,
            "approval.spender must be the router"
        );
        assert_eq!(approval.token, a, "approval.token must be span input a");
        assert_eq!(
            approval.min_allowance, amount_in,
            "allowance must equal amount_in"
        );
        assert!(!approval.reset_first, "reset_first must be false");
    }

    // ── 2-hop exact-out ERC-20 ────────────────────────────────────────────────

    /// A 2-hop exact-out: `exactOutput` with reversed path + `amountInMaximum`.
    #[test]
    fn two_hop_exact_out_erc20_encodes_exact_output_with_reversed_path() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = pool(a, b, 100);
        let p2 = pool(b, c, 200);
        let route = Route {
            pools: vec![&p1 as &dyn Pool, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: RouterKind::Slipstream,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x66);
        let amount_out = U256::from(800u64);
        let amount_in_max = U256::from(1_200u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_slipstream_span_exact_out(
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
        .expect("2-hop ERC-20 exact-out must build");

        assert_eq!(
            prepared.tx.to, ROUTER,
            "tx.to must be the Slipstream router"
        );
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "ERC-20 exact-out has no ETH value"
        );

        // Decode as exactOutput.
        let inner = ISlipstreamRouterPath::exactOutputCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as exactOutput");

        // Reversed path: token c ‖ ts₁(200) ‖ token b ‖ ts₀(100) ‖ token a.
        let path = &inner.params.path;
        assert_eq!(path.len(), 66, "2-hop reversed path must be 66 bytes");
        assert_eq!(
            &path[0..20],
            common::evm_addr(&c).as_slice(),
            "reversed[0] is c"
        );
        assert_eq!(&path[20..23], &[0x00, 0x00, 0xc8], "reversed ts[1] = 200");
        assert_eq!(
            &path[23..43],
            common::evm_addr(&b).as_slice(),
            "reversed[1] is b"
        );
        assert_eq!(&path[43..46], &[0x00, 0x00, 0x64], "reversed ts[0] = 100");
        assert_eq!(
            &path[46..66],
            common::evm_addr(&a).as_slice(),
            "reversed[2] is a"
        );

        assert_eq!(
            inner.params.recipient, recipient,
            "recipient must round-trip"
        );
        assert_eq!(
            inner.params.deadline,
            U256::from(deadline),
            "deadline round-trips"
        );
        assert_eq!(
            inner.params.amountOut, amount_out,
            "amountOut must equal amount_out"
        );
        assert_eq!(
            inner.params.amountInMaximum, amount_in_max,
            "amountInMaximum must be amount_in_max"
        );

        // PreparedSwap fields.
        assert_eq!(
            prepared.min_received.raw, amount_out,
            "min_received.raw == amount_out"
        );
        assert_eq!(
            prepared.min_received.asset, c,
            "min_received.asset is span output c"
        );
        let max_spent = prepared.max_spent.expect("exact-out must have max_spent");
        assert_eq!(
            max_spent.raw, amount_in_max,
            "max_spent.raw == amount_in_max"
        );
        assert_eq!(max_spent.asset, a, "max_spent.asset is span input a");

        let approval = prepared
            .approval
            .expect("ERC-20 exact-out must have approval");
        assert_eq!(
            approval.spender, ROUTER,
            "approval.spender must be the router"
        );
        assert_eq!(approval.token, a, "approval.token must be span input a");
        assert_eq!(
            approval.min_allowance, amount_in_max,
            "allowance == amount_in_max"
        );
        assert!(!approval.reset_first);
    }

    // ── native-out exact-in (token → ETH) ────────────────────────────────────

    /// A native-out 2-hop span: multicall with `exactInput(recipient=ADDRESS_THIS)`
    /// and `unwrapWETH9(min_out, recipient)`.
    #[test]
    fn two_hop_native_out_exact_in_encodes_multicall_with_unwrap() {
        let a = asset(0x11);
        let b = asset(0x22);
        let w = weth_base();
        let p1 = pool(a, b, 100);
        let p2 = pool(b, w, 200);
        let route = Route {
            pools: vec![&p1 as &dyn Pool, &p2],
            path: vec![a, b, w],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Slipstream,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x88);
        let amount_in = U256::from(3_000u64);
        let min_out = U256::from(2_700u64);
        let deadline = 1_800_000_000u64;

        let prepared = build_slipstream_span(
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
        .expect("native-out exact-in span must build");

        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "ERC-20 input has no ETH value"
        );

        // ERC-20 approval on span input `a`.
        let approval = prepared.approval.expect("ERC-20 input must have approval");
        assert_eq!(approval.token, a, "approval.token must be span input a");
        assert_eq!(approval.spender, ROUTER, "spender must be the router");
        assert_eq!(approval.min_allowance, amount_in, "allowance == amount_in");

        // Outer: multicall(bytes[]) — selector 0xac9650d8.
        assert_eq!(
            &prepared.tx.data[..4],
            &[0xac, 0x96, 0x50, 0xd8],
            "outer selector must be multicall(bytes[]) for native-out"
        );
        let outer = IMulticall::multicall_0Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_0Call");
        assert_eq!(outer.data.len(), 2, "native-out exact-in: two inner calls");

        // First inner: exactInput with recipient = ADDRESS_THIS.
        let swap = ISlipstreamRouterPath::exactInputCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactInput");
        assert_eq!(
            swap.params.recipient, ADDRESS_THIS,
            "swap recipient must be ADDRESS_THIS for native-out"
        );
        assert_eq!(swap.params.amountIn, amount_in, "amountIn must round-trip");
        assert_eq!(
            swap.params.amountOutMinimum, min_out,
            "amountOutMinimum must be min_out"
        );

        // Second inner: unwrapWETH9(min_out, recipient).
        let unwrap = ISlipstreamRouterPath::unwrapWETH9Call::abi_decode(&outer.data[1])
            .expect("second inner must decode as unwrapWETH9");
        assert_eq!(unwrap.amountMinimum, min_out, "amountMinimum == min_out");
        assert_eq!(unwrap.recipient, recipient, "unwrap recipient must be user");

        // min_received is the native asset.
        assert_eq!(
            prepared.min_received.raw, min_out,
            "min_received.raw == min_out"
        );
        assert!(prepared.max_spent.is_none(), "exact-in has no max_spent");
    }

    // ── native-in exact-in (ETH → token) ─────────────────────────────────────

    /// native-in 2-hop: `exactInput` raw call with `tx.value = amount_in`, no approval.
    #[test]
    fn two_hop_native_in_exact_in_encodes_exact_input_with_value() {
        let w = weth_base();
        let b = asset(0x22);
        let c = asset(0x33);
        let p1 = pool(w, b, 100);
        let p2 = pool(b, c, 200);
        let route = Route {
            pools: vec![&p1 as &dyn Pool, &p2],
            path: vec![w, b, c],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Slipstream,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x77);
        let amount_in = U256::from(2_000u64);
        let min_out = U256::from(1_800u64);
        let deadline = 9_999_999u64;

        let prepared = build_slipstream_span(
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
        .expect("native-in exact-in span must build");

        assert_eq!(
            prepared.tx.value, amount_in,
            "tx.value must be amount_in for native-in"
        );
        assert!(
            prepared.approval.is_none(),
            "native-in must have no approval"
        );

        // Decode as exactInput.
        let inner = ISlipstreamRouterPath::exactInputCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as exactInput for native-in");

        assert_eq!(
            inner.params.recipient, recipient,
            "recipient must round-trip"
        );
        assert_eq!(inner.params.amountIn, amount_in, "amountIn must round-trip");
        assert_eq!(
            inner.params.amountOutMinimum, min_out,
            "amountOutMinimum must be min_out"
        );
        assert_eq!(
            inner.params.deadline,
            U256::from(deadline),
            "deadline round-trips"
        );
    }

    // ── native-in exact-out (ETH → token) ────────────────────────────────────

    /// native-in exact-out: multicall([exactOutput(recipient=user), refundETH()]),
    /// tx.value = amount_in_max.
    #[test]
    fn two_hop_native_in_exact_out_encodes_multicall_with_refund() {
        let w = weth_base();
        let b = asset(0x22);
        let c = asset(0x33);
        let p1 = pool(w, b, 100);
        let p2 = pool(b, c, 200);
        let route = Route {
            pools: vec![&p1 as &dyn Pool, &p2],
            path: vec![w, b, c],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: RouterKind::Slipstream,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x77);
        let amount_out = U256::from(800u64);
        let amount_in_max = U256::from(1_200u64);
        let deadline = 1_700_000_000u64;

        let prepared = build_slipstream_span_exact_out(
            &ctx(),
            &route,
            &span,
            amount_out,
            amount_in_max,
            recipient,
            true,  // native_in
            false, // native_out
            deadline,
            true,
        )
        .expect("native-in exact-out span must build");

        assert_eq!(
            prepared.tx.value, amount_in_max,
            "tx.value == amount_in_max for native-in exact-out"
        );
        assert!(
            prepared.approval.is_none(),
            "native-in must have no approval"
        );

        // Outer: multicall(bytes[]).
        assert_eq!(
            &prepared.tx.data[..4],
            &[0xac, 0x96, 0x50, 0xd8],
            "outer selector must be multicall(bytes[]) for native-in exact-out"
        );
        let outer = IMulticall::multicall_0Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_0Call");
        assert_eq!(outer.data.len(), 2, "native-in exact-out: two inner calls");

        // First inner: exactOutput with recipient = user (not ADDRESS_THIS).
        let swap = ISlipstreamRouterPath::exactOutputCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactOutput");
        assert_eq!(
            swap.params.recipient, recipient,
            "native-in exact-out: recipient must be user, not ADDRESS_THIS"
        );
        assert_eq!(swap.params.amountOut, amount_out, "amountOut round-trips");
        assert_eq!(
            swap.params.amountInMaximum, amount_in_max,
            "amountInMaximum round-trips"
        );

        // Reversed path: c ‖ ts₁(200) ‖ b ‖ ts₀(100) ‖ w.
        let path = &swap.params.path;
        assert_eq!(
            &path[0..20],
            common::evm_addr(&c).as_slice(),
            "reversed[0] is c"
        );
        assert_eq!(&path[20..23], &[0x00, 0x00, 0xc8], "reversed ts[1] = 200");
        assert_eq!(
            &path[23..43],
            common::evm_addr(&b).as_slice(),
            "reversed[1] is b"
        );
        assert_eq!(&path[43..46], &[0x00, 0x00, 0x64], "reversed ts[0] = 100");
        assert_eq!(
            &path[46..66],
            common::evm_addr(&w).as_slice(),
            "reversed[2] is w"
        );

        // Second inner: refundETH().
        assert_eq!(
            &outer.data[1][..4],
            &ISlipstreamRouterPath::refundETHCall::SELECTOR,
            "second inner selector must be refundETH()"
        );

        let max_spent = prepared.max_spent.expect("exact-out must have max_spent");
        assert_eq!(
            max_spent.raw, amount_in_max,
            "max_spent.raw == amount_in_max"
        );
    }

    // ── native-out exact-out (token → ETH) ───────────────────────────────────

    /// native-out exact-out: multicall([exactOutput(recipient=ADDRESS_THIS), unwrapWETH9(amount_out, user)]).
    #[test]
    fn two_hop_native_out_exact_out_encodes_multicall_with_unwrap() {
        let a = asset(0x11);
        let b = asset(0x22);
        let w = weth_base();
        let p1 = pool(a, b, 100);
        let p2 = pool(b, w, 200);
        let route = Route {
            pools: vec![&p1 as &dyn Pool, &p2],
            path: vec![a, b, w],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: RouterKind::Slipstream,
            pools: 0..2,
        };

        let recipient = Address::repeat_byte(0x99);
        let amount_out = U256::from(400u64);
        let amount_in_max = U256::from(600u64);
        let deadline = 1_900_000_000u64;

        let prepared = build_slipstream_span_exact_out(
            &ctx(),
            &route,
            &span,
            amount_out,
            amount_in_max,
            recipient,
            false, // native_in
            true,  // native_out
            deadline,
            true,
        )
        .expect("native-out exact-out span must build");

        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "ERC-20 input has no ETH value"
        );

        let approval = prepared.approval.expect("ERC-20 input must have approval");
        assert_eq!(approval.token, a, "approval.token is span input a");
        assert_eq!(approval.spender, ROUTER, "spender is the router");
        assert_eq!(
            approval.min_allowance, amount_in_max,
            "allowance == amount_in_max"
        );

        // Outer: multicall(bytes[]).
        assert_eq!(
            &prepared.tx.data[..4],
            &[0xac, 0x96, 0x50, 0xd8],
            "outer selector must be multicall(bytes[]) for native-out exact-out"
        );
        let outer = IMulticall::multicall_0Call::abi_decode(&prepared.tx.data)
            .expect("outer must decode as multicall_0Call");
        assert_eq!(outer.data.len(), 2, "native-out exact-out: two inner calls");

        // First inner: exactOutput with recipient = ADDRESS_THIS.
        let swap = ISlipstreamRouterPath::exactOutputCall::abi_decode(&outer.data[0])
            .expect("first inner must decode as exactOutput");
        assert_eq!(
            swap.params.recipient, ADDRESS_THIS,
            "native-out exact-out: swap recipient must be ADDRESS_THIS"
        );
        assert_eq!(swap.params.amountOut, amount_out, "amountOut round-trips");
        assert_eq!(
            swap.params.amountInMaximum, amount_in_max,
            "amountInMaximum round-trips"
        );

        // Second inner: unwrapWETH9(amount_out_raw, user) — exact target.
        let unwrap = ISlipstreamRouterPath::unwrapWETH9Call::abi_decode(&outer.data[1])
            .expect("second inner must decode as unwrapWETH9");
        assert_eq!(
            unwrap.amountMinimum, amount_out,
            "unwrapWETH9.amountMinimum must be exact target (amount_out)"
        );
        assert_eq!(unwrap.recipient, recipient, "unwrap recipient must be user");

        let max_spent = prepared.max_spent.expect("exact-out must have max_spent");
        assert_eq!(
            max_spent.raw, amount_in_max,
            "max_spent.raw == amount_in_max"
        );
        assert_eq!(max_spent.asset, a, "max_spent.asset is span input a");
        assert_eq!(
            prepared.min_received.raw, amount_out,
            "min_received.raw == amount_out"
        );
    }

    // ── NativeMismatch guards ─────────────────────────────────────────────────

    #[test]
    fn native_in_and_native_out_exact_in_returns_native_mismatch() {
        let (a, b) = (asset(0x11), asset(0x22));
        let p = pool(a, b, 100);
        let route = Route {
            pools: vec![&p as &dyn Pool],
            path: vec![a, b],
            trade_type: TradeType::ExactIn,
        };
        let span = RouterSpan {
            kind: RouterKind::Slipstream,
            pools: 0..1,
        };
        let err = build_slipstream_span(
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

    #[test]
    fn native_in_and_native_out_exact_out_returns_native_mismatch() {
        let (a, b) = (asset(0x11), asset(0x22));
        let p = pool(a, b, 100);
        let route = Route {
            pools: vec![&p as &dyn Pool],
            path: vec![a, b],
            trade_type: TradeType::ExactOut,
        };
        let span = RouterSpan {
            kind: RouterKind::Slipstream,
            pools: 0..1,
        };
        let err = build_slipstream_span_exact_out(
            &ctx(),
            &route,
            &span,
            U256::from(80u64),
            U256::from(120u64),
            Address::repeat_byte(0x55),
            true,
            true,
            9_999_999u64,
            true,
        )
        .expect_err("native→native must return NativeMismatch");
        assert_eq!(err, BuildError::NativeMismatch);
    }
}
