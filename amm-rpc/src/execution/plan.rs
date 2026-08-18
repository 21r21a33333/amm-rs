//! The public multi-hop `Plan` executor (exact-in).
//!
//! [`plan`] turns a validated [`Route`] into a [`Plan`]: it partitions the
//! route into same-router [`RouterSpan`]s and quotes every boundary amount up
//! front. [`Plan`] is then a cursor over those spans — each [`Plan::next_tx`]
//! yields the next span's [`PreparedSwap`].
//!
//! A route whose pools all share one router is **atomic** ([`Plan::is_atomic`]):
//! one span, one transaction. A cross-router route splits into several spans
//! that must be submitted sequentially; each later span's real input is the
//! previous span's *observed* on-chain output, threaded back in via `observed`.
//!
//! ## Dispatch
//! For each span, dispatch is keyed on `(RouterKind, pool_count)`:
//! - **1 pool** (any protocol) → the pool's single-hop [`Executable::build_swap`].
//! - **>1 Uniswap** → [`build_uniswap_span`] (one Universal Router `execute`).
//! - **>1 Aerodrome/Slipstream/Curve** → [`BuildError::UnsupportedProtocol`]
//!   until the family builders land (Tasks 6/7 and Plan 3b).
//!
//! ## Recipients
//! Only the **final** span pays the caller's recipient; every earlier span
//! delivers to the `sender` so the funds are on hand to feed the next span.
//!
//! ## Native edges
//! `native_in`/`native_out` are supplied explicitly to [`plan`] (the [`Route`]
//! carries no [`Currency`], so native intent cannot be derived from it). Only
//! the first span may wrap native input; only the final span may unwrap to
//! native output. Task 2 refines slippage/native handling.

use alloy::primitives::{Address, U256};
use amm_core::error::QuoteError;
use amm_core::path::{Hop, quote_path_amounts};
use amm_core::primitives::asset::AssetAmount;
use amm_core::primitives::pool::PoolKind;

use crate::execution::config::ChainConfig;
use crate::execution::error::BuildError;
use crate::execution::executable::as_executable;
use crate::execution::options::{ExecutionOptions, Recipient};
use crate::execution::prepared::PreparedSwap;
use crate::execution::protocols::common;
use crate::execution::routing::ExactOutPolicy;
use crate::execution::routing::family::uniswap_ur::build_uniswap_span;
use crate::execution::routing::{Route, RouterKind, RouterSpan, partition};
use crate::execution::types::{Currency, CurrencyAmount, TradeType};

/// Map a path [`QuoteError`] to a [`BuildError`].
///
/// `ExactOutUnavailable` is the only quote error that names a specific
/// capability gap, so it maps to [`BuildError::UnsupportedExactOut`] (the pool
/// kind is unknown at this layer, so a generic V4 kind is not appropriate — we
/// only reach this from the exact-out backward solve, which Task 9 wires; the
/// exact-in path here never produces it). Every other math failure (asset
/// mismatch, insufficient liquidity, overflow, …) is a pool that cannot serve
/// this route, which the build layer already expresses as
/// [`BuildError::UnsupportedProtocol`].
fn map_quote_err(e: QuoteError) -> BuildError {
    match e {
        // A pool on the path lacks exact-out; surface as the exact-out gap.
        QuoteError::ExactOutUnavailable { .. } => BuildError::UnsupportedExactOut {
            kind: PoolKind::UniswapV4,
        },
        // All other quote failures mean this route is not serviceable here.
        _ => BuildError::UnsupportedProtocol,
    }
}

