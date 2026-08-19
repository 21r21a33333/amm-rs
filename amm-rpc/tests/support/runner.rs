//! `run_case` / `run_plan_case` — single-hop and multi-hop execution engines.
//!
//! `run_case` is the generalization of the per-protocol execution proofs in
//! `execution_v3.rs`, `execution_v4.rs`, and `execution_curve.rs`. It drives
//! one [`Case`] row against a live revm fork and asserts the on-chain output
//! equals (or beats) the off-chain quote to the wei.
//!
//! `run_plan_case` drives one [`PlanCase`] against the same fork by looping
//! `Plan::next_tx`, funding only the first span's input token, and threading
//! each span's observed output as the next span's input.  The final output is
//! asserted against the `PlanCase::expect` contract.
//!
//! # Design
//!
//! * `run_case` is single-hop only; `run_plan_case` handles multi-hop via Plan.
//! * Pure `match` dispatch throughout — no if/else chains.
//! * Snapshot/revert wraps the whole case; a `Vec<Case>` can share one fork.
//! * Residue check is Uniswap-family only: Curve and Aerodrome call the pool
//!   directly — no universal router holds intermediate funds.

#![allow(dead_code)]

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use amm_core::primitives::asset::{AssetAmount, AssetId};
use amm_rpc::execution::routing::{ExactOutPolicy, RouterKind};
use amm_rpc::execution::{
    ChainConfig, Currency, CurrencyAmount, Recipient, TradeType, as_executable, error::BuildError,
    plan,
};

use super::{
    BuildErrorKind, Case, Direction, Expect, Fixture, PlanCase, RecipientKind, Trade, asset,
    exec_opts, fixtures, fork::Fork,
};

// ── Fixed impersonated addresses ──────────────────────────────────────────────

/// Impersonated swap sender. Bytes chosen to avoid collision with real accounts.
const SENDER: Address = Address::repeat_byte(0xBE);

/// Distinct recipient used when `case.recipient == RecipientKind::Distinct`.
const DISTINCT_RECIPIENT: Address = Address::repeat_byte(0xD1);

/// 1 ETH in wei — used as a headroom buffer for native-out and exact-out funding.
const ONE_ETH: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

/// Vyper-storage tokens whose `balanceOf` mapping reverses the key order
/// (`keccak(slot ++ holder)` instead of Solidity's `keccak(holder ++ slot)`).
/// Returns the base slot for `fund_erc20_vyper`; consulted before
/// [`known_balance_slot`] so Vyper tokens fund at the correct storage key.
fn vyper_balance_slot(token: Address) -> Option<u64> {
    use alloy::primitives::address;
    match token {
        // crvUSD (Curve.Fi USD, a Vyper contract) — verified slot 1, reversed.
        t if t == address!("f939E0A03FB07F59A73314E73794Be0E57ac1b4E") => Some(1),
        _ => None,
    }
}

/// Slot-stuff an ERC-20 balance, dispatching to the Vyper-order funder for
/// tokens in [`vyper_balance_slot`] and the Solidity-order funder otherwise.
fn fund_erc20_dispatch<P: Provider + Clone>(
    fork: &mut Fork<P>,
    holder: Address,
    token: Address,
    fallback_slot: u64,
    amount: U256,
) {
    match vyper_balance_slot(token) {
        Some(vslot) => fork.fund_erc20_vyper_verified(holder, token, vslot, amount),
        None => fork.fund_erc20_verified(holder, token, fallback_slot, amount),
    }
}

