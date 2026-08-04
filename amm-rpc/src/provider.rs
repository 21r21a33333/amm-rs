//! Read-only HTTP provider construction.

use alloy::providers::RootProvider;
use alloy::transports::http::reqwest::Url;

use crate::error::RpcError;

/// The provider type used across the crate: a plain Ethereum-network HTTP root
/// provider, sufficient for reads (`eth_call`, block height). Cheap to clone —
/// reference-counted internally.
pub type EthProvider = RootProvider;

/// Build a read-only HTTP provider for `rpc_url`.
///
/// Fails with [`RpcError::Transport`] if `rpc_url` is not a valid URL. This does
/// not make a network call; a bad host surfaces only on first use.
pub fn make_provider(rpc_url: &str) -> Result<EthProvider, RpcError> {
    let url: Url = rpc_url
        .parse()
        .map_err(|_| RpcError::Transport(format!("invalid rpc url: {rpc_url}")))?;
    Ok(RootProvider::new_http(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_malformed_url() {
        assert!(matches!(
            make_provider("not a url"),
            Err(RpcError::Transport(_))
        ));
    }

    #[test]
    fn accepts_a_well_formed_url() {
        assert!(make_provider("https://example.com").is_ok());
    }
}