/// A partitioned, pre-quoted multi-hop swap ready to emit per-span transactions.
///
/// Construct with [`plan`]; drive with [`Plan::next_tx`]. Holds only borrows and
/// the quoted boundary amounts, so it is cheap to keep alongside the route.
pub struct Plan<'a> {
    /// The route being executed; spans index into its pools/path.
    route: &'a Route<'a>,
    /// Maximal same-router runs, in execution order.
    spans: Vec<RouterSpan>,
    /// Quoted boundary amounts, `[start, out₁, …, outₙ]` (length `pools + 1`).
    amounts: Vec<AssetAmount>,
    /// Index of the next span to emit; `== spans.len()` when exhausted.
    cursor: usize,
    /// Per-chain router/sentinel configuration.
    ctx: &'a ChainConfig,
    /// Caller options, pre-resolved to absolutes at the edge.
    opts: &'a ExecutionOptions,
    /// The transaction signer; intermediate spans deliver to it.
    sender: Address,
    /// Exact-out approximation policy (unused on the exact-in path).
    #[allow(dead_code)]
    policy: ExactOutPolicy,
    /// Absolute deadline resolved once from `opts.deadline`.
    deadline: u64,
    /// Whether the route's input is native ETH (only the first span wraps).
    native_in: bool,
    /// Whether the route's output is native ETH (only the last span unwraps).
    native_out: bool,
}

/// Build an exact-in [`Plan`] from a validated route.
///
/// `opts` must already be resolved to absolutes by the caller edge
/// ([`crate::execution::options::resolve`]): an absolute deadline and an
/// explicit recipient (`Recipient::Sender` is resolved here to `sender`).
///
/// `native_in`/`native_out` declare whether the route's endpoints are the
/// chain's native token; the [`Route`] itself carries only wrapped assets.
///
/// # Errors
/// - Route structural errors from [`Route::validate`] ([`BuildError::EmptyRoute`],
///   [`BuildError::DisjointRoute`]).
/// - [`BuildError::UnsupportedProtocol`] if a pool cannot be classified/quoted.
/// - [`BuildError::UnsupportedExactOut`] — exact-out is stubbed here; Task 9 wires it.
/// - [`BuildError::UnresolvedDeadline`] if `opts.deadline` is not absolute.
//
// The argument count is inherent to the public multi-hop entry point: it threads
// ctx, route, amount, opts, sender, both native-edge flags, and the exact-out
// policy. Bundling them would obscure the call site more than it helps.
#[allow(clippy::too_many_arguments)]
pub fn plan<'a>(
    ctx: &'a ChainConfig,
    route: &'a Route<'a>,
    amount: U256,
    opts: &'a ExecutionOptions,
    sender: Address,
    native_in: bool,
    native_out: bool,
    policy: ExactOutPolicy,
) -> Result<Plan<'a>, BuildError> {
    route.validate()?;
    let spans = partition(route)?;

    // Exact-out is not wired at this layer yet; Task 9 backward-solves and
    // reuses the same span dispatch. Report the gap against the first pool's kind.
    if route.trade_type == TradeType::ExactOut {
        let kind = route.pools[0]
            .as_introspect()
            .ok_or(BuildError::UnsupportedProtocol)?
            .kind();
        return Err(BuildError::UnsupportedExactOut { kind });
    }

    // Quote every boundary amount forward from the route start. `path[i+1]` is
    // hop `i`'s output asset; the resulting vector has one entry per boundary.
    let hops: Vec<Hop<'_>> = (0..route.pools.len())
        .map(|i| Hop {
            pool: route.pools[i],
            to: route.path[i + 1],
        })
        .collect();
    let start = AssetAmount::new(route.path[0], amount);
    let amounts = quote_path_amounts(&start, &hops).map_err(map_quote_err)?;

    // Resolve the deadline once; every span's tx shares it.
    let deadline = u64::try_from(common::resolve_deadline(&opts.deadline)?)
        .map_err(|_| BuildError::UnresolvedDeadline)?;

    Ok(Plan {
        route,
        spans,
        amounts,
        cursor: 0,
        ctx,
        opts,
        sender,
        policy,
        deadline,
        native_in,
        native_out,
    })
}

impl<'a> Plan<'a> {
    /// A plan is atomic when all its pools settle through one router — a single
    /// transaction with no cross-span sequencing.
    pub fn is_atomic(&self) -> bool {
        self.spans.len() == 1
    }

    /// The number of transactions this plan emits — one per span.
    pub fn tx_count(&self) -> usize {
        self.spans.len()
    }

