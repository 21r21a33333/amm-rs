//! `amm-rpc` — on-chain state fetching **and** swap calldata for `amm-core` pools.
//!
//! Two capabilities, usable independently:
//!
//! 1. **State fetching** — [`StateSource`] discovers pools for a set of assets
//!    and refreshes their on-chain state into quotable
//!    `Box<dyn amm_core::traits::pool::Pool>` values. Consumers that already
//!    hold pool state (subgraph, DB, fixtures) can skip this and build the
//!    `amm-core` pool structs directly.
//! 2. **Execution / calldata** — the [`execution`] module turns a quoted route
//!    into signed-ready transactions: [`execution::plan`] for multi-hop routes,
//!    or [`execution::as_executable`] + [`execution::Executable::build_swap`]
//!    for a single pool.
//!
//! # Quickstart (single-pool calldata)
//!
//! ```no_run
//! use amm_rpc::execution::{as_executable, ChainConfig, Currency, CurrencyAmount, Executable, ExecutionOptions};
//! use amm_rpc::{AssetAmount, Slippage, Bps};
//! # fn demo(pool: &dyn amm_core::traits::pool::Pool, cfg: &ChainConfig,
//! #         usdc: amm_rpc::AssetId, weth: amm_rpc::AssetId, quoted: &AssetAmount) -> Result<(), Box<dyn std::error::Error>> {
//! let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)));
//! let prepared = as_executable(pool)
//!     .ok_or("pool has no encoder")?
//!     .build_swap(cfg, CurrencyAmount { currency: Currency::Token(usdc), raw: quoted.raw },
//!                 Currency::Token(weth), quoted, &opts)?;
//! // prepared.tx — sign & send;  prepared.approval — grant first if `Some`.
//! # Ok(()) }
//! ```
//!
//! See `examples/build_calldata.rs` for an end-to-end refresh → quote → calldata flow.

mod discover;
pub mod error;
pub mod execution;
pub mod multicall;
pub mod protocols;
pub mod provider;
pub mod retry;
pub mod source;

pub use error::RpcError;
pub use provider::{EthProvider, make_provider};
pub use retry::retry_with_backoff;
pub use source::StateSource;

// ── Convenience re-exports of the `amm-core` types every consumer needs ──────
// So callers can `use amm_rpc::{AssetId, PoolKey, …}` instead of reaching into
// `amm_core::primitives::*`.
pub use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId, TokenMeta};
pub use amm_core::primitives::pool::{ExchangeId, PoolId, PoolKey, PoolKind};
pub use amm_core::primitives::ratio::Bps;
pub use amm_core::slippage::Slippage;
pub use amm_core::traits::exact_out::ExactOut;
pub use amm_core::traits::pool::Pool;
