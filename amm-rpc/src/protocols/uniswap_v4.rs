//! Uniswap V4 on-chain state source: read a pool's price, liquidity, fees, and a
//! window of tick liquidity out of the singleton `PoolManager`, then build a
//! quotable [`UniswapV4Pool`].
//!
//! V4 collapses every pool into one `PoolManager` contract. A pool is no longer
//! its own contract; it is identified by a 32-byte
//! `pool_id = keccak256(abi.encode(poolKey))`. There is no factory to enumerate
//! pools, so discovery is **config-driven** (like Curve): each pool's
//! currencies, fee, tick spacing, and hooks are configured up front, and its
//! `pool_id` is derived from them.
//!
//! State is read with `extsload(bytes32)` — raw storage-slot reads against the
//! `PoolManager`. The slot layout mirrors uniswap/v4-core's `StateLibrary`:
//!
//! | value               | slot                                             |
//! |---------------------|--------------------------------------------------|
//! | pool state base     | `keccak256(pool_id ‖ POOLS_SLOT)` (POOLS_SLOT=6)  |
//! | slot0 (packed)      | `base` → sqrtPriceX96, tick, protocolFee, lpFee   |
//! | liquidity (uint128) | `base + 3`                                        |
//! | ticks[tick]         | `keccak256(int256(tick) ‖ (base + 4))` → gross/net |
//!
//! Refresh is the same two dependent rounds as V3 — slot0 + liquidity, then a
//! tick window around the active tick — because the swap math (identical to V3)
//! needs the concentrated liquidity a swap might cross.

use alloy::eips::BlockId;
use alloy::primitives::aliases::{I24, U24};
use alloy::primitives::{Address, B256, I256, U256, keccak256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use amm_core::primitives::asset::AssetId;
use amm_core::primitives::pool::{ExchangeId, PoolId, PoolKey};
use amm_core::primitives::ratio::Bps;
use amm_core::protocols::uniswap::v4::{Hooks, TickData, TickInfo, UniswapV4Pool};
use amm_core::traits::pool::Pool;

use crate::error::RpcError;
use crate::multicall::{self, Call, CallResult};
use crate::source::StateSource;

sol! {
    /// The five fields whose `abi.encode` hashes to a V4 `pool_id`.
    struct V4PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }
    /// `PoolManager` raw storage read (single-slot variant).
    interface IExtsload {
        function extsload(bytes32 slot) external view returns (bytes32);
    }
}

// ─── StateLibrary storage-layout constants (uniswap/v4-core) ──────────────────

/// Base slot of the `mapping(PoolId => Pool.State) _pools`.
const POOLS_SLOT: u64 = 6;
/// `Pool.State.liquidity` offset within a pool's state struct.
const LIQUIDITY_OFFSET: u64 = 3;
/// `Pool.State.ticks` mapping offset within a pool's state struct.
const TICKS_OFFSET: u64 = 4;

/// How many tick-spacings each side of the active tick to fetch (as V3).
const TICK_WINDOW: i32 = 50;

/// Calls in round 1, per pool: `extsload(slot0)`, `extsload(liquidity)`.
const ROUND1_CALLS: usize = 2;

/// A configured V4 pool: its derived id, sorted currencies, and the fee / tick
/// spacing / hook classification the swap math needs.
#[derive(Clone, Debug)]
pub struct V4PoolConfig {
    /// `keccak256(abi.encode(poolKey))` — how the pool is addressed.
    pub pool_id: B256,
    /// `currency0` as an asset (address-sorted `currency0 < currency1`).
    pub token0: AssetId,
    /// `currency1` as an asset.
    pub token1: AssetId,
    /// The static LP fee (pips) from the pool key. Reported by `discover`; the
    /// live fee is read from `slot0` at refresh (dynamic-fee pools change it).
    pub fee: u32,
    /// The pool's tick spacing.
    pub tick_spacing: i32,
    /// Whether the pool's hook keeps quotes reproducible from static state.
    pub hooks: Hooks,
}

