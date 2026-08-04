//! `amm-rpc` — optional `alloy`-backed on-chain state fetching for `amm-core` pools.
//!
//! This crate implements the [`StateSource`] trait: it discovers pools for a set
//! of assets and refreshes their on-chain state into quotable
//! `Box<dyn amm_core::traits::pool::Pool>` values. Consumers that already have
//! pool state (subgraph, DB, fixtures) can skip this crate entirely and
//! construct the `amm-core` pool structs directly.

pub mod error;
pub mod multicall;
pub mod protocols;
pub mod provider;
pub mod retry;
pub mod source;

pub use error::RpcError;
pub use provider::{EthProvider, make_provider};
pub use retry::retry_with_backoff;
pub use source::StateSource;
