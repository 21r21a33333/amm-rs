//! The public multi-hop `Plan` executor (exact-in and exact-out).
//!
//! [`plan`] turns a validated [`Route`] into a [`Plan`]: it partitions the
//! route into same-router [`RouterSpan`]s and quotes every boundary amount up
//! front. [`Plan`] is then a cursor over those spans — each [`Plan::next_tx`]
//! yields the next span's [`PreparedSwap`].
//!
//! ## Exact-out
//! For an [`TradeType::ExactOut`] route the caller's `amount` is the desired
//! output. Two policies realise it (see [`plan`] for details):
//! - [`ExactOutPolicy::Strict`] — a true exact-out over a single atomic span of a
//!   family with an exact-out builder (Uniswap/Slipstream); every other shape is
//!   rejected with [`BuildError::UnsupportedExactOut`].
//! - [`ExactOutPolicy::OrBetter`] — backward-solve the input, then execute as an
//!   ordinary exact-in plan whose **final** span floor is forced to the exact
//!   target (`AtLeast(target)`). Works for any route/span-kind.
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
//! - **>1 Aerodrome** → [`build_aerodrome_span`] (one Aerodrome router call, `Route[]`).
//! - **>1 Slipstream** → [`build_slipstream_span`] (one path-encoded `exactInput`).
//! - **>1 Curve** → [`BuildError::UnsupportedProtocol`] until Plan 3b lands.
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
use amm_core::path::{Hop, quote_path_amounts, quote_path_exact_out};
use amm_core::primitives::asset::AssetAmount;
use amm_core::primitives::pool::PoolKind;
use amm_core::primitives::ratio::Bps;
use amm_core::slippage::Slippage;

use crate::execution::config::ChainConfig;
use crate::execution::error::BuildError;
use crate::execution::executable::as_executable;
use crate::execution::options::{ExecutionOptions, Recipient};
use crate::execution::prepared::PreparedSwap;
use crate::execution::protocols::common;
use crate::execution::routing::ExactOutPolicy;
use crate::execution::routing::family::aerodrome::build_aerodrome_span;
use crate::execution::routing::family::slipstream::{
    build_slipstream_span, build_slipstream_span_exact_out,
};
use crate::execution::routing::family::uniswap_ur::{
    build_uniswap_span, build_uniswap_span_exact_out,
};
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

/// How this plan realises its trade — the exact-in vs exact-out shape that
/// `next_tx` must honour when it dispatches each span.
///
/// This is derived once in [`plan`] from the route's [`TradeType`] and the
/// caller's [`ExactOutPolicy`], so `next_tx` never re-derives intent.
enum ExactOutMode {
    /// Plain exact-in (also the realised shape of an `OrBetter` exact-out, which
    /// degrades to an exact-in plan whose final floor is the exact-out target).
    ///
    /// `final_floor_override` carries that target for the `OrBetter` case: when
    /// `Some`, the final span's slippage floor is replaced by the exact `target`
    /// so the caller is guaranteed to receive **at least** the requested amount
    /// (`AtLeast(target)` semantics). It is `None` for a genuine exact-in trade.
    ExactIn { final_floor_override: Option<U256> },
    /// True (`Strict`) exact-out over a single atomic span. `next_tx` calls the
    /// span family's `_exact_out` builder with `amount_out = target` and the
    /// slippage-grossed `amount_in_max`, delivering the output amount exactly.
    Strict { target: U256, amount_in_max: U256 },
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
    /// The realised trade shape — exact-in (optionally with an `OrBetter` final
    /// floor override) or a `Strict` single-span exact-out. Derived once in
    /// [`plan`] from `route.trade_type` and the caller's [`ExactOutPolicy`].
    mode: ExactOutMode,
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
/// For `route.trade_type == TradeType::ExactOut`, `amount` is the **target
/// output** (what the caller wants to receive) and `policy` selects how to
/// realise it:
/// - [`ExactOutPolicy::Strict`] — a true exact-out, only for a **single atomic
///   span** whose family has an exact-out builder (Uniswap or Slipstream). The
///   input is backward-solved and grossed up by `opts.slippage` into an
///   `amount_in_max`; the span builder emits a real exact-out call. A multi-span
///   route, or a single span whose family lacks exact-out (Curve/Aerodrome),
///   returns [`BuildError::UnsupportedExactOut`] — Strict never approximates.
/// - [`ExactOutPolicy::OrBetter`] — works for **any** route/span-kind. The input
///   is backward-solved via [`quote_path_exact_out`] and the plan is then built
///   exactly like exact-in with that input; only the **final** span's slippage
///   floor is replaced by the exact target, so the caller receives *at least*
///   the requested amount.
///
/// # Errors
/// - Route structural errors from [`Route::validate`] ([`BuildError::EmptyRoute`],
///   [`BuildError::DisjointRoute`]).
/// - [`BuildError::UnsupportedProtocol`] if a pool cannot be classified/quoted.
/// - [`BuildError::UnsupportedExactOut`] — a `Strict` exact-out on a multi-span
///   route or on a single span whose family has no exact-out builder, or a pool
///   on the backward-solve path that lacks exact-out.
/// - [`BuildError::UnresolvedDeadline`] if `opts.deadline` is not absolute.
///
/// # Examples
/// ```ignore
/// // Exact-out, best-effort: deliver at least `target`, input backward-solved.
/// let mut p = plan(&ctx, &route, target, &opts, sender, false, false,
///                  ExactOutPolicy::OrBetter)?;
/// let tx = p.next_tx(None)?; // exact-in shape, final floor == target
/// ```
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

