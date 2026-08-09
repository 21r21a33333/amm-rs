//! Execution options: recipient, deadline, approval mode, and the edge
//! resolver that converts relative/identity placeholders to absolutes.
//!
//! The builder ([`ExecutionOptions`]) is pure — no clock, no sender. Call
//! [`resolve`] at the router edge (once `now` and `sender` are known) to pin
//! every relative field to its absolute form before handing off to the build
//! layer.

use std::time::Duration;

use alloy::primitives::{Address, Bytes};
use amm_core::primitives::price::Price;
use amm_core::slippage::Slippage;

/// Default time-to-live for a swap deadline, in seconds (5 minutes).
const DEFAULT_TTL_SECS: u64 = 300;

/// Who receives the output tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recipient {
    /// Use the transaction sender as the recipient. Resolved to [`Recipient::To`]
    /// by [`resolve`] before the build layer is invoked.
    Sender,
    /// An explicit EVM address to receive the output tokens.
    To(Address),
}

/// When the swap transaction should expire.
///
/// `None` is intentionally absent: every swap must have a bounded deadline to
/// prevent MEV exploits that replay stale transactions.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Deadline {
    /// Expire `ttl` after the transaction is submitted. Resolved to
    /// [`Deadline::AtTimestamp`] by [`resolve`] once the current block time
    /// is known.
    FromNow(Duration),
    /// Absolute Unix timestamp (seconds since the epoch) after which the swap
    /// reverts.
    AtTimestamp(u64),
    /// Absolute block number after which the swap reverts.
    AtBlock(u64),
}

/// How the router is permitted to pull the input token from the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalMode {
    /// The caller already holds a sufficient ERC-20 allowance; no approval step
    /// is added to the transaction.
    AssumeApproved,
    /// Prepend a standard ERC-20 `approve(router, amount)` call.
    Erc20,
    /// Use Permit2 single-signature approval. The signature must cover the
    /// exact token, amount, and nonce for this swap.
    Permit2 {
        /// Packed Permit2 signature bytes.
        signature: Bytes,
    },
}

/// Builder for swap execution parameters.
///
/// Constructed via [`ExecutionOptions::new`], which sets safe defaults for all
/// fields except `slippage` (which the caller must supply explicitly — there is
/// no sensible universal default). No [`Default`] impl is provided; callers
/// must choose a slippage tolerance.
///
/// All fields are resolved from relative/identity form to absolutes by
/// [`resolve`] before being passed to the build layer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecutionOptions {
    /// Who receives the output tokens.
    pub recipient: Recipient,
    /// Maximum acceptable slippage from the quoted price.
    pub slippage: Slippage,
    /// When the transaction expires.
    pub deadline: Deadline,
    /// Optional price limit (e.g. a V3 `sqrtPriceLimitX96`).
    pub price_limit: Option<Price>,
    /// How the router is permitted to spend the input token.
    pub approval: ApprovalMode,
}

impl ExecutionOptions {
    /// Construct with `slippage` and safe defaults:
    /// - `recipient = Recipient::Sender`
    /// - `deadline  = Deadline::FromNow(DEFAULT_TTL_SECS)` (5 min)
    /// - `price_limit = None`
    /// - `approval  = ApprovalMode::AssumeApproved`
    ///
    /// An unresolved `FromNow` deadline causes the build layer to return
    /// [`BuildError::UnresolvedDeadline`], so forgetting to call [`resolve`]
    /// is a hard error rather than a silent footgun.
    // no Default: slippage must be chosen explicitly
    pub fn new(slippage: Slippage) -> Self {
        Self {
            recipient: Recipient::Sender,
            slippage,
            deadline: Deadline::FromNow(Duration::from_secs(DEFAULT_TTL_SECS)),
            price_limit: None,
            approval: ApprovalMode::AssumeApproved,
        }
    }

    /// Override the recipient.
    pub fn with_recipient(mut self, recipient: Recipient) -> Self {
        self.recipient = recipient;
        self
    }

    /// Override the deadline.
    pub fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = deadline;
        self
    }

    /// Override the price limit.
    pub fn with_price_limit(mut self, price_limit: Option<Price>) -> Self {
        self.price_limit = price_limit;
        self
    }

    /// Override the approval mode.
    pub fn with_approval(mut self, approval: ApprovalMode) -> Self {
        self.approval = approval;
        self
    }
}

