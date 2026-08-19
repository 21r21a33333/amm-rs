//! Per-chain router and sentinel address configuration.
//!
//! [`ChainConfig`] carries everything the build layer needs to resolve a
//! concrete contract address for a given chain: the WETH asset, the native
//! sentinel, and the optional router addresses via [`Routers`].

use alloy::primitives::Address;
use amm_core::primitives::asset::{AssetId, ChainId};

use crate::execution::error::{BuildError, MissingAddr};

/// Optional Uniswap router contract addresses for a single chain.
///
/// All fields are `Option<Address>` because not every protocol is deployed on
/// every chain. Use [`Default`] to start with no addresses configured, then
/// fill in only the protocols you need.
///
/// `#[non_exhaustive]` allows new router variants to be added in future minor
/// releases without breaking existing struct-literal construction; callers must
/// use `..Default::default()` or builder methods.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Routers {
    /// Uniswap V2 router contract address.
    pub v2: Option<Address>,
    /// Uniswap V3 swap router contract address.
    pub v3: Option<Address>,
    /// Uniswap Universal Router contract address.
    pub universal: Option<Address>,
    /// Uniswap V4 PoolManager contract address.
    pub pool_manager: Option<Address>,
    /// Permit2 contract address.
    pub permit2: Option<Address>,
    /// Slipstream router contract address.
    pub slipstream: Option<Address>,
    /// Aerodrome (Solidly) router contract address.
    pub aerodrome: Option<Address>,
    /// Aerodrome (Solidly) factory contract address.
    pub aerodrome_factory: Option<Address>,
    /// Curve router (CurveRouterNG) contract address.
    pub curve: Option<Address>,
}

/// Per-chain address configuration required by the swap build layer.
///
/// Holds the WETH asset identity, a native sentinel address (used as a
/// placeholder for the native gas token in protocols that expect an address),
/// and the optional [`Routers`] set.
///
/// Construct with [`ChainConfig::new`] and customise with the `with_*` builder
/// methods; the `#[non_exhaustive]` attribute prevents direct struct-literal
/// construction so new fields can be added without breaking callers.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ChainConfig {
    /// The chain this configuration applies to.
    pub chain: ChainId,
    /// Wrapped-native asset identity (WETH, WMATIC, …) used by pool math.
    pub weth: AssetId,
    /// Address used as the native-token sentinel in protocols that require an
    /// address placeholder for ETH/native (e.g. `0x000…000` or
    /// `0xEeee…EEeE`). Defaults to [`Address::ZERO`].
    pub native_sentinel: Address,
    /// Optional router contract addresses.
    pub routers: Routers,
}

impl ChainConfig {
    /// Construct a minimal [`ChainConfig`] for `chain` with the given `weth`
    /// asset identity.
    ///
    /// Defaults:
    /// - `native_sentinel = Address::ZERO`
    /// - `routers = Routers::default()` (all `None`)
    pub fn new(chain: ChainId, weth: AssetId) -> Self {
        Self {
            chain,
            weth,
            native_sentinel: Address::ZERO,
            routers: Routers::default(),
        }
    }

    /// Override the router address set.
    pub fn with_routers(mut self, routers: Routers) -> Self {
        self.routers = routers;
        self
    }

    /// Override the native sentinel address.
    pub fn with_native_sentinel(mut self, sentinel: Address) -> Self {
        self.native_sentinel = sentinel;
        self
    }

    /// Return the Uniswap V2 router address, or a typed error if unconfigured.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::MissingChainConfig`] with
    /// `what = MissingAddr::V2Router` when no V2 router address has been set.
    pub fn router_v2(&self) -> Result<Address, BuildError> {
        self.routers.v2.ok_or(BuildError::MissingChainConfig {
            chain: self.chain,
            what: MissingAddr::V2Router,
        })
    }

    /// Return the Uniswap V3 router address, or a typed error if unconfigured.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::MissingChainConfig`] with
    /// `what = MissingAddr::V3Router` when no V3 router address has been set.
    pub fn router_v3(&self) -> Result<Address, BuildError> {
        self.routers.v3.ok_or(BuildError::MissingChainConfig {
            chain: self.chain,
            what: MissingAddr::V3Router,
        })
    }

