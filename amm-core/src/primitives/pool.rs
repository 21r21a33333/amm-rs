//! Pool identity and descriptors: [`PoolId`], [`ExchangeId`], [`PoolKey`], and
//! the [`PoolKind`] tag.

use crate::primitives::asset::{AssetId, ChainId};
use crate::primitives::ratio::Bps;

/// A stable, globally-unique pool identifier (e.g. `"1:univ3:0xabc…"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PoolId(String);

impl PoolId {
    /// Wrap a string identifier.
    pub fn new(s: &str) -> Self {
        Self(s.to_string())
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A protocol-family identifier (e.g. `"uniswap-v3"`, `"aerodrome"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExchangeId(String);

impl ExchangeId {
    /// Wrap a string identifier.
    pub fn new(s: &str) -> Self {
        Self(s.to_string())
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A descriptor for a pool discovered on-chain, before its state is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PoolKey {
    /// The protocol family this pool belongs to.
    pub exchange: ExchangeId,
    /// The chain the pool is deployed on.
    pub chain: ChainId,
    /// The pool's on-chain address (hex string; chain-agnostic).
    pub address: String,
    /// The assets the pool trades.
    pub assets: Vec<AssetId>,
    /// The pool's swap fee in basis points, if known/fixed.
    pub fee_bps: Option<Bps>,
}

/// The protocol family + variant of a pool. A display/introspection tag only —
/// dispatch is via the `Pool` trait, not this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum PoolKind {
    /// Uniswap V2 (and V2 forks): constant product.
    UniswapV2,
    /// Uniswap V3: concentrated liquidity.
    UniswapV3,
    /// Uniswap V4: singleton concentrated liquidity.
    UniswapV4,
    /// Curve StableSwap.
    CurveStable,
    /// Curve CryptoSwap.
    CurveCrypto,
    /// Aerodrome/Velodrome volatile (vAMM).
    AerodromeVolatile,
    /// Aerodrome/Velodrome stable (sAMM).
    AerodromeStable,
    /// Aerodrome Slipstream: concentrated liquidity.
    Slipstream,
}
