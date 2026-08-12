//! Uniswap V4 swap encoder: exact-in and exact-out → [`PreparedSwap`].
//!
//! Encodes single-hop V4 swaps via the Universal Router `V4_SWAP` (0x10) command.
//! The outer call is `execute(commands, inputs, deadline)` with one or more entries.
//! The `V4_SWAP` input is `abi.encode(bytes actions, bytes[] params)` where:
//!
//! - **exact-in** actions: `[0x06, 0x0c, 0x0e]` (`SWAP_EXACT_IN_SINGLE`, `SETTLE_ALL`, `TAKE`)
//! - **exact-out** actions: `[0x08, 0x0c, 0x0e]` (`SWAP_EXACT_OUT_SINGLE`, `SETTLE_ALL`, `TAKE`)
//!
//! ## WETH-wrap case (native-in, WETH-currency pool)
//!
//! When the pool exposes WETH as a currency (not `address(0)`) but the caller
//! holds native ETH, the router wraps ETH to WETH before the swap:
//!
//! - **exact-in** commands: `[WRAP_ETH, V4_SWAP]`
//!   - actions inside V4_SWAP: `[SWAP_EXACT_IN_SINGLE, SETTLE, TAKE]`
//!   - `SETTLE` (0x0b) settles from the router's post-wrap WETH balance (`payerIsUser = false`)
//! - **exact-out** commands: `[WRAP_ETH, V4_SWAP, UNWRAP_WETH]`
//!   - same SETTLE pattern; `UNWRAP_WETH` returns leftover WETH to recipient as ETH
//!
//! ## WETH-unwrap case (native-out, WETH-currency pool)
//!
//! When the pool exposes WETH and the caller wants native ETH out (token → ETH),
//! the router receives WETH from the pool and unwraps it to ETH:
//!
//! - **exact-in** commands: `[V4_SWAP, UNWRAP_WETH]`
//!   - actions inside V4_SWAP: `[SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE]`
//!   - `TAKE` delivers WETH to the router (`ADDRESS_THIS`); `UNWRAP_WETH` pays
//!     the recipient with the slippage floor as minimum ETH out
//! - **exact-out** commands: `[V4_SWAP, UNWRAP_WETH]`
//!   - `SETTLE_ALL` pulls the token input from the user via Permit2;
//!     `UNWRAP_WETH` uses the exact output amount as the minimum
//!
//! ## First-class native (address(0) pool)
//!
//! Native ETH is supported on pools where `currency0 == address(0)` (V4 native-first-class).
//! On a **native-in** swap the caller sends `tx.value = amountIn` and no Permit2 approval is
//! needed; on a **native-out** swap `tx.value = 0` and the token input is approved via Permit2.
//! A `price_limit` is unsupported: V4's Router swap actions carry no
//! `sqrtPriceLimitX96` — slippage is bounded by `amountOutMinimum`/`amountInMaximum`.
//! Approval spender is **Permit2**, not the router — V4 pulls via Permit2.

use alloy::primitives::aliases::{I24, U24};
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolValue;
use amm_core::primitives::asset::AssetAmount;
use amm_core::protocols::uniswap::v4::UniswapV4Pool;
use amm_core::traits::pool::Pool;

use crate::execution::{
    config::ChainConfig,
    error::BuildError,
    executable::{Executable, Sealed},
    options::ExecutionOptions,
    prepared::{PreparedSwap, Route},
    protocols::common,
    route_planner::{RoutePlanner, UNWRAP_WETH, V4_SWAP, WRAP_ETH},
    types::{Currency, CurrencyAmount, TradeType, UnsignedTx},
};

// ── V4 action bytes (single-hop) ────────────────────────────────────────────

/// V4 action byte: `SWAP_EXACT_IN_SINGLE` (0x06).
pub const ACTION_SWAP_EXACT_IN_SINGLE: u8 = 0x06;

/// V4 action byte: `SWAP_EXACT_OUT_SINGLE` (0x08).
pub const ACTION_SWAP_EXACT_OUT_SINGLE: u8 = 0x08;

/// V4 action byte: `SETTLE` (0x0b) — settle the exact swap debt from the router's
/// own balance when `payerIsUser = false`. Used in the WETH-wrap path after the
/// router has received WETH from `WRAP_ETH`.
pub const ACTION_SETTLE: u8 = 0x0b;

/// V4 action byte: `SETTLE_ALL` (0x0c) — settle input currency up to `amount`,
/// paying from the user via Permit2 or direct transfer.
pub const ACTION_SETTLE_ALL: u8 = 0x0c;

/// V4 action byte: `TAKE` (0x0e) — take `amount` of a currency to a recipient.
pub const ACTION_TAKE: u8 = 0x0e;

/// V4 amount sentinel: take/settle the full open delta for the currency.
const OPEN_DELTA: U256 = U256::ZERO;

/// Universal Router recipient sentinel: the router's own address.
/// Used as the destination for `WRAP_ETH` so the wrapped WETH stays in the
/// router and can be spent by the subsequent `SETTLE` V4 action.
const ADDRESS_THIS: Address = Address::with_last_byte(2);

/// Packed action sequence for an exact-in single-hop V4 swap (standard path).
///
/// `[SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE]`
pub const ACTIONS_EXACT_IN: [u8; 3] = [ACTION_SWAP_EXACT_IN_SINGLE, ACTION_SETTLE_ALL, ACTION_TAKE];

/// Packed action sequence for an exact-out single-hop V4 swap (standard path).
///
/// `[SWAP_EXACT_OUT_SINGLE, SETTLE_ALL, TAKE]`
pub const ACTIONS_EXACT_OUT: [u8; 3] =
    [ACTION_SWAP_EXACT_OUT_SINGLE, ACTION_SETTLE_ALL, ACTION_TAKE];

/// Packed action sequence for an exact-in wrap swap: SETTLE from the router's
/// WETH balance, not from the user.
///
/// `[SWAP_EXACT_IN_SINGLE, SETTLE, TAKE]`
const ACTIONS_WRAP_EXACT_IN: [u8; 3] = [ACTION_SWAP_EXACT_IN_SINGLE, ACTION_SETTLE, ACTION_TAKE];

/// Packed action sequence for an exact-out wrap swap: SETTLE from the router's
/// WETH balance.
///
/// `[SWAP_EXACT_OUT_SINGLE, SETTLE, TAKE]`
const ACTIONS_WRAP_EXACT_OUT: [u8; 3] = [ACTION_SWAP_EXACT_OUT_SINGLE, ACTION_SETTLE, ACTION_TAKE];

// ── Deployed ABI layout (DO NOT add `minHopPriceX36` — that is the v4-periphery
//    `main` layout that reverts every swap on the deployed router) ────────────

sol! {
    /// The V4 pool key identifying a pool in the PoolManager.
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    /// V4 `exactInputSingle` params — deployed layout (no `minHopPriceX36`).
    struct ExactInputSingleParams {
        PoolKey poolKey;
        bool zeroForOne;
        uint128 amountIn;
        uint128 amountOutMinimum;
        bytes hookData;
    }

    /// V4 `exactOutputSingle` params — deployed layout (no `minHopPriceX36`).
    struct ExactOutputSingleParams {
        PoolKey poolKey;
        bool zeroForOne;
        uint128 amountOut;
        uint128 amountInMaximum;
        bytes hookData;
    }
}

// ── Encoder implementation ───────────────────────────────────────────────────

/// Build the `PoolKey` for a V4 pool from its on-chain identity.
///
/// `assets()[0]` is always `currency0` (numerically smaller); the pool stores
/// them pre-sorted by the pool manager.
fn build_pool_key(pool: &UniswapV4Pool) -> Result<(PoolKey, Address, Address), BuildError> {
    let c0 = common::evm_addr(&pool.assets()[0]);
    let c1 = common::evm_addr(&pool.assets()[1]);
    let fee = U24::try_from(pool.key_fee()).map_err(|_| BuildError::Overflow)?;
    let tick_spacing = I24::try_from(pool.tick_spacing()).map_err(|_| BuildError::Overflow)?;
    let pool_key = PoolKey {
        currency0: c0,
        currency1: c1,
        fee,
        tickSpacing: tick_spacing,
        hooks: pool.hooks_address(),
    };
    Ok((pool_key, c0, c1))
}

/// Assemble the `V4_SWAP` input bytes:
/// `abi.encode(bytes actions, bytes[] params)` via `abi_encode_params`.
fn build_v4_swap_input(actions: &[u8], params: Vec<Bytes>) -> Bytes {
    let actions_bytes = Bytes::from(actions.to_vec());
    <(Bytes, Vec<Bytes>)>::abi_encode_params(&(actions_bytes, params)).into()
}