impl V4PoolConfig {
    /// Build a config from currency addresses (already sorted `currency0 <
    /// currency1`) and their asset ids, deriving the `pool_id`. `hooks_address`
    /// feeds the id derivation; `hooks` classifies the pool for quoting.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        currency0: Address,
        currency1: Address,
        token0: AssetId,
        token1: AssetId,
        fee: u32,
        tick_spacing: i32,
        hooks_address: Address,
        hooks: Hooks,
    ) -> Self {
        Self {
            pool_id: B256::from(derive_pool_id(
                currency0,
                currency1,
                fee,
                tick_spacing,
                hooks_address,
            )),
            token0,
            token1,
            fee,
            tick_spacing,
            hooks,
        }
    }
}

/// A [`StateSource`] for a configured set of Uniswap V4 pools living in one
/// `PoolManager`, over a provider `P`.
///
/// `refresh` reads and builds the configured pools whose keys are requested;
/// `discover` returns every configured pool as a key (V4 discovery is
/// config-driven, not on-chain — there is no factory to enumerate).
pub struct UniswapV4Source<P> {
    provider: P,
    pool_manager: Address,
    pools: Vec<V4PoolConfig>,
}

impl<P: Provider> UniswapV4Source<P> {
    /// Wrap a provider, the singleton `PoolManager` address, and the configured
    /// pools it holds.
    pub fn new(provider: P, pool_manager: Address, pools: Vec<V4PoolConfig>) -> Self {
        Self {
            provider,
            pool_manager,
            pools,
        }
    }

    /// Find the config a `PoolKey` refers to by its stored `pool_id` hex.
    fn config_for(&self, key: &PoolKey) -> Option<&V4PoolConfig> {
        let want = key.address.parse::<B256>().ok()?;
        self.pools.iter().find(|p| p.pool_id == want)
    }

    /// Resolve each requested key to the identity + storage base it needs, in
    /// request order. Keys that name no configured pool (or aren't 2-asset) drop.
    fn plans(&self, keys: &[PoolKey]) -> Vec<PoolPlan> {
        let mut plans = Vec::new();
        for key in keys {
            let Some(config) = self.config_for(key) else {
                continue;
            };
            let assets: [AssetId; 2] = match key.assets.as_slice() {
                [a, b] => [*a, *b],
                _ => continue,
            };
            plans.push(PoolPlan {
                id: PoolId::new(&format!(
                    "{}:{}:{}",
                    key.chain.0,
                    key.exchange.as_str(),
                    key.address
                )),
                pool_id: config.pool_id,
                assets,
                tick_spacing: config.tick_spacing,
                hooks: config.hooks,
                state_slot: pool_state_slot(config.pool_id),
            });
        }
        plans
    }
}

/// A resolved pool's identity and precomputed storage base, carried across both
/// refresh rounds.
struct PoolPlan {
    id: PoolId,
    pool_id: B256,
    assets: [AssetId; 2],
    tick_spacing: i32,
    hooks: Hooks,
    state_slot: B256,
}

/// A pool's round-1 state, plus the plan index round 2 needs.
struct PoolState {
    plan_idx: usize,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    /// Live LP fee (pips) from slot0 — for dynamic-fee pools this is the current
    /// value, not the static one in the pool key.
    lp_fee: u32,
    /// Packed per-direction protocol fee (pips): `zeroForOne` in the low 12
    /// bits, `oneForZero` in the high 12.
    protocol_fee: u32,
}

/// Round 1: `extsload(slot0)` + `extsload(liquidity)` per plan, against the
/// manager, in strides of [`ROUND1_CALLS`].
fn round1_calls(pool_manager: Address, plans: &[PoolPlan]) -> Vec<Call> {
    let mut calls = Vec::with_capacity(plans.len() * ROUND1_CALLS);
    for plan in plans {
        calls.push(extsload_call(pool_manager, plan.state_slot));
        calls.push(extsload_call(
            pool_manager,
            offset_slot(plan.state_slot, LIQUIDITY_OFFSET),
        ));
    }
    calls
}

