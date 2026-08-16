//! Environment-gated fork opening helpers — skippable tests for offline CI.
//!
//! When `$AMM_*_URL` env vars are unset, tests return early without
//! connecting to RPC; this allows offline CI to skip integration tests
//! without marking them as errors.  The [`open_fork`] function encodes
//! this pattern: return `None` (test skips gracefully).

use crate::support::fork_at;
use alloy::providers::{Provider, ProviderBuilder};
use amm_core::primitives::asset::ChainId;

use super::Fork;

/// Pinned fork block: `$AMM_FORK_BLOCK` if set and parseable, else `default`.
pub fn fork_block_or(default: u64) -> u64 {
    std::env::var("AMM_FORK_BLOCK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Open a revm fork from `$<env_var>` at `fork_block_or(default_block)`.
/// Returns `None` (test no-ops) when the env var is unset. The returned `u64`
/// is the resolved block.
pub async fn open_fork(
    env_var: &str,
    default_block: u64,
    chain: ChainId,
) -> Option<(Fork<impl Provider + Clone>, u64)> {
    let url = std::env::var(env_var).ok()?;
    let block = fork_block_or(default_block);
    let provider = ProviderBuilder::new().connect_http(url.parse().expect("invalid RPC url"));
    let fork = fork_at(&url, block, chain, provider).await;
    Some((fork, block))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn open_fork_returns_none_when_env_unset() {
        // Use a name no CI sets.
        assert!(
            open_fork("AMM_RPC_FORK_URL_DEFINITELY_UNSET", 1, ChainId(1))
                .await
                .is_none()
        );
    }

    #[test]
    fn fork_block_defaults_when_unset() {
        // Assumes AMM_FORK_BLOCK is unset in the unit-test environment.
        if std::env::var("AMM_FORK_BLOCK").is_err() {
            assert_eq!(fork_block_or(20_000_000), 20_000_000);
        }
    }
}