/// Determine whether the caller's native ETH must be wrapped to WETH before the swap.
///
/// Returns `true` when the caller provides native ETH (native-in) or expects
/// native ETH out (native-out), the pool exposes WETH as a currency, and the pool
/// does NOT have a first-class `address(0)` currency.  In that case the Universal
/// Router wraps/unwraps ETH around the `V4_SWAP`.
///
/// `weth` is `common::evm_addr(&ctx.weth)`; `c0`/`c1` are the pool's sorted
/// currency addresses from `build_pool_key`.
fn is_wrap_case(
    amount_in_currency: Currency,
    to: Currency,
    weth: Address,
    c0: Address,
    c1: Address,
) -> bool {
    let pool_has_weth = c0 == weth || c1 == weth;
    let pool_has_native = c0 == Address::ZERO || c1 == Address::ZERO;
    // Wrap is needed when native ETH is on either side (in or out), the pool
    // exposes WETH, and there is no first-class address(0) currency on the pool.
    (amount_in_currency.is_native() || to.is_native()) && pool_has_weth && !pool_has_native
}

impl Sealed for UniswapV4Pool {}

impl Executable for UniswapV4Pool {
    /// Build an exact-in V4 swap, supporting ERC-20, native-ETH (address(0) pool),
    /// ETH-to-token via WETH wrap, and token-to-native ETH via WETH unwrap.
    ///
    /// ## Address(0)-native path
    ///
    /// When the pool has `currency0 == address(0)`, native ETH is first-class.
    /// `tx.value = amountIn`; no Permit2 approval; commands = `[V4_SWAP]` with
    /// `SETTLE_ALL`.
    ///
    /// ## WETH-wrap path (native-in, WETH pool)
    ///
    /// When the pool's currencies include WETH (not address(0)) and the caller
    /// holds native ETH: commands = `[WRAP_ETH, V4_SWAP]`. `WRAP_ETH` deposits
    /// `amountIn` WETH to the router; the V4 `SETTLE` action (payerIsUser=false)
    /// pays the swap from that balance. `tx.value = amountIn`; `approval = None`.
    ///
    /// ## WETH-unwrap path (native-out, WETH pool)
    ///
    /// When the output is native ETH and the pool exposes WETH: commands =
    /// `[V4_SWAP, UNWRAP_WETH]`. The V4 `TAKE` action delivers WETH to the
    /// router (`ADDRESS_THIS`); `UNWRAP_WETH` converts it to ETH and pays the
    /// recipient, enforcing the slippage floor as the minimum ETH out.
    /// `tx.value = 0`; Permit2 approval required on the token input.
    ///
    /// ## ERC-20 path
    ///
    /// Token input: commands = `[V4_SWAP]`, `SETTLE_ALL`, Permit2 approval required.
    ///
    /// A `price_limit` returns [`BuildError::UnsupportedProtocol`]: the V4 Router
    /// swap actions carry no `sqrtPriceLimitX96` field (unlike V3).
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        route: &Route,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        // Must call build_pool_key before resolve_swap_with so we can compute
        // needs_wrap from the pool's actual currency addresses.
        let (pool_key, c0, _c1) = build_pool_key(self)?;
        let weth = common::evm_addr(&ctx.weth);

        let needs_wrap = is_wrap_case(amount_in.currency, to, weth, c0, _c1);

        // For the wrap case, resolve Native → ctx.weth so pool-membership checks
        // match the pool's WETH currency.  For all other paths, resolve Native →
        // address(0) (V4 first-class native).
        let native_asset = if needs_wrap {
            ctx.weth
        } else {
            amm_core::primitives::asset::AssetId::new(ctx.chain, alloy::primitives::B256::ZERO)
        };

        let r = common::resolve_swap_with(
            ctx,
            self,
            amount_in.currency,
            to,
            route,
            opts,
            TradeType::ExactIn,
            native_asset,
        )?;

        // V4's Router ExactInputSingle action has no sqrtPriceLimitX96 field
        // (unlike V3): a per-swap price limit would require calling
        // PoolManager.swap directly, which the Universal Router does not do.
        // Slippage is bounded by amountOutMinimum below.
        if opts.price_limit.is_some() {
            return Err(BuildError::UnsupportedProtocol);
        }

        let zero_for_one = common::evm_addr(&r.input) == c0;

        // Swap struct amounts are uint128; convert from U256.
        let amount_in_u128 = u128::try_from(amount_in.raw).map_err(|_| BuildError::Overflow)?;
        let min = opts.slippage.min_amount_out(quoted_out);
        let min_u128 = u128::try_from(min.raw).map_err(|_| BuildError::Overflow)?;

        let swap_params = ExactInputSingleParams {
            poolKey: pool_key,
            zeroForOne: zero_for_one,
            amountIn: amount_in_u128,
            amountOutMinimum: min_u128,
            hookData: Bytes::new(),
        };
        let p_swap: Bytes = swap_params.abi_encode().into();

        // TAKE: deliver the full output delta to the recipient. The slippage floor is
        // enforced by the swap action's amountOutMinimum, so no min is needed here.
        let p_take: Bytes = <(Address, Address, U256)>::abi_encode_params(&(
            common::evm_addr(&r.output),
            r.recipient,
            OPEN_DELTA,
        ))
        .into();

        let deadline_secs = u64::try_from(r.deadline).map_err(|_| BuildError::Overflow)?;

        let (data, value, approval) = match (needs_wrap, r.native_in) {
            // WETH-wrap exact-in: native ETH in → token out via WETH pool.
            // Prepend WRAP_ETH; the V4 SETTLE action drains the router's WETH
            // balance (payerIsUser=false) instead of pulling from the user.
            (true, true) => {
                // WRAP_ETH input: (recipient=ADDRESS_THIS, amount=amountIn).
                let wrap_input: Bytes =
                    <(Address, U256)>::abi_encode_params(&(ADDRESS_THIS, amount_in.raw)).into();
                // SETTLE: currency=WETH, amount=OPEN_DELTA, payerIsUser=false.
                let p_settle: Bytes =
                    <(Address, U256, bool)>::abi_encode_params(&(weth, OPEN_DELTA, false)).into();
                let v4_input =
                    build_v4_swap_input(&ACTIONS_WRAP_EXACT_IN, vec![p_swap, p_settle, p_take]);
                let mut planner = RoutePlanner::new();
                planner.add(WRAP_ETH, wrap_input).add(V4_SWAP, v4_input);
                // ETH is sent as tx.value; no Permit2 approval needed.
                (planner.encode(deadline_secs), amount_in.raw, None)
            }
            // WETH-wrap exact-in: token in → native ETH out via WETH pool.
            // The pool outputs WETH to the router (TAKE to ADDRESS_THIS); the
            // router then unwraps it to ETH and pays the recipient. The token
            // input is settled from the user via Permit2 (SETTLE_ALL).
            (true, false) => {
                // SETTLE_ALL: pull the ERC-20 input from the user via Permit2.
                let p_settle: Bytes = <(Address, U256)>::abi_encode_params(&(
                    common::evm_addr(&r.input),
                    amount_in.raw,
                ))
                .into();
                // TAKE to ADDRESS_THIS: router holds the WETH output for unwrapping.
                let p_take_router: Bytes = <(Address, Address, U256)>::abi_encode_params(&(
                    common::evm_addr(&r.output),
                    ADDRESS_THIS,
                    OPEN_DELTA,
                ))
                .into();
                let v4_input =
                    build_v4_swap_input(&ACTIONS_EXACT_IN, vec![p_swap, p_settle, p_take_router]);
                // UNWRAP_WETH: convert router WETH to ETH and send to recipient,
                // enforcing the slippage floor as the minimum ETH out.
                let unwrap_input: Bytes =
                    <(Address, U256)>::abi_encode_params(&(r.recipient, min.raw)).into();
                let mut planner = RoutePlanner::new();
                planner
                    .add(V4_SWAP, v4_input)
                    .add(UNWRAP_WETH, unwrap_input);
                // Token input: tx.value=0; Permit2 approval on the input token.
                let approval = Some(common::erc20_approval(
                    ctx.permit2()?,
                    r.input,
                    amount_in.raw,
                ));
                (planner.encode(deadline_secs), U256::ZERO, approval)
            }
            // Standard path: SETTLE_ALL from user (ERC-20 via Permit2, or address(0) native).
            _ => {
                // SETTLE_ALL: settle input currency up to amountIn (U256).
                let p_settle: Bytes = <(Address, U256)>::abi_encode_params(&(
                    common::evm_addr(&r.input),
                    amount_in.raw,
                ))
                .into();
                let v4_input =
                    build_v4_swap_input(&ACTIONS_EXACT_IN, vec![p_swap, p_settle, p_take]);
                let mut planner = RoutePlanner::new();
                planner.add(V4_SWAP, v4_input);
                let value = if r.native_in {
                    amount_in.raw
                } else {
                    U256::ZERO
                };
                let approval = if r.native_in {
                    None
                } else {
                    Some(common::erc20_approval(
                        ctx.permit2()?,
                        r.input,
                        amount_in.raw,
                    ))
                };
                (planner.encode(deadline_secs), value, approval)
            }
        };