/// Known ERC-20 `balanceOf` mapping slots by token address. Authoritative for
/// funding a multi-hop route's first input, where the token may be a Curve
/// pool's third coin (outside the two-token fixture struct). Returns `None` for
/// tokens not in the table (the caller falls back to the fixture's slot).
fn known_balance_slot(token: Address) -> Option<u64> {
    use alloy::primitives::address;
    // Ethereum mainnet + Base.
    match token {
        t if t == address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48") => Some(9), // USDC (ETH)
        t if t == address!("6B175474E89094C44Da98b954EedeAC495271d0F") => Some(2), // DAI
        t if t == address!("dAC17F958D2ee523a2206206994597C13D831ec7") => Some(2), // USDT
        t if t == address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2") => Some(3), // WETH
        t if t == address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599") => Some(0), // WBTC
        t if t == address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913") => Some(9), // USDC (Base)
        _ => None,
    }
}

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
    let (in_currency, out_currency, in_asset_id, out_asset_id) =
        resolve_swap_currencies(chain, in_token, out_token, case);

    // ── Recipient ─────────────────────────────────────────────────────────────

    let recipient = resolve_recipient(&case.recipient);

    let opts = exec_opts(SENDER).with_recipient(amm_rpc::execution::Recipient::To(recipient));

    // ── Snapshot before any mutation ──────────────────────────────────────────

    let snap = fork.snapshot();

    // ── Build the swap ────────────────────────────────────────────────────────

    let exe = as_executable(pool.as_ref()).expect("pool must be Executable");

    let build_result = build_swap_result(
        exe,
        cfg,
        pool.as_ref(),
        in_currency,
        out_currency,
        in_asset_id,
        out_asset_id,
        &opts,
        case,
    );

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

    if let (Some(in_before), Trade::ExactOut { .. }) = (in_before_exact_out, &case.trade) {
        assert_exact_out_spend(fork, in_token, case, &prepared, in_before);
    }

    // ── Residue check (Uniswap-family only) ───────────────────────────────────

    // Curve and Aerodrome call the pool directly — no universal router holds
    // intermediate funds. Only assert zero residue when a universal router is
    // configured (Uniswap V3/V4 path).
    if let Ok(router) = cfg.router_universal() {
        // Skip residue for native addresses (address(0)) — those are not ERC-20.
        let mut residue_tokens: Vec<Address> = Vec::new();
        push_residue_token(&mut residue_tokens, in_token.addr);
        if out_token.addr != in_token.addr {
            push_residue_token(&mut residue_tokens, out_token.addr);
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

// ── Multi-hop plan harness ────────────────────────────────────────────────────

/// Execute `case` against `fork` under `cfg`, driving the public `Plan`
/// executor's `next_tx` loop and asserting the end-to-end output satisfies
/// `case.expect`.
///
/// # Flow
///
/// 1. Refresh all `case.pools` at the fork's block.
/// 2. Build a `Route` via `make_route`; resolve path addresses to `AssetId`s.
/// 3. Construct `Plan` via `amm_rpc::execution::plan`.
/// 4. Fund **only** the first span's input (intermediates flow through the sender).
/// 5. Loop: for each span — auto-detect and apply approval, submit, read the
///    span's output delta, pass it as `observed` to the next call.
/// 6. After the loop: assert the final recipient's end-to-end delta vs `case.expect`;
///    assert zero residue on the Universal Router for each Uniswap span's tokens.
/// 7. Snapshot/revert wraps the whole case.
///
/// # Panics
///
/// * When a fixture name is unknown.
/// * When the path address cannot be parsed.
/// * When `plan()` returns an error (bad route or unsupported protocol).
/// * When any span's transaction reverts.
pub async fn run_plan_case<P>(fork: &mut Fork<P>, cfg: &ChainConfig, case: &PlanCase)
where
    P: Provider + Clone,
{
    // ── Snapshot before any mutation ──────────────────────────────────────────
    let snap = fork.snapshot();

    // ── Resolve path addresses to AssetId ────────────────────────────────────
    let path = resolve_path(case);

    // ── Refresh all pools at this fork's block ────────────────────────────────
    let block = fork.block_number();
    let pool_boxes = super::route::refresh_pools(case.pools, fork.provider().clone(), block).await;
    let pool_refs: Vec<&dyn amm_core::traits::pool::Pool> =
        pool_boxes.iter().map(|b| b.as_ref()).collect();

    // ── Build route + plan ────────────────────────────────────────────────────
    let route = super::route::make_route(pool_refs, path.clone(), case.trade_type);

    // Recipient: Sender → SENDER, Distinct → DISTINCT_RECIPIENT.
    let recipient = resolve_recipient(&case.recipient);

    // Execution options: 50 bps slippage, explicit recipient, far-future deadline.
    // The deadline must be an AbsoluteTimestamp so `plan()` can resolve it once.
    let opts = exec_opts(SENDER).with_recipient(Recipient::To(recipient));

    // Exact-out policy is inferred from the expected outcome: `AtLeast` means the
    // caller wants OrBetter (deliver ≥ target); anything else is Strict (deliver
    // == target). Ignored for exact-in routes.
    let policy = match case.expect {
        Expect::AtLeast => ExactOutPolicy::OrBetter,
        _ => ExactOutPolicy::Strict,
    };
    let plan_result = plan(
        cfg,
        &route,
        case.amount,
        &opts,
        SENDER,
        case.native_in,
        case.native_out,
        policy,
    );

    // Plan-level rejection (e.g. Strict exact-out through a span family with no
    // exact-out builder): assert plan() returns the expected error and stop
    // before funding/submitting anything.
    if let Expect::RejectBuild(ref kind) = case.expect {
        match plan_result {
            Ok(_) => panic!(
                "PlanCase {}: expected plan() to reject with {kind:?} but it succeeded",
                case.name
            ),
            Err(ref e) => assert_plan_build_error(e, kind, case.name),
        }
        fork.revert(snap);
        return;
    }

    let mut p =
        plan_result.unwrap_or_else(|e| panic!("PlanCase {}: plan() failed: {e:?}", case.name));

    // ── Fund the FIRST span's input only ──────────────────────────────────────
    fund_first_span(fork, case, &path);

    // ── Collect the spans before entering the loop ────────────────────────────
    // We need the span list to determine each span's output token index and
    // whether it is Uniswap-family (for zero-residue assertions later).
    // Plan::spans() was added as a minimal public accessor for this purpose.
    let spans: Vec<_> = p.spans().to_vec();
    let tx_count = p.tx_count();

    // ── Track the final recipient's output delta end-to-end ──────────────────
    // Read the output-side balance before submitting any span; compare after
    // the loop to get the true end-to-end delta.
    let final_out_token_addr: Address = Address::from_word(path[path.len() - 1].token);
    let final_balance_before = match case.native_out {
        true => fork.native_balance(recipient),
        false => fork.erc20_balance(final_out_token_addr, recipient),
    };

    // ── Drive the Plan loop ───────────────────────────────────────────────────
    let observed = drive_spans(fork, cfg, &mut p, &spans, tx_count, recipient, case, &path);

    // ── Assert end-to-end output ──────────────────────────────────────────────
    // Read the final recipient's balance now and diff against the snapshot
    // taken before any span was submitted.
    let final_balance_after = match case.native_out {
        true => fork.native_balance(recipient),
        false => fork.erc20_balance(final_out_token_addr, recipient),
    };
    let total_delta = final_balance_after - final_balance_before;

    // The reference the outcome is asserted against: for exact-in it's the last
    // span's on-chain output (`observed`); for exact-out it's the caller's target
    // (`case.amount`) — Strict must deliver exactly it, OrBetter at least it.
    let expected_out = match case.trade_type {
        TradeType::ExactOut => case.amount,
        _ => observed.as_ref().map(|a| a.raw).unwrap_or(U256::ZERO),
    };

    assert_outcome(&case.expect, total_delta, expected_out, case.name);

    // ── Distinct-recipient isolation ──────────────────────────────────────────
    assert_distinct_isolation_plan(fork, case, final_out_token_addr);

    // ── Zero-residue check for Uniswap spans ──────────────────────────────────
    assert_uniswap_residue_plan(fork, cfg, &spans, &path);

    // ── Revert so subsequent cases start from clean state ─────────────────────
    fork.revert(snap);
}

// ── Private helpers — shared ──────────────────────────────────────────────────

/// Map `RecipientKind` to the impersonated address used in EVM submissions.
/// `Sender` → `SENDER`; `Distinct` → `DISTINCT_RECIPIENT`.
/// Byte-identical in both `run_case` and `run_plan_case`; lifted here to avoid
/// duplication.
fn resolve_recipient(kind: &RecipientKind) -> Address {
    match kind {
        RecipientKind::Sender => SENDER,
        RecipientKind::Distinct => DISTINCT_RECIPIENT,
    }
}

/// Push `addr` onto `v` only when it is non-zero (not native ETH) and not
/// already present.  Used by both residue collectors to avoid double-checking
/// the same token.
fn push_residue_token(v: &mut Vec<Address>, addr: Address) {
    if addr != Address::ZERO && !v.contains(&addr) {
        v.push(addr);
    }
}

// ── Private helpers — run_case ────────────────────────────────────────────────

/// Resolve the `Currency`, `AssetId` pair for in/out sides of a single-hop swap.
///
/// Asset IDs used for quoting are the pool's ACTUAL currencies from the
/// fixture — WETH for WETH-holding pools (V2/V3/V4-WETH), address(0) for a
/// native-ETH V4 pool. The `native_in`/`native_out` flags only change the
/// `Currency` handed to the builder (wrap/unwrap) and the funding path; they
/// must not rewrite the quote asset (resolving native→WETH would look up WETH
/// in a pool that holds address(0) and fail with AssetNotInPool).
fn resolve_swap_currencies(
    chain: u64,
    in_token: &super::fixtures::TokenInfo,
    out_token: &super::fixtures::TokenInfo,
    case: &Case,
) -> (Currency, Currency, AssetId, AssetId) {
    let in_currency = resolve_currency(chain, in_token, case.native_in);
    let out_currency = resolve_currency(chain, out_token, case.native_out);
    let in_asset_id = asset(chain, in_token.addr);
    let out_asset_id = asset(chain, out_token.addr);
    (in_currency, out_currency, in_asset_id, out_asset_id)
}

/// Build the swap and carry the expected output amount.
///
/// Owns the entire `match &case.trade { ExactIn.. / ExactOut.. }` build block
/// including the reverse-quote sub-block for exact-out.  Returns
/// `(prepared, expected_out)` on success, or a `BuildError` on the path that
/// the `RejectBuild` assertion catches.
#[allow(clippy::too_many_arguments)]
fn build_swap_result(
    exe: &dyn amm_rpc::execution::Executable,
    cfg: &ChainConfig,
    pool: &dyn amm_core::traits::pool::Pool,
    in_currency: Currency,
    out_currency: Currency,
    in_asset_id: AssetId,
    out_asset_id: AssetId,
    opts: &amm_rpc::execution::ExecutionOptions,
    case: &Case,
) -> Result<(amm_rpc::execution::PreparedSwap, U256), BuildError> {
    match &case.trade {
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
                opts,
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
                opts,
            );
            // The expected output for exact-out is the exact target amount.
            result.map(|p| (p, *amount_out))
        }
    }
}