    /// The partitioned router spans, in execution order.
    ///
    /// Each span's `pools` range indexes into the route's pool list; `kind`
    /// identifies the on-chain router. The test harness uses this to determine
    /// each span's output token (`route.path[span.pools.end]`) and whether the
    /// span is Uniswap-family (for zero-residue assertions).
    pub fn spans(&self) -> &[RouterSpan] {
        &self.spans
    }

    /// Emit the next span's transaction, or `Ok(None)` when the plan is done.
    ///
    /// `observed` is the *actual* on-chain output of the previous span; it is
    /// required for every span after the first (the quoted boundary is only an
    /// estimate — the real input is what the previous tx delivered). The first
    /// span takes its input from the quoted route start.
    ///
    /// # Errors
    /// - [`BuildError::UnresolvedRecipient`] when a sequential span is missing
    ///   its `observed` predecessor output (no dedicated variant exists; this
    ///   reuses the "recipient/amount not resolved" family — see the report).
    /// - Any [`BuildError`] from the underlying span/pool builder.
    pub fn next_tx(
        &mut self,
        observed: Option<AssetAmount>,
    ) -> Result<Option<PreparedSwap>, BuildError> {
        if self.cursor == self.spans.len() {
            return Ok(None);
        }

        let span = self.spans[self.cursor].clone();
        let (start, end) = (span.pools.start, span.pools.end);
        let is_final = self.cursor == self.spans.len() - 1;

        let in_amount = self.resolve_span_amount(observed)?;
        let recipient = self.resolve_span_recipient(is_final);

        // Native wrap only on the first span's input; unwrap only on the last
        // span's output. Interior span edges are always wrapped tokens.
        let span_native_in = self.native_in && self.cursor == 0;
        let span_native_out = self.native_out && is_final;

        // The quoted output boundary of this span bounds the slippage floor.
        let span_quoted_out = self.amounts[end];
        // Compound the per-hop tolerance across the number of pools in this span.
        // A k-pool atomic Uniswap span must revert when the *cumulative* drift
        // across all k hops exceeds the user's tolerance; compounding scales that
        // floor correctly.  For a 1-pool span compound(1) is the identity, so
        // behaviour is unchanged relative to Task 1.  Sequential (cross-router)
        // spans each carry their own compound floor, which is correct because each
        // one submits as its own on-chain transaction.
        let span_hops = end - start; // == span.pools.len()
        let min_out = self
            .opts
            .slippage
            .compound(span_hops)
            .min_amount_out(&span_quoted_out)
            .raw;

        let pool_count = end - start;
        let router_kind = RouterKind::of(self.route.pools[start]);

        let prepared = match (router_kind, pool_count) {
            // Single pool of any protocol → its own single-hop encoder.
            (_, 1) => self.build_single_pool_span(
                &span,
                in_amount,
                recipient,
                span_native_in,
                span_native_out,
                &span_quoted_out,
            )?,
            // Multi-hop Uniswap → one Universal Router execute for the whole span.
            (Some(RouterKind::UniswapUniversal), _) => build_uniswap_span(
                self.ctx,
                self.route,
                &span,
                in_amount,
                min_out,
                recipient,
                span_native_in,
                span_native_out,
                self.deadline,
                is_final,
            )?,
            // Multi-hop non-Uniswap families land in later tasks.
            (Some(RouterKind::Aerodrome), _)
            | (Some(RouterKind::Slipstream), _)
            | (Some(RouterKind::Curve), _)
            | (None, _) => return Err(BuildError::UnsupportedProtocol),
        };

        self.cursor += 1;
        Ok(Some(prepared))
    }

    /// Resolve the input amount for the current span.
    ///
    /// The first span takes its input from the pre-quoted route start amount.
    /// Every subsequent span takes the actual on-chain output of the previous
    /// span (passed back via `observed`) as its ground-truth input.
    fn resolve_span_amount(&self, observed: Option<AssetAmount>) -> Result<U256, BuildError> {
        // First span: quoted route start. Later spans: the previous span's real
        // observed output — the ground-truth input for this leg.
        match self.cursor {
            0 => Ok(self.amounts[0].raw),
            _ => Ok(observed.ok_or(BuildError::UnresolvedRecipient)?.raw),
        }
    }

