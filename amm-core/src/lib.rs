//! `amm-core` — wei-exact, `no_std`-capable AMM quoting primitives and traits.
//!
//! The crate exposes an open, object-safe [`Pool`](crate::traits) trait, typed
//! value objects that carry their token identity, and per-protocol pure
//! quoters. It has zero network dependencies; on-chain state fetching lives in
//! the separate `amm-rpc` crate.
//!
//! Modules are populated task-by-task per the implementation plan.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
