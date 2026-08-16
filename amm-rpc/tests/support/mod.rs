//! Shared revm-fork test harness — SRP module split.
//!
//! - `fork`     : revm fork mechanics (fund / approve / submit / snapshot).
//! - `dsl`      : pure test helpers and chain-config builders (offline, no RPC).
//! - `env`      : environment-gated fork opening (skip tests when RPC URL unset).
//! - `case`     : matrix vocabulary (Case model and outcome enums).
//! - `fixtures` : central pool catalog + per-protocol `refresh` dispatch.
//! - `runner`   : `run_case` single-hop execution engine (Task 7).

mod case;
mod dsl;
mod env;
mod fixtures;
mod fork;
mod route;
mod runner;

#[allow(unused_imports)]
pub use case::*;
#[allow(unused_imports)]
pub use dsl::*;
#[allow(unused_imports)]
pub use env::*;
#[allow(unused_imports)]
pub use fixtures::*;
#[allow(unused_imports)]
pub use fork::{Fork, fork_at};
#[allow(unused_imports)]
pub use route::*;
#[allow(unused_imports)]
pub use runner::*;
