//! Ready-made [`ChainConfig`] presets for the chains this library supports.
//!
//! Building calldata requires a [`ChainConfig`] populated with the correct
//! router/factory addresses for the target chain. These presets bundle the
//! verified mainnet and Base addresses so consumers don't have to hardcode them.
//!
//! ```
//! let cfg = amm_rpc::execution::chains::ethereum();
//! assert!(cfg.router_universal().is_ok());
//! ```
//!
//! Addresses are the same ones the crate's fork tests run against.

use alloy::primitives::{Address, address};
use amm_core::primitives::asset::{AssetId, ChainId};

use crate::execution::config::{ChainConfig, Routers};

// ── Ethereum mainnet (chain id 1) ──────────────────────────────────────────────

/// WETH on Ethereum mainnet.
const WETH_MAINNET: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Uniswap Universal Router (mainnet).
const UNIVERSAL_ROUTER_MAINNET: Address = address!("0x66a9893cc07d91d95644aedd05d03f95e1dba8af");
/// Permit2 (canonical, all chains).
const PERMIT2: Address = address!("0x000000000022D473030F116dDEE9F6B43aC78BA3");
/// Uniswap V2 router (mainnet).
const V2_ROUTER_MAINNET: Address = address!("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
/// Uniswap V3 SwapRouter02 (mainnet).
const V3_ROUTER_MAINNET: Address = address!("0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45");
/// Uniswap V4 PoolManager (mainnet).
const V4_MANAGER_MAINNET: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
/// CurveRouterNG v1.2 (mainnet).
const CURVE_ROUTER_MAINNET: Address = address!("0x45312ea0eFf7E09C83CBE249fa1d7598c4C8cd4e");

// ── Base (chain id 8453) ───────────────────────────────────────────────────────

/// WETH on Base.
const WETH_BASE: Address = address!("0x4200000000000000000000000000000000000006");
/// Uniswap Universal Router (Base).
const UNIVERSAL_ROUTER_BASE: Address = address!("0x6fF5693b99212Da76ad316178A184AB56D299b43");
/// Uniswap V3 SwapRouter02 (Base).
const V3_ROUTER_BASE: Address = address!("0x2626664c2603336E57B271c5C0b26F421741e481");
/// Aerodrome (Solidly) router (Base).
const AERODROME_ROUTER_BASE: Address = address!("0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");
/// Aerodrome Slipstream SwapRouter (Base).
const SLIPSTREAM_ROUTER_BASE: Address = address!("0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5");

/// Wrap `addr` as the WETH [`AssetId`] for `chain`.
fn weth_asset(chain: ChainId, addr: Address) -> AssetId {
    AssetId::new(chain, addr.into_word())
}

/// [`ChainConfig`] for **Ethereum mainnet** (Uniswap V2/V3/V4 + Curve).
#[must_use]
pub fn ethereum() -> ChainConfig {
    let chain = ChainId(1);
    ChainConfig::new(chain, weth_asset(chain, WETH_MAINNET)).with_routers(Routers {
        v2: Some(V2_ROUTER_MAINNET),
        v3: Some(V3_ROUTER_MAINNET),
        universal: Some(UNIVERSAL_ROUTER_MAINNET),
        pool_manager: Some(V4_MANAGER_MAINNET),
        permit2: Some(PERMIT2),
        curve: Some(CURVE_ROUTER_MAINNET),
        ..Default::default()
    })
}

/// [`ChainConfig`] for **Base** (Uniswap V2/V3/V4 + Aerodrome + Slipstream).
#[must_use]
pub fn base() -> ChainConfig {
    let chain = ChainId(8453);
    ChainConfig::new(chain, weth_asset(chain, WETH_BASE)).with_routers(Routers {
        v3: Some(V3_ROUTER_BASE),
        universal: Some(UNIVERSAL_ROUTER_BASE),
        permit2: Some(PERMIT2),
        aerodrome: Some(AERODROME_ROUTER_BASE),
        aerodrome_factory: Some(amm_core::protocols::aerodrome::BASE_POOL_FACTORY),
        slipstream: Some(SLIPSTREAM_ROUTER_BASE),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ethereum_preset_has_the_uniswap_and_curve_routers() {
        let cfg = ethereum();
        assert_eq!(cfg.chain, ChainId(1));
        assert!(cfg.router_universal().is_ok());
        assert!(cfg.router_v2().is_ok());
        assert!(cfg.router_v3().is_ok());
        assert!(cfg.permit2().is_ok());
        assert!(cfg.router_curve().is_ok());
    }

    #[test]
    fn base_preset_has_the_aerodrome_and_slipstream_routers() {
        let cfg = base();
        assert_eq!(cfg.chain, ChainId(8453));
        assert!(cfg.router_aerodrome().is_ok());
        assert!(cfg.aerodrome_factory().is_ok());
        assert!(cfg.router_slipstream().is_ok());
        assert!(cfg.router_universal().is_ok());
    }
}
