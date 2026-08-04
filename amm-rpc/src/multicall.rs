//! Multicall3 read helper: batch many `eth_call`s into one block-pinned round
//! trip via `aggregate3`, tolerating per-call reverts.
//!
//! A reverting pool read surfaces as `success = false` for that call rather than
//! failing the whole batch — so one bad pool in a refresh does not sink the rest.
//! Modeled on garden-rs's `Multicall` (a `Call`/`Result` batch) but read-only and
//! block-pinned, which is all state fetching needs.

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, address};
use alloy::providers::Provider;
use alloy::sol;

use crate::error::RpcError;

/// Canonical Multicall3 deployment — the same address on every supported chain.
pub const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
}

/// One call in a batch: which contract, and the ABI-encoded calldata.
#[derive(Clone, Debug)]
pub struct Call {
    /// The contract to call.
    pub target: Address,
    /// ABI-encoded function calldata.
    pub call_data: Bytes,
}

/// The outcome of one batched call.
#[derive(Clone, Debug)]
pub struct CallResult {
    /// Whether the sub-call succeeded (a revert is `false`, not a batch failure).
    pub success: bool,
    /// The raw return data (empty on revert).
    pub return_data: Bytes,
}

/// Run `calls` in one Multicall3 `aggregate3` pinned to `block`, tolerating
/// per-call reverts. The result is index-aligned with `calls`.
pub async fn aggregate3<P: Provider>(
    provider: &P,
    calls: Vec<Call>,
    block: BlockId,
) -> Result<Vec<CallResult>, RpcError> {
    let batch: Vec<IMulticall3::Call3> = calls
        .into_iter()
        .map(|c| IMulticall3::Call3 {
            target: c.target,
            allowFailure: true,
            callData: c.call_data,
        })
        .collect();

    let results = IMulticall3::new(MULTICALL3, provider)
        .aggregate3(batch)
        .block(block)
        .call()
        .await
        .map_err(|e| RpcError::Transport(e.to_string()))?;

    Ok(results
        .into_iter()
        .map(|r| CallResult {
            success: r.success,
            return_data: r.returnData,
        })
        .collect())
}
