//! Central pool catalog + per-protocol `refresh` dispatch.
//!
//! This module provides a `FIXTURES` registry that centralises the pool
//! metadata currently embedded in the per-protocol `fetch_*` helper functions
//! across the six execution test files. Each `Fixture` encodes the exact
//! Source + PoolKey construction for its protocol so the `refresh` function
//! can reproduce those fns faithfully.
//!
//! # Design choices
//!
//! * `FIXTURES` is a `std::sync::LazyLock<Vec<Fixture>>` rather than a plain
//!   `&'static [Fixture]` because (a) `CurvePoolConfig` coins use `Vec` which
//!   is not const-constructible, and (b) the Curve variant of `Adapter` is
//!   conditionally compiled behind the `curve` feature, so we push those
//!   entries only when the feature is enabled.
//!
//! * The `Curve` adapter stores `CurveVariant` (from `curve_adapter`) instead
//!   of `CurveInterface` (from `amm_core`). The brief skeleton shows
//!   `CurveInterface`, but `CurvePoolConfig` (used inside `refresh`) requires
//!   `CurveVariant`, and the two are not in 1-to-1 correspondence across all
//!   12 variants. Storing `CurveVariant` avoids a lossy roundtrip. A separate
//!   `decimals: &'static [u8]` field is also added (not in the brief skeleton)
//!   because `CurvePoolConfig` requires per-coin decimal counts and the base
//!   `TokenInfo` struct only covers two tokens.
//!
//! * `pool: Address` is set to `Address::ZERO` for Uniswap V4 fixtures because
//!   the V4 pool is addressed by a computed `pool_id` (B256), not a deployment
//!   address. The `pool_id` is re-derived inside `refresh` from the adapter's
//!   fee/tick_spacing/hooks fields.
//!
//! # Dead-code note
//!
//! Items here are consumed by the runner and self-check in later tasks. The
//! `#![allow(dead_code)]` at the module level keeps clippy clean during Phase 1.

#![allow(dead_code)]

use alloy::eips::BlockId;
use alloy::primitives::{Address, address};
use alloy::providers::Provider;
use amm_core::primitives::asset::{AssetId, ChainId};
use amm_core::primitives::pool::{ExchangeId, PoolKey};
use amm_core::traits::pool::Pool;
use amm_rpc::source::StateSource;
use std::sync::LazyLock;

// ── Types ─────────────────────────────────────────────────────────────────────

/// Token metadata needed to fund and verify balances in fork tests.
pub struct TokenInfo {
    /// On-chain EVM address (or `Address::ZERO` for native ETH).
    pub addr: Address,
    /// ERC-20 decimal count.
    pub decimals: u8,
    /// Storage slot of the `balanceOf` mapping (for slot-injection funding).
    pub balance_slot: u64,
}

/// Per-protocol metadata required to rebuild the exact `Source` + `PoolKey`
/// that each `fetch_*` helper constructs.
pub enum Adapter {
    /// Uniswap V2 — no per-pool config beyond the pair address.
    UniswapV2,

    /// Uniswap V3 concentrated-liquidity pool.
    UniswapV3 {
        /// Static LP fee in pips (e.g. 500 = 0.05%).
        fee: u32,
    },

    /// Uniswap V4 concentrated-liquidity pool.
    ///
    /// `currency0` / `currency1` are taken from the fixture's `token0.addr` /
    /// `token1.addr`; together with `fee`, `tick_spacing`, and `hooks_address`
    /// they reproduce the on-chain `pool_id` via `V4PoolConfig::new`.
    UniswapV4 {
        /// Static LP fee in pips.
        fee: u32,
        /// Pool tick spacing.
        tick_spacing: i32,
        /// Hook contract address (`Address::ZERO` when no hook is deployed).
        hooks_address: Address,
        /// V4 PoolManager address on this chain.
        manager: Address,
    },