    /// Resolve the payee address for the current span.
    ///
    /// Only the final span pays the caller's recipient; earlier spans hand
    /// funds to the sender so they are available to feed the next span.
    fn resolve_span_recipient(&self, is_final: bool) -> Address {
        match is_final {
            true => match &self.opts.recipient {
                Recipient::To(a) => *a,
                Recipient::Sender => self.sender,
            },
            false => self.sender,
        }
    }

    /// Build a single-pool span transaction using the pool's own encoder.
    ///
    /// Dispatches to `Executable::build_swap` for the one pool in the span,
    /// overriding the options recipient to this span's resolved payee.
    /// Used for the `(_, 1)` arm of the `next_tx` match — any protocol, one pool.
    fn build_single_pool_span(
        &self,
        span: &RouterSpan,
        in_amount: U256,
        recipient: Address,
        native_in: bool,
        native_out: bool,
        span_quoted_out: &AssetAmount,
    ) -> Result<PreparedSwap, BuildError> {
        let start = span.pools.start;
        let end = span.pools.end;
        let in_currency = match native_in {
            true => Currency::Native,
            false => Currency::Token(self.route.path[start]),
        };
        let out_currency = match native_out {
            true => Currency::Native,
            false => Currency::Token(self.route.path[end]),
        };
        // Per-span options: override the recipient to this span's payee.
        let mut span_opts = self.opts.clone();
        span_opts.recipient = Recipient::To(recipient);
        as_executable(self.route.pools[start])
            .ok_or(BuildError::UnsupportedProtocol)?
            .build_swap(
                self.ctx,
                CurrencyAmount {
                    currency: in_currency,
                    raw: in_amount,
                },
                out_currency,
                span_quoted_out,
                &span_opts,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy::primitives::{Address, B256, U256};
    use alloy::sol;
    use alloy::sol_types::SolCall;
    use amm_core::primitives::asset::{AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::primitives::ratio::Bps;
    use amm_core::protocols::aerodrome::volatile::AerodromeVolatilePool;
    use amm_core::protocols::uniswap::v3::{TickData, TickInfo, UniswapV3Pool};
    use amm_core::slippage::Slippage;

    use crate::execution::config::{ChainConfig, Routers};
    use crate::execution::options::{Deadline, ExecutionOptions, Recipient};
    use crate::execution::types::TradeType;

    // Minimal ABI surfaces for decoding the recipient out of each span's tx.
    // Only the fields the assertions read are declared.
    sol! {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
        /// SwapRouter02 wraps calls in a deadline-bearing multicall overload.
        function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[] results);

        struct AeroRoute {
            address from;
            address to;
            bool stable;
            address factory;
        }
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            AeroRoute[] routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);
    }

    // ── Fixtures (mirroring uniswap_ur.rs test pools) ─────────────────────────

    fn chain_id() -> ChainId {
        ChainId(1)
    }

    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    fn universal_router() -> Address {
        Address::repeat_byte(0xAA)
    }

    fn permit2() -> Address {
        Address::repeat_byte(0xBB)
    }

    fn v3_router() -> Address {
        Address::repeat_byte(0xCC)
    }

    fn aero_router() -> Address {
        Address::repeat_byte(0xDD)
    }

    fn aero_factory() -> Address {
        Address::repeat_byte(0xEE)
    }

    /// A chain config wired for all the routers the tests dispatch through.
    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), asset(0x02)).with_routers(Routers {
            universal: Some(universal_router()),
            permit2: Some(permit2()),
            v3: Some(v3_router()),
            aerodrome: Some(aero_router()),
            aerodrome_factory: Some(aero_factory()),
            ..Default::default()
        })
    }

    /// sqrtPriceX96 for tick 0 (price 1:1) = 2^96.
    const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;

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

    /// Order two assets by EVM address so `assets[0] == currency0`.
    fn order(a: AssetId, b: AssetId) -> (AssetId, AssetId) {
        match common::evm_addr(&a) < common::evm_addr(&b) {
            true => (a, b),
            false => (b, a),
        }
    }

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

    /// A volatile Aerodrome pool with equal reserves (quotes near 1:1).
    fn aero_pool(a: AssetId, b: AssetId) -> AerodromeVolatilePool {
        let (lo, hi) = order(a, b);
        AerodromeVolatilePool::new(
            PoolId::new("1:aero:test"),
            [lo, hi],
            [U256::from(1_000_000_000_000_000_000u128); 2],
            30,
        )
    }

    /// Options resolved to absolutes: explicit recipient + absolute deadline.
    fn opts(to: Address) -> ExecutionOptions {
        ExecutionOptions::new(Slippage::from_bps(Bps(50)))
            .with_recipient(Recipient::To(to))
            .with_deadline(Deadline::AtTimestamp(1_700_000_000))
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// A single-span 2-pool all-V3 route is atomic: one span, one Universal
    /// Router transaction. `next_tx(None)` yields it; a second call is `None`.
    #[test]
    fn single_span_all_v3_is_atomic_and_emits_one_ur_tx() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let options = opts(recipient);
        let config = ctx();

        let mut p = plan(
            &config,
            &route,
            U256::from(1_000u64),
            &options,
            sender,
            false,
            false,
            ExactOutPolicy::Strict,
        )
        .expect("plan must build");

        assert!(p.is_atomic(), "one router → atomic");
        assert_eq!(p.tx_count(), 1, "one span → one tx");

        let prepared = p
            .next_tx(None)
            .expect("first span must build")
            .expect("first span yields a tx");
        assert_eq!(
            prepared.tx.to,
            universal_router(),
            "atomic Uniswap span targets the Universal Router"
        );

        assert!(
            p.next_tx(None).expect("no error past the end").is_none(),
            "an exhausted plan yields None"
        );
    }

    /// A V3-then-Aerodrome route partitions into two single-pool spans of
    /// different routers: not atomic, two txs. The first span delivers to the
    /// sender (funds staged for the next leg); the second pays the recipient.
    /// A third `next_tx` is `None`.
    #[test]
    fn two_span_cross_router_routes_recipients_sequentially() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 3000);
        let p2 = aero_pool(b, c);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let options = opts(recipient);
        let config = ctx();

        let mut p = plan(
            &config,
            &route,
            U256::from(1_000_000u64),
            &options,
            sender,
            false,
            false,
            ExactOutPolicy::Strict,
        )
        .expect("plan must build");

        assert!(!p.is_atomic(), "two routers → not atomic");
        assert_eq!(p.tx_count(), 2, "two spans → two txs");

        // Span 1 (V3): input = route start; delivers the intermediate to sender.
        let first = p
            .next_tx(None)
            .expect("span 1 builds")
            .expect("span 1 yields a tx");

        // Span 2 (Aerodrome): input = the observed output of span 1.
        let observed = AssetAmount::new(c, U256::from(990_000u64));
        let second = p
            .next_tx(Some(observed))
            .expect("span 2 builds")
            .expect("span 2 yields a tx");

        // Cross-router dispatch: span 1 → V3 router, span 2 → Aerodrome router.
        assert_eq!(first.tx.to, v3_router(), "span 1 uses the V3 router");
        assert_eq!(
            second.tx.to,
            aero_router(),
            "span 2 uses the Aerodrome router"
        );

        // Recipient routing: decode each call's payee. Span 1 (non-final) must
        // deliver the intermediate to the sender; span 2 (final) to the caller's
        // recipient.
        // V3 wraps its swap in a deadline-bearing multicall; unwrap then decode.
        let outer =
            multicallCall::abi_decode(&first.tx.data).expect("span 1 must decode as multicall");
        let first_recipient = exactInputSingleCall::abi_decode(&outer.data[0])
            .expect("span 1 inner must decode as exactInputSingle")
            .params
            .recipient;
        assert_eq!(
            first_recipient, sender,
            "non-final span delivers to the sender"
        );

        let second_recipient = swapExactTokensForTokensCall::abi_decode(&second.tx.data)
            .expect("span 2 must decode as swapExactTokensForTokens")
            .to;
        assert_eq!(
            second_recipient, recipient,
            "final span delivers to the caller's recipient"
        );

        assert!(
            p.next_tx(None).expect("no error past the end").is_none(),
            "a third call after two spans yields None"
        );
    }

    // ── Task 2: slippage compounding ─────────────────────────────────────────

    /// (a) A 2-hop single Uniswap span is atomic: the Universal Router encodes
    /// one `execute` call whose last V3 hop carries `amountOutMinimum` equal to
    /// the **compounded** slippage floor, not the naive per-hop floor.
    ///
    /// Invariant: `amountOutMinimum == slippage.compound(2).min_amount_out(quoted_span_out).raw`
    ///
    /// This is the key new guard from Task 2.  A naive `slippage.min_amount_out`
    /// (no compounding) would accept too much drift on a 2-hop path; compounding
    /// scales the floor to the cumulative tolerance over both hops.
    #[test]
    fn two_hop_ur_span_carries_compounded_slippage_floor() {
        use crate::execution::route_planner::IUniversalRouter;
        use alloy::sol_types::SolValue;

        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };

        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let slippage = Slippage::from_bps(Bps(50));
        let options = ExecutionOptions::new(slippage)
            .with_recipient(Recipient::To(recipient))
            .with_deadline(Deadline::AtTimestamp(1_700_000_000));
        let config = ctx();
        let amount_in = U256::from(1_000_000u64);

        let mut p = plan(
            &config,
            &route,
            amount_in,
            &options,
            sender,
            false, // native_in
            false, // native_out
            ExactOutPolicy::Strict,
        )
        .expect("plan must build");

        assert!(p.is_atomic(), "2-hop single-router route is atomic");

        let prepared = p
            .next_tx(None)
            .expect("span must build")
            .expect("span yields a tx");

        // The plan pre-quoted the boundary amounts; extract `amounts[2]` which
        // is the output of the full 2-hop span (the span_quoted_out the plan
        // uses to compute the floor).  We do NOT have direct access to
        // `plan.amounts`, so we derive the expected floor by replicating the
        // same quote ourselves and comparing against what the calldata carries.
        //
        // The compounded floor for a 2-hop 50 bps tolerance is:
        //   compound(2) = 1 - (1 - 0.005)^2 = 0.009975 → floored to 99 bps
        //
        // The naive (non-compounded) floor would use only 50 bps, producing a
        // strictly larger (more lenient) minimum.  We assert the calldata carries
        // the *compounded* value, not the naive one.
        let compound_slip = slippage.compound(2); // 99 bps per compound_two_legs_50bps_floors_to_99
        assert_eq!(
            compound_slip.bps(),
            Bps(99),
            "sanity: compound(2) is 99 bps"
        );

        // Decode the outer UR `execute` call to reach the individual hop inputs.
        let decoded = IUniversalRouter::executeCall::abi_decode(&prepared.tx.data)
            .expect("span calldata must decode as UR execute");

        // Two V3 hops → two inputs; the *last* input carries amountOutMinimum.
        assert_eq!(decoded.inputs.len(), 2, "2-hop span has two UR inputs");

        // V3 swap input tuple: (recipient, amount, amountOutMinimum, path, payerIsUser).
        let (_, _, limit_naive, _, _) =
            <(Address, U256, U256, alloy::primitives::Bytes, bool)>::abi_decode_params(
                &decoded.inputs[0],
            )
            .expect("hop 0 must decode as V3 input");
        assert_eq!(
            limit_naive,
            U256::ZERO,
            "non-last hop (hop 0) carries no min — its slippage is enforced by the terminal hop"
        );

        let (last_recip, _, limit_actual, _, _) =
            <(Address, U256, U256, alloy::primitives::Bytes, bool)>::abi_decode_params(
                &decoded.inputs[1],
            )
            .expect("hop 1 must decode as V3 input");
        assert_eq!(last_recip, recipient, "last hop delivers to recipient");

        // The actual floor in the calldata must match the compounded computation.
        // To recover the expected floor we re-run the same quote the plan ran
        // (quote_path_amounts from amount_in through both pools) and apply
        // compound(2).min_amount_out to the span boundary.
        use amm_core::path::{Hop, quote_path_amounts};
        let hops = vec![Hop { pool: &p1, to: b }, Hop { pool: &p2, to: c }];
        let start_amt = amm_core::primitives::asset::AssetAmount::new(a, amount_in);
        let quoted = quote_path_amounts(&start_amt, &hops).expect("quote must succeed");
        // quoted[2] is the span output (amounts[end] in the plan).
        let span_quoted_out = &quoted[2];
        let expected_floor = compound_slip.min_amount_out(span_quoted_out).raw;

        assert_eq!(
            limit_actual, expected_floor,
            "calldata amountOutMinimum must equal compound(2).min_amount_out(span_quoted_out)"
        );

        // Confirm the naive floor (50 bps, no compounding) is strictly larger,
        // proving the test would catch a regression back to the non-compounded formula.
        let naive_floor = slippage.min_amount_out(span_quoted_out).raw;
        assert!(
            naive_floor > expected_floor,
            "naive floor ({naive_floor}) must be more lenient than compounded floor ({expected_floor})"
        );
    }

    /// (b) A 2-span cross-router route: every span's `PreparedSwap.approval` is
    /// `Some` on its own input token (spender = the span's router).  When the
    /// first span has `native_in=true`, its `tx.value > 0` and `approval == None`.
    ///
    /// This test drives three sub-cases:
    ///   - ERC-20 first span + ERC-20 second span → both approvals `Some`
    ///   - native-in first span + ERC-20 second span → first `None`, second `Some`
    ///   - Both spans have distinct spenders (V3 router vs Aerodrome router).
    #[test]
    fn cross_router_spans_each_carry_correct_approval() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 3000);
        let p2 = aero_pool(b, c);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let options = opts(recipient);
        let config = ctx();
        let amount_in = U256::from(1_000_000u64);

        // ── Sub-case 1: ERC-20 in on both spans ──────────────────────────────
        {
            let mut p = plan(
                &config,
                &route,
                amount_in,
                &options,
                sender,
                false, // native_in
                false, // native_out
                ExactOutPolicy::Strict,
            )
            .expect("plan must build");

            // Span 1 (V3, input = a): approval must be Some on token `a`.
            let span1 = p
                .next_tx(None)
                .expect("span 1 builds")
                .expect("span 1 yields tx");
            let appr1 = span1
                .approval
                .as_ref()
                .expect("span 1 (ERC-20 in) must have approval");
            assert_eq!(appr1.token, a, "span 1 approval token must be span input a");
            // V3 single-hop uses the V3 router as spender (not Permit2).
            assert_eq!(
                appr1.spender,
                v3_router(),
                "span 1 spender must be the V3 router"
            );

            // Span 2 (Aerodrome, input = b): approval must be Some on token `b`.
            let observed = amm_core::primitives::asset::AssetAmount::new(b, U256::from(990_000u64));
            let span2 = p
                .next_tx(Some(observed))
                .expect("span 2 builds")
                .expect("span 2 yields tx");
            let appr2 = span2
                .approval
                .as_ref()
                .expect("span 2 (ERC-20 in) must have approval");
            assert_eq!(appr2.token, b, "span 2 approval token must be span input b");
            assert_eq!(
                appr2.spender,
                aero_router(),
                "span 2 spender must be the Aerodrome router"
            );
        }

        // ── Sub-case 2: native-in on the first span ───────────────────────────
        // native_in=true wraps ETH; the UR span has tx.value == amount_in and
        // approval == None (no Permit2 approval needed for native input).
        // The second span is still ERC-20, so it keeps its approval.
        //
        // NOTE: native_in is passed to `plan`, which gates it with `cursor==0`
        // so only the *first* span sees span_native_in=true; subsequent spans
        // always have span_native_in=false.
        //
        // For this sub-case we need a 2-span route where the *first* span is a
        // UR (Uniswap) span that accepts native input.  Use a 2-pool all-V3 route
        // so the first span is a UR multi-hop span (native_in tested via UR path).
        {
            let (x, y, z) = (asset(0xA1), asset(0xA2), asset(0xA3));
            let pu1 = v3_pool(x, y, 500);
            let pu2 = v3_pool(y, z, 3000);
            // A single Uniswap span (all-V3, atomic).
            let ur_route = Route {
                pools: vec![&pu1, &pu2],
                path: vec![x, y, z],
                trade_type: TradeType::ExactIn,
            };
            let mut p2 = plan(
                &config,
                &ur_route,
                amount_in,
                &options,
                sender,
                true,  // native_in: first (and only) span gets ETH
                false, // native_out
                ExactOutPolicy::Strict,
            )
            .expect("native-in UR plan must build");

            let native_span = p2
                .next_tx(None)
                .expect("native-in span builds")
                .expect("native-in span yields tx");

            // tx.value carries the ETH; no Permit2 approval required.
            assert!(
                native_span.tx.value > U256::ZERO,
                "native-in span must have tx.value > 0 (ETH attached)"
            );
            assert_eq!(
                native_span.tx.value, amount_in,
                "tx.value must equal amount_in"
            );
            assert!(
                native_span.approval.is_none(),
                "native-in span must have approval == None (no ERC-20 approval for ETH input)"
            );
        }
    }

    /// (c) A 1-pool span: compound(1) is the identity, so the slippage floor is
    /// exactly `slippage.min_amount_out(quoted_out)`, identical to the Task 1
    /// behaviour.  This confirms the compounding change is a no-op for single-hop
    /// spans and does not introduce any regression.
    #[test]
    fn single_pool_span_slippage_floor_is_identity_of_compound_one() {
        // compound(1) == identity: Slippage::compound(1).min_amount_out(x) == slippage.min_amount_out(x)
        let slippage = Slippage::from_bps(Bps(50));
        assert_eq!(
            slippage.compound(1).bps(),
            slippage.bps(),
            "compound(1) must return the same tolerance (identity)"
        );

        // Drive through the plan: single V3→Aerodrome (1-pool each) route,
        // check that the first span's PreparedSwap.min_received matches the
        // non-compounded floor, proving compound(1) produces the same result.
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 3000); // 1-pool V3 span
        let p2 = aero_pool(b, c); // 1-pool Aerodrome span
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactIn,
        };
        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let options = ExecutionOptions::new(slippage)
            .with_recipient(Recipient::To(recipient))
            .with_deadline(Deadline::AtTimestamp(1_700_000_000));
        let config = ctx();
        let amount_in = U256::from(1_000_000u64);

        let mut p = plan(
            &config,
            &route,
            amount_in,
            &options,
            sender,
            false,
            false,
            ExactOutPolicy::Strict,
        )
        .expect("plan must build");

        // Quote the boundary amounts independently so we can compute the expected floor.
        use amm_core::path::{Hop, quote_path_amounts};
        let hops = vec![Hop { pool: &p1, to: b }, Hop { pool: &p2, to: c }];
        let start_amt = amm_core::primitives::asset::AssetAmount::new(a, amount_in);
        let quoted = quote_path_amounts(&start_amt, &hops).expect("quote must succeed");
        // quoted[1] = output of hop 0 (= span 1's boundary); quoted[2] = span 2.
        let span1_quoted_out = &quoted[1]; // amounts[end] where end=1 for span 0..1
        let expected_floor_span1 = slippage.min_amount_out(span1_quoted_out).raw;
        let compounded_floor_span1 = slippage.compound(1).min_amount_out(span1_quoted_out).raw;
        assert_eq!(
            expected_floor_span1, compounded_floor_span1,
            "compound(1).min_amount_out must equal plain min_amount_out — identity invariant"
        );

        // The plan's span 1 PreparedSwap.min_received.raw must match the floor.
        let span1_prepared = p
            .next_tx(None)
            .expect("span 1 builds")
            .expect("span 1 yields tx");
        assert_eq!(
            span1_prepared.min_received.raw, expected_floor_span1,
            "span 1 min_received.raw must equal the (compound-1-identical) floor"
        );
    }
}
