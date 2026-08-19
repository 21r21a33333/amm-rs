//! Pure test helpers and chain-config builders shared across all execution
//! matrix tests.
//!
//! Nothing here touches the network — every function is offline and
//! deterministic.  Fork-dependent helpers live in `support::fork`.
//!
//! Phase 1 is additive: the execution_*.rs files still carry their own local
//! copies of `asset`/`exec_opts`/`key2`.  The dead_code lint fires on the
//! items here until Phase 2 removes those locals and imports from `support::dsl`
//! instead.  The allow attribute below keeps clippy clean during Phase 1.
#![allow(dead_code)]

use alloy::primitives::{Address, address};
use amm_core::primitives::asset::{AssetId, ChainId};
use amm_core::primitives::pool::{ExchangeId, PoolKey};
use amm_core::primitives::ratio::Bps;
use amm_core::slippage::Slippage;
use amm_rpc::execution::{ChainConfig, Deadline, ExecutionOptions, Recipient, Routers};

// ── Mainnet infrastructure addresses ─────────────────────────────────────────

/// Uniswap Universal Router on Ethereum mainnet.
/// Source: `execution_v4.rs` line 43 — `const UR`.
pub const UR: Address = address!("0x66a9893cc07d91d95644aedd05d03f95e1dba8af");

/// Permit2 singleton on Ethereum mainnet.
/// Source: `execution_v4.rs` line 45 — `const PERMIT2`.
pub const PERMIT2: Address = address!("0x000000000022D473030F116dDEE9F6B43aC78BA3");

/// Uniswap V4 PoolManager on Ethereum mainnet.
/// Source: `execution_v4.rs` line 47 — `const MANAGER`.
pub const V4_MANAGER: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");

/// Uniswap V2 Router02 on Ethereum mainnet.
/// Source: `execution_v2.rs` line 36 — `const ROUTER`.
pub const V2_ROUTER: Address = address!("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D");

/// Uniswap V3 SwapRouter02 on Ethereum mainnet.
/// Source: `execution_v3.rs` line 39 — `const ROUTER`.
pub const V3_ROUTER: Address = address!("0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45");

/// WETH on Ethereum mainnet (wrapped native).
/// Source: `execution_v2.rs` line 40 — `const WETH_ADDR`.
pub const WETH_MAINNET: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// CurveRouterNG (v1.2) on Ethereum — the atomic multi-hop `exchange` router.
/// Verified on-chain to hold the 6-arg `exchange` selector 0xc872a3c5.
pub const CURVE_ROUTER: Address = address!("0x45312ea0eFf7E09C83CBE249fa1d7598c4C8cd4e");

// ── Base infrastructure addresses ─────────────────────────────────────────────

/// Aerodrome (Solidly) Router on Base.
/// Source: `execution_aerodrome.rs` line 40 — `const ROUTER`.
pub const AERODROME_ROUTER: Address = address!("0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");

/// Aerodrome (Solidly) pool factory on Base — reuse the library constant (DRY).
pub const AERODROME_FACTORY: Address = amm_core::protocols::aerodrome::BASE_POOL_FACTORY;

/// Aerodrome Slipstream SwapRouter on Base.
/// Source: `execution_slipstream.rs` line 39 — `const ROUTER`.
pub const SLIPSTREAM_ROUTER: Address = address!("0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5");

/// WETH on Base (wrapped native).
/// Source: `execution_aerodrome.rs` line 42 — `const WETH_ADDR`.
pub const WETH_BASE: Address = address!("0x4200000000000000000000000000000000000006");

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// Wrap a raw EVM address into the `AssetId` used by the pool layer.
///
/// Body copied verbatim from `execution_v2.rs` line 57 — byte-identical across
/// all six execution test files.
pub fn asset(chain: u64, token: Address) -> AssetId {
    AssetId::new(ChainId(chain), token.into_word())
}

/// Build a two-asset `PoolKey` with assets in address-sorted `[token0, token1]`
/// order, matching the Uniswap V2 pair convention.
///
/// Body copied verbatim from `execution_v2.rs` line 63.
pub fn key2(exchange: &str, chain: u64, pool: Address, a: Address, b: Address) -> PoolKey {
    let (t0, t1) = match a < b {
        true => (a, b),
        false => (b, a),
    };
    PoolKey {
        exchange: ExchangeId::new(exchange),
        chain: ChainId(chain),
        address: pool.to_string(),
        assets: vec![asset(chain, t0), asset(chain, t1)],
        fee_bps: None,
    }
}

/// Resolved execution options shared across all swap directions.
///
/// - 50 bps slippage
/// - Explicit recipient (`sender`)
/// - Absolute deadline (`u64::MAX / 2`) — no `resolve` call needed.
/// - `sender` recorded so receiver-less encoders (e.g. some Curve ABIs) can
///   verify delivery. Callers that then override the recipient to a distinct
///   address exercise the receiver-less rejection path.
pub fn exec_opts(sender: Address) -> ExecutionOptions {
    let mut opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))
        .with_recipient(Recipient::To(sender))
        .with_deadline(Deadline::AtTimestamp(u64::MAX / 2));
    opts.sender = Some(sender);
    opts
}

// ── Chain-config builders ─────────────────────────────────────────────────────

/// Mainnet `ChainConfig` wired with every router the matrix exercises
/// (Universal Router + Permit2 + V2/V3 routers + PoolManager), WETH as the
/// wrapped-native asset.
pub fn mainnet_chain_config() -> ChainConfig {
    let mut routers = Routers::default();
    routers.universal = Some(UR);
    routers.permit2 = Some(PERMIT2);
    routers.v2 = Some(V2_ROUTER);
    routers.v3 = Some(V3_ROUTER);
    routers.pool_manager = Some(V4_MANAGER);
    routers.curve = Some(CURVE_ROUTER);
    ChainConfig::new(ChainId(1), asset(1, WETH_MAINNET)).with_routers(routers)
}

/// Base `ChainConfig` for Aerodrome + Slipstream.
pub fn base_chain_config() -> ChainConfig {
    let mut routers = Routers::default();
    routers.aerodrome = Some(AERODROME_ROUTER);
    routers.aerodrome_factory = Some(AERODROME_FACTORY);
    routers.slipstream = Some(SLIPSTREAM_ROUTER);
    ChainConfig::new(ChainId(8453), asset(8453, WETH_BASE)).with_routers(routers)
}

// ── Offline unit tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_builds_chain_scoped_id() {
        let a = asset(1, Address::repeat_byte(0x11));
        assert_eq!(a.chain, ChainId(1));
        assert_eq!(Address::from_word(a.token), Address::repeat_byte(0x11));
    }

    #[test]
    fn mainnet_config_has_universal_router() {
        let cfg = mainnet_chain_config();
        assert_eq!(cfg.router_universal().unwrap(), UR);
        assert_eq!(cfg.permit2().unwrap(), PERMIT2);
    }
}