    /// Curve pool (any variant). Only available with the `curve` feature.
    #[cfg(feature = "curve")]
    Curve {
        /// Which `curve_adapter` variant this pool implements. Stored as
        /// `CurveVariant` (12-way) rather than `CurveInterface` (4-way) so
        /// that `refresh` can pass it directly to `CurvePoolConfig::variant`
        /// without a lossy mapping.
        variant: curve_adapter::CurveVariant,
        /// All coin addresses in coin-index order (may be 2 or 3 coins).
        coins: &'static [Address],
        /// Decimal count per coin, index-aligned with `coins`.
        decimals: &'static [u8],
    },

    /// Aerodrome (Solidly) volatile or stable AMM pool on Base.
    Aerodrome {
        /// `true` for a stable (sAMM) pool, `false` for a volatile (vAMM) pool.
        /// Informational only — the source reads the flag from chain.
        stable: bool,
        /// Aerodrome PoolFactory address on this chain.
        factory: Address,
    },

    /// Aerodrome Slipstream (CL fork of Uniswap V3) pool on Base.
    Slipstream {
        /// Pool tick spacing — informational; the source reads it from chain.
        tick_spacing: i32,
    },
}

/// A single verified pool fixture: addresses, adapter config, and fork-test metadata.
pub struct Fixture {
    /// Human-readable name used as the lookup key in `fixture(name)`.
    pub name: &'static str,
    /// EIP-155 chain identifier (1 = Ethereum mainnet, 8453 = Base).
    pub chain: ChainId,
    /// Pool contract address (or `Address::ZERO` for V4 whose identity is a
    /// 32-byte hash; the `pool_id` is recomputed inside `refresh`).
    pub pool: Address,
    /// First token (address-sorted `token0`).
    pub token0: TokenInfo,
    /// Second token (address-sorted `token1`).
    pub token1: TokenInfo,
    /// Protocol-specific metadata consumed by `refresh`.
    pub adapter: Adapter,
    /// Default mainnet/Base block at which the pool is known to be live and
    /// liquid, used when no `AMM_FORK_BLOCK` override is set.
    pub default_block: u64,
}

// ── Address constants (shared across fixture entries) ─────────────────────────

// ── Mainnet token addresses ───────────────────────────────────────────────────
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDC_MAINNET: Address = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const WETH_MAINNET: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
const CRVUSD: Address = address!("f939E0A03FB07F59A73314E73794Be0E57ac1b4E");
const TC_NG_TOKEN: Address = address!("1cfa5641c01406aB8AC350dEd7d735ec41298372");

// ── Mainnet pool addresses ────────────────────────────────────────────────────
const V2_USDC_WETH_PAIR: Address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
const V3_USDC_WETH_POOL: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
const CURVE_3POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
const CURVE_TRICRYPTO2: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");
const CURVE_STABLE_NG: Address = address!("4DEcE678ceceb27446b35C672dC7d61F30bAD69E");
const CURVE_TWOCRYPTO_NG: Address = address!("592878b920101946fb5915ab97961bc546f211cc");

// ── Mainnet infrastructure ────────────────────────────────────────────────────
const V4_MANAGER_MAINNET: Address = address!("000000000004444c5dc75cB358380D2e3dE08A90");

// ── Balance slots (ground truth from execution_curve.rs:86-94 and others) ────
// USDC slot = 9 (all chains)
// DAI slot  = 2
// USDT slot = 2
// WETH slot = 3
// WBTC slot = 0
const SLOT_USDC: u64 = 9;
const SLOT_DAI: u64 = 2;
const SLOT_USDT: u64 = 2;
const SLOT_WETH: u64 = 3;
const SLOT_WBTC: u64 = 0;

// ── Base token addresses ──────────────────────────────────────────────────────
const WETH_BASE: Address = address!("4200000000000000000000000000000000000006");
const USDC_BASE: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const USDBC: Address = address!("d9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA");

// ── Base pool addresses ───────────────────────────────────────────────────────
const AERO_VOL_WETH_USDC: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");
const AERO_STABLE_USDC_USDBC: Address = address!("27a8Afa3Bd49406e48a074350fB7b2020c43B2bD");
const SLIPSTREAM_WETH_USDC: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");

// ── Base infrastructure ───────────────────────────────────────────────────────
const AERO_FACTORY_BASE: Address = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");

