//! `run_case` — the single-hop execution engine for the matrix test suite.
//!
//! This module is the generalization of the per-protocol execution proofs in
//! `execution_v3.rs`, `execution_v4.rs`, and `execution_curve.rs`. It drives
//! one [`Case`] row against a live revm fork and asserts the on-chain output
//! equals (or beats) the off-chain quote to the wei.
//!
//! # Design
//!
//! * Single-hop only (Plan 3 adds the multi-hop seam via a public executor).
//! * Pure `match` dispatch throughout — no if/else chains.
//! * Snapshot/revert is the caller's responsibility; the runner does a
//!   snapshot before any state mutation and reverts unconditionally at the
//!   end so that a `Vec<Case>` can share a single fork.
//! * Residue check is Uniswap-family only: Curve and Aerodrome call the pool
//!   directly — no universal router holds intermediate funds.

#![allow(dead_code)]

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use amm_core::primitives::asset::AssetAmount;
use amm_rpc::execution::{ChainConfig, Currency, CurrencyAmount, as_executable, error::BuildError};

use super::{
    BuildErrorKind, Case, Direction, Expect, Fixture, RecipientKind, Trade, asset, exec_opts,
    fixtures, fork::Fork,
};

// ── Fixed impersonated addresses ──────────────────────────────────────────────

/// Impersonated swap sender. Bytes chosen to avoid collision with real accounts.
const SENDER: Address = Address::repeat_byte(0xBE);

/// Distinct recipient used when `case.recipient == RecipientKind::Distinct`.
const DISTINCT_RECIPIENT: Address = Address::repeat_byte(0xD1);

/// 1 ETH in wei — used as a headroom buffer for native-out and exact-out funding.
const ONE_ETH: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

// ── Public entry-point ────────────────────────────────────────────────────────

