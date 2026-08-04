//! Per-protocol on-chain state sources. Each submodule fetches and decodes one
//! AMM family's state into quotable `amm-core` pools.

pub mod aerodrome;
#[cfg(feature = "curve")]
pub mod curve;
pub mod uniswap_v2;