/// Resolve clock/identity-relative options to absolutes at the edge, so the
/// pure builder never needs a clock or a sender. Idempotent on already-absolute
/// values.
///
/// - `Deadline::FromNow(ttl)` → `Deadline::AtTimestamp(now.saturating_add(ttl.as_secs()))`
/// - `Recipient::Sender`      → `Recipient::To(sender)`
/// - All other variants pass through unchanged.
pub fn resolve(mut opts: ExecutionOptions, now: u64, sender: Address) -> ExecutionOptions {
    opts.deadline = match opts.deadline {
        Deadline::FromNow(ttl) => Deadline::AtTimestamp(now.saturating_add(ttl.as_secs())),
        other => other,
    };
    opts.recipient = match opts.recipient {
        Recipient::Sender => Recipient::To(sender),
        other => other,
    };
    opts
}

#[cfg(test)]
mod tests {
    use super::*;
    use amm_core::primitives::ratio::Bps;

    fn slippage_50bps() -> Slippage {
        Slippage::from_bps(Bps(50))
    }

    #[test]
    fn new_sets_expected_defaults() {
        let opts = ExecutionOptions::new(slippage_50bps());
        assert_eq!(opts.recipient, Recipient::Sender);
        assert_eq!(opts.slippage, slippage_50bps());
        assert_eq!(opts.deadline, Deadline::FromNow(Duration::from_secs(300)));
        assert!(opts.price_limit.is_none());
        assert_eq!(opts.approval, ApprovalMode::AssumeApproved);
    }

    #[test]
    fn with_recipient_override_sticks() {
        let addr = Address::repeat_byte(0xab);
        let opts = ExecutionOptions::new(slippage_50bps())
            .with_recipient(Recipient::To(addr));
        assert_eq!(opts.recipient, Recipient::To(addr));
    }

    #[test]
    fn with_deadline_override_sticks() {
        let opts = ExecutionOptions::new(slippage_50bps())
            .with_deadline(Deadline::AtBlock(999));
        assert_eq!(opts.deadline, Deadline::AtBlock(999));
    }

    #[test]
    fn with_approval_override_sticks() {
        let sig = Bytes::from(vec![0xde, 0xad]);
        let opts = ExecutionOptions::new(slippage_50bps())
            .with_approval(ApprovalMode::Permit2 { signature: sig.clone() });
        assert_eq!(opts.approval, ApprovalMode::Permit2 { signature: sig });
    }

    #[test]
    fn with_price_limit_override_sticks() {
        use alloy::primitives::{B256, U256};
        use amm_core::primitives::asset::{AssetId, ChainId};
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let base = AssetId::new(ChainId(1), B256::left_padding_from(&[0xaa]));
        let quote = AssetId::new(ChainId(1), B256::left_padding_from(&[0xbb]));
        let ratio = Ratio::new(U256::from(3u64), U256::from(1u64)).unwrap();
        let price = Price::new(base, quote, ratio).unwrap();

        let opts = ExecutionOptions::new(slippage_50bps())
            .with_price_limit(Some(price.clone()));
        assert_eq!(opts.price_limit, Some(price));
    }

    #[test]
    fn resolve_turns_from_now_and_sender_into_absolutes() {
        let now = 1_000_000u64;
        let sender = Address::repeat_byte(0x11);
        let opts = ExecutionOptions::new(slippage_50bps());
        // defaults: FromNow(300s) + Sender
        let resolved = resolve(opts, now, sender);
        assert_eq!(resolved.deadline, Deadline::AtTimestamp(now + 300));
        assert_eq!(resolved.recipient, Recipient::To(sender));
    }

    #[test]
    fn resolve_is_noop_on_already_absolute_deadline() {
        let now = 1_000_000u64;
        let sender = Address::repeat_byte(0x22);
        let abs_ts = 9_999_999u64;
        let opts = ExecutionOptions::new(slippage_50bps())
            .with_deadline(Deadline::AtTimestamp(abs_ts))
            .with_recipient(Recipient::To(sender));
        let resolved = resolve(opts, now, sender);
        assert_eq!(resolved.deadline, Deadline::AtTimestamp(abs_ts));
        assert_eq!(resolved.recipient, Recipient::To(sender));
    }

    #[test]
    fn resolve_is_noop_on_at_block_deadline() {
        let now = 1_000_000u64;
        let sender = Address::repeat_byte(0x33);
        let opts = ExecutionOptions::new(slippage_50bps())
            .with_deadline(Deadline::AtBlock(200));
        let resolved = resolve(opts, now, sender);
        assert_eq!(resolved.deadline, Deadline::AtBlock(200));
    }

    #[test]
    fn resolve_saturates_on_huge_ttl() {
        let now = u64::MAX;
        let sender = Address::repeat_byte(0x44);
        let opts = ExecutionOptions::new(slippage_50bps())
            .with_deadline(Deadline::FromNow(Duration::from_secs(100)));
        let resolved = resolve(opts, now, sender);
        // saturating_add: u64::MAX + 100 saturates to u64::MAX
        assert_eq!(resolved.deadline, Deadline::AtTimestamp(u64::MAX));
    }
}