/// Execute `case` against `fork` under `cfg`, asserting the on-chain output
/// matches the off-chain quote to the wei.
///
/// # Panics
///
/// * When `case.pools.len() > 1` — multi-hop is deferred to Plan 3.
/// * When the fixture is unknown or the pool fails to refresh.
/// * When the build succeeds but the EVM transaction reverts (unless
///   `case.expect` is `RejectBuild`).
pub async fn run_case<P>(fork: &mut Fork<P>, cfg: &ChainConfig, case: &Case)
where
    P: Provider + Clone,
{
    // Multi-hop guard — loud and honest: this runner only handles single-hop.
    if case.is_multi_hop() {
        panic!("multi-hop run_case lands in Plan 3 with the public executor seam");
    }

    // ── Resolve fixture + pool ────────────────────────────────────────────────

    let fx: &Fixture = fixtures::fixture(case.pools[0]);
    // Refresh pool state at the fork's own block (not the fixture's default) so
    // the quote is taken against the exact EVM state the swap executes against —
    // required for the wei-exact assertion, and lets an AMM_FORK_BLOCK override
    // run the whole case at a recent block a keyless node still serves.
    let pool = fixtures::refresh(fx, fork.provider().clone(), fork.block_number()).await;

    // ── Resolve tokens from direction ─────────────────────────────────────────

    let (in_token, out_token) = resolve_tokens(fx, &case.direction);

    // ── Build currencies ──────────────────────────────────────────────────────

    let chain = case.chain.0;
    let in_currency = resolve_currency(chain, in_token, case.native_in);
    let out_currency = resolve_currency(chain, out_token, case.native_out);

    // Asset IDs used for quoting are the pool's ACTUAL currencies from the
    // fixture — WETH for WETH-holding pools (V2/V3/V4-WETH), address(0) for a
    // native-ETH V4 pool. The `native_in`/`native_out` flags only change the
    // `Currency` handed to the builder (wrap/unwrap) and the funding path; they
    // must not rewrite the quote asset (resolving native→WETH would look up WETH
    // in a pool that holds address(0) and fail with AssetNotInPool).
    let in_asset_id = asset(chain, in_token.addr);
    let out_asset_id = asset(chain, out_token.addr);

    // ── Recipient ─────────────────────────────────────────────────────────────

    let recipient = match case.recipient {
        RecipientKind::Sender => SENDER,
        RecipientKind::Distinct => DISTINCT_RECIPIENT,
    };

    let opts = exec_opts(SENDER).with_recipient(amm_rpc::execution::Recipient::To(recipient));

    // ── Snapshot before any mutation ──────────────────────────────────────────

    let snap = fork.snapshot();

    // ── Build the swap ────────────────────────────────────────────────────────

    let exe = as_executable(pool.as_ref()).expect("pool must be Executable");

    let build_result = match &case.trade {
        Trade::ExactIn { amount_in } => {
            let quoted = pool
                .quote(&AssetAmount::new(in_asset_id, *amount_in), &out_asset_id)
                .expect("pool must produce an exact-in quote");

            let result = exe.build_swap(
                cfg,
                CurrencyAmount {
                    currency: in_currency,
                    raw: *amount_in,
                },
                out_currency,
                &quoted,
                &opts,
            );
            // Carry the quoted output amount so the outcome asserter can use it.
            result.map(|p| (p, quoted.raw))
        }

        Trade::ExactOut {
            amount_out,
            policy: _,
        } => {
            // Attempt exact-out via the pool's reverse quoter. If the pool
            // does not implement ExactOut, build_swap_exact_out will return
            // UnsupportedExactOut — which is the expected error for RejectBuild
            // cases. For pools that do support it we get the input ceiling.
            //
            // `policy` is ExactOutPolicy; for single-hop the only behavioural
            // distinction is Strict vs OrBetter, which maps to whether we error
            // or execute-as-exact-in. Since build_swap_exact_out is the pool's
            // own path (no multi-hop routing), the policy is informational here;
            // the runner delegates fully to the Executable API and the outcome
            // assertion handles Exact vs AtLeast accordingly.
            let quoted_in = match pool.as_exact_out() {
                Some(eo) => eo
                    .quote_exact_out(&AssetAmount::new(out_asset_id, *amount_out), &in_asset_id)
                    .expect("exact-out quoter must produce an input estimate"),
                // No ExactOut support: let build_swap_exact_out return the
                // typed error so RejectBuild cases catch it cleanly.
                None => AssetAmount::new(in_asset_id, U256::ZERO),
            };

            let result = exe.build_swap_exact_out(
                cfg,
                CurrencyAmount {
                    currency: out_currency,
                    raw: *amount_out,
                },
                in_currency,
                &quoted_in,
                &opts,
            );
            // The expected output for exact-out is the exact target amount.
            result.map(|p| (p, *amount_out))
        }
    };

    // ── Negative case: builder must reject ────────────────────────────────────

    if let Expect::RejectBuild(ref kind) = case.expect {
        assert_build_error(build_result.map(|(p, _)| p), kind, case.name);
        // No on-chain submission; revert the (unmodified) snapshot and return.
        fork.revert(snap);
        return;
    }

    // ── Positive case: build must succeed ─────────────────────────────────────

    let (prepared, expected_out) =
        build_result.unwrap_or_else(|_| panic!("build must succeed for case {}", case.name));

    // ── Fund the sender ───────────────────────────────────────────────────────

    fund_input(fork, in_token, case, &prepared);

    // Apply the approval declared in the prepared swap.
    fork.apply_approval(SENDER, &prepared, case.approval);

    // For native-output swaps: seed the sender with 1 ETH so the EVM's
    // balance assertion (sender.balance >= tx.value) passes when tx.value == 0.
    // Mirrors execution_curve.rs:828 and execution_v4.rs:286.
    if case.native_out {
        fork.fund_native(SENDER, ONE_ETH);
    }

    // ── Record balances before submit ─────────────────────────────────────────

    let before = out_balance(fork, out_token, recipient, case.native_out);

    // Also record the sender's out-balance when the recipient is distinct, so
    // we can assert the sender received nothing.
    let sender_before = if matches!(case.recipient, RecipientKind::Distinct) {
        Some(out_balance(fork, out_token, SENDER, case.native_out))
    } else {
        None
    };

    // For ExactOut: snapshot the sender's INPUT balance before submit so we can
    // verify the actual spend does not exceed `prepared.max_spent`.
    let in_before_exact_out: Option<U256> = match &case.trade {
        Trade::ExactOut { .. } => Some(match case.native_in {
            true => fork.native_balance(SENDER),
            false => fork.erc20_balance(in_token.addr, SENDER),
        }),
        Trade::ExactIn { .. } => None,
    };

    // ── Submit ────────────────────────────────────────────────────────────────

    assert!(
        fork.submit(SENDER, &prepared.tx),
        "{}: swap reverted",
        case.name
    );

    // ── Measure output delta ──────────────────────────────────────────────────

    let after = out_balance(fork, out_token, recipient, case.native_out);
    let delta = after - before;

    // ── Assert outcome ────────────────────────────────────────────────────────

    assert_outcome(&case.expect, delta, expected_out, case.name);

    // ── ExactOut input-spend bound check ──────────────────────────────────────

    // Verify the sender did not over-spend the input token beyond the
    // max_spent ceiling declared in the prepared swap. Mirrors
    // execution_v3.rs:247-256 (`assert!(usdc_spent <= max_spent)`).
    //
    // Native-in note: gas_price=0, so the sender's ETH delta equals the amount
    // actually consumed by the router (the router refunds any unused WETH back
    // as ETH). The before/after native balance therefore cleanly measures spend.
    if let (Some(in_before), Trade::ExactOut { .. }) = (in_before_exact_out, &case.trade) {
        if let Some(ms) = &prepared.max_spent {
            let in_after = match case.native_in {
                true => fork.native_balance(SENDER),
                false => fork.erc20_balance(in_token.addr, SENDER),
            };
            let spent = in_before - in_after;
            assert!(
                spent <= ms.raw,
                "{}: input spent {spent} exceeds max_spent {}",
                case.name,
                ms.raw
            );
        }
    }

    // ── Residue check (Uniswap-family only) ───────────────────────────────────

    // Curve and Aerodrome call the pool directly — no universal router holds
    // intermediate funds. Only assert zero residue when a universal router is
    // configured (Uniswap V3/V4 path).
    if let Ok(router) = cfg.router_universal() {
        // Skip residue for native addresses (address(0)) — those are not ERC-20.
        let mut residue_tokens: Vec<Address> = Vec::new();
        if in_token.addr != Address::ZERO {
            residue_tokens.push(in_token.addr);
        }
        if out_token.addr != Address::ZERO && out_token.addr != in_token.addr {
            residue_tokens.push(out_token.addr);
        }
        if !residue_tokens.is_empty() {
            fork.assert_zero_residue(router, &residue_tokens);
        }
    }

    // ── Distinct-recipient isolation check ────────────────────────────────────

    // When recipient is Distinct, assert the sender did NOT receive any output.
    if let Some(sb) = sender_before {
        let sender_after = out_balance(fork, out_token, SENDER, case.native_out);
        assert_eq!(
            sender_after, sb,
            "{}: sender must not receive output when recipient is Distinct",
            case.name
        );
    }

    // ── Revert so subsequent cases start from clean state ─────────────────────

    fork.revert(snap);
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Resolve which fixture token is `in` and which is `out` based on direction.
fn resolve_tokens<'a>(
    fx: &'a Fixture,
    direction: &Direction,
) -> (
    &'a super::fixtures::TokenInfo,
    &'a super::fixtures::TokenInfo,
) {
    match direction {
        Direction::Forward => (&fx.token0, &fx.token1),
        Direction::Reverse => (&fx.token1, &fx.token0),
    }
}

