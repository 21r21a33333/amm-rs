//! Aerodrome (Solidly-fork) quoters: `volatile` (constant product with the fee
//! removed from the input first), `stable` (the `x³y + y³x` invariant), and
//! `slipstream` (concentrated liquidity, sharing the protocol-agnostic tick
//! engine in [`super::concentrated`]).

use alloy_primitives::{Address, address};

pub mod slipstream;
pub mod stable;
pub mod volatile;

/// Canonical Aerodrome `PoolFactory` on Base. Aerodrome's Solidly router takes
/// a per-route `factory` address; all first-party pools share this factory.
pub const BASE_POOL_FACTORY: Address = address!("0x420DD381b31aEf6683db6B902084cB0FFECe40Da");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_pool_factory_is_canonical() {
        assert_eq!(
            BASE_POOL_FACTORY,
            address!("0x420DD381b31aEf6683db6B902084cB0FFECe40Da"),
        );
    }
}
