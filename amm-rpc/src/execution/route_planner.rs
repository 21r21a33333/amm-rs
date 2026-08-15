//! Universal Router command accumulator (`RoutePlanner`).
//!
//! Assembles `(command_byte, ABI-encoded input)` pairs and encodes them into an
//! `execute(bytes commands, bytes[] inputs, uint256 deadline)` call understood by
//! Uniswap's Universal Router (selector `0x3593564c`).
//!
//! The constants below are the full canonical Universal Router command set.
//! `WRAP_ETH` and `UNWRAP_WETH` are emitted by the V4 encoder for WETH-currency
//! pools swapped with native ETH (wrap the input / unwrap the output). `SWEEP`
//! and `PERMIT2_PERMIT` are forward-looking — not emitted today, reserved for
//! future multi-command routes (in-stream Permit2 permits, post-unwrap dust
//! sweeps).

use alloy::primitives::{Address, Bytes, U256};
use alloy::{
    sol,
    sol_types::{SolCall, SolValue},
};

/// UR command: Uniswap V3 exact-in swap.
pub const V3_SWAP_EXACT_IN: u8 = 0x00;

/// UR command: Uniswap V3 exact-out swap.
pub const V3_SWAP_EXACT_OUT: u8 = 0x01;

/// UR command: Uniswap V2 exact-in swap.
pub const V2_SWAP_EXACT_IN: u8 = 0x08;

/// UR command: Uniswap V2 exact-out swap.
pub const V2_SWAP_EXACT_OUT: u8 = 0x09;

/// Universal Router command byte: Uniswap V4 swap.
pub const V4_SWAP: u8 = 0x10;

/// Universal Router command byte: wrap native ETH into WETH.
///
/// Emitted by the V4 encoder for a WETH-currency pool swapped with native ETH
/// input — the router wraps the caller's ETH before the swap settles WETH.
pub const WRAP_ETH: u8 = 0x0b;

/// Universal Router command byte: unwrap WETH back to native ETH.
///
/// Emitted by the V4 encoder for a WETH-currency pool with native-ETH output
/// (unwrap the taken WETH) and to return any exact-out wrap remainder as ETH.
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

/// Amount sentinel: use the router's entire balance of the input token.
///
/// Equals 2^255 (top bit set). Limbs are little-endian 64-bit words, so limb[3]
/// holds the most-significant word — setting it to `0x8000_0000_0000_0000` sets
/// exactly bit 255.
pub const CONTRACT_BALANCE: U256 = U256::from_limbs([0, 0, 0, 0x8000_0000_0000_0000]);

/// Recipient sentinel: the transaction sender (`address(1)`).
pub const MSG_SENDER: Address = Address::with_last_byte(1);

/// Recipient sentinel: the router itself (`address(2)`), holds intermediates mid-route.
pub const ADDRESS_THIS: Address = Address::with_last_byte(2);

/// Encode a Uniswap V3 multi-hop path: `token ‖ fee(u24 be) ‖ token ‖ …`.
///
/// `fees.len() == tokens.len() - 1`. When `reversed`, emit the path back to
/// front (exact-out requires the reversed path).
pub fn v3_path(tokens: &[Address], fees: &[u32], reversed: bool) -> Bytes {
    let mut out = Vec::with_capacity(tokens.len() * 20 + fees.len() * 3);
    let push = |out: &mut Vec<u8>, tok: &Address, fee: Option<u32>| {
        out.extend_from_slice(tok.as_slice());
        if let Some(f) = fee {
            out.extend_from_slice(&f.to_be_bytes()[1..4]); // low 3 bytes = u24
        }
    };
    match reversed {
        false => {
            for (i, tok) in tokens.iter().enumerate() {
                push(&mut out, tok, fees.get(i).copied());
            }
        }
        true => {
            for (i, tok) in tokens.iter().enumerate().rev() {
                let fee = i.checked_sub(1).and_then(|j| fees.get(j).copied());
                push(&mut out, tok, fee);
            }
        }
    }
    out.into()
}

/// V3 command input: `(recipient, amount, limit, path, payerIsUser)`. `amount`/
/// `limit` are amountIn/amountOutMin (exact-in) or amountOut/amountInMax (exact-out).
/// `_exact_in` is documentation-only — the caller selects swap direction via the
/// command byte (`V3_SWAP_EXACT_IN` / `V3_SWAP_EXACT_OUT`), not this flag.
pub fn v3_swap_input(
    _exact_in: bool,
    recipient: Address,
    amount: U256,
    limit: U256,
    path: Bytes,
    payer_is_user: bool,
) -> Bytes {
    <(Address, U256, U256, Bytes, bool)>::abi_encode_params(&(
        recipient,
        amount,
        limit,
        path,
        payer_is_user,
    ))
    .into()
}

