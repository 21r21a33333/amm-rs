//! Matrix vocabulary: `Case` model and execution outcome enums.
//!
//! Consumed by the runner (Task 7) and Phases 2–3 (case synthesis, execution).

use alloy::primitives::U256;
use amm_core::primitives::asset::ChainId;
use amm_rpc::execution::routing::ExactOutPolicy;

/// Which token in the fixture is the input side of the trade.
///
/// `Forward` = token0 → token1; `Reverse` = token1 → token0.
/// The runner resolves this to concrete `in_token`/`out_token` at runtime.
#[allow(dead_code)]
pub enum Direction {
    /// Swap token0 → token1 (address-sorted first → second).
    Forward,
    /// Swap token1 → token0 (address-sorted second → first).
    Reverse,
}

#[allow(dead_code)]
/// One matrix row: fully describes an execution proof.
pub struct Case {
    pub name: &'static str,
    pub chain: ChainId,
    /// Fixture names in hop order; len 1 = single-hop, len N = multi-hop span.
    pub pools: &'static [&'static str],
    /// Which fixture token is the input side of the trade.
    ///
    /// `Forward` = token0 → token1; `Reverse` = token1 → token0.
    pub direction: Direction,
    /// Token addresses/decimals are resolved from the fixtures at runtime; the
    /// case only pins the trade shape and the edges.
    pub trade: Trade,
    pub native_in: bool,
    pub native_out: bool,
    pub recipient: RecipientKind,
    pub approval: ApprovalKind,
    pub expect: Expect,
}

#[allow(dead_code)]
pub enum Trade {
    ExactIn {
        amount_in: U256,
    },
    ExactOut {
        amount_out: U256,
        policy: ExactOutPolicy,
    },
}

#[allow(dead_code)]
pub enum RecipientKind {
    Sender,
    Distinct,
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum ApprovalKind {
    Erc20,
    Permit2,
    ZeroFirst,
}

/// The asserted on-chain outcome. No PPM tolerance — exactness only.
#[allow(dead_code)]
pub enum Expect {
    /// Exact-in: output delta == quoted output.
    WeiExact,
    /// Exact-out Strict: output == target, input <= max.
    Exact,
    /// Exact-out OrBetter: output >= target (designed to over-deliver).
    AtLeast,
    /// The builder must reject before producing calldata.
    RejectBuild(BuildErrorKind),
}

/// Coarse `BuildError` discriminant for negative cases (avoids matching on
/// fields the test doesn't care about).
#[allow(dead_code)]
#[derive(Debug)]
pub enum BuildErrorKind {
    UnsupportedExactOut,
    UnsupportedProtocol,
    NativeIntermediate,
}

#[allow(dead_code)]
impl Case {
    /// Number of pools in the route.
    pub fn hops(&self) -> usize {
        self.pools.len()
    }

    /// True if the route crosses more than one pool.
    pub fn is_multi_hop(&self) -> bool {
        self.pools.len() > 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_reports_hop_count() {
        let c = Case {
            name: "x",
            chain: ChainId(1),
            pools: &["a", "b"],
            direction: Direction::Forward,
            trade: Trade::ExactIn {
                amount_in: U256::from(1u64),
            },
            native_in: false,
            native_out: false,
            recipient: RecipientKind::Sender,
            approval: ApprovalKind::Erc20,
            expect: Expect::WeiExact,
        };
        assert_eq!(c.pools.len(), 2);
        assert!(c.is_multi_hop());
    }
}
