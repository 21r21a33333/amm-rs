//! The build-layer error: typed and total — no panics on caller input.

use amm_core::primitives::asset::{AssetId, ChainId};

/// Which contract address is absent for a given chain.
///
/// Kept `Copy` so it can be embedded in `BuildError` without cloning;
/// `#[non_exhaustive]` is intentionally absent here — exhaustive matching
/// is what callers need to fill in chain configs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingAddr {
    /// Uniswap V2 router contract.
    V2Router,
    /// Uniswap V3 swap router contract.
    V3Router,
    /// Uniswap Universal Router contract.
    UniversalRouter,
    /// Uniswap V4 PoolManager contract.
    PoolManager,
    /// Permit2 contract address.
    Permit2,
    /// Wrapped native token (WETH/WMATIC/…) contract.
    Weth,
}

/// All the ways a swap build step can fail before touching the network.
///
/// `#[non_exhaustive]` lets downstream code compile without covering variants
/// added in future minor releases. Add `#[source]`/`#[from]` to new variants
/// when a wrapped cause first appears (e.g. ABI or amm-core encoding error);
/// `PartialEq` must be dropped at that point unless the wrapped type is also
/// `PartialEq`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildError {
    /// The requested protocol has no registered swap encoder.
    #[error("protocol has no swap encoder")]
    UnsupportedProtocol,

    /// A required contract address is absent from the chain configuration.
    #[error("missing {what:?} for chain {}", chain.0)]
    MissingChainConfig {
        /// The chain whose config is incomplete.
        chain: ChainId,
        /// Which contract address is missing.
        what: MissingAddr,
    },

    /// Recipient was left as a placeholder; must be an explicit address.
    #[error("recipient must be resolved to an explicit address before building")]
    UnresolvedRecipient,

    /// Deadline was left as a relative offset; must be an absolute Unix timestamp.
    #[error("deadline must be an absolute timestamp before building")]
    UnresolvedDeadline,

    /// Both sides of the swap specified the native gas token.
    #[error("native intent used on both sides of the swap")]
    NativeMismatch,

    /// Neither asset belongs to the nominated pool.
    #[error("asset not in pool: {input} -> {output}")]
    AssetNotInPool {
        /// The swap input asset.
        input: AssetId,
        /// The swap output asset.
        output: AssetId,
    },

    /// A calldata encoding step produced a value that overflows its target type.
    #[error("numeric overflow encoding the swap")]
    Overflow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    #[test]
    fn missing_config_displays_chain_and_what() {
        let e = BuildError::MissingChainConfig {
            chain: ChainId(8453),
            what: MissingAddr::V2Router,
        };
        let msg = format!("{e}");
        assert!(
            msg.contains("V2Router") && msg.contains("8453"),
            "expected display to contain 'V2Router' and '8453', got: {msg:?}"
        );
    }

    #[test]
    fn asset_not_in_pool_displays_both_assets() {
        let input = AssetId::new(ChainId(1), B256::left_padding_from(&[1]));
        let output = AssetId::new(ChainId(1), B256::left_padding_from(&[2]));
        let e = BuildError::AssetNotInPool { input, output };
        let msg = format!("{e}");
        assert!(msg.contains("->"), "expected '->' in display, got: {msg:?}");
    }

    #[test]
    fn distinct_unit_variants_are_not_equal() {
        assert_ne!(BuildError::UnsupportedProtocol, BuildError::Overflow);
    }
}
