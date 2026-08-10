//! Uniswap V4 swap encoder: exact-in and exact-out → [`PreparedSwap`].
//!
//! Encodes single-hop V4 swaps via the Universal Router `V4_SWAP` (0x10) command.
//! The outer call is `execute(commands, inputs, deadline)` with one `V4_SWAP` entry.
//! The `V4_SWAP` input is `abi.encode(bytes actions, bytes[] params)` where:
//!
//! - **exact-in** actions: `[0x06, 0x0c, 0x0f]` (`SWAP_EXACT_IN_SINGLE`, `SETTLE_ALL`, `TAKE_ALL`)
//! - **exact-out** actions: `[0x08, 0x0c, 0x0f]` (`SWAP_EXACT_OUT_SINGLE`, `SETTLE_ALL`, `TAKE_ALL`)
//!
//! Native ETH is supported on pools where `currency0 == address(0)` (V4 native-first-class).
//! On a **native-in** swap the caller sends `tx.value = amountIn` and no Permit2 approval is
//! needed; on a **native-out** swap `tx.value = 0` and the token input is approved via Permit2.
//! `price_limit` is deferred to a later task.
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
    route_planner::{RoutePlanner, V4_SWAP},
    types::{Currency, CurrencyAmount, TradeType, UnsignedTx},
};

// ── V4 action bytes (single-hop) ────────────────────────────────────────────

/// V4 action byte: `SWAP_EXACT_IN_SINGLE` (0x06).
pub const ACTION_SWAP_EXACT_IN_SINGLE: u8 = 0x06;

/// V4 action byte: `SWAP_EXACT_OUT_SINGLE` (0x08).
pub const ACTION_SWAP_EXACT_OUT_SINGLE: u8 = 0x08;

/// V4 action byte: `SETTLE_ALL` (0x0c) — settle the input currency.
pub const ACTION_SETTLE_ALL: u8 = 0x0c;

/// V4 action byte: `TAKE_ALL` (0x0f) — take the output currency.
pub const ACTION_TAKE_ALL: u8 = 0x0f;

/// Packed action sequence for an exact-in single-hop V4 swap.
///
/// `[SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE_ALL]`
pub const ACTIONS_EXACT_IN: [u8; 3] = [
    ACTION_SWAP_EXACT_IN_SINGLE,
    ACTION_SETTLE_ALL,
    ACTION_TAKE_ALL,
];

/// Packed action sequence for an exact-out single-hop V4 swap.
///
/// `[SWAP_EXACT_OUT_SINGLE, SETTLE_ALL, TAKE_ALL]`
pub const ACTIONS_EXACT_OUT: [u8; 3] = [
    ACTION_SWAP_EXACT_OUT_SINGLE,
    ACTION_SETTLE_ALL,
    ACTION_TAKE_ALL,
];

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

impl Sealed for UniswapV4Pool {}

impl Executable for UniswapV4Pool {
    /// Build an exact-in V4 swap, supporting both ERC-20 and native-ETH inputs/outputs.
    ///
    /// Native ETH is handled when the pool has `currency0 == address(0)`.  On a
    /// native-in swap `tx.value = amountIn` and no Permit2 approval is required;
    /// on a native-out swap `tx.value = 0` and Permit2 approves the token input.
    /// A `price_limit` returns [`BuildError::UnsupportedProtocol`] (deferred).
    ///
    /// # Note — recipient delivery
    ///
    /// V4 output is delivered via `TAKE_ALL` to the Universal Router's `msgSender()`,
    /// which is the EOA that submits `execute` directly.  Consequently,
    /// `Recipient::To(other)` where `other != sender` is **not** honored by this
    /// single-hop encoder — `opts.recipient` is resolved (so `Recipient::Sender`
    /// returns [`BuildError::UnresolvedRecipient`]) but the resolved address is not
    /// threaded into the `TAKE_ALL` params.  Cross-recipient delivery is part of the
    /// deferred `msg.sender`/multicall audit.
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        route: &Route,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        // V4 resolves Native → address(0) (first-class currency), not WETH.
        let native_asset =
            amm_core::primitives::asset::AssetId::new(ctx.chain, alloy::primitives::B256::ZERO);
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

