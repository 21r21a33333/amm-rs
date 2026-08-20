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

// ── Crate-root re-exports (the types & traits every consumer touches) ────────
// Lets callers `use amm_core::{Pool, AssetId, Slippage, …}` instead of reaching
// into deep module paths. Calling `pool.quote(..)` or `pool.quote_exact_out(..)`
// requires the `Pool`/`ExactOut` traits in scope, so both are re-exported here.
pub use error::{ParseError, QuoteError};
pub use path::{Hop, quote_path, quote_path_amounts, quote_path_exact_out};
pub use primitives::asset::{AssetAmount, AssetId, ChainId, TokenMeta};
pub use primitives::pool::{ExchangeId, PoolId, PoolKey, PoolKind};
pub use primitives::price::Price;
pub use primitives::ratio::{Bps, Ratio, Rounding};
pub use slippage::Slippage;
pub use traits::exact_out::ExactOut;
pub use traits::introspect::Introspect;
pub use traits::pool::Pool;
pub use traits::pricing::Pricing;