/// Build a `Currency` for a fixture token. When `is_native` is `true` the
/// address is irrelevant — the user explicitly requested native ETH as input or
/// output, and the pool math side will use WETH via `Currency::resolve`.
fn resolve_currency(chain: u64, token: &super::fixtures::TokenInfo, is_native: bool) -> Currency {
    match is_native {
        true => Currency::Native,
        false => Currency::Token(asset(chain, token.addr)),
    }
}

/// Fund the sender with sufficient input token before submitting the swap.
///
/// For exact-out we fund with `amount_in ceiling × 2` (using the `max_spent`
/// field from the prepared swap when available; otherwise `quoted_in × 2`).
fn fund_input<P>(
    fork: &mut Fork<P>,
    in_token: &super::fixtures::TokenInfo,
    case: &Case,
    prepared: &amm_rpc::execution::PreparedSwap,
) where
    P: Provider + Clone,
{
    match &case.trade {
        Trade::ExactIn { amount_in } => match case.native_in {
            true => {
                // Fund amount_in + 1 ETH buffer. gas_price=0 but tx.value
                // consumes exact amount_in from the sender's balance.
                fork.fund_native(SENDER, *amount_in + ONE_ETH);
            }
            false => {
                fork.fund_erc20_verified(
                    SENDER,
                    in_token.addr,
                    in_token.balance_slot,
                    *amount_in * U256::from(2u64),
                );
            }
        },
        Trade::ExactOut {
            amount_out: _,
            policy: _,
        } => match case.native_in {
            true => {
                // For native-in exact-out, fund max_spent (with a 2× buffer)
                // plus 1 ETH headroom.
                let ceiling = prepared
                    .max_spent
                    .as_ref()
                    .map(|ms| ms.raw)
                    .unwrap_or(ONE_ETH);
                fork.fund_native(SENDER, ceiling * U256::from(2u64) + ONE_ETH);
            }
            false => {
                // Fund the max_spent ceiling (× 2) for ERC-20 exact-out.
                let ceiling = prepared
                    .max_spent
                    .as_ref()
                    .map(|ms| ms.raw)
                    .unwrap_or(ONE_ETH);
                fork.fund_erc20_verified(
                    SENDER,
                    in_token.addr,
                    in_token.balance_slot,
                    ceiling * U256::from(2u64),
                );
            }
        },
    }
}