        Ok(PreparedSwap {
            tx: UnsignedTx {
                chain: ctx.chain,
                to: ctx.router_universal()?,
                data,
                value,
            },
            min_received: min,
            max_spent: None,
            approval,
            price_impact: None,
        })
    }

    /// Build an exact-out V4 swap, supporting ERC-20, native-ETH (address(0) pool),
    /// ETH-to-token via WETH wrap, and token-to-native ETH via WETH unwrap.
    ///
    /// ## Address(0)-native path
    ///
    /// When the pool has `currency0 == address(0)`, native ETH is first-class.
    /// `tx.value = maxAmountIn`; no Permit2 approval; commands = `[V4_SWAP]` with
    /// `SETTLE_ALL`.
    ///
    /// ## WETH-wrap path (native-in, WETH pool)
    ///
    /// When the pool's currencies include WETH (not address(0)) and the caller
    /// holds native ETH: commands = `[WRAP_ETH, V4_SWAP, UNWRAP_WETH]`. `WRAP_ETH`
    /// deposits `maxAmountIn` WETH to the router; the V4 `SETTLE` action pays the
    /// actual swap amount; `UNWRAP_WETH` returns leftover WETH to the recipient as
    /// ETH. `tx.value = maxAmountIn`; `approval = None`.
    ///
    /// ## WETH-unwrap path (native-out, WETH pool)
    ///
    /// When the output is native ETH and the pool exposes WETH: commands =
    /// `[V4_SWAP, UNWRAP_WETH]`. The V4 `TAKE` action delivers WETH to the
    /// router (`ADDRESS_THIS`); `UNWRAP_WETH` converts it to ETH and pays the
    /// recipient, enforcing `amount_out` as the minimum (exact-out guarantees the
    /// pool delivers exactly that). `tx.value = 0`; Permit2 approval on the token
    /// input.
    ///
    /// ## ERC-20 path
    ///
    /// Token input: commands = `[V4_SWAP]`, `SETTLE_ALL`, Permit2 approval required.
    ///
    /// A `price_limit` returns [`BuildError::UnsupportedProtocol`].
    fn build_swap_exact_out(
        &self,
        ctx: &ChainConfig,
        amount_out: CurrencyAmount,
        from: Currency,
        route: &Route,
        quoted_in: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        // Must call build_pool_key before resolve so we can compute needs_wrap.
        let (pool_key, c0, _c1) = build_pool_key(self)?;
        let weth = common::evm_addr(&ctx.weth);

        let needs_wrap = is_wrap_case(from, amount_out.currency, weth, c0, _c1);

        // For the wrap case, resolve Native → ctx.weth so pool-membership checks
        // match the pool's WETH currency.
        let native_asset = if needs_wrap {
            ctx.weth
        } else {
            amm_core::primitives::asset::AssetId::new(ctx.chain, alloy::primitives::B256::ZERO)
        };

        let r = common::resolve_swap_with(
            ctx,
            self,
            from,
            amount_out.currency,
            route,
            opts,
            TradeType::ExactOut,
            native_asset,
        )?;

        // No sqrtPriceLimitX96 on V4's Router actions (see build_swap);
        // amountInMaximum bounds the exact-out swap instead.
        if opts.price_limit.is_some() {
            return Err(BuildError::UnsupportedProtocol);
        }

        let zero_for_one = common::evm_addr(&r.input) == c0;

        // Swap struct amounts are uint128.
        let out_u128 = u128::try_from(amount_out.raw).map_err(|_| BuildError::Overflow)?;
        let max = opts.slippage.max_amount_in(quoted_in);
        let max_u128 = u128::try_from(max.raw).map_err(|_| BuildError::Overflow)?;

        let swap_params = ExactOutputSingleParams {
            poolKey: pool_key,
            zeroForOne: zero_for_one,
            amountOut: out_u128,
            amountInMaximum: max_u128,
            hookData: Bytes::new(),
        };
        let p_swap: Bytes = swap_params.abi_encode().into();

        // TAKE: deliver the full output delta to the recipient.
        let p_take: Bytes = <(Address, Address, U256)>::abi_encode_params(&(
            common::evm_addr(&r.output),
            r.recipient,
            OPEN_DELTA,
        ))
        .into();

        let deadline_secs = u64::try_from(r.deadline).map_err(|_| BuildError::Overflow)?;

        let (data, value, approval) = match (needs_wrap, r.native_in) {
            // WETH-wrap exact-out: native ETH in → token out via WETH pool.
            // Prepend WRAP_ETH (max); V4 SETTLE drains only what the swap needs;
            // UNWRAP_WETH returns leftover WETH to the recipient as ETH.
            (true, true) => {
                // WRAP_ETH input: (recipient=ADDRESS_THIS, amount=max).
                let wrap_input: Bytes =
                    <(Address, U256)>::abi_encode_params(&(ADDRESS_THIS, max.raw)).into();
                // SETTLE: currency=WETH, amount=OPEN_DELTA, payerIsUser=false.
                let p_settle: Bytes =
                    <(Address, U256, bool)>::abi_encode_params(&(weth, OPEN_DELTA, false)).into();
                let v4_input =
                    build_v4_swap_input(&ACTIONS_WRAP_EXACT_OUT, vec![p_swap, p_settle, p_take]);
                // UNWRAP_WETH: return all remaining router WETH to the recipient as ETH.
                // min=0 means unwrap everything; any leftover from max-actual is refunded.
                let unwrap_input: Bytes =
                    <(Address, U256)>::abi_encode_params(&(r.recipient, U256::ZERO)).into();
                let mut planner = RoutePlanner::new();
                planner
                    .add(WRAP_ETH, wrap_input)
                    .add(V4_SWAP, v4_input)
                    .add(UNWRAP_WETH, unwrap_input);
                // tx.value = max (the ETH the router wraps); no Permit2 approval.
                (planner.encode(deadline_secs), max.raw, None)
            }
            // WETH-wrap exact-out: token in → native ETH out via WETH pool.
            // The pool outputs exactly `amount_out` WETH to the router (TAKE to
            // ADDRESS_THIS); UNWRAP_WETH converts it to ETH and enforces the
            // exact output as the minimum, then sends to the recipient.
            (true, false) => {
                // SETTLE_ALL: pull the ERC-20 input from the user via Permit2.
                let p_settle: Bytes =
                    <(Address, U256)>::abi_encode_params(&(common::evm_addr(&r.input), max.raw))
                        .into();
                // TAKE to ADDRESS_THIS: router holds the WETH output for unwrapping.
                let p_take_router: Bytes = <(Address, Address, U256)>::abi_encode_params(&(
                    common::evm_addr(&r.output),
                    ADDRESS_THIS,
                    OPEN_DELTA,
                ))
                .into();
                let v4_input =
                    build_v4_swap_input(&ACTIONS_EXACT_OUT, vec![p_swap, p_settle, p_take_router]);
                // UNWRAP_WETH: enforce exact output as the minimum; any excess is
                // not possible in exact-out (the pool delivers exactly amount_out).
                let unwrap_input: Bytes =
                    <(Address, U256)>::abi_encode_params(&(r.recipient, amount_out.raw)).into();
                let mut planner = RoutePlanner::new();
                planner
                    .add(V4_SWAP, v4_input)
                    .add(UNWRAP_WETH, unwrap_input);
                // Token input: tx.value=0; Permit2 approval on the input token.
                let approval = Some(common::erc20_approval(ctx.permit2()?, r.input, max.raw));
                (planner.encode(deadline_secs), U256::ZERO, approval)
            }
            // Standard path: SETTLE_ALL from user.
            _ => {
                // SETTLE_ALL: settle input currency up to max (U256).
                let p_settle: Bytes =
                    <(Address, U256)>::abi_encode_params(&(common::evm_addr(&r.input), max.raw))
                        .into();
                let v4_input =
                    build_v4_swap_input(&ACTIONS_EXACT_OUT, vec![p_swap, p_settle, p_take]);
                let mut planner = RoutePlanner::new();
                planner.add(V4_SWAP, v4_input);
                let value = if r.native_in { max.raw } else { U256::ZERO };
                let approval = if r.native_in {
                    None
                } else {
                    Some(common::erc20_approval(ctx.permit2()?, r.input, max.raw))
                };
                (planner.encode(deadline_secs), value, approval)
            }
        };

        Ok(PreparedSwap {
            tx: UnsignedTx {
                chain: ctx.chain,
                to: ctx.router_universal()?,
                data,
                value,
            },
            min_received: AssetAmount::new(r.output, amount_out.raw),
            max_spent: Some(AssetAmount::new(r.input, max.raw)),
            approval,
            price_impact: None,
        })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, U256};
    use alloy::sol_types::SolValue;
    use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::primitives::ratio::Bps;
    use amm_core::protocols::uniswap::v4::{Hooks, TickData, TickInfo, UniswapV4Pool};
    use amm_core::slippage::Slippage;

    use super::{
        ACTIONS_EXACT_IN, ACTIONS_EXACT_OUT, ExactInputSingleParams, ExactOutputSingleParams,
    };
    use crate::execution::{
        config::{ChainConfig, Routers},
        error::BuildError,
        executable::Executable,
        options::{Deadline, ExecutionOptions, Recipient},
        prepared::Route,
        route_planner::IUniversalRouter,
        types::{Currency, CurrencyAmount, TradeType},
    };

    // ── Test fixtures ────────────────────────────────────────────────────────

    fn chain_id() -> ChainId {
        ChainId(1)
    }

    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    /// Native ETH — address(0) as a first-class V4 currency.
    fn eth() -> AssetId {
        AssetId::new(chain_id(), B256::ZERO)
    }

    /// USDC — token0 (smaller byte value → smaller numeric address).
    fn usdc() -> AssetId {
        asset(0x01)
    }

    /// WETH — token1 (used in ERC-20 pool; NOT address(0)).
    fn weth() -> AssetId {
        asset(0x02)
    }

    /// A third ERC-20 token with no WETH or native relation.
    fn token_x() -> AssetId {
        asset(0x10)
    }

    fn universal_router() -> Address {
        Address::repeat_byte(0xAA)
    }

    fn permit2() -> Address {
        Address::repeat_byte(0xBB)
    }

    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth()).with_routers(Routers {
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

    /// A full-range USDC/WETH V4 pool (ERC-20 only), 1e18 liquidity, 0.30% fee.
    /// currency0 = USDC, currency1 = WETH (no address(0) currency).
    fn pool() -> UniswapV4Pool {
        let liq: i128 = 1_000_000_000_000_000_000;
        UniswapV4Pool::new(
            PoolId::new("1:univ4:0xtest"),
            B256::repeat_byte(0xCC),
            [usdc(), weth()],
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

    /// A full-range ETH/USDC native V4 pool, 1e18 liquidity, 0.05% fee.
    /// currency0 = address(0) (native ETH), currency1 = USDC.
    fn native_pool() -> UniswapV4Pool {
        let liq: i128 = 1_000_000_000_000_000_000;
        UniswapV4Pool::new(
            PoolId::new("1:univ4:0xnative"),
            B256::repeat_byte(0xDD),
            [eth(), usdc()],
            U256::from(SQRT_1_1),
            liq as u128,
            0,
            500,
            500,
            full_range_ticks(liq),
            Hooks::None,
            500,
            10,
            Address::ZERO,
        )
    }

    /// A full-range WETH/TOKEN_X V4 pool with WETH as currency0.
    /// currency0 = WETH (0x02), currency1 = TOKEN_X (0x10).
    /// No address(0) — wrap case applies when caller provides native ETH.
    fn weth_pool() -> UniswapV4Pool {
        let liq: i128 = 1_000_000_000_000_000_000;
        UniswapV4Pool::new(
            PoolId::new("1:univ4:0xwethpool"),
            B256::repeat_byte(0xEE),
            [weth(), token_x()],
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

    fn opts(to: Address, deadline_ts: u64, slippage_bps: u16) -> ExecutionOptions {
        ExecutionOptions::new(Slippage::from_bps(Bps(slippage_bps)))
            .with_recipient(Recipient::To(to))
            .with_deadline(Deadline::AtTimestamp(deadline_ts))
    }

    // ── Decode helpers ───────────────────────────────────────────────────────

    /// Decode the outer `execute(bytes, bytes[], uint256)` calldata.
    fn decode_outer(
        data: &[u8],
    ) -> (
        alloy::primitives::Bytes,
        Vec<alloy::primitives::Bytes>,
        U256,
    ) {
        use alloy::sol_types::SolCall;
        let decoded = IUniversalRouter::executeCall::abi_decode(data)
            .expect("outer must decode as IUniversalRouter::execute");
        (decoded.commands, decoded.inputs, decoded.deadline)
    }

    /// Decode the V4_SWAP input `(bytes actions, bytes[] params)`.
    fn decode_v4_input(
        v4_bytes: &alloy::primitives::Bytes,
    ) -> (alloy::primitives::Bytes, Vec<alloy::primitives::Bytes>) {
        <(alloy::primitives::Bytes, Vec<alloy::primitives::Bytes>)>::abi_decode_params(v4_bytes)
            .expect("V4_SWAP input must decode as (Bytes, Bytes[])")
    }

    // ── Exact-in ERC-20 round-trip ───────────────────────────────────────────

    #[test]
    fn exact_in_erc20_encodes_correct_structure() {
        let p = pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x55);
        let deadline_ts = 1_700_000_000u64;
        // 100 bps slippage: quoted_out 1000 → min = floor(1000 * 9900/10000) = 990
        let opts = opts(recipient, deadline_ts, 100);

        let amount_in = CurrencyAmount {
            currency: Currency::Token(usdc()),
            raw: U256::from(500u64),
        };
        let quoted_out = AssetAmount::new(weth(), U256::from(1000u64));
        let route = Route::new_single_hop(usdc(), weth(), TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(weth()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("exact-in ERC-20 must succeed");

        // tx fields.
        assert_eq!(
            prepared.tx.to,
            universal_router(),
            "tx.to must be the Universal Router"
        );
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for ERC-20"
        );
        assert_eq!(prepared.tx.chain, c.chain);

        // Outer: commands == [0x10 (V4_SWAP)], one input, deadline round-trips.
        let (commands, inputs, dl) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[0x10u8],
            "commands must be [V4_SWAP=0x10]"
        );
        assert_eq!(inputs.len(), 1, "must have exactly one V4_SWAP input");
        assert_eq!(dl, U256::from(deadline_ts), "deadline must round-trip");

        // Inner V4_SWAP input: actions + 3 params.
        let (actions, params) = decode_v4_input(&inputs[0]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_EXACT_IN[..],
            "actions must be [0x06,0x0c,0x0e]"
        );
        assert_eq!(params.len(), 3, "must have 3 params");

        // params[0]: ExactInputSingleParams.
        let swap = ExactInputSingleParams::abi_decode(&params[0])
            .expect("params[0] must decode as ExactInputSingleParams");
        assert_eq!(
            swap.poolKey.currency0,
            crate::execution::protocols::common::evm_addr(&usdc()),
            "currency0 must be usdc"
        );
        assert_eq!(
            swap.poolKey.currency1,
            crate::execution::protocols::common::evm_addr(&weth()),
            "currency1 must be weth"
        );
        assert_eq!(swap.poolKey.fee.to::<u32>(), 3000u32, "fee must be 3000");
        assert_eq!(
            swap.poolKey.tickSpacing.as_i32(),
            60,
            "tickSpacing must be 60"
        );
        assert_eq!(swap.poolKey.hooks, Address::ZERO, "hooks must be zero");
        assert!(
            swap.zeroForOne,
            "zeroForOne must be true (usdc=c0, input=usdc)"
        );
        assert_eq!(swap.amountIn, 500u128, "amountIn must be 500");
        assert_eq!(
            swap.amountOutMinimum, 990u128,
            "amountOutMinimum must be 990 at 100bps"
        );
        assert!(swap.hookData.is_empty(), "hookData must be empty");

        // params[1]: SETTLE_ALL (input, amountIn as U256).
        let (settle_addr, settle_amt) = <(Address, U256)>::abi_decode_params(&params[1])
            .expect("params[1] must decode as (Address, U256)");
        assert_eq!(
            settle_addr,
            crate::execution::protocols::common::evm_addr(&usdc()),
            "settle address must be usdc"
        );
        assert_eq!(
            settle_amt,
            U256::from(500u64),
            "settle amount must be amountIn"
        );

        // params[2]: TAKE (output, recipient, OPEN_DELTA as U256).
        let (take_addr, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2])
                .expect("params[2] must decode as (Address, Address, U256)");
        assert_eq!(
            take_addr,
            crate::execution::protocols::common::evm_addr(&weth()),
            "take currency must be weth"
        );
        assert_eq!(
            take_to, recipient,
            "take recipient must match opts recipient"
        );
        assert_eq!(take_amt, U256::ZERO, "take amount must be OPEN_DELTA");

        // min_received == 990 of weth.
        assert_eq!(prepared.min_received.raw, U256::from(990u64));
        assert_eq!(prepared.min_received.asset, weth());
        assert!(
            prepared.max_spent.is_none(),
            "max_spent must be None for exact-in"
        );

        // approval: spender = Permit2, token = usdc, min_allowance = amountIn.
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender,
            permit2(),
            "approval.spender must be Permit2"
        );
        assert_eq!(approval.token, usdc(), "approval.token must be usdc");
        assert_eq!(
            approval.min_allowance,
            U256::from(500u64),
            "approval.min_allowance must be amountIn"
        );
        assert!(!approval.reset_first);
    }

    // ── Exact-in reverse direction (weth → usdc) ────────────────────────────

    #[test]
    fn exact_in_erc20_reverse_direction_sets_zero_for_one_false() {
        let p = pool();
        let c = ctx();
        let opts = opts(Address::repeat_byte(0x55), 1_700_000_000, 100);

        let amount_in = CurrencyAmount {
            currency: Currency::Token(weth()),
            raw: U256::from(500u64),
        };
        let quoted_out = AssetAmount::new(usdc(), U256::from(1000u64));
        let route = Route::new_single_hop(weth(), usdc(), TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(usdc()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("reverse direction must succeed");

        let (_, inputs, _) = decode_outer(&prepared.tx.data);
        let (_, params) = decode_v4_input(&inputs[0]);
        let swap = ExactInputSingleParams::abi_decode(&params[0]).unwrap();
        assert!(
            !swap.zeroForOne,
            "zeroForOne must be false for weth→usdc (weth=c1, input=weth)"
        );
    }

    // ── Exact-out ERC-20 round-trip ──────────────────────────────────────────

    #[test]
    fn exact_out_erc20_encodes_correct_structure() {
        let p = pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x66);
        let deadline_ts = 1_800_000_000u64;
        // 100 bps slippage: quoted_in 500 → max = ceil(500 * 10100/10000) = 505
        let opts = opts(recipient, deadline_ts, 100);

        let out_raw = U256::from(400u64);
        let amount_out = CurrencyAmount {
            currency: Currency::Token(weth()),
            raw: out_raw,
        };
        let quoted_in = AssetAmount::new(usdc(), U256::from(500u64));
        let route = Route::new_single_hop(usdc(), weth(), TradeType::ExactOut);

        let prepared = p
            .build_swap_exact_out(
                &c,
                amount_out,
                Currency::Token(usdc()),
                &route,
                &quoted_in,
                &opts,
            )
            .expect("exact-out ERC-20 must succeed");

        // tx fields.
        assert_eq!(prepared.tx.to, universal_router());
        assert_eq!(prepared.tx.value, U256::ZERO);

        // Outer.
        let (commands, inputs, dl) = decode_outer(&prepared.tx.data);
        assert_eq!(commands.as_ref(), &[0x10u8]);
        assert_eq!(inputs.len(), 1);
        assert_eq!(dl, U256::from(deadline_ts));

        // V4 input.
        let (actions, params) = decode_v4_input(&inputs[0]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_EXACT_OUT[..],
            "actions must be [0x08,0x0c,0x0e]"
        );
        assert_eq!(params.len(), 3);

        // params[0]: ExactOutputSingleParams.
        let swap = ExactOutputSingleParams::abi_decode(&params[0])
            .expect("params[0] must decode as ExactOutputSingleParams");
        assert_eq!(swap.poolKey.fee.to::<u32>(), 3000u32);
        assert_eq!(swap.poolKey.tickSpacing.as_i32(), 60);
        assert_eq!(swap.poolKey.hooks, Address::ZERO);
        assert!(swap.zeroForOne, "zeroForOne true: usdc=c0 is input");
        assert_eq!(swap.amountOut, 400u128, "amountOut must be 400");
        // max_amount_in == ceil(500 * 10100/10000) = 505
        assert_eq!(
            swap.amountInMaximum, 505u128,
            "amountInMaximum must be 505 at 100bps"
        );
        assert!(swap.hookData.is_empty());

        // params[1]: SETTLE_ALL (input, max as U256).
        let (settle_addr, settle_amt) = <(Address, U256)>::abi_decode_params(&params[1]).unwrap();
        assert_eq!(
            settle_addr,
            crate::execution::protocols::common::evm_addr(&usdc())
        );
        assert_eq!(
            settle_amt,
            U256::from(505u64),
            "settle amount must be maxAmountIn=505"
        );

        // params[2]: TAKE (output, recipient, OPEN_DELTA as U256).
        let (take_addr, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2]).unwrap();
        assert_eq!(
            take_addr,
            crate::execution::protocols::common::evm_addr(&weth())
        );
        assert_eq!(
            take_to, recipient,
            "take recipient must match opts recipient"
        );
        assert_eq!(take_amt, U256::ZERO, "take amount must be OPEN_DELTA");

        // min_received == exact output in weth.
        assert_eq!(prepared.min_received.raw, out_raw);
        assert_eq!(prepared.min_received.asset, weth());

        // max_spent == Some(usdc, 505).
        let max_spent = prepared
            .max_spent
            .expect("max_spent must be Some for exact-out");
        assert_eq!(max_spent.raw, U256::from(505u64));
        assert_eq!(max_spent.asset, usdc());

        // approval: spender = Permit2, token = usdc, min_allowance = max.
        let approval = prepared.approval.expect("approval must be Some");
        assert_eq!(approval.spender, permit2(), "spender must be Permit2");
        assert_eq!(approval.token, usdc());
        assert_eq!(
            approval.min_allowance,
            U256::from(505u64),
            "min_allowance must be maxAmountIn"
        );
        assert!(!approval.reset_first);
    }

    // ── Native-in: ETH → USDC on a native V4 pool ───────────────────────────

    #[test]
    fn native_in_eth_to_token_sets_value_and_no_approval() {
        let p = native_pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x55);
        // 50 bps slippage: quoted_out 1000 → min = floor(1000 * 9950/10000) = 995
        let opts = opts(recipient, 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(500u64),
        };
        let quoted_out = AssetAmount::new(usdc(), U256::from(1000u64));
        // Route from eth (address(0)) to usdc.
        let route = Route::new_single_hop(eth(), usdc(), TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(usdc()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("native-in on native pool must succeed");

        // tx.value must equal amountIn; no Permit2 approval.
        assert_eq!(
            prepared.tx.value,
            U256::from(500u64),
            "tx.value must equal amountIn for native-in"
        );
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in"
        );

        // Decode and check pool key + actions.
        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        assert_eq!(commands.as_ref(), &[0x10u8], "commands must be [V4_SWAP]");
        let (actions, params) = decode_v4_input(&inputs[0]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_EXACT_IN[..],
            "actions must be exact-in sequence"
        );

        // params[0]: poolKey.currency0 == address(0), zeroForOne == true.
        let swap = ExactInputSingleParams::abi_decode(&params[0])
            .expect("params[0] must decode as ExactInputSingleParams");
        assert_eq!(
            swap.poolKey.currency0,
            Address::ZERO,
            "currency0 must be address(0) on native pool"
        );
        assert_eq!(
            swap.poolKey.currency1,
            crate::execution::protocols::common::evm_addr(&usdc()),
            "currency1 must be usdc"
        );
        assert!(
            swap.zeroForOne,
            "zeroForOne must be true: ETH=currency0 is the input"
        );
        assert_eq!(swap.amountIn, 500u128, "amountIn must be 500");

        // params[1]: SETTLE_ALL currency == address(0).
        let (settle_addr, settle_amt) = <(Address, U256)>::abi_decode_params(&params[1])
            .expect("params[1] must decode as (Address, U256)");
        assert_eq!(
            settle_addr,
            Address::ZERO,
            "SETTLE_ALL currency must be address(0)"
        );
        assert_eq!(
            settle_amt,
            U256::from(500u64),
            "SETTLE_ALL amount must be amountIn"
        );

        // params[2]: TAKE currency == usdc, recipient == opts recipient, amount == OPEN_DELTA.
        let (take_addr, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2])
                .expect("params[2] must decode as (Address, Address, U256)");
        assert_eq!(
            take_addr,
            crate::execution::protocols::common::evm_addr(&usdc()),
            "TAKE currency must be usdc"
        );
        assert_eq!(
            take_to, recipient,
            "TAKE recipient must match opts recipient"
        );
        assert_eq!(take_amt, U256::ZERO, "TAKE amount must be OPEN_DELTA");
    }

    // ── Native-out: USDC → ETH on a native V4 pool ──────────────────────────

    #[test]
    fn native_out_token_to_eth_uses_permit2_and_zero_value() {
        let p = native_pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x55);
        // 50 bps slippage: quoted_out 1000 → min = floor(1000 * 9950/10000) = 995
        let opts = opts(recipient, 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Token(usdc()),
            raw: U256::from(500u64),
        };
        let quoted_out = AssetAmount::new(eth(), U256::from(1000u64));
        // Route from usdc to eth (address(0)).
        let route = Route::new_single_hop(usdc(), eth(), TradeType::ExactIn);

        let prepared = p
            .build_swap(&c, amount_in, Currency::Native, &route, &quoted_out, &opts)
            .expect("native-out on native pool must succeed");

        // tx.value must be zero; Permit2 approval required for token input.
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for native-out"
        );
        let approval = prepared
            .approval
            .expect("approval must be Some for token input");
        assert_eq!(
            approval.spender,
            permit2(),
            "approval.spender must be Permit2"
        );
        assert_eq!(approval.token, usdc(), "approval.token must be usdc");
        assert_eq!(
            approval.min_allowance,
            U256::from(500u64),
            "approval.min_allowance must be amountIn"
        );

        // Decode and check: zeroForOne == false (USDC=currency1? No — eth=c0, usdc=c1).
        // input=usdc=c1, so zeroForOne == false.
        let (_, inputs, _) = decode_outer(&prepared.tx.data);
        let (actions, params) = decode_v4_input(&inputs[0]);
        assert_eq!(actions.as_ref(), &ACTIONS_EXACT_IN[..]);

        let swap = ExactInputSingleParams::abi_decode(&params[0]).unwrap();
        assert!(
            !swap.zeroForOne,
            "zeroForOne must be false: USDC=currency1 is the input"
        );

        // SETTLE_ALL currency == usdc.
        let (settle_addr, _) = <(Address, U256)>::abi_decode_params(&params[1]).unwrap();
        assert_eq!(
            settle_addr,
            crate::execution::protocols::common::evm_addr(&usdc()),
            "SETTLE_ALL currency must be usdc"
        );

        // TAKE currency == address(0), recipient == opts recipient, amount == OPEN_DELTA.
        let (take_addr, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2]).unwrap();
        assert_eq!(take_addr, Address::ZERO, "TAKE currency must be address(0)");
        assert_eq!(
            take_to, recipient,
            "TAKE recipient must match opts recipient"
        );
        assert_eq!(take_amt, U256::ZERO, "TAKE amount must be OPEN_DELTA");
    }

    // ── Guard: both-native → NativeMismatch ─────────────────────────────────

    #[test]
    fn both_native_returns_native_mismatch() {
        let p = native_pool();
        let c = ctx();
        let opts = opts(Address::repeat_byte(0x55), 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(eth(), U256::from(100u64));
        let route = Route::new_single_hop(eth(), usdc(), TradeType::ExactIn);

        let err = p
            .build_swap(&c, amount_in, Currency::Native, &route, &quoted_out, &opts)
            .expect_err("both-native must fail");
        assert_eq!(
            err,
            BuildError::NativeMismatch,
            "both-native must yield NativeMismatch"
        );
    }

    // ── Guard: native input on a non-WETH, non-native V4 pool → AssetNotInPool

    #[test]
    fn native_currency_on_non_native_non_weth_pool_returns_asset_not_in_pool() {
        // A pool with currencies that are neither address(0) nor WETH:
        // native input cannot be wrapped to match either currency.
        let liq: i128 = 1_000_000_000_000_000_000;
        let p = UniswapV4Pool::new(
            PoolId::new("1:univ4:0xother"),
            B256::repeat_byte(0xFF),
            [usdc(), token_x()],
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
        );
        let c = ctx();
        let opts = opts(Address::repeat_byte(0x55), 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(usdc(), U256::from(100u64));
        // needs_wrap=false (no weth in pool, no address(0)) → native_asset=address(0)
        // → resolve maps Native→address(0) → not in pool → AssetNotInPool
        let route = Route::new_single_hop(usdc(), token_x(), TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(usdc()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("native on non-native/non-weth pool must fail");
        assert!(
            matches!(err, BuildError::AssetNotInPool { .. }),
            "expected AssetNotInPool, got {err:?}"
        );
    }

    // ── Guard: price_limit → UnsupportedProtocol ────────────────────────────

    #[test]
    fn price_limit_exact_in_returns_unsupported_protocol() {
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let p = pool();
        let c = ctx();
        let ratio = Ratio::new(U256::from(2u64), U256::from(1u64)).unwrap();
        let price_limit = Price::new(usdc(), weth(), ratio).unwrap();
        let opts =
            opts(Address::repeat_byte(0x55), 9_999_999, 50).with_price_limit(Some(price_limit));

        let amount_in = CurrencyAmount {
            currency: Currency::Token(usdc()),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(weth(), U256::from(100u64));
        let route = Route::new_single_hop(usdc(), weth(), TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(weth()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("price_limit must return UnsupportedProtocol");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ── Guard: Recipient::Sender → UnresolvedRecipient ──────────────────────

    #[test]
    fn sender_recipient_returns_unresolved_recipient() {
        let p = pool();
        let c = ctx();
        let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
            .with_deadline(Deadline::AtTimestamp(9_999_999));

        let amount_in = CurrencyAmount {
            currency: Currency::Token(usdc()),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(weth(), U256::from(100u64));
        let route = Route::new_single_hop(usdc(), weth(), TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(weth()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("Sender recipient must fail");
        assert_eq!(err, BuildError::UnresolvedRecipient);
    }

    // ── Guard: wrong trade type → UnsupportedProtocol ───────────────────────

    #[test]
    fn exact_out_route_in_build_swap_returns_unsupported_protocol() {
        let p = pool();
        let c = ctx();
        let opts = opts(Address::repeat_byte(0x55), 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Token(usdc()),
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(weth(), U256::from(100u64));
        // Wrong trade type for build_swap.
        let route = Route::new_single_hop(usdc(), weth(), TradeType::ExactOut);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(weth()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("ExactOut route in build_swap must fail");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    #[test]
    fn exact_in_route_in_build_swap_exact_out_returns_unsupported_protocol() {
        let p = pool();
        let c = ctx();
        let opts = opts(Address::repeat_byte(0x55), 9_999_999, 50);

        let amount_out = CurrencyAmount {
            currency: Currency::Token(weth()),
            raw: U256::from(100u64),
        };
        let quoted_in = AssetAmount::new(usdc(), U256::from(100u64));
        // Wrong trade type for build_swap_exact_out.
        let route = Route::new_single_hop(usdc(), weth(), TradeType::ExactIn);

        let err = p
            .build_swap_exact_out(
                &c,
                amount_out,
                Currency::Token(usdc()),
                &route,
                &quoted_in,
                &opts,
            )
            .expect_err("ExactIn route in build_swap_exact_out must fail");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ── TAKE recipient delivery ──────────────────────────────────────────────

    #[test]
    fn take_delivers_to_the_resolved_recipient() {
        // Build an ERC-20 exact-in swap on a V4 pool with a recipient that is NOT
        // the sender; decode the V4_SWAP input and assert the final action is TAKE
        // with (outputCurrency, recipient, OPEN_DELTA).
        use super::{ACTION_TAKE, OPEN_DELTA};

        let recipient = Address::repeat_byte(0xCD);
        let o = opts(recipient, 1_700_000_000, 100);
        let p = pool();
        let c = ctx();

        let amount_in = CurrencyAmount {
            currency: Currency::Token(usdc()),
            raw: U256::from(500u64),
        };
        let quoted_out = AssetAmount::new(weth(), U256::from(1000u64));
        let route = Route::new_single_hop(usdc(), weth(), TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(weth()),
                &route,
                &quoted_out,
                &o,
            )
            .expect("exact-in with explicit recipient must succeed");

        let (_, inputs, _) = decode_outer(&prepared.tx.data);
        let (actions, params) = decode_v4_input(&inputs[0]);

        assert_eq!(
            actions.last(),
            Some(&ACTION_TAKE),
            "final action must be TAKE (0x0e)"
        );

        let (currency, to, amount) =
            <(Address, Address, U256)>::abi_decode_params(params.last().unwrap())
                .expect("TAKE params must decode as (Address, Address, U256)");
        assert_eq!(
            currency,
            crate::execution::protocols::common::evm_addr(&weth()),
            "TAKE currency must be the output asset"
        );
        assert_eq!(
            to, recipient,
            "TAKE recipient must be the resolved recipient"
        );
        assert_eq!(amount, OPEN_DELTA, "TAKE amount must be OPEN_DELTA (0)");
    }

    // ── Action constant values ───────────────────────────────────────────────

    #[test]
    fn action_constants_have_expected_values() {
        use super::{
            ACTION_SETTLE, ACTION_SETTLE_ALL, ACTION_SWAP_EXACT_IN_SINGLE,
            ACTION_SWAP_EXACT_OUT_SINGLE, ACTION_TAKE,
        };
        assert_eq!(ACTION_SWAP_EXACT_IN_SINGLE, 0x06);
        assert_eq!(ACTION_SWAP_EXACT_OUT_SINGLE, 0x08);
        assert_eq!(ACTION_SETTLE, 0x0b);
        assert_eq!(ACTION_SETTLE_ALL, 0x0c);
        assert_eq!(ACTION_TAKE, 0x0e);
        assert_eq!(ACTIONS_EXACT_IN, [0x06u8, 0x0c, 0x0e]);
        assert_eq!(ACTIONS_EXACT_OUT, [0x08u8, 0x0c, 0x0e]);
    }

    // ── WETH-wrap exact-in: ETH → TOKEN_X on a WETH-currency pool ───────────

    /// ETH→token exact-in on a WETH-currency pool: the encoder must prepend
    /// `WRAP_ETH` and use `SETTLE` (payerIsUser=false) instead of `SETTLE_ALL`.
    #[test]
    fn native_in_wrap_exact_in_prepends_wrap_and_settles_from_router() {
        use super::{ACTION_SETTLE, ACTIONS_WRAP_EXACT_IN, ADDRESS_THIS};
        use crate::execution::route_planner::{V4_SWAP, WRAP_ETH};

        let p = weth_pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x77);
        let amount_raw = U256::from(1_000u64);
        // 50 bps slippage.
        let opts = opts(recipient, 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: amount_raw,
        };
        let quoted_out = AssetAmount::new(token_x(), U256::from(2_000u64));
        // Route: weth → token_x (after wrap, input resolves to weth).
        let route = Route::new_single_hop(weth(), token_x(), TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(token_x()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("WETH-wrap exact-in must succeed");

        // tx.value == amountIn; no Permit2 approval.
        assert_eq!(
            prepared.tx.value, amount_raw,
            "tx.value must equal amountIn for wrap path"
        );
        assert!(
            prepared.approval.is_none(),
            "approval must be None for wrap path (ETH sent as value)"
        );

        // Outer: commands == [WRAP_ETH, V4_SWAP].
        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[WRAP_ETH, V4_SWAP],
            "commands must be [WRAP_ETH=0x0b, V4_SWAP=0x10]"
        );
        assert_eq!(inputs.len(), 2, "must have two inputs: WRAP_ETH + V4_SWAP");

        // WRAP_ETH input: (ADDRESS_THIS, amountIn).
        let (wrap_recipient, wrap_amount) = <(Address, U256)>::abi_decode_params(&inputs[0])
            .expect("WRAP_ETH input must decode as (Address, U256)");
        assert_eq!(
            wrap_recipient, ADDRESS_THIS,
            "WRAP_ETH recipient must be ADDRESS_THIS (address(2))"
        );
        assert_eq!(
            wrap_amount, amount_raw,
            "WRAP_ETH amount must equal amountIn"
        );

        // V4_SWAP input: actions == [SWAP_EXACT_IN_SINGLE, SETTLE, TAKE].
        let (actions, params) = decode_v4_input(&inputs[1]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_WRAP_EXACT_IN[..],
            "V4 actions must be [0x06, SETTLE=0x0b, 0x0e]"
        );
        assert_eq!(params.len(), 3);

        // params[0]: ExactInputSingleParams — pool key, zeroForOne, amounts.
        let swap = ExactInputSingleParams::abi_decode(&params[0])
            .expect("params[0] must decode as ExactInputSingleParams");
        // weth_pool has currency0=WETH, currency1=TOKEN_X; input=weth → zeroForOne=true.
        assert!(
            swap.zeroForOne,
            "zeroForOne must be true (weth=c0, input=weth after wrap)"
        );
        assert_eq!(swap.amountIn, 1_000u128, "amountIn must be 1000");

        // params[1]: SETTLE — (weth, OPEN_DELTA, payerIsUser=false).
        let (settle_currency, settle_amount, payer_is_user) =
            <(Address, U256, bool)>::abi_decode_params(&params[1])
                .expect("SETTLE param must decode as (Address, U256, bool)");
        let weth_addr = crate::execution::protocols::common::evm_addr(&weth());
        assert_eq!(settle_currency, weth_addr, "SETTLE currency must be WETH");
        assert_eq!(
            settle_amount,
            U256::ZERO,
            "SETTLE amount must be OPEN_DELTA (0)"
        );
        assert!(
            !payer_is_user,
            "payerIsUser must be false (router pays from its WETH balance)"
        );

        // params[2]: TAKE — output goes to recipient.
        let (take_addr, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2])
                .expect("TAKE param must decode as (Address, Address, U256)");
        assert_eq!(
            take_addr,
            crate::execution::protocols::common::evm_addr(&token_x()),
            "TAKE currency must be token_x (output)"
        );
        assert_eq!(
            take_to, recipient,
            "TAKE recipient must match opts recipient"
        );
        assert_eq!(take_amt, U256::ZERO, "TAKE amount must be OPEN_DELTA");

        // Verify ACTION_SETTLE byte value.
        assert_eq!(
            actions[1], ACTION_SETTLE,
            "second action byte must be SETTLE=0x0b"
        );
    }

    // ── WETH-wrap exact-out: ETH → TOKEN_X on a WETH-currency pool ──────────

    /// ETH→token exact-out: commands must be `[WRAP_ETH, V4_SWAP, UNWRAP_WETH]`;
    /// `UNWRAP_WETH` returns any leftover WETH to the recipient as ETH.
    #[test]
    fn native_in_wrap_exact_out_appends_leftover_unwrap() {
        use super::{ACTIONS_WRAP_EXACT_OUT, ADDRESS_THIS};
        use crate::execution::route_planner::{UNWRAP_WETH, V4_SWAP, WRAP_ETH};

        let p = weth_pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x88);
        let out_raw = U256::from(500u64);
        // 100 bps slippage: quoted_in 1000 → max = ceil(1000 * 10100/10000) = 1010
        let opts = opts(recipient, 9_999_999, 100);

        let amount_out = CurrencyAmount {
            currency: Currency::Token(token_x()),
            raw: out_raw,
        };
        let quoted_in = AssetAmount::new(weth(), U256::from(1_000u64));
        // Route: weth → token_x (after wrap, input=weth).
        let route = Route::new_single_hop(weth(), token_x(), TradeType::ExactOut);

        let prepared = p
            .build_swap_exact_out(&c, amount_out, Currency::Native, &route, &quoted_in, &opts)
            .expect("WETH-wrap exact-out must succeed");

        // max = ceil(1000 * 10100/10000) = 1010
        let expected_max = U256::from(1_010u64);

        // tx.value == max; no Permit2 approval.
        assert_eq!(
            prepared.tx.value, expected_max,
            "tx.value must equal max for wrap exact-out"
        );
        assert!(
            prepared.approval.is_none(),
            "approval must be None for wrap exact-out"
        );

        // Outer: commands == [WRAP_ETH, V4_SWAP, UNWRAP_WETH].
        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[WRAP_ETH, V4_SWAP, UNWRAP_WETH],
            "commands must be [WRAP_ETH=0x0b, V4_SWAP=0x10, UNWRAP_WETH=0x0c]"
        );
        assert_eq!(inputs.len(), 3, "must have 3 inputs");

        // WRAP_ETH input: (ADDRESS_THIS, max).
        let (wrap_recipient, wrap_amount) = <(Address, U256)>::abi_decode_params(&inputs[0])
            .expect("WRAP_ETH input must decode as (Address, U256)");
        assert_eq!(
            wrap_recipient, ADDRESS_THIS,
            "WRAP_ETH recipient must be ADDRESS_THIS"
        );
        assert_eq!(
            wrap_amount, expected_max,
            "WRAP_ETH amount must be max (1010)"
        );

        // V4_SWAP actions == [SWAP_EXACT_OUT_SINGLE, SETTLE, TAKE].
        let (actions, params) = decode_v4_input(&inputs[1]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_WRAP_EXACT_OUT[..],
            "V4 actions must be [0x08, SETTLE=0x0b, 0x0e]"
        );
        assert_eq!(params.len(), 3);

        // params[1]: SETTLE — (weth, OPEN_DELTA, payerIsUser=false).
        let (settle_currency, settle_amount, payer_is_user) =
            <(Address, U256, bool)>::abi_decode_params(&params[1])
                .expect("SETTLE param must decode as (Address, U256, bool)");
        let weth_addr = crate::execution::protocols::common::evm_addr(&weth());
        assert_eq!(settle_currency, weth_addr, "SETTLE currency must be WETH");
        assert_eq!(
            settle_amount,
            U256::ZERO,
            "SETTLE amount must be OPEN_DELTA"
        );
        assert!(!payer_is_user, "payerIsUser must be false");

        // UNWRAP_WETH input: (recipient, 0) — unwrap all remaining WETH to recipient as ETH.
        let (unwrap_recipient, unwrap_min) = <(Address, U256)>::abi_decode_params(&inputs[2])
            .expect("UNWRAP_WETH input must decode as (Address, U256)");
        assert_eq!(
            unwrap_recipient, recipient,
            "UNWRAP_WETH recipient must match opts recipient"
        );
        assert_eq!(
            unwrap_min,
            U256::ZERO,
            "UNWRAP_WETH min must be 0 (unwrap everything)"
        );
    }

    // ── WETH-wrap native-out exact-in: TOKEN_X → ETH on WETH-currency pool ───

    /// Token→ETH exact-in on a WETH-currency pool: the encoder must use `SETTLE_ALL`
    /// for the token input from the user, `TAKE` to the router (ADDRESS_THIS) for the
    /// WETH output, and then append `UNWRAP_WETH` to convert the WETH to ETH and deliver
    /// it to the recipient.
    #[test]
    fn native_out_unwrap_exact_in_takes_to_router_then_unwraps() {
        use super::{ACTIONS_EXACT_IN, ADDRESS_THIS, OPEN_DELTA};
        use crate::execution::route_planner::{UNWRAP_WETH, V4_SWAP};

        let p = weth_pool(); // currency0=WETH, currency1=TOKEN_X
        let c = ctx();
        let recipient = Address::repeat_byte(0x99);
        let amount_raw = U256::from(800u64);
        // 50 bps slippage: quoted_out 2000 → min = floor(2000 * 9950/10000) = 1990
        let opts = opts(recipient, 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Token(token_x()),
            raw: amount_raw,
        };
        let quoted_out = AssetAmount::new(weth(), U256::from(2_000u64));
        // Route: token_x → weth (after resolve, output=weth; native_out=true).
        let route = Route::new_single_hop(token_x(), weth(), TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                // to == Native triggers needs_wrap on a WETH-currency pool
                Currency::Native,
                &route,
                &quoted_out,
                &opts,
            )
            .expect("native-out wrap exact-in must succeed");

        // tx.value == 0 (token input, no ETH sent); Permit2 approval on token_x.
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for native-out wrap (token input)"
        );
        let approval = prepared
            .approval
            .expect("approval must be Some for token input");
        assert_eq!(
            approval.spender,
            permit2(),
            "approval.spender must be Permit2"
        );
        assert_eq!(approval.token, token_x(), "approval.token must be token_x");
        assert_eq!(
            approval.min_allowance, amount_raw,
            "approval.min_allowance must be amountIn"
        );

        // Outer: commands == [V4_SWAP, UNWRAP_WETH].
        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[V4_SWAP, UNWRAP_WETH],
            "commands must be [V4_SWAP=0x10, UNWRAP_WETH=0x0c]"
        );
        assert_eq!(
            inputs.len(),
            2,
            "must have two inputs: V4_SWAP + UNWRAP_WETH"
        );

        // V4_SWAP input: actions == [SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE].
        let (actions, params) = decode_v4_input(&inputs[0]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_EXACT_IN[..],
            "V4 actions must be [SWAP_EXACT_IN_SINGLE=0x06, SETTLE_ALL=0x0c, TAKE=0x0e]"
        );
        assert_eq!(params.len(), 3);

        // params[0]: ExactInputSingleParams — token_x is input (c1), so zeroForOne=false.
        let swap = ExactInputSingleParams::abi_decode(&params[0])
            .expect("params[0] must decode as ExactInputSingleParams");
        assert!(
            !swap.zeroForOne,
            "zeroForOne must be false (token_x=c1 is the input)"
        );
        assert_eq!(swap.amountIn, 800u128, "amountIn must be 800");
        // min = floor(2000 * 9950/10000) = 1990
        assert_eq!(
            swap.amountOutMinimum, 1_990u128,
            "amountOutMinimum must be 1990 at 50bps"
        );

        // params[1]: SETTLE_ALL — (token_x, amountIn).
        let (settle_addr, settle_amt) = <(Address, U256)>::abi_decode_params(&params[1])
            .expect("params[1] must decode as (Address, U256)");
        assert_eq!(
            settle_addr,
            crate::execution::protocols::common::evm_addr(&token_x()),
            "SETTLE_ALL currency must be token_x (input)"
        );
        assert_eq!(settle_amt, amount_raw, "SETTLE_ALL amount must be amountIn");

        // params[2]: TAKE — (weth, ADDRESS_THIS, OPEN_DELTA).
        // WETH output goes to the router (not the recipient) so UNWRAP_WETH can
        // convert it to ETH.
        let (take_addr, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2])
                .expect("params[2] must decode as (Address, Address, U256)");
        let weth_addr = crate::execution::protocols::common::evm_addr(&weth());
        assert_eq!(
            take_addr, weth_addr,
            "TAKE currency must be WETH (the swap output)"
        );
        assert_eq!(
            take_to, ADDRESS_THIS,
            "TAKE recipient must be ADDRESS_THIS so router holds WETH"
        );
        assert_eq!(take_amt, OPEN_DELTA, "TAKE amount must be OPEN_DELTA (0)");

        // UNWRAP_WETH input: (recipient, min) where min == slippage floor (1990).
        let (unwrap_recipient, unwrap_min) = <(Address, U256)>::abi_decode_params(&inputs[1])
            .expect("UNWRAP_WETH input must decode as (Address, U256)");
        assert_eq!(
            unwrap_recipient, recipient,
            "UNWRAP_WETH recipient must be opts recipient"
        );
        assert_eq!(
            unwrap_min,
            U256::from(1_990u64),
            "UNWRAP_WETH min must be slippage floor (1990)"
        );
    }

    // ── WETH-wrap native-out exact-out: TOKEN_X → ETH on WETH-currency pool ──

    /// Token→ETH exact-out on a WETH-currency pool: commands == `[V4_SWAP, UNWRAP_WETH]`;
    /// `UNWRAP_WETH` uses `amount_out` (not 0) as the min, enforcing the exact output.
    #[test]
    fn native_out_unwrap_exact_out_unwraps_exact_output() {
        use super::{ACTIONS_EXACT_OUT, ADDRESS_THIS, OPEN_DELTA};
        use crate::execution::route_planner::{UNWRAP_WETH, V4_SWAP};

        let p = weth_pool(); // currency0=WETH, currency1=TOKEN_X
        let c = ctx();
        let recipient = Address::repeat_byte(0xAA);
        let out_raw = U256::from(500u64);
        // 100 bps slippage: quoted_in 1000 → max = ceil(1000 * 10100/10000) = 1010
        let opts = opts(recipient, 9_999_999, 100);

        let amount_out = CurrencyAmount {
            currency: Currency::Native,
            raw: out_raw,
        };
        let quoted_in = AssetAmount::new(token_x(), U256::from(1_000u64));
        // Route: token_x → weth (after resolve, output=weth; native_out=true).
        let route = Route::new_single_hop(token_x(), weth(), TradeType::ExactOut);

        let prepared = p
            .build_swap_exact_out(
                &c,
                amount_out,
                // from == Token(token_x) — ERC-20 input
                Currency::Token(token_x()),
                &route,
                &quoted_in,
                &opts,
            )
            .expect("native-out wrap exact-out must succeed");

        let expected_max = U256::from(1_010u64);

        // tx.value == 0; Permit2 approval on token_x for maxAmountIn.
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for native-out wrap exact-out"
        );
        let approval = prepared
            .approval
            .expect("approval must be Some for token input");
        assert_eq!(approval.spender, permit2(), "spender must be Permit2");
        assert_eq!(approval.token, token_x(), "approval.token must be token_x");
        assert_eq!(
            approval.min_allowance, expected_max,
            "approval.min_allowance must be maxAmountIn (1010)"
        );

        // Outer: commands == [V4_SWAP, UNWRAP_WETH].
        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[V4_SWAP, UNWRAP_WETH],
            "commands must be [V4_SWAP=0x10, UNWRAP_WETH=0x0c]"
        );
        assert_eq!(inputs.len(), 2);

        // V4_SWAP actions == [SWAP_EXACT_OUT_SINGLE, SETTLE_ALL, TAKE].
        let (actions, params) = decode_v4_input(&inputs[0]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_EXACT_OUT[..],
            "V4 actions must be [SWAP_EXACT_OUT_SINGLE=0x08, SETTLE_ALL=0x0c, TAKE=0x0e]"
        );
        assert_eq!(params.len(), 3);

        // params[0]: ExactOutputSingleParams.
        let swap = ExactOutputSingleParams::abi_decode(&params[0])
            .expect("params[0] must decode as ExactOutputSingleParams");
        // token_x=c1, weth=c0; input=token_x → zeroForOne=false (one→zero).
        assert!(
            !swap.zeroForOne,
            "zeroForOne must be false (token_x=c1 is the input)"
        );
        assert_eq!(swap.amountOut, 500u128, "amountOut must be 500");
        assert_eq!(
            swap.amountInMaximum, 1_010u128,
            "amountInMaximum must be 1010 at 100bps"
        );

        // params[2]: TAKE — (weth, ADDRESS_THIS, OPEN_DELTA).
        let (take_addr, take_to, take_amt) =
            <(Address, Address, U256)>::abi_decode_params(&params[2])
                .expect("params[2] must decode as (Address, Address, U256)");
        let weth_addr = crate::execution::protocols::common::evm_addr(&weth());
        assert_eq!(take_addr, weth_addr, "TAKE currency must be WETH");
        assert_eq!(
            take_to, ADDRESS_THIS,
            "TAKE recipient must be ADDRESS_THIS so router holds WETH"
        );
        assert_eq!(take_amt, OPEN_DELTA, "TAKE amount must be OPEN_DELTA (0)");

        // UNWRAP_WETH input: (recipient, amount_out) — enforce exact output as min.
        let (unwrap_recipient, unwrap_min) = <(Address, U256)>::abi_decode_params(&inputs[1])
            .expect("UNWRAP_WETH input must decode as (Address, U256)");
        assert_eq!(
            unwrap_recipient, recipient,
            "UNWRAP_WETH recipient must be opts recipient"
        );
        assert_eq!(
            unwrap_min, out_raw,
            "UNWRAP_WETH min must be the exact output amount (500)"
        );
    }

    // ── address(0)-native pool is unchanged by wrap detection ────────────────

    /// An address(0)-native V4 pool with `Currency::Native` still produces a single
    /// `[V4_SWAP]` command with `SETTLE_ALL` — the wrap path must NOT activate.
    #[test]
    fn address0_native_pool_unchanged() {
        use crate::execution::route_planner::V4_SWAP;

        let p = native_pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x55);
        let opts = opts(recipient, 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(300u64),
        };
        let quoted_out = AssetAmount::new(usdc(), U256::from(600u64));
        let route = Route::new_single_hop(eth(), usdc(), TradeType::ExactIn);

        let prepared = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(usdc()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect("native pool with Currency::Native must succeed unchanged");

        // Single [V4_SWAP] command — no WRAP_ETH prepended.
        let (commands, inputs, _) = decode_outer(&prepared.tx.data);
        assert_eq!(
            commands.as_ref(),
            &[V4_SWAP],
            "native pool must still use single V4_SWAP command"
        );
        assert_eq!(inputs.len(), 1);

        // Actions inside V4_SWAP must be the standard SETTLE_ALL sequence.
        let (actions, _params) = decode_v4_input(&inputs[0]);
        assert_eq!(
            actions.as_ref(),
            &ACTIONS_EXACT_IN[..],
            "actions must be [SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE] — no wrap"
        );

        // tx.value == amountIn; no approval.
        assert_eq!(prepared.tx.value, U256::from(300u64));
        assert!(prepared.approval.is_none());
    }
}