/// Assert the ExactOut max-spent bound: the actual input spent must not exceed
/// `prepared.max_spent`.
///
/// Mirrors execution_v3.rs:247-256 (`assert!(usdc_spent <= max_spent)`).
///
/// Native-in note: gas_price=0, so the sender's ETH delta equals the amount
/// actually consumed by the router (the router refunds any unused WETH back
/// as ETH). The before/after native balance therefore cleanly measures spend.
fn assert_exact_out_spend<P>(
    fork: &mut Fork<P>,
    in_token: &super::fixtures::TokenInfo,
    case: &Case,
    prepared: &amm_rpc::execution::PreparedSwap,
    in_before: U256,
) where
    P: Provider + Clone,
{
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

// ── Private helpers — run_plan_case ──────────────────────────────────────────

/// Parse `case.path` hex addresses into chain-scoped `AssetId`s.
///
/// `case.path` entries are hex EVM addresses; parse each to `Address` then
/// build the chain-scoped `AssetId` used by the pool and routing layers.
fn resolve_path(case: &PlanCase) -> Vec<AssetId> {
    let chain = case.chain.0;
    case.path
        .iter()
        .map(|hex| {
            let addr: Address = hex
                .parse()
                .unwrap_or_else(|_| panic!("PlanCase {}: invalid address {hex}", case.name));
            asset(chain, addr)
        })
        .collect()
}

/// Fund the first span's input token only.
///
/// Subsequent spans receive their input from the previous span's output which
/// lands on SENDER.  Only the first span's input must be pre-funded.
///
/// Resolves the input token's balance slot via `known_balance_slot` first
/// (authoritative for Curve third-coin inputs), then falls back to the
/// fixture's two-token match.
fn fund_first_span<P>(fork: &mut Fork<P>, case: &PlanCase, path: &[AssetId])
where
    P: Provider + Clone,
{
    let first_token_addr: Address = Address::from_word(path[0].token);
    let first_fx = fixtures::fixture(case.pools[0]);
    // Resolve the input token's balance slot. A 2-token fixture's token0/token1
    // covers most pools, but a 3-coin Curve pool's input may be its third coin
    // (not token0/token1) — so consult the known-slots table by address first,
    // falling back to the fixture's two-token match.
    let first_token_slot = known_balance_slot(first_token_addr).unwrap_or(
        match first_token_addr == first_fx.token0.addr {
            true => first_fx.token0.balance_slot,
            false => first_fx.token1.balance_slot,
        },
    );

    // For exact-in, `case.amount` is the input, so 2× covers it. For exact-out,
    // `case.amount` is the OUTPUT target — the required input varies with price
    // and decimals (and can dwarf the target's raw magnitude, e.g. WETH→cbBTC),
    // so slot-inject a large, decimals-agnostic balance. Each span's ERC-20
    // approval is bounded to its `max_spent`, so the router pulls only what the
    // swap needs; over-funding is harmless and the spent-delta bound still holds.
    let erc20_fund = match case.trade_type {
        TradeType::ExactIn => case.amount * U256::from(2u64),
        _ => U256::from(10u64).pow(U256::from(30u64)),
    };
    let native_fund = match case.trade_type {
        TradeType::ExactIn => case.amount * U256::from(2u64) + ONE_ETH,
        _ => U256::from(10u64).pow(U256::from(24u64)), // 1e24 wei ≈ 1e6 ETH
    };

    match case.native_in {
        true => {
            // native_in: fund native ETH (2× amount for exact-in, a large
            // ceiling for exact-out) with headroom.
            fork.fund_native(SENDER, native_fund);
        }
        false => {
            // ERC-20 input: slot-inject the fund amount and verify the read-back.
            // Vyper tokens (crvUSD) use the reversed mapping-key order.
            fund_erc20_dispatch(fork, SENDER, first_token_addr, first_token_slot, erc20_fund);
            // Also seed a small ETH buffer so the EVM does not reject the tx.
            fork.fund_native(SENDER, ONE_ETH);
        }
    }
}

/// Drive the `Plan::next_tx` loop, applying approvals and submitting each span.
///
/// Returns the last `observed` `AssetAmount` after the loop (the final span's
/// on-chain output), or `None` if the plan produced zero transactions.
#[allow(clippy::too_many_arguments)]
fn drive_spans<P>(
    fork: &mut Fork<P>,
    cfg: &ChainConfig,
    p: &mut amm_rpc::execution::Plan<'_>,
    spans: &[amm_rpc::execution::routing::RouterSpan],
    tx_count: usize,
    recipient: Address,
    case: &PlanCase,
    path: &[AssetId],
) -> Option<AssetAmount>
where
    P: Provider + Clone,
{
    let mut observed: Option<AssetAmount> = None;
    let mut span_idx: usize = 0;

    while let Some(tx) = p.next_tx(observed).unwrap_or_else(|e| {
        panic!(
            "PlanCase {}: next_tx span {span_idx} failed: {e:?}",
            case.name
        )
    }) {
        let span = &spans[span_idx];
        let is_last = span_idx == tx_count - 1;

        // ── Approval auto-detect ──────────────────────────────────────────────
        // Each span declares its own approval requirement in `tx.approval`.
        // Uniswap UR spans use Permit2; single-pool V3/Curve/Aero spans approve
        // the router/pool contract directly.
        if let Some(req) = &tx.approval {
            let token = Address::from_word(req.token.token);
            let permit2_addr = cfg.permit2().unwrap_or_default();
            match req.spender == permit2_addr {
                true => {
                    // Uniswap Universal Router path: ERC-20 → Permit2 → UR.
                    fork.permit2_approve(
                        SENDER,
                        token,
                        req.spender,    // permit2
                        super::dsl::UR, // UR is the Permit2 downstream spender
                        req.min_allowance,
                        1_000_000_000_000u64, // far-future expiration that fits uint48
                    );
                }
                false => {
                    // Direct-approval path (V3 router, Aerodrome router, …).
                    fork.approve(SENDER, token, req.spender, req.min_allowance);
                }
            }
        }

        // ── Determine this span's output token and recipient ──────────────────
        // `span.pools.end` is the boundary index into `route.path` for this
        // span's output token.  For the last span, that is the route end;
        // for earlier spans it is an intermediate token that lands on SENDER.
        let out_token_idx = span.pools.end;
        let out_asset_id = path[out_token_idx];
        let out_token_addr = Address::from_word(out_asset_id.token);

        // Balance of the span's output party before submission.
        // Non-final spans deliver to SENDER; the final span to the recipient.
        let span_recipient = match is_last {
            true => recipient,
            false => SENDER,
        };
        let balance_before = match is_last && case.native_out {
            true => fork.native_balance(span_recipient),
            false => fork.erc20_balance(out_token_addr, span_recipient),
        };

        // ── Submit ────────────────────────────────────────────────────────────
        assert!(
            fork.submit(SENDER, &tx.tx),
            "PlanCase {}: span {} reverted",
            case.name,
            span_idx,
        );

        // ── Measure output delta ──────────────────────────────────────────────
        let balance_after = match is_last && case.native_out {
            true => fork.native_balance(span_recipient),
            false => fork.erc20_balance(out_token_addr, span_recipient),
        };
        let delta = balance_after - balance_before;

        // The observed output becomes the next span's input amount.
        observed = Some(AssetAmount::new(out_asset_id, delta));
        span_idx += 1;
    }

    observed
}

/// Assert the distinct-recipient isolation invariant for `run_plan_case`.
///
/// When the recipient is Distinct, assert the SENDER did not receive the
/// final output (only intermediate tokens should pass through SENDER).
fn assert_distinct_isolation_plan<P>(
    fork: &mut Fork<P>,
    case: &PlanCase,
    final_out_token_addr: Address,
) where
    P: Provider + Clone,
{
    if matches!(case.recipient, RecipientKind::Distinct) {
        // We only check the final output token here; intermediate residue is
        // covered by the zero-residue check below for Uniswap spans.
        let sender_out_after = match case.native_out {
            true => fork.native_balance(SENDER),
            false => fork.erc20_balance(final_out_token_addr, SENDER),
        };
        // SENDER was seeded with ONE_ETH for gas; only assert ERC-20 isolation.
        if !case.native_out {
            // After all spans complete, the SENDER should not hold any of the
            // final output token (the last span paid the distinct recipient).
            assert_eq!(
                sender_out_after,
                U256::ZERO,
                "PlanCase {}: SENDER must not receive final output when recipient is Distinct",
                case.name,
            );
        }
    }
}

/// Assert zero residue on the Universal Router for all Uniswap-family spans.
///
/// The Universal Router may hold intermediate token balances while executing
/// a multi-hop swap; those must be fully swept.  Check after all spans.
fn assert_uniswap_residue_plan<P>(
    fork: &mut Fork<P>,
    cfg: &ChainConfig,
    spans: &[amm_rpc::execution::routing::RouterSpan],
    path: &[AssetId],
) where
    P: Provider + Clone,
{
    if let Ok(router) = cfg.router_universal() {
        // Collect intermediate tokens touched by each Uniswap-family span.
        // Span boundaries: path[span.pools.start..=span.pools.end].
        // We check every non-zero ERC-20 address in those boundary slots.
        let mut residue_tokens: Vec<Address> = Vec::new();
        for span in spans {
            if span.kind != RouterKind::UniswapUniversal {
                // Non-Uniswap spans do not route through the UR.
                continue;
            }
            // Check all tokens that this Uniswap span could hold:
            // input (path[start]), any intermediates, and output (path[end]).
            for asset_id in path.iter().take(span.pools.end + 1).skip(span.pools.start) {
                let addr = Address::from_word(asset_id.token);
                push_residue_token(&mut residue_tokens, addr);
            }
        }
        if !residue_tokens.is_empty() {
            fork.assert_zero_residue(router, &residue_tokens);
        }
    }
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
                fund_erc20_dispatch(
                    fork,
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
                fund_erc20_dispatch(
                    fork,
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

/// Assert a `plan()` error matches the expected `BuildErrorKind` discriminant.
///
/// The multi-hop analogue of [`assert_build_error`]: `plan()` returns
/// `Result<Plan, BuildError>` (not `Result<PreparedSwap, _>`), so the reject is
/// asserted on the error directly.
fn assert_plan_build_error(err: &BuildError, kind: &BuildErrorKind, name: &str) {
    let matched = matches!(
        (kind, err),
        (
            BuildErrorKind::UnsupportedExactOut,
            BuildError::UnsupportedExactOut { .. }
        ) | (
            BuildErrorKind::UnsupportedProtocol,
            BuildError::UnsupportedProtocol
        ) | (
            BuildErrorKind::NativeIntermediate,
            BuildError::NativeIntermediate
        ) | (
            BuildErrorKind::RecipientNotSupported,
            BuildError::RecipientNotSupported
        )
    );
    assert!(
        matched,
        "{name}: expected plan() error {kind:?}, got: {err:?}"
    );
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
            (BuildErrorKind::RecipientNotSupported, BuildError::RecipientNotSupported) => {}
            _ => panic!("{name}: expected build error {kind:?}, got: {err:?}"),
        },
    }
}
