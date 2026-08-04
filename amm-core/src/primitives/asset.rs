//! Token identity ([`AssetId`]) and amounts ([`AssetAmount`]).
//!
//! Identity is cheap (`Copy`) and chain-agnostic: a [`ChainId`] plus a 32-byte
//! address slot. Decimals are metadata ([`TokenMeta`]), deliberately *not* part
//! of identity — an `AssetId` can be built without first fetching metadata.

use alloc::string::String;
use core::fmt;
use core::str::FromStr;

use alloy_primitives::{B256, U256};

use crate::error::{ParseError, QuoteError};

/// A chain identifier (e.g. `1` = Ethereum mainnet, `8453` = Base).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChainId(pub u64);

/// A chain-agnostic token identity: a chain plus a 32-byte address slot.
///
/// EVM addresses are left-padded into the low 20 bytes; chains with 32-byte
/// addresses (e.g. Solana) use the full slot. `Copy` and `Hash`, so it is a
/// cheap key in routing/quoting loops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetId {
    /// The chain this token lives on.
    pub chain: ChainId,
    /// The 32-byte address slot (EVM addresses left-padded).
    pub token: B256,
}

impl AssetId {
    /// Construct an identity from a chain and a 32-byte token slot.
    pub const fn new(chain: ChainId, token: B256) -> Self {
        Self { chain, token }
    }
}

impl fmt::Display for AssetId {
    /// Canonical form `"<chain>:0x<64-hex>"` (full 32-byte slot, lower-case).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:0x{:x}", self.chain.0, self.token)
    }
}

impl FromStr for AssetId {
    type Err = ParseError;

    /// Parse `"<chain>:0x<hex>"`. The hex may be shorter than 32 bytes (e.g. a
    /// 20-byte EVM address); it is left-padded into the slot.
    fn from_str(s: &str) -> Result<Self, ParseError> {
        let (chain, addr) = s.split_once(':').ok_or(ParseError::AssetId)?;
        let chain = chain.parse::<u64>().map_err(|_| ParseError::AssetId)?;
        let hex = addr.strip_prefix("0x").unwrap_or(addr);
        let bytes = alloy_primitives::hex::decode(hex).map_err(|_| ParseError::AssetId)?;
        if bytes.len() > 32 {
            return Err(ParseError::AssetId);
        }
        Ok(Self {
            chain: ChainId(chain),
            token: B256::left_padding_from(&bytes),
        })
    }
}

/// An amount of a specific token: a wei-exact `raw` integer that carries its
/// [`AssetId`]. Mixing assets in arithmetic is a `Result` error, never a silent
/// wrong number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetAmount {
    /// The token this amount is denominated in.
    pub asset: AssetId,
    /// The raw base-unit (wei) amount.
    pub raw: U256,
}

impl AssetAmount {
    /// Construct from a raw base-unit amount.
    pub const fn new(asset: AssetId, raw: U256) -> Self {
        Self { asset, raw }
    }

    /// Parse a human decimal string (e.g. `"1.5"`) into a raw amount using
    /// `decimals`. Rejects more fractional digits than `decimals`.
    pub fn from_decimal(asset: AssetId, decimals: u8, s: &str) -> Result<Self, ParseError> {
        let s = s.trim();
        let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
        if (int_part.is_empty() && frac_part.is_empty())
            || frac_part.len() > decimals as usize
            || !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(ParseError::Decimal);
        }
        let scale = pow10(decimals as u32)?;
        let int_val = parse_u256(if int_part.is_empty() { "0" } else { int_part })?;
        let mut raw = int_val.checked_mul(scale).ok_or(ParseError::Overflow)?;
        if !frac_part.is_empty() {
            let frac_val = parse_u256(frac_part)?;
            let pad = pow10(decimals as u32 - frac_part.len() as u32)?;
            let scaled = frac_val.checked_mul(pad).ok_or(ParseError::Overflow)?;
            raw = raw.checked_add(scaled).ok_or(ParseError::Overflow)?;
        }
        Ok(Self { asset, raw })
    }

    /// Add two amounts of the *same* asset. `Err(AssetMismatch)` otherwise.
    pub fn try_add(self, other: Self) -> Result<Self, QuoteError> {
        self.same_asset(&other)?;
        let raw = self.raw.checked_add(other.raw).ok_or(QuoteError::Overflow)?;
        Ok(Self { asset: self.asset, raw })
    }

    /// Subtract two amounts of the *same* asset. `Err(AssetMismatch)` on a
    /// different asset, `Err(Overflow)` on underflow.
    pub fn try_sub(self, other: Self) -> Result<Self, QuoteError> {
        self.same_asset(&other)?;
        let raw = self.raw.checked_sub(other.raw).ok_or(QuoteError::Overflow)?;
        Ok(Self { asset: self.asset, raw })
    }

    fn same_asset(&self, other: &Self) -> Result<(), QuoteError> {
        match self.asset == other.asset {
            true => Ok(()),
            false => Err(QuoteError::AssetMismatch {
                expected: self.asset,
                got: other.asset,
            }),
        }
    }
}

/// Off-chain metadata for a token. Fetched separately from identity.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TokenMeta {
    /// Number of decimal places (e.g. 6 for USDC, 18 for WETH).
    pub decimals: u8,
    /// Human ticker symbol.
    pub symbol: String,
}

fn parse_u256(s: &str) -> Result<U256, ParseError> {
    s.parse::<U256>().map_err(|_| ParseError::Decimal)
}

fn pow10(exp: u32) -> Result<U256, ParseError> {
    U256::from(10u64).checked_pow(U256::from(exp)).ok_or(ParseError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(chain: u64, low: u8) -> AssetId {
        AssetId::new(ChainId(chain), B256::left_padding_from(&[low]))
    }

    #[test]
    fn asset_id_roundtrips_via_string() {
        let s = "1:0x000000000000000000000000000000000000000000000000000000000000abcd";
        let a: AssetId = s.parse().unwrap();
        assert_eq!(a.chain, ChainId(1));
        assert_eq!(a.to_string(), s);
    }

    #[test]
    fn asset_id_accepts_short_evm_address_and_canonicalizes() {
        let a: AssetId = "8453:0xabcd".parse().unwrap();
        assert_eq!(
            a.to_string(),
            "8453:0x000000000000000000000000000000000000000000000000000000000000abcd"
        );
    }

    #[test]
    fn asset_id_rejects_malformed() {
        assert!("ethereum".parse::<AssetId>().is_err());
        assert!("1:not-hex".parse::<AssetId>().is_err());
    }

    #[test]
    fn asset_amount_try_add_rejects_mismatch() {
        let x = AssetAmount::new(asset(1, 0xaa), U256::from(1u64));
        let y = AssetAmount::new(asset(1, 0xbb), U256::from(1u64));
        assert!(matches!(x.try_add(y), Err(QuoteError::AssetMismatch { .. })));
    }

    #[test]
    fn asset_amount_try_add_same_asset_sums() {
        let a = asset(1, 0xaa);
        let x = AssetAmount::new(a, U256::from(10u64));
        let y = AssetAmount::new(a, U256::from(5u64));
        assert_eq!(x.try_add(y).unwrap().raw, U256::from(15u64));
    }

    #[test]
    fn from_decimal_scales_by_decimals() {
        let amt = AssetAmount::from_decimal(asset(1, 0xaa), 6, "1.5").unwrap();
        assert_eq!(amt.raw, U256::from(1_500_000u64));
    }

    #[test]
    fn from_decimal_rejects_too_many_fractional_digits() {
        assert!(AssetAmount::from_decimal(asset(1, 0xaa), 2, "1.234").is_err());
    }
}
