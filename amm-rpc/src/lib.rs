//! `amm-rpc` — optional `alloy`-backed on-chain state fetching for `amm-core` pools.
//!
//! This crate implements the `StateSource` trait: it discovers pools for a set
//! of assets and refreshes their on-chain state into quotable
//! `Box<dyn amm_core::traits::Pool>` values. Consumers that already have pool
//! state (subgraph, DB, fixtures) can skip this crate entirely and construct
//! `amm-core` pool structs directly.
//!
//! Modules are populated task-by-task per the implementation plan.