/// Decode each plan's slot0 + liquidity word pair, skipping any pool whose
/// either read reverted / mis-decoded.
fn decode_states(plans: &[PoolPlan], results: &[CallResult]) -> Vec<PoolState> {
    let mut states = Vec::new();
    for plan_idx in 0..plans.len() {
        let (Some(slot0), Some(liquidity)) = (
            results.get(plan_idx * ROUND1_CALLS),
            results.get(plan_idx * ROUND1_CALLS + 1),
        ) else {
            continue;
        };
        let (Some(slot0_word), Some(liq_word)) = (
            decode_word(&slot0.return_data),
            decode_word(&liquidity.return_data),
        ) else {
            continue;
        };
        let (sqrt_price_x96, tick, lp_fee, protocol_fee) = decode_slot0(slot0_word);
        states.push(PoolState {
            plan_idx,
            sqrt_price_x96,
            tick,
            liquidity: decode_liquidity(liq_word),
            lp_fee,
            protocol_fee,
        });
    }
    states
}

/// Round 2: one `extsload(ticks[t])` per tick-spacing in the window around each
/// pool's active tick. Returns the calls and, for each, `(state_index, tick)`.
fn round2_calls(
    pool_manager: Address,
    plans: &[PoolPlan],
    states: &[PoolState],
) -> (Vec<Call>, Vec<(usize, i32)>) {
    let mut calls = Vec::new();
    let mut refs = Vec::new();
    for (state_idx, state) in states.iter().enumerate() {
        let plan = &plans[state.plan_idx];
        let center = (state.tick / plan.tick_spacing) * plan.tick_spacing;
        for step in -TICK_WINDOW..=TICK_WINDOW {
            let tick = center + step * plan.tick_spacing;
            calls.push(extsload_call(
                pool_manager,
                tick_info_slot(plan.state_slot, tick),
            ));
            refs.push((state_idx, tick));
        }
    }
    (calls, refs)
}

/// Reconstruct each pool's initialized ticks from the round-2 results.
fn tick_windows(
    n: usize,
    results: &[CallResult],
    refs: &[(usize, i32)],
) -> Vec<Vec<(i32, TickInfo)>> {
    let mut windows: Vec<Vec<(i32, TickInfo)>> = vec![Vec::new(); n];
    for (&(state_idx, tick), result) in refs.iter().zip(results) {
        let Some(word) = decode_word(&result.return_data) else {
            continue;
        };
        let (gross, net) = decode_tick(word);
        // Only initialized ticks (nonzero gross) carry crossable liquidity.
        if gross != 0 {
            windows[state_idx].push((
                tick,
                TickInfo {
                    liquidity_net: net,
                    initialized: true,
                },
            ));
        }
    }
    windows
}

/// Assemble each decoded state + its tick window into a boxed [`UniswapV4Pool`].
fn build_pools(
    plans: &[PoolPlan],
    states: Vec<PoolState>,
    windows: Vec<Vec<(i32, TickInfo)>>,
) -> Vec<Box<dyn Pool>> {
    states
        .into_iter()
        .zip(windows)
        .map(|(state, ticks)| {
            let plan = &plans[state.plan_idx];
            // Effective fee = live LP fee compounded with the per-direction V4
            // protocol fee (low 12 bits = zeroForOne, high 12 = oneForZero).
            let fee_zero_for_one =
                UniswapV4Pool::combined_fee(state.lp_fee, state.protocol_fee & 0xfff);
            let fee_one_for_zero =
                UniswapV4Pool::combined_fee(state.lp_fee, state.protocol_fee >> 12);
            Box::new(UniswapV4Pool::new(
                plan.id.clone(),
                plan.pool_id,
                plan.assets,
                state.sqrt_price_x96,
                state.liquidity,
                state.tick,
                fee_zero_for_one,
                fee_one_for_zero,
                TickData::from_ticks(plan.tick_spacing, ticks),
                plan.hooks,
            )) as Box<dyn Pool>
        })
        .collect()
}

