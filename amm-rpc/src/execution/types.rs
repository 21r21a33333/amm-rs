//! Execution-layer value types. `Currency` carries native-vs-token intent in the
//! type (no flag); `UnsignedTx` is the minimal signer-ready payload.

use alloy::primitives::{Address, Bytes, U256};
use amm_core::primitives::asset::{AssetId, ChainId};

/// A signer-ready transaction payload. Contains only the fields needed to
/// submit a swap on-chain; signing and nonce management are handled by the
/// caller.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct UnsignedTx {
    /// The chain this transaction targets.
    pub chain: ChainId,
    /// The contract to call (router / permit2 entry-point).
    pub to: Address,
    /// Encoded calldata.
    pub data: Bytes,
    /// Native-token value to attach (zero for ERC-20-only swaps).
    pub value: U256,
}

impl From<&UnsignedTx> for alloy::rpc::types::TransactionRequest {
    fn from(tx: &UnsignedTx) -> Self {
        use alloy::network::TransactionBuilder;
        alloy::rpc::types::TransactionRequest::default()
            .with_to(tx.to)
            .with_input(tx.data.clone())
            .with_value(tx.value)
            .with_chain_id(tx.chain.0)
    }
}

/// The input or output token in a trade. `Native` represents the chain's
/// native gas token (e.g. ETH); `Token` wraps a concrete [`AssetId`].
///
/// `resolve` collapses `Native` into a concrete WETH `AssetId` at quote time
/// so pool math never sees the native distinction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Currency {
    /// The chain's native gas token (ETH, MATIC, …).
    Native,
    /// A specific ERC-20 / wrapped token identified by [`AssetId`].
    Token(AssetId),
}

impl Currency {
    /// Returns `true` for the native gas token variant.
    pub fn is_native(&self) -> bool {
        matches!(self, Currency::Native)
    }

    /// Resolve to a concrete [`AssetId`], substituting `weth` for `Native`.
    /// Pool math operates on wrapped tokens; callers wrap/unwrap as needed.
    pub fn resolve(self, weth: AssetId) -> AssetId {
        match self {
            Currency::Native => weth,
            Currency::Token(a) => a,
        }
    }
}

impl core::fmt::Display for Currency {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Currency::Native => write!(f, "ETH"),
            Currency::Token(a) => write!(f, "{a}"),
        }
    }
}

/// A token amount carrying its currency. `raw` is always a wei-exact base-unit
/// integer; callers apply decimal scaling for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CurrencyAmount {
    /// The currency this amount is denominated in.
    pub currency: Currency,
    /// Raw base-unit (wei-exact) amount.
    pub raw: U256,
}

/// Whether the exact constraint is on the input or the output side of a trade.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TradeType {
    /// Exact amount in — output is a minimum.
    ExactIn,
    /// Exact amount out — input is a maximum.
    ExactOut,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    fn weth() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0xC0]))
    }

    #[test]
    fn native_resolves_to_weth_and_displays_as_eth() {
        let usdc = AssetId::new(ChainId(1), B256::left_padding_from(&[1]));
        assert!(Currency::Native.is_native());
        assert_eq!(Currency::Native.resolve(weth()), weth());
        assert_eq!(Currency::Token(usdc).resolve(weth()), usdc);
        assert_eq!(format!("{}", Currency::Native), "ETH");
    }

    #[test]
    fn token_displays_as_asset_id() {
        let usdc = AssetId::new(ChainId(1), B256::left_padding_from(&[1]));
        assert!(!Currency::Token(usdc).is_native());
        let displayed = format!("{}", Currency::Token(usdc));
        assert!(displayed.starts_with("1:0x"));
    }

    #[test]
    fn unsigned_tx_bridges_to_transaction_request() {
        let addr = Address::repeat_byte(1);
        let calldata = Bytes::from(vec![0xde, 0xad]);
        let tx = UnsignedTx {
            chain: ChainId(1),
            to: addr,
            data: calldata.clone(),
            value: U256::from(5u64),
        };
        let req: alloy::rpc::types::TransactionRequest = (&tx).into();
        assert_eq!(req.value, Some(U256::from(5u64)));
        assert_eq!(req.chain_id, Some(1));
        // `with_to` stores the address as TxKind::Call
        assert_eq!(req.to, Some(alloy::primitives::TxKind::Call(addr)));
        // `with_input` round-trips the calldata bytes exactly
        assert_eq!(req.input.input, Some(calldata));
    }

    #[test]
    fn currency_amount_hash_and_eq() {
        use std::collections::HashSet;
        let weth_asset = weth();
        let a = CurrencyAmount { currency: Currency::Token(weth_asset), raw: U256::from(100u64) };
        let b = CurrencyAmount { currency: Currency::Token(weth_asset), raw: U256::from(100u64) };
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }


}