/// V2 command input: `(recipient, amount, limit, address[] path, payerIsUser)`.
/// `_exact_in` is documentation-only — the caller selects swap direction via the
/// command byte (`V2_SWAP_EXACT_IN` / `V2_SWAP_EXACT_OUT`), not this flag.
pub fn v2_swap_input(
    _exact_in: bool,
    recipient: Address,
    amount: U256,
    limit: U256,
    path: Vec<Address>,
    payer_is_user: bool,
) -> Bytes {
    <(Address, U256, U256, Vec<Address>, bool)>::abi_encode_params(&(
        recipient,
        amount,
        limit,
        path,
        payer_is_user,
    ))
    .into()
}

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

    #[test]
    fn v2_v3_command_constants_have_expected_values() {
        assert_eq!(V3_SWAP_EXACT_IN, 0x00);
        assert_eq!(V3_SWAP_EXACT_OUT, 0x01);
        assert_eq!(V2_SWAP_EXACT_IN, 0x08);
        assert_eq!(V2_SWAP_EXACT_OUT, 0x09);
        // Sentinels match Uniswap v4-periphery ActionConstants.
        assert_eq!(ADDRESS_THIS, Address::with_last_byte(2));
        assert_eq!(MSG_SENDER, Address::with_last_byte(1));
        // CONTRACT_BALANCE == 2^255 (highest bit of U256).
        assert_eq!(CONTRACT_BALANCE, U256::from(1u8) << 255);
    }

    #[test]
    fn v3_path_encodes_forward_and_reversed() {
        let t0 = Address::with_last_byte(0xA0);
        let t1 = Address::with_last_byte(0xB1);
        let t2 = Address::with_last_byte(0xC2);
        let fwd = v3_path(&[t0, t1, t2], &[500, 3000], false);
        // 20 + 3 + 20 + 3 + 20 = 66 bytes
        assert_eq!(fwd.len(), 66);
        assert_eq!(&fwd[0..20], t0.as_slice());
        assert_eq!(&fwd[20..23], &[0x00, 0x01, 0xf4]); // 500 as u24 big-endian
        assert_eq!(&fwd[23..43], t1.as_slice());
        // reversed swaps token order and fee order
        let rev = v3_path(&[t0, t1, t2], &[500, 3000], true);
        assert_eq!(&rev[0..20], t2.as_slice());
        assert_eq!(&rev[20..23], &[0x00, 0x0b, 0xb8]); // 3000 as u24
    }

    #[test]
    fn v3_swap_input_round_trip() {
        let recipient = Address::with_last_byte(0x11);
        let amount = U256::from(1_000_000u64);
        let limit = U256::from(900_000u64);
        let path = Bytes::from(vec![0xaa, 0xbb, 0xcc]);
        let payer_is_user = true;

        let encoded = v3_swap_input(true, recipient, amount, limit, path.clone(), payer_is_user);
        let decoded = <(Address, U256, U256, Bytes, bool)>::abi_decode_params(&encoded)
            .expect("failed to decode V3 swap input");

        assert_eq!(decoded.0, recipient);
        assert_eq!(decoded.1, amount);
        assert_eq!(decoded.2, limit);
        assert_eq!(decoded.3, path);
        assert_eq!(decoded.4, payer_is_user);
    }

    #[test]
    fn v2_swap_input_round_trip() {
        let recipient = Address::with_last_byte(0x22);
        let amount = U256::from(2_000_000u64);
        let limit = U256::from(1_800_000u64);
        let path = vec![
            Address::with_last_byte(0xA0),
            Address::with_last_byte(0xA1),
            Address::with_last_byte(0xA2),
        ];
        let payer_is_user = false;

        let encoded = v2_swap_input(false, recipient, amount, limit, path.clone(), payer_is_user);
        let decoded = <(Address, U256, U256, Vec<Address>, bool)>::abi_decode_params(&encoded)
            .expect("failed to decode V2 swap input");

        assert_eq!(decoded.0, recipient);
        assert_eq!(decoded.1, amount);
        assert_eq!(decoded.2, limit);
        assert_eq!(decoded.3, path);
        assert_eq!(decoded.4, payer_is_user);
    }
}