    /// Return the Slipstream router address, or a typed error if unconfigured.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::MissingChainConfig`] with
    /// `what = MissingAddr::SlipstreamRouter` when no Slipstream router address has been set.
    pub fn router_slipstream(&self) -> Result<Address, BuildError> {
        self.routers
            .slipstream
            .ok_or(BuildError::MissingChainConfig {
                chain: self.chain,
                what: MissingAddr::SlipstreamRouter,
            })
    }

    /// Return the Aerodrome (Solidly) router address, or a typed error if unconfigured.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::MissingChainConfig`] with
    /// `what = MissingAddr::AerodromeRouter` when no Aerodrome router address has been set.
    pub fn router_aerodrome(&self) -> Result<Address, BuildError> {
        self.routers
            .aerodrome
            .ok_or(BuildError::MissingChainConfig {
                chain: self.chain,
                what: MissingAddr::AerodromeRouter,
            })
    }

    /// Return the Aerodrome (Solidly) factory address, or a typed error if unconfigured.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::MissingChainConfig`] with
    /// `what = MissingAddr::AerodromeFactory` when no Aerodrome factory address has been set.
    pub fn aerodrome_factory(&self) -> Result<Address, BuildError> {
        self.routers
            .aerodrome_factory
            .ok_or(BuildError::MissingChainConfig {
                chain: self.chain,
                what: MissingAddr::AerodromeFactory,
            })
    }

    /// Return the Curve router (CurveRouterNG) address, or a typed error if unconfigured.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::MissingChainConfig`] with
    /// `what = MissingAddr::CurveRouter` when no Curve router address has been set.
    pub fn router_curve(&self) -> Result<Address, BuildError> {
        self.routers.curve.ok_or(BuildError::MissingChainConfig {
            chain: self.chain,
            what: MissingAddr::CurveRouter,
        })
    }

    /// Return the Uniswap Universal Router address, or a typed error if unconfigured.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::MissingChainConfig`] with
    /// `what = MissingAddr::UniversalRouter` when no Universal Router address has been set.
    pub fn router_universal(&self) -> Result<Address, BuildError> {
        self.routers
            .universal
            .ok_or(BuildError::MissingChainConfig {
                chain: self.chain,
                what: MissingAddr::UniversalRouter,
            })
    }

