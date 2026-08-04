//! The I/O error type for on-chain state fetching.

/// An error from fetching or decoding on-chain pool state.
///
/// Deliberately string-carrying rather than wrapping `alloy`'s error types, so
/// the public surface does not leak a specific provider/transport version.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    /// The transport failed (bad URL, timeout, disconnect, rate limit).
    #[error("rpc transport error: {0}")]
    Transport(String),
    /// On-chain return data could not be decoded into the expected shape.
    #[error("decode error: {0}")]
    Decode(String),
    /// A requested pool or contract was not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// Any other failure that does not fit the categories above.
    #[error("internal error: {0}")]
    Internal(String),
}
