//! Per-protocol swap encoders.
//!
//! Each sub-module implements [`crate::execution::executable::Executable`] for
//! its pool type and provides the ABI-encoding machinery needed to produce a
//! valid [`crate::execution::prepared::PreparedSwap`].

pub mod aerodrome;
pub(crate) mod common;
pub mod slipstream;
pub(crate) mod swaprouter02;
pub mod uniswap_v2;
pub mod uniswap_v3;
