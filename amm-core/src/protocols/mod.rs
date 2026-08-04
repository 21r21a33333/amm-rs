//! Per-protocol pure quoters.
//!
//! Each submodule adapts one AMM family to the core [`Pool`](crate::traits::pool)
//! trait, wei-exact against that protocol's on-chain contract. Quoters are pure:
//! they hold a snapshot of pool state and do no I/O (state fetching lives in the
//! separate `amm-rpc` crate).

#[cfg(any(feature = "uniswap-v2", feature = "uniswap-v3"))]
pub mod uniswap;
