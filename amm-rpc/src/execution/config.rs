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
}