        // price_limit is deferred: V4 single-hop CAN accept a limit, but that
        // integration is out of scope for this task.
        if opts.price_limit.is_some() {
            return Err(BuildError::UnsupportedProtocol);
        }

        let (pool_key, c0, _c1) = build_pool_key(self)?;
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

        // SETTLE_ALL: settle input currency up to amountIn (U256).
        let p_settle: Bytes =
            <(Address, U256)>::abi_encode_params(&(common::evm_addr(&r.input), amount_in.raw))
                .into();
        // TAKE_ALL: take output currency at least minOut (U256).
        let p_take: Bytes =
            <(Address, U256)>::abi_encode_params(&(common::evm_addr(&r.output), min.raw)).into();
        // swap params encoded as a bare struct (no function selector).
        let p_swap: Bytes = swap_params.abi_encode().into();

        let v4_input = build_v4_swap_input(&ACTIONS_EXACT_IN, vec![p_swap, p_settle, p_take]);

        let mut planner = RoutePlanner::new();
        planner.add(V4_SWAP, v4_input);
        let deadline_secs = u64::try_from(r.deadline).map_err(|_| BuildError::Overflow)?;
        let data = planner.encode(deadline_secs);

        // native-in: caller sends ETH via tx.value; no Permit2 approval needed.
        // native-out: token input uses Permit2; tx.value stays zero.
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

