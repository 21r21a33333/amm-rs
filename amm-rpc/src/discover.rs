//! Shared pool-discovery helpers.
//!
//! Factory-based discovery enumerates the pools connecting a token set: for
//! every unordered pair (× each fee tier / tick spacing / stable flag), ask the
//! factory for the pool address and keep the non-zero answers. Each protocol's
//! source builds its own factory calls; these helpers cover the parts common to
//! all of them — pair enumeration and decoding a factory's address return.

use alloy::primitives::{Address, B256};
use amm_core::primitives::asset::AssetId;

use crate::multicall::CallResult;

/// Every unordered pair of `assets`, each sorted `(token0, token1)` by token
/// address — the order factories key their pools by. A `B256` left-padded
/// address sorts identically to the `Address` it holds.
pub(crate) fn sorted_pairs(assets: &[AssetId]) -> Vec<(AssetId, AssetId)> {
    let mut pairs = Vec::new();
    for i in 0..assets.len() {
        for j in (i + 1)..assets.len() {
            let (a, b) = (assets[i], assets[j]);
            match a.token < b.token {
                true => pairs.push((a, b)),
                false => pairs.push((b, a)),
            }
        }
    }
    pairs
}

/// The `Address` for a token, taken from the low 20 bytes of its slot.
pub(crate) fn asset_address(asset: &AssetId) -> Address {
    Address::from_word(asset.token)
}

/// Decode a factory address return (a single right-aligned 32-byte word) into a
/// non-zero `Address`, or `None` if the call reverted or returned the zero
/// address (no such pool).
pub(crate) fn decode_pool_address(result: &CallResult) -> Option<Address> {
    if !result.success || result.return_data.len() < 32 {
        return None;
    }
    let addr = Address::from_word(B256::from_slice(&result.return_data[..32]));
    match addr.is_zero() {
        true => None,
        false => Some(addr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amm_core::primitives::asset::ChainId;

    fn asset(byte: u8) -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[byte]))
    }

    #[test]
    fn sorted_pairs_enumerates_unordered_and_sorts_each() {
        let pairs = sorted_pairs(&[asset(0x03), asset(0x01), asset(0x02)]);
        // 3 assets -> 3 unordered pairs, each (low, high) by address.
        assert_eq!(pairs.len(), 3);
        for (a, b) in pairs {
            assert!(a.token < b.token, "each pair must be address-sorted");
        }
    }

    #[test]
    fn decode_pool_address_rejects_zero_and_reverts() {
        let zero = CallResult {
            success: true,
            return_data: B256::ZERO.to_vec().into(),
        };
        assert!(decode_pool_address(&zero).is_none());

        let reverted = CallResult {
            success: false,
            return_data: Default::default(),
        };
        assert!(decode_pool_address(&reverted).is_none());

        let addr = Address::from([0x11u8; 20]);
        let ok = CallResult {
            success: true,
            return_data: addr.into_word().to_vec().into(),
        };
        assert_eq!(decode_pool_address(&ok), Some(addr));
    }
}
