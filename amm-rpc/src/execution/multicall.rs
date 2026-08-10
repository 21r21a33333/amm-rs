//! SwapRouter02-family multicall assembly.

use alloy::primitives::{Bytes, U256};
use alloy::{sol, sol_types::SolCall};

sol! {
    interface IMulticall {
        function multicall(bytes[] data) external returns (bytes[] results);
        function multicall(uint256 deadline, bytes[] data) external returns (bytes[] results);
    }
}

/// Assemble SwapRouter02-family sub-calls. A single call is returned raw
/// (no multicall overhead); more than one is wrapped in `multicall(bytes[])`,
/// or `multicall(uint256 deadline, bytes[])` when `deadline` is `Some`.
pub fn encode_multicall(deadline: Option<u64>, calls: Vec<Bytes>) -> Bytes {
    match (deadline, calls.len()) {
        (None, 1) => calls.into_iter().next().expect("len==1"),
        (None, _) => IMulticall::multicall_0Call { data: calls }
            .abi_encode()
            .into(),
        (Some(d), _) => IMulticall::multicall_1Call {
            deadline: U256::from(d),
            data: calls,
        }
        .abi_encode()
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_call_no_deadline_passes_through_raw() {
        let inner = Bytes::from(vec![0x04, 0xe4, 0x5a, 0xaf, 0xAA]);
        assert_eq!(encode_multicall(None, vec![inner.clone()]), inner);
    }

    #[test]
    fn multi_call_no_deadline_wraps_in_multicall_bytes() {
        let a = Bytes::from(vec![0x11]);
        let b = Bytes::from(vec![0x22]);
        let out = encode_multicall(None, vec![a.clone(), b.clone()]);
        assert_eq!(&out[..4], &[0xac, 0x96, 0x50, 0xd8]); // multicall(bytes[])
        let decoded = IMulticall::multicall_0Call::abi_decode(&out).unwrap();
        assert_eq!(decoded.data, vec![a, b]);
    }

    #[test]
    fn with_deadline_uses_deadline_overload() {
        let a = Bytes::from(vec![0x11]);
        let b = Bytes::from(vec![0x22]);
        let out = encode_multicall(Some(1_700_000_000), vec![a.clone(), b.clone()]);
        assert_eq!(&out[..4], &[0x5a, 0xe4, 0x01, 0xdc]); // multicall(uint256,bytes[])
        let decoded = IMulticall::multicall_1Call::abi_decode(&out).unwrap();
        assert_eq!(decoded.deadline, U256::from(1_700_000_000u64));
        assert_eq!(decoded.data, vec![a, b]);
    }

    #[test]
    fn single_call_with_deadline_wraps_in_deadline_overload() {
        let a = Bytes::from(vec![0x11]);
        let out = encode_multicall(Some(1_700_000_000), vec![a.clone()]);
        assert_eq!(&out[..4], &[0x5a, 0xe4, 0x01, 0xdc]); // multicall(uint256,bytes[])
    }
}