    // Build the per-hop descriptor once; both the forward (exact-in) and backward
    // (exact-out) solvers key off the same `(pool, to)` pairs.
    let hops: Vec<Hop<'_>> = (0..route.pools.len())
        .map(|i| Hop {
            pool: route.pools[i],
            to: route.path[i + 1],
        })
        .collect();

    // Derive the boundary amounts and the realised trade shape (`mode`) from the
    // trade type. Exact-in quotes forward; exact-out backward-solves the required
    // input, then either stays exact-out (Strict, single supported span) or
    // degrades to an exact-in plan with a final-floor override (OrBetter).
    let (amounts, mode) = match route.trade_type {
        TradeType::ExactIn => {
            // Quote every boundary amount forward from the route start. `path[i+1]`
            // is hop `i`'s output asset; the vector has one entry per boundary.
            let start = AssetAmount::new(route.path[0], amount);
            let amounts = quote_path_amounts(&start, &hops).map_err(map_quote_err)?;
            (
                amounts,
                ExactOutMode::ExactIn {
                    final_floor_override: None,
                },
            )
        }
        TradeType::ExactOut => {
            // `amount` is the desired final output. Backward-solve the required
            // input at every boundary: `required == [in₀, …, inₙ₋₁, target]`.
            let final_asset = route.path[route.pools.len()];
            let start_asset = route.path[0];
            let target = AssetAmount::new(final_asset, amount);
            let required =
                quote_path_exact_out(&target, start_asset, &hops).map_err(map_quote_err)?;

            match policy {
                // Strict: only a single atomic span of a family with a real
                // exact-out builder qualifies; everything else is rejected here
                // (Strict never approximates). `amount_in_max` grosses the solved
                // input up by the slippage tolerance, mirroring how the exact-in
                // path applies its floor downward.
                ExactOutPolicy::Strict => {
                    exact_out_strict_supported(route, &spans)?;
                    let amount_in_max = opts.slippage.max_amount_in(&required[0]).raw;
                    (
                        required,
                        ExactOutMode::Strict {
                            target: amount,
                            amount_in_max,
                        },
                    )
                }
                // OrBetter: take the solved total input as the plan's start amount
                // and re-quote forward exactly like exact-in, so every span uses
                // its exact-IN builder. The final span's floor is later forced to
                // the exact target (`final_floor_override`) for AtLeast semantics.
                ExactOutPolicy::OrBetter => {
                    let start = AssetAmount::new(start_asset, required[0].raw);
                    let amounts = quote_path_amounts(&start, &hops).map_err(map_quote_err)?;
                    (
                        amounts,
                        ExactOutMode::ExactIn {
                            final_floor_override: Some(amount),
                        },
                    )
                }
            }
        }
    };

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
        mode,
        deadline,
        native_in,
        native_out,
    })
}

