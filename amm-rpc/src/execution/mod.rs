//! Execution-layer abstractions: value types that flow from route selection
//! through signing to on-chain submission.
//!
//! The primary exports are:
//! - [`UnsignedTx`] — minimal signer-ready transaction payload
//! - [`Currency`] — native-vs-token discrimination (collapses via `resolve`)
//! - [`CurrencyAmount`] — a wei-exact amount carrying its currency
//! - [`TradeType`] — exact-in vs exact-out trade direction
//! - [`BuildError`] / [`MissingAddr`] — typed build-layer errors
//! - [`ExecutionOptions`] — swap builder with recipient/deadline/approval options
//! - [`Recipient`] / [`Deadline`] / [`ApprovalMode`] — option enums
//! - [`resolve`] — edge helper: converts relative options to absolutes
//! - [`ChainConfig`] / [`Routers`] — per-chain router and sentinel address config
//! - [`Route`] — quote-to-build bridge (ordered hop sequence + fee tiers)
//! - [`PreparedSwap`] — fully-built swap output (tx + approval + bounds)
//! - [`ApprovalRequirement`] — ERC-20 allowance descriptor with reset-first flag
//! - [`Executable`] — sealed trait for pools that can encode swap transactions
//! - [`as_executable`] — dispatch: recovers a pool's encoder by concrete type

pub mod config;
pub mod error;
pub mod executable;
pub mod options;
pub mod prepared;
pub mod protocols;
pub mod types;

pub use config::{ChainConfig, Routers};
pub use error::{BuildError, MissingAddr};
pub use executable::{Executable, as_executable};
pub use options::{ApprovalMode, Deadline, ExecutionOptions, Recipient, resolve};
pub use prepared::{ApprovalRequirement, PreparedSwap, Route};
pub use types::{Currency, CurrencyAmount, TradeType, UnsignedTx};