    /// Build an exact-out V4 swap, supporting both ERC-20 and native-ETH inputs/outputs.
    ///
    /// Native ETH is handled when the pool has `currency0 == address(0)`.  On a
    /// native-in swap `tx.value = maxAmountIn` and no Permit2 approval is required;
    /// on a native-out swap `tx.value = 0` and Permit2 approves the token input.
    /// A `price_limit` returns [`BuildError::UnsupportedProtocol`] (deferred).
    ///
    /// # Note — recipient delivery
    ///
    /// V4 output is delivered via `TAKE_ALL` to the Universal Router's `msgSender()`,
    /// which is the EOA that submits `execute` directly.  Consequently,
    /// `Recipient::To(other)` where `other != sender` is **not** honored by this
    /// single-hop encoder — `opts.recipient` is resolved (so `Recipient::Sender`
    /// returns [`BuildError::UnresolvedRecipient`]) but the resolved address is not
    /// threaded into the `TAKE_ALL` params.  Cross-recipient delivery is part of the
    /// deferred `msg.sender`/multicall audit.
    fn build_swap_exact_out(
        &self,
        ctx: &ChainConfig,
        amount_out: CurrencyAmount,
        from: Currency,
        route: &Route,
        quoted_in: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        // V4 resolves Native → address(0) (first-class currency), not WETH.
        let native_asset =
            amm_core::primitives::asset::AssetId::new(ctx.chain, alloy::primitives::B256::ZERO);
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

        // price_limit deferred.
        if opts.price_limit.is_some() {
            return Err(BuildError::UnsupportedProtocol);
        }

        let (pool_key, c0, _c1) = build_pool_key(self)?;
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

        // SETTLE_ALL: settle input currency up to max (U256).
        let p_settle: Bytes =
            <(Address, U256)>::abi_encode_params(&(common::evm_addr(&r.input), max.raw)).into();
        // TAKE_ALL: take exact output amount (U256).
        let p_take: Bytes =
            <(Address, U256)>::abi_encode_params(&(common::evm_addr(&r.output), amount_out.raw))
                .into();
        let p_swap: Bytes = swap_params.abi_encode().into();

        let v4_input = build_v4_swap_input(&ACTIONS_EXACT_OUT, vec![p_swap, p_settle, p_take]);

        let mut planner = RoutePlanner::new();
        planner.add(V4_SWAP, v4_input);
        let deadline_secs = u64::try_from(r.deadline).map_err(|_| BuildError::Overflow)?;
        let data = planner.encode(deadline_secs);

        // native-in: caller sends ETH via tx.value (maxAmountIn); no Permit2 approval needed.
        // native-out: token input uses Permit2; tx.value stays zero.
        let value = if r.native_in { max.raw } else { U256::ZERO };
        let approval = if r.native_in {
            None
        } else {
            Some(common::erc20_approval(ctx.permit2()?, r.input, max.raw))
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
            "actions must be [0x06,0x0c,0x0f]"
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

        // params[2]: TAKE_ALL (output, minOut as U256).
        let (take_addr, take_amt) = <(Address, U256)>::abi_decode_params(&params[2])
            .expect("params[2] must decode as (Address, U256)");
        assert_eq!(
            take_addr,
            crate::execution::protocols::common::evm_addr(&weth()),
            "take address must be weth"
        );
        assert_eq!(
            take_amt,
            U256::from(990u64),
            "take amount must be minOut=990"
        );

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
            "actions must be [0x08,0x0c,0x0f]"
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

        // params[2]: TAKE_ALL (output, exact output as U256).
        let (take_addr, take_amt) = <(Address, U256)>::abi_decode_params(&params[2]).unwrap();
        assert_eq!(
            take_addr,
            crate::execution::protocols::common::evm_addr(&weth())
        );
        assert_eq!(take_amt, out_raw, "take amount must be amountOut=400");

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

        // params[2]: TAKE_ALL currency == usdc.
        let (take_addr, _) = <(Address, U256)>::abi_decode_params(&params[2])
            .expect("params[2] must decode as (Address, U256)");
        assert_eq!(
            take_addr,
            crate::execution::protocols::common::evm_addr(&usdc()),
            "TAKE_ALL currency must be usdc"
        );
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

        // TAKE_ALL currency == address(0).
        let (take_addr, take_amt) = <(Address, U256)>::abi_decode_params(&params[2]).unwrap();
        assert_eq!(
            take_addr,
            Address::ZERO,
            "TAKE_ALL currency must be address(0)"
        );
        // min out = floor(1000 * 9950/10000) = 995
        assert_eq!(
            take_amt,
            U256::from(995u64),
            "TAKE_ALL amount must be minOut"
        );
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

    // ── Guard: native input on non-native V4 pool → AssetNotInPool ──────────

    #[test]
    fn native_currency_on_non_native_v4_pool_returns_asset_not_in_pool() {
        // pool() is USDC/WETH (no address(0) currency) — wrap case is deferred.
        let p = pool();
        let c = ctx();
        let opts = opts(Address::repeat_byte(0x55), 9_999_999, 50);

        let amount_in = CurrencyAmount {
            currency: Currency::Native,
            raw: U256::from(100u64),
        };
        let quoted_out = AssetAmount::new(usdc(), U256::from(100u64));
        let route = Route::new_single_hop(usdc(), weth(), TradeType::ExactIn);

        let err = p
            .build_swap(
                &c,
                amount_in,
                Currency::Token(usdc()),
                &route,
                &quoted_out,
                &opts,
            )
            .expect_err("native on non-native pool must fail");
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

    // ── Action constant values ───────────────────────────────────────────────

    #[test]
    fn action_constants_have_expected_values() {
        use super::{
            ACTION_SETTLE_ALL, ACTION_SWAP_EXACT_IN_SINGLE, ACTION_SWAP_EXACT_OUT_SINGLE,
            ACTION_TAKE_ALL,
        };
        assert_eq!(ACTION_SWAP_EXACT_IN_SINGLE, 0x06);
        assert_eq!(ACTION_SWAP_EXACT_OUT_SINGLE, 0x08);
        assert_eq!(ACTION_SETTLE_ALL, 0x0c);
        assert_eq!(ACTION_TAKE_ALL, 0x0f);
        assert_eq!(ACTIONS_EXACT_IN, [0x06u8, 0x0c, 0x0f]);
        assert_eq!(ACTIONS_EXACT_OUT, [0x08u8, 0x0c, 0x0f]);
    }
}