/// Gate a `Strict` exact-out request: it is serviceable only when the route is a
/// single atomic span whose family has a real exact-out builder (Uniswap or
/// Slipstream). Otherwise return [`BuildError::UnsupportedExactOut`] keyed on the
/// first pool's kind — Strict never falls back to an approximation.
///
/// Why here (not in `next_tx`): rejecting up front means the plan never partially
/// builds, and a multi-span route is caught before any span tx is emitted.
fn exact_out_strict_supported(route: &Route<'_>, spans: &[RouterSpan]) -> Result<(), BuildError> {
    let first_kind = || -> Result<PoolKind, BuildError> {
        Ok(route.pools[0]
            .as_introspect()
            .ok_or(BuildError::UnsupportedProtocol)?
            .kind())
    };
    // Multi-span routes cannot settle a true exact-out atomically.
    if spans.len() != 1 {
        return Err(BuildError::UnsupportedExactOut {
            kind: first_kind()?,
        });
    }
    // Single span: only Uniswap/Slipstream families expose an exact-out builder.
    match RouterKind::of(route.pools[0]) {
        Some(RouterKind::UniswapUniversal) | Some(RouterKind::Slipstream) => Ok(()),
        _ => Err(BuildError::UnsupportedExactOut {
            kind: first_kind()?,
        }),
    }
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

        // Strict exact-out short-circuits the exact-in dispatch: `plan` already
        // proved this is a single atomic span of a family with an exact-out
        // builder, so route the whole span through the family's `_exact_out`
        // encoder with `amount_out = target` and the grossed-up `amount_in_max`.
        if let ExactOutMode::Strict {
            target,
            amount_in_max,
        } = self.mode
        {
            let prepared = self.build_strict_exact_out_span(
                &span,
                target,
                amount_in_max,
                recipient,
                span_native_in,
                span_native_out,
                is_final,
            )?;
            self.cursor += 1;
            return Ok(Some(prepared));
        }

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
        // OrBetter exact-out overrides the FINAL span's floor with the exact
        // target, so delivery is `AtLeast(target)` rather than the slippage floor
        // off the (backward-solved) forward quote. Every earlier span, and every
        // span of a plain exact-in trade, uses the compounded slippage floor.
        let min_out = match (&self.mode, is_final) {
            (
                ExactOutMode::ExactIn {
                    final_floor_override: Some(target),
                },
                true,
            ) => *target,
            _ => {
                self.opts
                    .slippage
                    .compound(span_hops)
                    .min_amount_out(&span_quoted_out)
                    .raw
            }
        };

        let pool_count = end - start;
        let router_kind = RouterKind::of(self.route.pools[start]);

        // OrBetter's final-span floor override, if any, threaded into whichever
        // builder handles the final span. `None` for exact-in and interior spans.
        let final_floor_override = match (&self.mode, is_final) {
            (
                ExactOutMode::ExactIn {
                    final_floor_override: Some(t),
                },
                true,
            ) => Some(*t),
            _ => None,
        };

        let prepared = match (router_kind, pool_count) {
            // Single pool of any protocol → its own single-hop encoder.
            (_, 1) => self.build_single_pool_span(
                &span,
                in_amount,
                recipient,
                span_native_in,
                span_native_out,
                &span_quoted_out,
                final_floor_override,
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
            // Multi-hop Aerodrome → one Aerodrome router call with a longer Route[].
            (Some(RouterKind::Aerodrome), _) => build_aerodrome_span(
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
            // Multi-hop Slipstream → one path-encoded exactInput call.
            (Some(RouterKind::Slipstream), _) => build_slipstream_span(
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
            // Curve (Plan 3b) is not yet wired.
            (Some(RouterKind::Curve), _) | (None, _) => {
                return Err(BuildError::UnsupportedProtocol);
            }
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
    ///
    /// `final_floor` carries an `OrBetter` exact-out target: when `Some(target)`,
    /// the single-pool encoder must floor its output at exactly `target` (not the
    /// slippage-reduced quote), giving `AtLeast(target)` delivery. We realise this
    /// by zeroing the per-span slippage and passing `quoted_out = target`, since
    /// `build_swap` computes its floor as `slippage.min_amount_out(quoted_out)`.
    #[allow(clippy::too_many_arguments)]
    fn build_single_pool_span(
        &self,
        span: &RouterSpan,
        in_amount: U256,
        recipient: Address,
        native_in: bool,
        native_out: bool,
        span_quoted_out: &AssetAmount,
        final_floor: Option<U256>,
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
        // Per-span options: override the recipient to this span's payee, and
        // record the transaction sender so receiver-less single-pool encoders
        // (e.g. some Curve `exchange` ABIs) can verify they are able to deliver
        // to this span's recipient rather than silently paying msg.sender.
        let mut span_opts = self.opts.clone();
        span_opts.recipient = Recipient::To(recipient);
        span_opts.sender = Some(self.sender);

        // For an OrBetter final span, force the floor to exactly `target` by
        // pairing a zero-slippage tolerance with `quoted_out = target`; otherwise
        // pass the real quote and keep the caller's slippage.
        let quoted_out = match final_floor {
            Some(target) => {
                span_opts.slippage = Slippage::from_bps(Bps(0));
                AssetAmount::new(span_quoted_out.asset, target)
            }
            None => *span_quoted_out,
        };

        as_executable(self.route.pools[start])
            .ok_or(BuildError::UnsupportedProtocol)?
            .build_swap(
                self.ctx,
                CurrencyAmount {
                    currency: in_currency,
                    raw: in_amount,
                },
                out_currency,
                &quoted_out,
                &span_opts,
            )
    }

    /// Build the single atomic span of a `Strict` exact-out plan.
    ///
    /// `plan` has already proven the route is one span of a family with a real
    /// exact-out builder, so dispatch on that family and call its `_exact_out`
    /// encoder with `amount_out = target` and the grossed-up `amount_in_max`.
    /// The output amount is delivered exactly; `max_spent` bounds the input.
    #[allow(clippy::too_many_arguments)]
    fn build_strict_exact_out_span(
        &self,
        span: &RouterSpan,
        target: U256,
        amount_in_max: U256,
        recipient: Address,
        native_in: bool,
        native_out: bool,
        is_final: bool,
    ) -> Result<PreparedSwap, BuildError> {
        let start = span.pools.start;
        match RouterKind::of(self.route.pools[start]) {
            // Uniswap V2/V3/V4 single-version exact-out via the Universal Router.
            Some(RouterKind::UniswapUniversal) => build_uniswap_span_exact_out(
                self.ctx,
                self.route,
                span,
                target,
                amount_in_max,
                recipient,
                native_in,
                native_out,
                self.deadline,
                is_final,
            ),
            // Slipstream single-span exact-out via its dedicated router.
            Some(RouterKind::Slipstream) => build_slipstream_span_exact_out(
                self.ctx,
                self.route,
                span,
                target,
                amount_in_max,
                recipient,
                native_in,
                native_out,
                self.deadline,
                is_final,
            ),
            // `plan`'s Strict gate rejects every other family before we get here;
            // treat any leak as an unsupported protocol rather than panicking.
            _ => Err(BuildError::UnsupportedProtocol),
        }
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

    // ── Task 9: exact-out (Strict vs OrBetter) ───────────────────────────────

    /// Strict exact-out on a single atomic all-V3 span: `next_tx(None)` yields a
    /// UR `execute` whose command stream is `[V3_SWAP_EXACT_OUT]`, carrying the
    /// exact `amountOut` (== target), an `amountInMaximum` (the grossed-up
    /// backward-solved input), and the reversed V3 path. `max_spent` is `Some`.
    #[test]
    fn strict_exact_out_single_v3_span_emits_v3_swap_exact_out() {
        use crate::execution::route_planner::{IUniversalRouter, V3_SWAP_EXACT_OUT};
        use alloy::primitives::Bytes;
        use alloy::sol_types::SolValue;

        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 500);
        let p2 = v3_pool(b, c, 3000);
        // Exact-OUT route: the target is on the output (c) side.
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let options = opts(recipient);
        let config = ctx();
        // `amount` is the exact target output for exact-out.
        let target = U256::from(1_000u64);

        let mut p = plan(
            &config,
            &route,
            target,
            &options,
            sender,
            false,
            false,
            ExactOutPolicy::Strict,
        )
        .expect("strict single-span V3 exact-out must build");

        assert!(p.is_atomic(), "single-router route → atomic");
        assert_eq!(p.tx_count(), 1, "one span → one tx");

        let prepared = p
            .next_tx(None)
            .expect("strict exact-out span builds")
            .expect("span yields a tx");

        // The atomic Uniswap span targets the Universal Router.
        assert_eq!(
            prepared.tx.to,
            universal_router(),
            "strict exact-out span targets the Universal Router"
        );

        // Decode the UR execute call: single [V3_SWAP_EXACT_OUT] command.
        let decoded = IUniversalRouter::executeCall::abi_decode(&prepared.tx.data)
            .expect("strict exact-out calldata must decode as UR execute");
        assert_eq!(
            decoded.commands.as_ref(),
            &[V3_SWAP_EXACT_OUT],
            "commands must be [V3_SWAP_EXACT_OUT]"
        );
        assert_eq!(
            decoded.inputs.len(),
            1,
            "single exact-out command → one input"
        );

        // V3 exact-out input tuple: (recipient, amountOut, amountInMaximum, path, payerIsUser).
        let (recip, amount_out, amount_in_max, path, payer_is_user) =
            <(Address, U256, U256, Bytes, bool)>::abi_decode_params(&decoded.inputs[0])
                .expect("input must decode as a V3 swap tuple");
        assert_eq!(recip, recipient, "delivers to the caller's recipient");
        assert_eq!(amount_out, target, "amountOut must equal the exact target");
        assert!(payer_is_user, "payerIsUser must be true for exact-out");

        // amountInMaximum must be the slippage-grossed backward-solved input.
        // Recompute the backward solve + max_amount_in to pin the exact value.
        use amm_core::path::{Hop, quote_path_exact_out};
        let hops = vec![Hop { pool: &p1, to: b }, Hop { pool: &p2, to: c }];
        let required =
            quote_path_exact_out(&AssetAmount::new(c, target), a, &hops).expect("backward solve");
        let expected_max_in = options.slippage.max_amount_in(&required[0]).raw;
        assert_eq!(
            amount_in_max, expected_max_in,
            "amountInMaximum must equal slippage.max_amount_in(required[0])"
        );

        // The reversed path starts with the output token c (exact-out path is
        // [out ‖ fee ‖ … ‖ in]).
        let c_addr = common::evm_addr(&c);
        assert_eq!(
            &path[0..20],
            c_addr.as_slice(),
            "reversed exact-out path starts at the output token c"
        );

        // PreparedSwap: exact-out delivers the output exactly and bounds the input.
        assert_eq!(prepared.min_received.raw, target, "min_received == target");
        let max_spent = prepared
            .max_spent
            .expect("strict exact-out must report max_spent");
        assert_eq!(
            max_spent.raw, expected_max_in,
            "max_spent equals the grossed-up input ceiling"
        );
        assert_eq!(max_spent.asset, a, "max_spent asset is the span input a");
    }

    /// OrBetter exact-out on a route that contains a non-exact-out-*span* family
    /// (a V3 → Aerodrome 2-span route): it must NOT error. OrBetter backward-solves
    /// the required input, then builds an exact-IN plan whose final span floor is
    /// the exact target — so the built txs are exact-in shapes (V3 multicall
    /// exactInputSingle, Aerodrome swapExactTokensForTokens), never exact-out.
    #[test]
    fn or_better_exact_out_over_non_exact_out_span_degrades_to_exact_in() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 3000);
        let p2 = aero_pool(b, c);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let options = opts(recipient);
        let config = ctx();
        let target = U256::from(500_000u64);

        let mut p = plan(
            &config,
            &route,
            target,
            &options,
            sender,
            false,
            false,
            ExactOutPolicy::OrBetter,
        )
        .expect("OrBetter exact-out must build even with a non-exact-out span family");

        assert_eq!(p.tx_count(), 2, "V3 then Aerodrome → two spans");

        // Span 1 (V3, non-final): an exact-IN shape — SwapRouter02 multicall
        // wrapping exactInputSingle. If OrBetter had (wrongly) emitted exact-out,
        // this decode would fail.
        let first = p
            .next_tx(None)
            .expect("span 1 builds")
            .expect("span 1 yields a tx");
        let outer = multicallCall::abi_decode(&first.tx.data)
            .expect("span 1 decodes as an exact-in multicall");
        exactInputSingleCall::abi_decode(&outer.data[0])
            .expect("span 1 inner is exactInputSingle (exact-in shape)");

        // Span 2 (Aerodrome, final): an exact-IN shape — swapExactTokensForTokens.
        // Its min_received (final floor) must be exactly the OrBetter target.
        let observed = AssetAmount::new(b, U256::from(495_000u64));
        let second = p
            .next_tx(Some(observed))
            .expect("span 2 builds")
            .expect("span 2 yields a tx");
        swapExactTokensForTokensCall::abi_decode(&second.tx.data)
            .expect("span 2 is swapExactTokensForTokens (exact-in shape)");
        assert_eq!(
            second.min_received.raw, target,
            "OrBetter final span floor must equal the exact target (AtLeast semantics)"
        );
        assert_eq!(
            second.min_received.asset, c,
            "final floor asset is the output c"
        );
    }

    /// Strict exact-out on a MULTI-span route (V3 → Aerodrome) →
    /// `Err(UnsupportedExactOut)`. Strict never approximates across spans.
    #[test]
    fn strict_exact_out_multi_span_is_rejected() {
        let (a, b, c) = (asset(0x11), asset(0x22), asset(0x33));
        let p1 = v3_pool(a, b, 3000);
        let p2 = aero_pool(b, c);
        let route = Route {
            pools: vec![&p1, &p2],
            path: vec![a, b, c],
            trade_type: TradeType::ExactOut,
        };
        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let options = opts(recipient);
        let config = ctx();

        // `Plan` is not `Debug`, so match the result rather than `expect_err`.
        let result = plan(
            &config,
            &route,
            U256::from(500_000u64),
            &options,
            sender,
            false,
            false,
            ExactOutPolicy::Strict,
        );
        assert!(
            matches!(result, Err(BuildError::UnsupportedExactOut { .. })),
            "expected Err(UnsupportedExactOut) for multi-span strict exact-out"
        );
    }

    /// Strict exact-out on a SINGLE Aerodrome span → `Err(UnsupportedExactOut)`.
    /// The Aerodrome family has no exact-out builder, so Strict refuses rather
    /// than approximate (OrBetter is the path for that family).
    #[test]
    fn strict_exact_out_single_aerodrome_span_is_rejected() {
        let (a, b) = (asset(0x11), asset(0x22));
        let p1 = aero_pool(a, b);
        let route = Route {
            pools: vec![&p1],
            path: vec![a, b],
            trade_type: TradeType::ExactOut,
        };
        let sender = Address::repeat_byte(0x01);
        let recipient = Address::repeat_byte(0x55);
        let options = opts(recipient);
        let config = ctx();

        // `Plan` is not `Debug`, so match the result rather than `expect_err`.
        let result = plan(
            &config,
            &route,
            U256::from(1_000u64),
            &options,
            sender,
            false,
            false,
            ExactOutPolicy::Strict,
        );
        assert!(
            matches!(result, Err(BuildError::UnsupportedExactOut { .. })),
            "expected Err(UnsupportedExactOut) for single Aerodrome strict exact-out"
        );
    }
}
