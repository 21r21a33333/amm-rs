//! `amm-core` — wei-exact AMM quoting primitives and traits.
//!
//! The crate exposes an open, object-safe [`Pool`](crate::traits) trait, typed
//! value objects that carry their token identity, and per-protocol pure
//! quoters. It has zero network dependencies; on-chain state fetching lives in
//! the separate `amm-rpc` crate.
//!
//! Protocol quoters are opt-in via Cargo features (`uniswap-v2`, `uniswap-v3`,
//! `uniswap-v4`, `curve`, `aerodrome`); the default build enables none.

pub mod error;
pub mod path;
pub mod primitives;
pub mod protocols;
pub mod slippage;
pub mod traits;