// ─── slot derivation ──────────────────────────────────────────────────────────

/// `pool_id = keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks))`.
fn derive_pool_id(
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
) -> [u8; 32] {
    let key = V4PoolKey {
        currency0,
        currency1,
        fee: U24::from(fee),
        tickSpacing: I24::try_from(tick_spacing).unwrap_or(I24::ZERO),
        hooks,
    };
    keccak256(key.abi_encode()).0
}

/// Base storage slot of a pool's `Pool.State`: `keccak256(pool_id ‖ POOLS_SLOT)`.
fn pool_state_slot(pool_id: B256) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(pool_id.as_slice());
    preimage[32..].copy_from_slice(&U256::from(POOLS_SLOT).to_be_bytes::<32>());
    keccak256(preimage)
}

/// A fixed offset added to a state base slot (for the packed scalar fields).
fn offset_slot(state_slot: B256, offset: u64) -> B256 {
    B256::from((U256::from_be_bytes(state_slot.0) + U256::from(offset)).to_be_bytes::<32>())
}

/// Storage slot of `ticks[tick]`: `keccak256(int256(tick) ‖ (base + TICKS_OFFSET))`.
/// The mapping key is the full sign-extended `int256`, as `StateLibrary` uses.
fn tick_info_slot(state_slot: B256, tick: i32) -> B256 {
    let ticks_map = U256::from_be_bytes(state_slot.0) + U256::from(TICKS_OFFSET);
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(
        &I256::try_from(tick)
            .unwrap_or(I256::ZERO)
            .to_be_bytes::<32>(),
    );
    preimage[32..].copy_from_slice(&ticks_map.to_be_bytes::<32>());
    keccak256(preimage)
}

// ─── ABI / word decoders ──────────────────────────────────────────────────────

/// An `extsload(slot)` call against `target`.
fn extsload_call(target: Address, slot: B256) -> Call {
    Call {
        target,
        call_data: IExtsload::extsloadCall { slot }.abi_encode().into(),
    }
}

/// Decode an `extsload` return (a single `bytes32`) into a word, or `None` on a
/// reverted / mis-encoded result.
fn decode_word(data: &[u8]) -> Option<B256> {
    IExtsload::extsloadCall::abi_decode_returns(data).ok()
}

