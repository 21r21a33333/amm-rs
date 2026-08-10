//! Universal Router command accumulator (`RoutePlanner`).
//!
//! Assembles `(command_byte, ABI-encoded input)` pairs and encodes them into an
//! `execute(bytes commands, bytes[] inputs, uint256 deadline)` call understood by
//! Uniswap's Universal Router (selector `0x3593564c`).
//!
//! The constants below are the full canonical Universal Router command set.
//! `WRAP_ETH`, `UNWRAP_WETH`, `SWEEP`, and `PERMIT2_PERMIT` are forward-looking:
//! they are not emitted by the current single-hop encoders (only `V4_SWAP` is used
//! today — V4 native paths use `TAKE_ALL` on first-class `address(0)` rather than
//! wrap/unwrap/sweep), but will be used by future multi-command routes (in-stream
//! Permit2 permits, and WETH-pool wrap/unwrap/sweep cases).

use alloy::primitives::{Bytes, U256};
use alloy::{sol, sol_types::SolCall};

/// Universal Router command byte: Uniswap V4 swap.
pub const V4_SWAP: u8 = 0x10;

/// Universal Router command byte: wrap native ETH into WETH.
///
/// Forward-looking — not emitted by the current single-hop encoders; reserved for
/// future multi-command routes that target WETH pools.
pub const WRAP_ETH: u8 = 0x0b;

/// Universal Router command byte: unwrap WETH back to native ETH.
///
/// Forward-looking — not emitted by the current single-hop encoders; reserved for
/// future multi-command routes that target WETH pools.
pub const UNWRAP_WETH: u8 = 0x0c;

/// Universal Router command byte: process a Permit2 `permit` approval.
///
/// Forward-looking — not emitted by the current single-hop encoders; reserved for
/// future multi-command routes that embed an in-stream Permit2 permit.
pub const PERMIT2_PERMIT: u8 = 0x0a;

/// Universal Router command byte: sweep a token to a recipient.
///
/// Forward-looking — not emitted by the current single-hop encoders; reserved for
/// future multi-command routes (e.g. post-unwrap dust sweep).
pub const SWEEP: u8 = 0x04;

sol! {
    /// Minimal ABI surface for the Uniswap Universal Router.
    interface IUniversalRouter {
        /// Execute a batch of commands in a single transaction.
        function execute(bytes commands, bytes[] inputs, uint256 deadline) external payable;
    }
}

/// Accumulates Universal Router `(command_byte, input_bytes)` pairs and encodes
/// them into an `execute(bytes commands, bytes[] inputs, uint256 deadline)` call.
///
/// # Example
/// ```rust
/// use amm_rpc::execution::route_planner::{RoutePlanner, V4_SWAP, SWEEP};
/// use alloy::primitives::Bytes;
///
/// let mut planner = RoutePlanner::new();
/// planner
///     .add(V4_SWAP, Bytes::from(vec![0xaa]))
///     .add(SWEEP, Bytes::from(vec![0xbb]));
/// let calldata = planner.encode(1_700_000_000);
/// assert_eq!(&calldata[..4], &[0x35, 0x93, 0x56, 0x4c]);
/// ```
pub struct RoutePlanner {
    /// Packed command bytes; one byte per queued command.
    commands: Vec<u8>,
    /// ABI-encoded inputs; parallel to `commands`.
    inputs: Vec<Bytes>,
}

impl RoutePlanner {
    /// Create an empty planner with no commands queued.
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            inputs: Vec::new(),
        }
    }

    /// Append one `command` byte and its ABI-encoded `input`.
    ///
    /// Returns `&mut Self` so calls can be chained.
    pub fn add(&mut self, command: u8, input: Bytes) -> &mut Self {
        self.commands.push(command);
        self.inputs.push(input);
        self
    }

    /// Encode all accumulated commands into an
    /// `execute(bytes commands, bytes[] inputs, uint256 deadline)` calldata blob.
    ///
    /// The returned [`Bytes`] starts with selector `0x3593564c`.
    pub fn encode(&self, deadline: u64) -> Bytes {
        IUniversalRouter::executeCall {
            commands: Bytes::from(self.commands.clone()),
            inputs: self.inputs.clone(),
            deadline: U256::from(deadline),
        }
        .abi_encode()
        .into()
    }
}

impl Default for RoutePlanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::SolCall;

    #[test]
    fn empty_encodes_execute_with_no_commands() {
        let out = RoutePlanner::new().encode(123);
        assert_eq!(&out[..4], &[0x35, 0x93, 0x56, 0x4c]);
        let decoded = IUniversalRouter::executeCall::abi_decode(&out).unwrap();
        assert!(decoded.commands.is_empty());
        assert!(decoded.inputs.is_empty());
        assert_eq!(decoded.deadline, U256::from(123u64));
    }

    #[test]
    fn add_accumulates_commands_and_inputs_in_order() {
        let bytes_a = Bytes::from(vec![0xaa, 0xbb]);
        let bytes_b = Bytes::from(vec![0xcc]);
        let mut planner = RoutePlanner::new();
        planner
            .add(V4_SWAP, bytes_a.clone())
            .add(SWEEP, bytes_b.clone());
        let out = planner.encode(999);
        let decoded = IUniversalRouter::executeCall::abi_decode(&out).unwrap();
        assert_eq!(decoded.commands, Bytes::from(vec![0x10, 0x04]));
        assert_eq!(decoded.inputs, vec![bytes_a, bytes_b]);
        assert_eq!(decoded.deadline, U256::from(999u64));
    }

    #[test]
    fn add_returns_self_for_chaining() {
        let a = Bytes::from(vec![0x01]);
        let b = Bytes::from(vec![0x02]);
        let mut planner = RoutePlanner::new();
        planner.add(WRAP_ETH, a).add(V4_SWAP, b);
        let out = planner.encode(0);
        let decoded = IUniversalRouter::executeCall::abi_decode(&out).unwrap();
        assert_eq!(decoded.commands, Bytes::from(vec![0x0b, 0x10]));
    }

    #[test]
    fn command_constants_have_expected_values() {
        assert_eq!(V4_SWAP, 0x10);
        assert_eq!(WRAP_ETH, 0x0b);
        assert_eq!(UNWRAP_WETH, 0x0c);
        assert_eq!(PERMIT2_PERMIT, 0x0a);
        assert_eq!(SWEEP, 0x04);
    }
}