    /// Return the Permit2 contract address, or a typed error if unconfigured.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::MissingChainConfig`] with
    /// `what = MissingAddr::Permit2` when no Permit2 address has been set.
    pub fn permit2(&self) -> Result<Address, BuildError> {
        self.routers.permit2.ok_or(BuildError::MissingChainConfig {
            chain: self.chain,
            what: MissingAddr::Permit2,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;

    use super::*;

    fn test_weth() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0xC0]))
    }

    #[test]
    fn native_sentinel_default_is_zero() {
        let cfg = ChainConfig::new(ChainId(1), test_weth());
        assert_eq!(cfg.native_sentinel, Address::ZERO);
    }

    #[test]
    fn router_v2_missing_returns_missing_chain_config() {
        let cfg = ChainConfig::new(ChainId(1), test_weth());
        let err = cfg.router_v2().unwrap_err();
        assert!(
            matches!(
                err,
                BuildError::MissingChainConfig {
                    what: MissingAddr::V2Router,
                    ..
                }
            ),
            "expected MissingChainConfig {{ what: V2Router, .. }}, got: {err:?}"
        );
    }

    #[test]
    fn router_v2_present_returns_ok_address() {
        let addr = Address::repeat_byte(0xAB);
        let cfg = ChainConfig::new(ChainId(1), test_weth()).with_routers(Routers {
            v2: Some(addr),
            ..Default::default()
        });
        assert_eq!(cfg.router_v2(), Ok(addr));
    }

    #[test]
    fn router_v3_missing_returns_missing_chain_config() {
        let cfg = ChainConfig::new(ChainId(1), test_weth());
        let err = cfg.router_v3().unwrap_err();
        assert!(
            matches!(
                err,
                BuildError::MissingChainConfig {
                    what: MissingAddr::V3Router,
                    ..
                }
            ),
            "expected MissingChainConfig {{ what: V3Router, .. }}, got: {err:?}"
        );
    }

    #[test]
    fn router_v3_present_returns_ok_address() {
        let addr = Address::repeat_byte(0xCD);
        let cfg = ChainConfig::new(ChainId(1), test_weth()).with_routers(Routers {
            v3: Some(addr),
            ..Default::default()
        });
        assert_eq!(cfg.router_v3(), Ok(addr));
    }

    #[test]
    fn router_slipstream_missing_returns_missing_chain_config() {
        let cfg = ChainConfig::new(ChainId(1), test_weth());
        let err = cfg.router_slipstream().unwrap_err();
        assert!(
            matches!(
                err,
                BuildError::MissingChainConfig {
                    what: MissingAddr::SlipstreamRouter,
                    ..
                }
            ),
            "expected MissingChainConfig {{ what: SlipstreamRouter, .. }}, got: {err:?}"
        );
    }

    #[test]
    fn router_slipstream_present_returns_ok_address() {
        let addr = Address::repeat_byte(0xEF);
        let cfg = ChainConfig::new(ChainId(1), test_weth()).with_routers(Routers {
            slipstream: Some(addr),
            ..Default::default()
        });
        assert_eq!(cfg.router_slipstream(), Ok(addr));
    }

    #[test]
    fn router_aerodrome_missing_returns_missing_chain_config() {
        let cfg = ChainConfig::new(ChainId(1), test_weth());
        let err = cfg.router_aerodrome().unwrap_err();
        assert!(
            matches!(
                err,
                BuildError::MissingChainConfig {
                    what: MissingAddr::AerodromeRouter,
                    ..
                }
            ),
            "expected MissingChainConfig {{ what: AerodromeRouter, .. }}, got: {err:?}"
        );
    }

    #[test]
    fn router_aerodrome_present_returns_ok_address() {
        let addr = Address::repeat_byte(0x12);
        let cfg = ChainConfig::new(ChainId(1), test_weth()).with_routers(Routers {
            aerodrome: Some(addr),
            ..Default::default()
        });
        assert_eq!(cfg.router_aerodrome(), Ok(addr));
    }

    #[test]
    fn aerodrome_factory_missing_returns_missing_chain_config() {
        let cfg = ChainConfig::new(ChainId(1), test_weth());
        let err = cfg.aerodrome_factory().unwrap_err();
        assert!(
            matches!(
                err,
                BuildError::MissingChainConfig {
                    what: MissingAddr::AerodromeFactory,
                    ..
                }
            ),
            "expected MissingChainConfig {{ what: AerodromeFactory, .. }}, got: {err:?}"
        );
    }

    #[test]
    fn aerodrome_factory_present_returns_ok_address() {
        let f = Address::repeat_byte(0x42);
        let cfg = ChainConfig::new(ChainId(8453), test_weth()).with_routers(Routers {
            aerodrome_factory: Some(f),
            ..Default::default()
        });
        assert_eq!(cfg.aerodrome_factory(), Ok(f));
    }

    #[test]
    fn router_universal_missing_returns_missing_chain_config() {
        let cfg = ChainConfig::new(ChainId(1), test_weth());
        let err = cfg.router_universal().unwrap_err();
        assert!(
            matches!(
                err,
                BuildError::MissingChainConfig {
                    what: MissingAddr::UniversalRouter,
                    ..
                }
            ),
            "expected MissingChainConfig {{ what: UniversalRouter, .. }}, got: {err:?}"
        );
    }

    #[test]
    fn router_universal_present_returns_ok_address() {
        let addr = Address::repeat_byte(0x34);
        let cfg = ChainConfig::new(ChainId(1), test_weth()).with_routers(Routers {
            universal: Some(addr),
            ..Default::default()
        });
        assert_eq!(cfg.router_universal(), Ok(addr));
    }

    #[test]
    fn permit2_missing_returns_missing_chain_config() {
        let cfg = ChainConfig::new(ChainId(1), test_weth());
        let err = cfg.permit2().unwrap_err();
        assert!(
            matches!(
                err,
                BuildError::MissingChainConfig {
                    what: MissingAddr::Permit2,
                    ..
                }
            ),
            "expected MissingChainConfig {{ what: Permit2, .. }}, got: {err:?}"
        );
    }

    #[test]
    fn permit2_present_returns_ok_address() {
        let addr = Address::repeat_byte(0x56);
        let cfg = ChainConfig::new(ChainId(1), test_weth()).with_routers(Routers {
            permit2: Some(addr),
            ..Default::default()
        });
        assert_eq!(cfg.permit2(), Ok(addr));
    }
}
