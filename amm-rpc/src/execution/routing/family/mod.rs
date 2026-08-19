//! Router-family span builders.
//!
//! Each sub-module turns one same-router [`RouterSpan`](crate::execution::routing::RouterSpan)
//! into a single on-chain transaction, chaining every hop in the span through
//! that router's native multi-command surface (e.g. Uniswap's Universal Router
//! or the Aerodrome multi-hop router).

pub mod aerodrome;
#[cfg(feature = "curve")]
pub mod curve;
pub mod slipstream;
pub mod uniswap_ur;