// ── Curve coin lists (static, for Curve adapter's `coins` field) ──────────────
#[cfg(feature = "curve")]
static COINS_3POOL: &[Address] = &[DAI, USDC_MAINNET, USDT];
#[cfg(feature = "curve")]
static DECIMALS_3POOL: &[u8] = &[18, 6, 6];

#[cfg(feature = "curve")]
static COINS_TRICRYPTO2: &[Address] = &[USDT, WBTC, WETH_MAINNET];
#[cfg(feature = "curve")]
static DECIMALS_TRICRYPTO2: &[u8] = &[6, 8, 18];

#[cfg(feature = "curve")]
static COINS_STABLE_NG: &[Address] = &[USDC_MAINNET, CRVUSD];
#[cfg(feature = "curve")]
static DECIMALS_STABLE_NG: &[u8] = &[6, 18];

#[cfg(feature = "curve")]
static COINS_TWOCRYPTO_NG: &[Address] = &[WETH_MAINNET, TC_NG_TOKEN];
#[cfg(feature = "curve")]
static DECIMALS_TWOCRYPTO_NG: &[u8] = &[18, 18];

// ── FIXTURES catalog ──────────────────────────────────────────────────────────

/// All known pool fixtures. Populated once at first access via `LazyLock`.
///
/// Curve entries are only pushed when the `curve` feature is enabled, which is
/// why this is a `LazyLock<Vec<Fixture>>` rather than a plain `static` slice.
pub static FIXTURES: LazyLock<Vec<Fixture>> = LazyLock::new(|| {
    let mut v: Vec<Fixture> = vec![
        // ── Uniswap V2 ───────────────────────────────────────────────────────
        Fixture {
            name: "usdc_weth_v2",
            chain: ChainId(1),
            pool: V2_USDC_WETH_PAIR,
            // USDC < WETH numerically → token0 = USDC, token1 = WETH.
            // Source: execution_v2.rs:38-44 (PAIR, USDC_ADDR, WETH_ADDR, USDC_SLOT).
            token0: TokenInfo {
                addr: USDC_MAINNET,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            token1: TokenInfo {
                addr: WETH_MAINNET,
                decimals: 18,
                balance_slot: SLOT_WETH,
            },
            adapter: Adapter::UniswapV2,
            default_block: 20_000_000,
        },
        // ── Uniswap V3 ───────────────────────────────────────────────────────
        Fixture {
            name: "usdc_weth_v3_005",
            chain: ChainId(1),
            pool: V3_USDC_WETH_POOL,
            // token0 = USDC (0xa0b8...) < WETH (0xC02a...) → address-sorted.
            // Source: execution_v3.rs:68-86 (POOL_ADDR, USDC_ADDR, WETH_ADDR).
            token0: TokenInfo {
                addr: USDC_MAINNET,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            token1: TokenInfo {
                addr: WETH_MAINNET,
                decimals: 18,
                balance_slot: SLOT_WETH,
            },
            adapter: Adapter::UniswapV3 { fee: 500 },
            default_block: 20_000_000,
        },
        // Multi-hop hop-2 pool (WETH/USDT 0.05% on mainnet).
        // TODO(verify-on-fork): confirm address AND token0/token1 order at the pinned block.
        Fixture {
            name: "weth_usdt_v3_005",
            chain: ChainId(1),
            // Canonical WETH/USDT 0.05% pool on mainnet (address-sorted).
            // WETH 0xC02a… < USDT 0xdAC1… → token0 = WETH, token1 = USDT.
            pool: address!("4e68Ccd3E89f51C3074ca5072bbAC773960dFa36"),
            token0: TokenInfo {
                addr: WETH_MAINNET,
                decimals: 18,
                balance_slot: SLOT_WETH,
            },
            token1: TokenInfo {
                addr: USDT,
                decimals: 6,
                balance_slot: SLOT_USDT,
            },
            adapter: Adapter::UniswapV3 { fee: 500 },
            default_block: 20_000_000,
        },
        // Multi-hop hop-2 pool (DAI/USDC 0.01% on mainnet).
        // TODO(verify-on-fork): confirm address/slot at pinned block 20_000_000.
        Fixture {
            name: "dai_usdc_v3",
            chain: ChainId(1),
            // Canonical DAI/USDC 0.01% pool on mainnet (address-sorted: DAI < USDC).
            pool: address!("5777d92f208679DB4b9778590Fa3CAB3aC9e2168"),
            token0: TokenInfo {
                addr: DAI,
                decimals: 18,
                balance_slot: SLOT_DAI,
            },
            token1: TokenInfo {
                addr: USDC_MAINNET,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            adapter: Adapter::UniswapV3 { fee: 100 },
            default_block: 20_000_000,
        },
        // ── Uniswap V4 ───────────────────────────────────────────────────────
        Fixture {
            name: "eth_usdc_v4_005",
            chain: ChainId(1),
            // V4 pool identity is a 32-byte pool_id, not a contract address.
            // `pool` is Address::ZERO here; refresh recomputes the pool_id from
            // the adapter's fee/tick_spacing/hooks_address fields.
            // Source: execution_v4.rs:80-107 (fetch_v4_pool).
            pool: Address::ZERO,
            // currency0 = address(0) (native ETH), currency1 = USDC.
            token0: TokenInfo {
                addr: Address::ZERO, // ETH — address(0) is V4's native currency
                decimals: 18,
                balance_slot: 0, // native ETH has no ERC-20 slot
            },
            token1: TokenInfo {
                addr: USDC_MAINNET,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            adapter: Adapter::UniswapV4 {
                fee: 500,
                tick_spacing: 10,
                hooks_address: Address::ZERO,
                manager: V4_MANAGER_MAINNET,
            },
            default_block: 25_724_266, // post-V4-launch block; V4 not at block 20M
        },
        Fixture {
            name: "usdc_weth_v4_005",
            chain: ChainId(1),
            // WETH-currency V4 pool (USDC=currency0, WETH=currency1 as ERC-20).
            // pool_id recomputed inside refresh — Address::ZERO placeholder.
            // Source: execution_v4.rs:341-368 (fetch_v4_weth_pool).
            pool: Address::ZERO,
            token0: TokenInfo {
                addr: USDC_MAINNET, // currency0 (USDC < WETH numerically)
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            token1: TokenInfo {
                addr: WETH_MAINNET, // currency1
                decimals: 18,
                balance_slot: SLOT_WETH,
            },
            adapter: Adapter::UniswapV4 {
                fee: 500,
                tick_spacing: 10,
                hooks_address: Address::ZERO,
                manager: V4_MANAGER_MAINNET,
            },
            default_block: 25_724_266,
        },
        // ── Aerodrome (Base) ─────────────────────────────────────────────────
        Fixture {
            name: "aero_vol_weth_usdc",
            chain: ChainId(8453),
            pool: AERO_VOL_WETH_USDC,
            // token0 = WETH (0x4200…) < USDC (0x8335…) on Base — address-sorted.
            // Source: execution_aerodrome.rs:79-115 (fetch_aerodrome_pools, volatile).
            token0: TokenInfo {
                addr: WETH_BASE,
                decimals: 18,
                balance_slot: SLOT_WETH,
            },
            token1: TokenInfo {
                addr: USDC_BASE,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            adapter: Adapter::Aerodrome {
                stable: false,
                factory: AERO_FACTORY_BASE,
            },
            default_block: 30_000_000,
        },
        Fixture {
            name: "aero_stable_usdc_usdbc",
            chain: ChainId(8453),
            pool: AERO_STABLE_USDC_USDBC,
            // token0 = USDC (0x8335…) < USDbC (0xd9aA…) on Base — address-sorted.
            // Source: execution_aerodrome.rs:79-115 (fetch_aerodrome_pools, stable).
            token0: TokenInfo {
                addr: USDC_BASE,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            token1: TokenInfo {
                addr: USDBC,
                decimals: 6,
                // USDbC is a proxy (EIP-1967 impl 0x1833c6…) with a non-standard
                // balance layout — not in mapping slots 0..40, so it cannot be
                // funded via simple slot-stuffing. Only usable as swap OUTPUT
                // (USDC→USDbC), never as a funded input. Slot is a placeholder.
                balance_slot: 9,
            },
            adapter: Adapter::Aerodrome {
                stable: true,
                factory: AERO_FACTORY_BASE,
            },
            default_block: 30_000_000,
        },
        // ── Aerodrome Slipstream (Base) ───────────────────────────────────────
        Fixture {
            name: "slipstream_weth_usdc",
            chain: ChainId(8453),
            pool: SLIPSTREAM_WETH_USDC,
            // token0 = WETH (0x4200…) < USDC (0x8335…) on Base — address-sorted.
            // Source: execution_slipstream.rs:69-87 (fetch_slipstream_pool).
            token0: TokenInfo {
                addr: WETH_BASE,
                decimals: 18,
                balance_slot: SLOT_WETH,
            },
            token1: TokenInfo {
                addr: USDC_BASE,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            adapter: Adapter::Slipstream {
                tick_spacing: 100, // WETH/USDC CL pool tick spacing (read from chain)
            },
            default_block: 30_000_000,
        },
    ];

    // ── Curve fixtures (only present when the `curve` feature is enabled) ────
    #[cfg(feature = "curve")]
    {
        use curve_adapter::CurveVariant;
        v.push(Fixture {
            name: "curve_3pool",
            chain: ChainId(1),
            pool: CURVE_3POOL,
            // token0 = DAI (coin[0]), token1 = USDC (coin[1]) — representative
            // two-token view; full coin list is in the adapter's `coins` field.
            // Source: execution_curve.rs:116-149 (fetch_3pool).
            token0: TokenInfo {
                addr: DAI,
                decimals: 18,
                balance_slot: SLOT_DAI,
            },
            token1: TokenInfo {
                addr: USDC_MAINNET,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            adapter: Adapter::Curve {
                variant: CurveVariant::StableSwapV1,
                coins: COINS_3POOL,
                decimals: DECIMALS_3POOL,
            },
            default_block: 20_000_000,
        });
        v.push(Fixture {
            name: "curve_tricrypto2",
            chain: ChainId(1),
            pool: CURVE_TRICRYPTO2,
            // token0 = USDT (coin[0]), token1 = WBTC (coin[1]).
            // Source: execution_curve.rs:231-264 (fetch_tricrypto2).
            token0: TokenInfo {
                addr: USDT,
                decimals: 6,
                balance_slot: SLOT_USDT,
            },
            token1: TokenInfo {
                addr: WBTC,
                decimals: 8,
                balance_slot: SLOT_WBTC,
            },
            adapter: Adapter::Curve {
                variant: CurveVariant::TriCryptoV1,
                coins: COINS_TRICRYPTO2,
                decimals: DECIMALS_TRICRYPTO2,
            },
            default_block: 20_000_000,
        });
        v.push(Fixture {
            name: "curve_stable_ng",
            chain: ChainId(1),
            pool: CURVE_STABLE_NG,
            // token0 = USDC (coin[0]), token1 = crvUSD (coin[1]).
            // Source: execution_curve.rs:155-187 (fetch_stable_ng).
            token0: TokenInfo {
                addr: USDC_MAINNET,
                decimals: 6,
                balance_slot: SLOT_USDC,
            },
            token1: TokenInfo {
                addr: CRVUSD,
                decimals: 18,
                // TODO(verify-on-fork): confirm crvUSD balanceOf slot at block 20_000_000.
                balance_slot: 0,
            },
            adapter: Adapter::Curve {
                variant: CurveVariant::StableSwapNG,
                coins: COINS_STABLE_NG,
                decimals: DECIMALS_STABLE_NG,
            },
            default_block: 20_000_000,
        });
        v.push(Fixture {
            name: "curve_twocrypto_ng",
            chain: ChainId(1),
            pool: CURVE_TWOCRYPTO_NG,
            // token0 = WETH (coin[0]), token1 = TC_NG_TOKEN (coin[1]).
            // Source: execution_curve.rs:193-225 (fetch_twocrypto_ng).
            token0: TokenInfo {
                addr: WETH_MAINNET,
                decimals: 18,
                balance_slot: SLOT_WETH,
            },
            token1: TokenInfo {
                addr: TC_NG_TOKEN,
                decimals: 18,
                // TODO(verify-on-fork): confirm TC_NG_TOKEN balanceOf slot at block 20_000_000.
                balance_slot: 0,
            },
            adapter: Adapter::Curve {
                variant: CurveVariant::TwoCryptoNG,
                coins: COINS_TWOCRYPTO_NG,
                decimals: DECIMALS_TWOCRYPTO_NG,
            },
            default_block: 20_000_000,
        });
    }

    v
});

// ── Lookup ────────────────────────────────────────────────────────────────────

/// Look up a fixture by name. Panics with a clear message on unknown names.
pub fn fixture(name: &str) -> &'static Fixture {
    FIXTURES
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("unknown fixture {name}"))
}

/// Iterate over all mainnet (chain 1) fixtures.
///
/// Used by the fork self-check to verify every mainnet pool refreshes cleanly.
pub fn mainnet_fixtures() -> impl Iterator<Item = &'static Fixture> {
    FIXTURES.iter().filter(|f| f.chain == ChainId(1))
}

// ── Refresh ───────────────────────────────────────────────────────────────────

/// Build the on-chain `Source` + `PoolKey` for `fx` and return a refreshed pool.
///
/// Mirrors the exact `Source::new` + `PoolKey` construction used in each
/// protocol's `fetch_*` helper function (the ground truth for per-protocol
/// metadata).  Panics when the refresh fails — that is always a broken test
/// setup, not a production error.
pub async fn refresh(fx: &Fixture, provider: impl Provider + Clone, block: u64) -> Box<dyn Pool> {
    let bid = BlockId::number(block);

    // Helper: build an `AssetId` from a chain id and EVM token address.
    let asset =
        |chain: u64, addr: Address| -> AssetId { AssetId::new(ChainId(chain), addr.into_word()) };

    match &fx.adapter {
        // ── UniswapV2 ─────────────────────────────────────────────────────────
        // Mirrors execution_v2.rs:81-90 (fetch_v2_pool).
        Adapter::UniswapV2 => {
            let source = amm_rpc::protocols::uniswap_v2::UniswapV2Source::new(&provider);
            let key = PoolKey {
                exchange: ExchangeId::new("uniswap-v2"),
                chain: fx.chain,
                address: fx.pool.to_string(),
                assets: vec![
                    asset(fx.chain.0, fx.token0.addr),
                    asset(fx.chain.0, fx.token1.addr),
                ],
                fee_bps: None,
            };
            let mut pools = source
                .refresh(&[key], bid)
                .await
                .expect("UniswapV2Source::refresh");
            assert_eq!(pools.len(), 1, "{}: expected exactly one pool", fx.name);
            pools.remove(0)
        }

        // ── UniswapV3 ─────────────────────────────────────────────────────────
        // Mirrors execution_v3.rs:68-86 (fetch_v3_pool).
        Adapter::UniswapV3 { fee: _ } => {
            let source = amm_rpc::protocols::uniswap_v3::UniswapV3Source::new(&provider);
            let key = PoolKey {
                exchange: ExchangeId::new("uniswap-v3"),
                chain: fx.chain,
                address: fx.pool.to_string(),
                assets: vec![
                    asset(fx.chain.0, fx.token0.addr),
                    asset(fx.chain.0, fx.token1.addr),
                ],
                fee_bps: None,
            };
            let mut pools = source
                .refresh(&[key], bid)
                .await
                .expect("UniswapV3Source::refresh");
            assert_eq!(pools.len(), 1, "{}: expected exactly one pool", fx.name);
            pools.remove(0)
        }

        // ── UniswapV4 ─────────────────────────────────────────────────────────
        // Mirrors execution_v4.rs:80-107 (fetch_v4_pool) and :341-368 (fetch_v4_weth_pool).
        Adapter::UniswapV4 {
            fee,
            tick_spacing,
            hooks_address,
            manager,
        } => {
            use amm_core::protocols::uniswap::v4::Hooks;
            use amm_rpc::protocols::uniswap_v4::{UniswapV4Source, V4PoolConfig};

            let t0 = asset(fx.chain.0, fx.token0.addr);
            let t1 = asset(fx.chain.0, fx.token1.addr);

            // `V4PoolConfig::new` derives the pool_id from the key fields.
            // When token0 is ETH (address(0)), the B256 slot is already zero.
            let currency0 = fx.token0.addr; // address(0) for native ETH pool
            let currency1 = fx.token1.addr;

            let config = V4PoolConfig::new(
                currency0,
                currency1,
                t0,
                t1,
                *fee,
                *tick_spacing,
                *hooks_address,
                Hooks::None,
            );
            let source = UniswapV4Source::new(provider, *manager, vec![config.clone()]);
            let key = PoolKey {
                exchange: ExchangeId::new("uniswap-v4"),
                chain: fx.chain,
                address: config.pool_id.to_string(),
                assets: vec![t0, t1],
                fee_bps: None,
            };
            let mut pools = source
                .refresh(&[key], bid)
                .await
                .expect("UniswapV4Source::refresh");
            assert_eq!(pools.len(), 1, "{}: expected exactly one pool", fx.name);
            pools.remove(0)
        }

        // ── Curve ─────────────────────────────────────────────────────────────
        // Mirrors execution_curve.rs:116-149 (fetch_3pool), :155-187 (fetch_stable_ng),
        // :193-225 (fetch_twocrypto_ng), and :231-264 (fetch_tricrypto2).
        #[cfg(feature = "curve")]
        Adapter::Curve {
            variant,
            coins,
            decimals,
        } => {
            use amm_rpc::protocols::curve::{CurvePoolConfig, CurveSource};

            let coin_assets: Vec<AssetId> =
                coins.iter().map(|&addr| asset(fx.chain.0, addr)).collect();

            let config = CurvePoolConfig {
                address: fx.pool,
                variant: *variant,
                coins: coin_assets.clone(),
                decimals: decimals.to_vec(),
                base_pool: None,
                eth_variant: None,
            };
            let source = CurveSource::new(&provider, vec![config]);
            let key = PoolKey {
                exchange: ExchangeId::new("curve"),
                chain: fx.chain,
                address: fx.pool.to_string(),
                assets: coin_assets,
                fee_bps: None,
            };
            let mut pools = source
                .refresh(&[key], bid)
                .await
                .expect("CurveSource::refresh");
            assert_eq!(pools.len(), 1, "{}: expected exactly one pool", fx.name);
            pools.remove(0)
        }

        // ── Aerodrome ─────────────────────────────────────────────────────────
        // Mirrors execution_aerodrome.rs:79-115 (fetch_aerodrome_pools).
        // Each fixture covers one pool; `AerodromeSource::refresh` reads the
        // `stable()` flag from chain — the adapter's `stable` field is metadata only.
        Adapter::Aerodrome { stable: _, factory } => {
            let source = amm_rpc::protocols::aerodrome::AerodromeSource::new(&provider, *factory);
            let key = PoolKey {
                exchange: ExchangeId::new("aerodrome"),
                chain: fx.chain,
                address: fx.pool.to_string(),
                assets: vec![
                    asset(fx.chain.0, fx.token0.addr),
                    asset(fx.chain.0, fx.token1.addr),
                ],
                fee_bps: None,
            };
            let mut pools = source
                .refresh(&[key], bid)
                .await
                .expect("AerodromeSource::refresh");
            assert_eq!(pools.len(), 1, "{}: expected exactly one pool", fx.name);
            pools.remove(0)
        }

        // ── Slipstream ────────────────────────────────────────────────────────
        // Mirrors execution_slipstream.rs:69-87 (fetch_slipstream_pool).
        // `tick_spacing` is read from chain by the source; the adapter field is metadata.
        Adapter::Slipstream { tick_spacing: _ } => {
            let source = amm_rpc::protocols::slipstream::SlipstreamSource::new(&provider);
            let key = PoolKey {
                exchange: ExchangeId::new("aerodrome-slipstream"),
                chain: fx.chain,
                address: fx.pool.to_string(),
                assets: vec![
                    asset(fx.chain.0, fx.token0.addr),
                    asset(fx.chain.0, fx.token1.addr),
                ],
                fee_bps: None,
            };
            let mut pools = source
                .refresh(&[key], bid)
                .await
                .expect("SlipstreamSource::refresh");
            assert_eq!(pools.len(), 1, "{}: expected exactly one pool", fx.name);
            pools.remove(0)
        }
    }
}