/// Read the output-side balance (native or ERC-20) of `who`.
fn out_balance<P>(
    fork: &mut Fork<P>,
    out_token: &super::fixtures::TokenInfo,
    who: Address,
    native_out: bool,
) -> U256
where
    P: Provider + Clone,
{
    match native_out {
        true => fork.native_balance(who),
        false => fork.erc20_balance(out_token.addr, who),
    }
}

/// Assert the on-chain `delta` satisfies the [`Expect`] contract.
fn assert_outcome(expect: &Expect, delta: U256, expected_out: U256, name: &str) {
    match expect {
        // Exact-in: delta must equal the quoted output to the wei.
        Expect::WeiExact => assert_eq!(
            delta, expected_out,
            "{name}: exact-in output delta {delta} must equal quoted {expected_out}"
        ),
        // Exact-out Strict: received exactly the target.
        Expect::Exact => assert_eq!(
            delta, expected_out,
            "{name}: exact-out delta {delta} must equal target {expected_out}"
        ),
        // Exact-out OrBetter: received at least the target.
        Expect::AtLeast => assert!(
            delta >= expected_out,
            "{name}: exact-out OrBetter delta {delta} must be >= target {expected_out}"
        ),
        // RejectBuild is handled before this function is reached.
        Expect::RejectBuild(_) => unreachable!("RejectBuild must be handled before assert_outcome"),
    }
}

/// Assert that `result` is `Err` and its discriminant matches `kind`.
fn assert_build_error(
    result: Result<amm_rpc::execution::PreparedSwap, BuildError>,
    kind: &BuildErrorKind,
    name: &str,
) {
    match result {
        Ok(_) => panic!("{name}: expected build to fail with {kind:?} but it succeeded"),
        Err(err) => match (kind, &err) {
            (BuildErrorKind::UnsupportedExactOut, BuildError::UnsupportedExactOut { .. }) => {}
            (BuildErrorKind::UnsupportedProtocol, BuildError::UnsupportedProtocol) => {}
            (BuildErrorKind::NativeIntermediate, BuildError::NativeIntermediate) => {}
            _ => panic!("{name}: expected build error {kind:?}, got: {err:?}"),
        },
    }
}