/// Decode a packed `slot0` word into `(sqrtPriceX96, tick, lpFee, protocolFee)`.
///
/// Layout: `sqrtPriceX96` bits `0..160`, `tick` (int24) `160..184`,
/// `protocolFee` (uint24) `184..208`, `lpFee` (uint24) `208..232`.
fn decode_slot0(word: B256) -> (U256, i32, u32, u32) {
    let value = U256::from_be_bytes(word.0);
    let mask160 = (U256::from(1u8) << 160) - U256::from(1u8);
    let sqrt_price_x96 = value & mask160;
    let raw = ((value >> 160usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    // Sign-extend the 24-bit tick.
    let tick = match raw & 0x80_0000 != 0 {
        true => raw as i32 - (1 << 24),
        false => raw as i32,
    };
    let protocol_fee = ((value >> 184usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    let lp_fee = ((value >> 208usize) & U256::from(0xFF_FFFFu32)).to::<u32>();
    (sqrt_price_x96, tick, lp_fee, protocol_fee)
}

/// Decode a `liquidity` word — the pool's active liquidity in the low 128 bits.
fn decode_liquidity(word: B256) -> u128 {
    (U256::from_be_bytes(word.0) & low128()).to::<u128>()
}

/// Decode a `ticks[t]` word into `(liquidityGross, liquidityNet)` — gross in the
/// low 128 bits, net (signed) in the high 128.
fn decode_tick(word: B256) -> (u128, i128) {
    let value = U256::from_be_bytes(word.0);
    let gross = (value & low128()).to::<u128>();
    let net = (value >> 128usize).to::<u128>() as i128; // reinterpret bits as two's-complement
    (gross, net)
}

/// The `2^128 - 1` low-128-bit mask.
fn low128() -> U256 {
    (U256::from(1u8) << 128) - U256::from(1u8)
}

#[async_trait::async_trait]
impl<P: Provider + Send + Sync> StateSource for UniswapV4Source<P> {
    async fn discover(
        &self,
        chain: &amm_core::primitives::asset::ChainId,
        _assets: &[AssetId],
    ) -> Result<Vec<PoolKey>, RpcError> {
        // Config-driven: every configured pool is a key, addressed by its id.
        Ok(self
            .pools
            .iter()
            .map(|pool| PoolKey {
                exchange: ExchangeId::new("uniswap-v4"),
                chain: *chain,
                address: pool.pool_id.to_string(),
                assets: vec![pool.token0, pool.token1],
                // Pool-key fee is pips (millionths); 100 pips = 1 bp.
                fee_bps: Some(Bps((pool.fee / 100) as u16)),
            })
            .collect())
    }

    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError> {
        let plans = self.plans(keys);

        // Round 1: price + liquidity for every pool, all read from the manager.
        let round1 =
            multicall::aggregate3(&self.provider, round1_calls(self.pool_manager, &plans), at)
                .await?;
        let states = decode_states(&plans, &round1);

        // Round 2 (same block): a tick window around each active tick.
        let (round2_calls, refs) = round2_calls(self.pool_manager, &plans, &states);
        let round2 = multicall::aggregate3(&self.provider, round2_calls, at).await?;
        let windows = tick_windows(states.len(), &round2, &refs);

        Ok(build_pools(&plans, states, windows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use amm_core::primitives::asset::{AssetAmount, ChainId};

    fn usdc() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x01]))
    }
    fn weth() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x02]))
    }

    const C0: Address = address!("0x0000000000000000000000000000000000000001");
    const C1: Address = address!("0x0000000000000000000000000000000000000002");
    /// Mainnet Uniswap V4 `PoolManager`.
    const MANAGER: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");

    /// sqrtPriceX96 for tick 0 (1:1) = 2^96.
    const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;

    /// Golden vector: the canonical mainnet ETH/USDC 0.05% pool — native ETH as
    /// `currency0 = address(0)`, USDC as `currency1`, fee 500, tick spacing 10,
    /// no hooks — hashes to its published `pool_id`. Anchors the whole derivation
    /// (abi.encode shape + keccak) against a real on-chain value.
    #[test]
    fn derive_pool_id_matches_mainnet_eth_usdc() {
        let usdc_addr = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let got = derive_pool_id(Address::ZERO, usdc_addr, 500, 10, Address::ZERO);
        let expected: [u8; 32] =
            "0x21c67e77068de97969ba93d4aab21826d33ca12bb9f565d8496e8fda8a82ca27"
                .parse::<B256>()
                .unwrap()
                .0;
        assert_eq!(got, expected);
    }

    /// A negative `tickSpacing` must sign-extend (differ from its positive twin),
    /// proving the `int24`→`int256` padding is signed.
    #[test]
    fn derive_pool_id_distinguishes_sign() {
        let pos = derive_pool_id(C0, C1, 3000, 60, Address::ZERO);
        let neg = derive_pool_id(C0, C1, 3000, -60, Address::ZERO);
        assert_ne!(pos, neg);
    }

    /// Pack a `slot0` word from its fields, then decode it back — proving the
    /// bit layout (sqrtPrice / signed tick / lpFee) round-trips.
    fn slot0_word(sqrt: U256, tick: i32, lp_fee: u32) -> B256 {
        let tick_bits = U256::from((tick as i64 & 0xFF_FFFF) as u64) << 160usize;
        let lp_fee_bits = U256::from(lp_fee) << 208usize;
        B256::from((sqrt | tick_bits | lp_fee_bits).to_be_bytes::<32>())
    }

    #[test]
    fn decode_slot0_extracts_sqrt_signed_tick_and_fee() {
        let sqrt = U256::from(1u8) << 96usize; // 2^96 → price 1
        let (got_sqrt, got_tick, lp_fee, protocol_fee) = decode_slot0(slot0_word(sqrt, -60, 3000));
        assert_eq!(got_sqrt, sqrt);
        assert_eq!(got_tick, -60);
        assert_eq!(lp_fee, 3000);
        assert_eq!(protocol_fee, 0);
    }

    #[test]
    fn decode_tick_splits_gross_and_signed_net() {
        let gross = 5_000u128;
        let net = -250i128;
        let word = B256::from(
            (U256::from(gross) | (U256::from(net as u128) << 128usize)).to_be_bytes::<32>(),
        );
        assert_eq!(decode_tick(word), (gross, net));
    }

    /// Build a full-range pool from a decoded state + its tick window (the shape
    /// a refresh assembles) and quote through it — network-free.
    #[test]
    fn build_full_range_pool_quotes_in_the_fee_band() {
        let liq: i128 = 1_000_000_000_000_000_000;
        let plan = PoolPlan {
            id: PoolId::new("1:uniswap-v4:0xfull"),
            pool_id: B256::repeat_byte(0xAA),
            assets: [usdc(), weth()],
            tick_spacing: 60,
            hooks: Hooks::None,
            state_slot: B256::ZERO,
        };
        let state = PoolState {
            plan_idx: 0,
            sqrt_price_x96: U256::from(SQRT_1_1),
            tick: 0,
            liquidity: liq as u128,
            lp_fee: 3000,
            protocol_fee: 0,
        };
        let ticks = vec![
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
        ];
        let pools = build_pools(&[plan], vec![state], vec![ticks]);
        assert_eq!(pools.len(), 1);
        let amount_in = U256::from(1_000_000_000u64);
        let out = pools[0]
            .quote(&AssetAmount::new(usdc(), amount_in), &weth())
            .unwrap();
        assert!(out.raw < amount_in);
        assert!(out.raw > amount_in * U256::from(996u64) / U256::from(1000u64));
    }

    /// End-to-end refresh against a forked mainnet RPC. Gated: set
    /// `AMM_RPC_FORK_URL` and run with `cargo test -p amm-rpc -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a forked RPC at $AMM_RPC_FORK_URL"]
    async fn refresh_mainnet_eth_usdc_v4_pool_quotes() {
        let Ok(url) = std::env::var("AMM_RPC_FORK_URL") else {
            return;
        };
        let provider = crate::provider::make_provider(&url).unwrap();

        // Mainnet ETH/USDC 0.05% V4 pool: currency0 = native ETH (address(0)),
        // currency1 = USDC, fee 500, tick spacing 10, no hooks.
        let usdc_addr = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let eth = AssetId::new(ChainId(1), B256::ZERO);
        let usdc = AssetId::new(ChainId(1), usdc_addr.into_word());
        let config = V4PoolConfig::new(
            Address::ZERO,
            usdc_addr,
            eth,
            usdc,
            500,
            10,
            Address::ZERO,
            Hooks::None,
        );
        let source = UniswapV4Source::new(provider, MANAGER, vec![config.clone()]);

        let key = PoolKey {
            exchange: ExchangeId::new("uniswap-v4"),
            chain: ChainId(1),
            address: config.pool_id.to_string(),
            assets: vec![eth, usdc],
            fee_bps: Some(Bps(5)),
        };
        let pools = source.refresh(&[key], BlockId::latest()).await.unwrap();
        assert_eq!(pools.len(), 1);
        let out = pools[0]
            .quote(
                &AssetAmount::new(eth, U256::from(1_000_000_000_000_000_000u128)),
                &usdc,
            )
            .unwrap();
        assert!(
            out.raw > U256::ZERO,
            "refreshed V4 pool must quote a positive output"
        );
    }
}
