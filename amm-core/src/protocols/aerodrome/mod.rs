//! Aerodrome (Solidly-fork) quoters: `volatile` (constant product with the fee
//! removed from the input first), `stable` (the `x³y + y³x` invariant), and
//! `slipstream` (concentrated liquidity, sharing the protocol-agnostic tick
//! engine in [`super::concentrated`]).

pub mod slipstream;
pub mod stable;
pub mod volatile;
