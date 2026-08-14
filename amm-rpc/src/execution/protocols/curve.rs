//! Curve `exchange` encoder: dispatches on `pool.interface()` to the correct
//! ABI family and returns a [`PreparedSwap`] with `tx.to = pool_address` and
//! the ERC-20 approval targeting the pool itself (Curve pulls via
//! `transferFrom`).
//!
//! **Implemented ABI families (E4.2–E4.4):**
//! - `StableI128` / `StableI128Ng` → `exchange(int128,int128,uint256,uint256)` selector `0x3df02124`
//! - `CryptoU256UseEth`             → `exchange(uint256,uint256,uint256,uint256,bool)` selector `0x394747c5`
//! - `CryptoU256Receiver`           → `exchange(uint256,uint256,uint256,uint256,address)` (Twocrypto-NG)
//!
//! Every other [`CurveInterface`] variant, and the `None` (no-execution-metadata)
//! case, immediately return [`BuildError::UnsupportedProtocol`]. Exact-out is
//! not supported by any Curve pool; `build_swap_exact_out` also returns
//! [`BuildError::UnsupportedProtocol`].
//!
//! **Variant coverage (E4.5 — no silent caps).** The Phase-0 `interface_of` map
//! routes all twelve `curve_adapter::CurveVariant`s onto exactly these four
//! `CurveInterface` families, all of which are now implemented — so every Curve
//! variant can encode a direct coin-to-coin `exchange`. One capability is
//! deliberately **deferred** and returns [`BuildError::UnsupportedProtocol`]:
//! - **`exchange_underlying`** (meta / lending pools trading a base-pool
//!   underlying coin) — a different method with its own index space; plain
//!   `exchange` covers the coin-level swaps that carry the volume.
//!
//! **Native ETH (`use_eth`) support.** `CryptoU256UseEth` pools (tricrypto2-style)
//! accept native ETH on either side via the payable `use_eth` flag. When
//! `Currency::Native` appears on either side, `common::resolve_swap` maps it to
//! `ctx.weth` and sets `r.native_in` / `r.native_out`; the encoder then sets
//! `use_eth = true`, sends value = `amount_in.raw` for native-in, and skips the
//! ERC-20 approval for native-in (no token to approve — ETH is sent as tx.value).
//! All other interfaces (`StableI128`, `StableI128Ng`, `CryptoU256Receiver`, `None`)
//! still reject `Currency::Native` with [`BuildError::UnsupportedProtocol`].
//!
//! **Recipient delivery contract:**
//! - `CryptoU256Receiver` (`exchange(…, address receiver)`) delivers the output
//!   to `r.recipient`; this is the only Curve ABI with a recipient argument.
//! - `StableI128`, `StableI128Ng`, and `CryptoU256UseEth` have no `receiver`
//!   parameter and always pay `msg.sender`. `Recipient::Sender` is collapsed to
//!   `Recipient::To(sender)` by `options::resolve` at the router edge before any
//!   encoder runs, so the builder always receives a concrete address; on these
//!   receiver-less ABIs that address can only be honored when it equals the
//!   transaction sender — which the builder cannot verify. Callers must avoid
//!   routing to a receiver-less pool when the intended recipient differs from the
//!   transaction sender.

#[cfg(feature = "curve")]
use alloy::primitives::U256;
#[cfg(feature = "curve")]
use alloy::{sol, sol_types::SolCall};
#[cfg(feature = "curve")]
use amm_core::primitives::asset::AssetAmount;
#[cfg(feature = "curve")]
use amm_core::protocols::curve::interface::CurveInterface;
#[cfg(feature = "curve")]
use amm_core::protocols::curve::pool::CurvePool;

#[cfg(feature = "curve")]
use crate::execution::{
    config::ChainConfig,
    error::BuildError,
    executable::{Executable, Sealed},
    options::ExecutionOptions,
    prepared::PreparedSwap,
    protocols::common,
    types::{Currency, CurrencyAmount},
};

// ── ABI ──────────────────────────────────────────────────────────────────────

#[cfg(feature = "curve")]
sol! {
    /// Curve StableSwap `exchange` — the classic V1 ABI and StableSwap-NG.
    ///
    /// Selector `0x3df02124`. Void return on V1; NG returns `uint256`, but
    /// the selector and args are identical — the return value is not part of
    /// the calldata we send, so both families share this encoding.
    /// Both `i` and `j` are `int128` coin indices.
    interface ICurveStableI128 {
        function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy) external;
    }

    /// Curve CryptoSwap `exchange` — tricrypto / two-crypto V1 ABI.
    ///
    /// Selector `0x394747c5`. The `use_eth` flag controls whether to unwrap
    /// ETH on the way in/out. When `Currency::Native` is present, the encoder
    /// sets `use_eth = true`; for pure ERC-20 swaps it is `false`. For native-in
    /// the call is payable and tx.value carries the ETH amount; for native-out
    /// or pure ERC-20 the call is not payable. Indices are `uint256` (not `int128`).
    interface ICurveCryptoUseEth {
        function exchange(uint256 i, uint256 j, uint256 dx, uint256 min_dy, bool use_eth) external payable;
    }

    /// Curve Twocrypto-NG `exchange` — 5th arg is `receiver`, NOT `use_eth`.
    ///
    /// Used for `CryptoU256Receiver` (TwoCryptoNG / TwoCryptoStable). The
    /// burn-in trap: this looks similar to `ICurveCryptoUseEth` but the 5th
    /// parameter is an `address receiver` — passing a bool there would corrupt
    /// the call. The selector differs from `0x394747c5`.
    interface ICurveCryptoReceiver {
        function exchange(uint256 i, uint256 j, uint256 dx, uint256 min_dy, address receiver) external returns (uint256);
    }
}

// ── Sealed + Executable ───────────────────────────────────────────────────────

#[cfg(feature = "curve")]
impl Sealed for CurvePool {}

#[cfg(feature = "curve")]
impl Executable for CurvePool {
    /// Build an exact-in `exchange` call for a Curve pool.
    ///
    /// # Dispatch
    ///
    /// `CurveInterface::StableI128`, `CurveInterface::StableI128Ng`,
    /// `CurveInterface::CryptoU256UseEth`, and
    /// `CurveInterface::CryptoU256Receiver` are handled. Any other
    /// `interface()` value (including `None`) returns
    /// [`BuildError::UnsupportedProtocol`].
    ///
    /// # Errors
    ///
    /// - [`BuildError::UnsupportedProtocol`] — `opts.price_limit` is `Some`
    ///   (Curve has no price bound), `Currency::Native` appears on either side
    ///   for any interface other than `CryptoU256UseEth` (which supports native
    ///   ETH via `use_eth`), the pool has no registered interface, or the
    ///   interface is not yet implemented.
    /// - [`BuildError::AssetNotInPool`] — coins not found in this pool.
    /// - [`BuildError::Overflow`] — coin index exceeds `i128::MAX` (defensive;
    ///   applies to StableI128 only — CryptoU256UseEth uses `U256` indices).
    /// - Propagated from [`common::resolve_swap`]: [`BuildError::UnresolvedRecipient`],
    ///   [`BuildError::UnresolvedDeadline`], [`BuildError::NativeMismatch`].
    fn build_swap(
        &self,
        ctx: &ChainConfig,
        amount_in: CurrencyAmount,
        to: Currency,
        quoted_out: &AssetAmount,
        opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        // Curve has no price-bound mechanism.
        if opts.price_limit.is_some() {
            return Err(BuildError::UnsupportedProtocol);
        }

        // Only CryptoU256UseEth supports native ETH via the payable `use_eth`
        // flag. Every other interface — StableI128, StableI128Ng,
        // CryptoU256Receiver, and the no-metadata None case — does not accept
        // native currency; reject early so callers get a clear error rather than
        // a silent coin-lookup failure.
        let is_use_eth_iface = self.interface() == Some(CurveInterface::CryptoU256UseEth);
        if (amount_in.currency.is_native() || to.is_native()) && !is_use_eth_iface {
            return Err(BuildError::UnsupportedProtocol);
        }

        // resolve_swap validates membership, recipient, deadline.
        // For native-in/out it maps Currency::Native → ctx.weth and sets
        // r.native_in / r.native_out. coin_indices then finds the WETH slot.
        // Curve's classic `exchange` ignores recipient and deadline at the ABI
        // level, but we still enforce they are resolved (prevents callers from
        // omitting resolution accidentally).
        let r = common::resolve_swap(ctx, self, amount_in.currency, to, opts)?;

        // Shared prologue: extract pool address, coin indices, and min-out.
        // Only the calldata bytes differ between ABI families.
        let pool_addr = self.pool_address().ok_or(BuildError::UnsupportedProtocol)?;

        let (i, j) = self
            .coin_indices(&r.input, &r.output)
            .ok_or(BuildError::AssetNotInPool {
                input: r.input,
                output: r.output,
            })?;

        let min = opts.slippage.min_amount_out(quoted_out);

        // Build the family-specific calldata; everything else is shared.
        let data = match self.interface() {
            Some(CurveInterface::StableI128) | Some(CurveInterface::StableI128Ng) => {
                // StableSwap-NG uses the same `exchange(int128,int128,uint256,uint256)`
                // selector and args as the classic V1 ABI (selector `0x3df02124`).
                // The only difference is that NG returns `uint256`; that does not
                // affect the calldata we send.
                //
                // sol! maps Solidity `int128` to Rust `i128`. Coin indices are
                // always small non-negative integers (at most 8 for any live
                // Curve pool), so this cast is purely defensive.
                let call = ICurveStableI128::exchangeCall {
                    i: i128::try_from(i).map_err(|_| BuildError::Overflow)?,
                    j: i128::try_from(j).map_err(|_| BuildError::Overflow)?,
                    dx: amount_in.raw,
                    min_dy: min.raw,
                };
                call.abi_encode()
            }
            Some(CurveInterface::CryptoU256UseEth) => {
                // use_eth=true enables the pool's native-ETH path on either
                // side: native-in → the call is payable and the pool wraps the
                // received ETH; native-out → the pool unwraps WETH before
                // delivering. For pure ERC-20 swaps, use_eth stays false.
                let call = ICurveCryptoUseEth::exchangeCall {
                    i: U256::from(i),
                    j: U256::from(j),
                    dx: amount_in.raw,
                    min_dy: min.raw,
                    use_eth: r.native_in || r.native_out,
                };
                call.abi_encode()
            }
            Some(CurveInterface::CryptoU256Receiver) => {
                // Twocrypto-NG: the 5th argument is `receiver` (the resolved
                // recipient address), NOT a `use_eth` bool. Passing a bool
                // here would silently encode the wrong ABI — the burn-in trap.
                // The selector differs from `0x394747c5` (CryptoUseEth).
                let call = ICurveCryptoReceiver::exchangeCall {
                    i: U256::from(i),
                    j: U256::from(j),
                    dx: amount_in.raw,
                    min_dy: min.raw,
                    receiver: r.recipient,
                };
                call.abi_encode()
            }
            // No interface registered, or an unimplemented variant.
            _ => return Err(BuildError::UnsupportedProtocol),
        };

        // Shared epilogue: tx.to = pool. `value` and `approval` are two facets
        // of the same input-side decision, so they resolve together:
        // - native ETH in → the payable call carries `dx` in tx.value, nothing
        //   to approve;
        // - token in (including native-out, whose input is an ERC-20) → zero
        //   value, and the pool pulls `dx` via transferFrom under an approval.
        let (value, approval) = match r.native_in {
            true => (amount_in.raw, None),
            false => (
                U256::ZERO,
                Some(common::erc20_approval(pool_addr, r.input, amount_in.raw)),
            ),
        };
        Ok(common::prepared(
            ctx,
            pool_addr,
            data.into(),
            value,
            min,
            None,
            approval,
        ))
    }

    /// Curve has no exact-out entrypoint — always returns
    /// [`BuildError::UnsupportedProtocol`].
    fn build_swap_exact_out(
        &self,
        _ctx: &ChainConfig,
        _amount_out: CurrencyAmount,
        _from: Currency,
        _quoted_in: &AssetAmount,
        _opts: &ExecutionOptions,
    ) -> Result<PreparedSwap, BuildError> {
        Err(BuildError::UnsupportedProtocol)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "curve"))]
mod tests {
    use alloy::primitives::{Address, B256, U256, address};
    use alloy::sol_types::SolCall;
    use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
    use amm_core::primitives::pool::PoolId;
    use amm_core::primitives::ratio::Bps;
    use amm_core::protocols::curve::interface::CurveInterface;
    use amm_core::protocols::curve::pool::CurvePool;
    use amm_core::slippage::Slippage;
    use amm_core::traits::pool::Pool;
    use curve_math::Pool as CurveMathPool;

    use super::{ICurveCryptoReceiver, ICurveCryptoUseEth, ICurveStableI128};
    use crate::execution::{
        config::ChainConfig,
        error::BuildError,
        executable::{Executable, as_executable},
        options::{Deadline, ExecutionOptions, Recipient},
        types::{Currency, CurrencyAmount},
    };

    // ── constants ─────────────────────────────────────────────────────────────

    /// 3pool address on Ethereum mainnet.
    const POOL_ADDR: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

    /// tricrypto2 address on Ethereum mainnet.
    const CRYPTO_POOL_ADDR: Address = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46");

    // ── asset helpers ─────────────────────────────────────────────────────────

    fn chain_id() -> ChainId {
        ChainId(1)
    }

    /// Build an `AssetId` from a single discriminator byte.
    fn asset(byte: u8) -> AssetId {
        AssetId::new(chain_id(), B256::left_padding_from(&[byte]))
    }

    fn dai() -> AssetId {
        asset(0x01)
    }
    fn usdc() -> AssetId {
        asset(0x02)
    }
    fn usdt() -> AssetId {
        asset(0x03)
    }
    fn wbtc() -> AssetId {
        asset(0x04)
    }
    fn weth() -> AssetId {
        asset(0xC0)
    }

    // ── pool fixtures ─────────────────────────────────────────────────────────

    /// Balanced 3-coin StableSwap (1M units each, 1 bp fee) with execution
    /// metadata attached.
    fn stable_3pool() -> CurvePool {
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let bal = e18 * U256::from(1_000_000u64);
        let inner = CurveMathPool::StableSwapV1 {
            balances: vec![bal, bal, bal],
            rates: vec![e18, e18, e18],
            amp: U256::from(2_000u64),
            fee: U256::from(1_000_000u64),
        };
        CurvePool::new(
            PoolId::new("1:curve:0x3pool"),
            vec![dai(), usdc(), usdt()],
            inner,
        )
        .with_execution(POOL_ADDR, CurveInterface::StableI128)
    }

    /// Balanced 3-coin tricrypto2-like CryptoSwap pool with execution
    /// metadata attached (`CryptoU256UseEth`). Coins: USDT / WBTC / WETH.
    ///
    /// Uses `TwoCryptoStable` as the closest constructible `CurveMathPool`
    /// variant with crypto fee params (no gamma needed for unit tests). We
    /// give it a third coin slot by structuring the outer `CurvePool` assets
    /// list as [usdt, wbtc, weth]; the math pool beneath is 2-coin but the
    /// dispatch under test only reads coin_indices from the wrapper.
    ///
    /// For the encoder test we only need the ABI encoding to be correct; the
    /// math layer is exercised by separate amm-core tests.
    fn crypto_3pool() -> CurvePool {
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        // A simple 2-coin TwoCryptoStable inner (the math pool type with crypto
        // fee params). The encoder dispatch cares only about the interface()
        // tag and coin_indices, not the math internals.
        let inner = CurveMathPool::TwoCryptoStable {
            balances: [e18 * U256::from(1_000_000u64), e18 * U256::from(100u64)],
            precisions: [U256::from(1u64), U256::from(1u64)],
            price_scale: e18 * U256::from(20_000u64), // ~20k USD/BTC
            d: e18 * U256::from(2_000_000u64),
            ann: U256::from(400_000u64),
            mid_fee: U256::from(3_000_000u64),
            out_fee: U256::from(30_000_000u64),
            fee_gamma: U256::from(230_000_000_000_000u64),
        };
        // Three-coin outer wrapper (USDT=0, WBTC=1, WETH=2) over the 2-coin
        // math pool. The encoder reads i/j from assets[], not the math pool.
        CurvePool::new(
            PoolId::new("1:curve:0xtricrypto2"),
            vec![usdt(), wbtc(), weth()],
            inner,
        )
        .with_execution(CRYPTO_POOL_ADDR, CurveInterface::CryptoU256UseEth)
    }

    // ── ctx / opts helpers ────────────────────────────────────────────────────

    /// Minimal chain config — Curve needs no router address, just weth + chain.
    fn ctx() -> ChainConfig {
        ChainConfig::new(chain_id(), weth())
    }

    /// Fully-resolved execution options.
    fn opts_resolved(to: Address, deadline_ts: u64, slippage_bps: u16) -> ExecutionOptions {
        ExecutionOptions::new(Slippage::from_bps(Bps(slippage_bps)))
            .with_recipient(Recipient::To(to))
            .with_deadline(Deadline::AtTimestamp(deadline_ts))
    }

    // ── selector sanity ────────────────────────────────────────────────────────

    /// All three Curve ABI selectors must match on-chain values, and
    /// `CryptoReceiver` must differ from `CryptoUseEth` (burn-in trap guard).
    ///
    /// - `exchange(int128,int128,uint256,uint256)` → `0x3df02124`
    /// - `exchange(uint256,uint256,uint256,uint256,bool)` → `0x394747c5`
    /// - `exchange(uint256,uint256,uint256,uint256,address)` → differs from `0x394747c5`
    #[test]
    fn selectors_are_correct() {
        assert_eq!(
            ICurveStableI128::exchangeCall::SELECTOR,
            [0x3d, 0xf0, 0x21, 0x24],
            "StableI128 exchange selector must be 0x3df02124"
        );
        assert_eq!(
            ICurveCryptoUseEth::exchangeCall::SELECTOR,
            [0x39, 0x47, 0x47, 0xc5],
            "CryptoUseEth exchange selector must be 0x394747c5"
        );
        // Burn-in trap guard: CryptoReceiver (address 5th arg) must NOT share
        // the selector with CryptoUseEth (bool 5th arg).
        assert_ne!(
            ICurveCryptoReceiver::exchangeCall::SELECTOR,
            [0x39, 0x47, 0x47, 0xc5],
            "CryptoReceiver selector must differ from CryptoUseEth 0x394747c5"
        );
    }

    // ── DAI → USDC exact-in ERC-20 (StableI128) ──────────────────────────────

    /// Build a DAI → USDC exact-in swap and verify every field of the result.
    ///
    /// Expected: `i=0, j=1, dx=amount_in, min_dy=min`, `tx.to=pool`, `value=0`,
    /// approval spender is the pool with `min_allowance=amount_in.raw`.
    #[test]
    fn exact_in_dai_to_usdc_encodes_correctly() {
        let pool = stable_3pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x55);
        let deadline_ts = 1_700_000_000u64;
        // 100 bps slippage: quoted 1000 → min = floor(1000 * 9900/10000) = 990
        let opts = opts_resolved(recipient, deadline_ts, 100);

        let amount_in_raw = U256::from(1_000_000_000_000_000_000u64); // 1 DAI (18 dec)
        let quoted_out = AssetAmount::new(usdc(), U256::from(1_000_000_000_000_000_000u64));
        let prepared = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(dai()),
                    raw: amount_in_raw,
                },
                Currency::Token(usdc()),
                &quoted_out,
                &opts,
            )
            .expect("build_swap DAI→USDC must succeed");

        // tx fields
        assert_eq!(prepared.tx.to, POOL_ADDR, "tx.to must be the pool address");
        assert_eq!(prepared.tx.value, U256::ZERO, "StableSwap is never payable");
        assert_eq!(prepared.tx.chain, chain_id(), "chain must round-trip");

        // Decode calldata
        let decoded = ICurveStableI128::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as exchange(int128,int128,uint256,uint256)");

        // sol! maps `int128` to Rust `i128`.
        assert_eq!(decoded.i, 0i128, "i must be 0 (DAI index)");
        assert_eq!(decoded.j, 1i128, "j must be 1 (USDC index)");
        assert_eq!(decoded.dx, amount_in_raw, "dx must equal amount_in.raw");
        // min_dy = floor(1000e18 * 9900/10000) = 990e18
        let expected_min =
            U256::from(1_000_000_000_000_000_000u64) * U256::from(9900u64) / U256::from(10000u64);
        assert_eq!(
            decoded.min_dy, expected_min,
            "min_dy must be 990e18 at 100bps"
        );

        // min_received
        assert_eq!(prepared.min_received.raw, expected_min);
        assert_eq!(prepared.min_received.asset, usdc());

        // max_spent == None (exact-in)
        assert!(
            prepared.max_spent.is_none(),
            "max_spent must be None for exact-in"
        );

        // Approval — spender is the pool, token is DAI
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender, POOL_ADDR,
            "approval.spender must be the pool (Curve pulls via transferFrom)"
        );
        assert_eq!(
            approval.token,
            dai(),
            "approval.token must be the input asset"
        );
        assert_eq!(
            approval.min_allowance, amount_in_raw,
            "approval.min_allowance must equal amount_in.raw"
        );
        assert!(!approval.reset_first, "reset_first must be false");
    }

    // ── USDT → WETH exact-in ERC-20 (CryptoU256UseEth) ──────────────────────

    /// Build a USDT → WETH exact-in swap on a tricrypto2-like pool and verify
    /// every field of the result.
    ///
    /// Expected: `i=0, j=2, dx=amount_in, min_dy=min, use_eth=false`,
    /// `tx.to=crypto_pool`, `value=0`, approval spender is the pool.
    #[test]
    fn exact_in_usdt_to_weth_crypto_encodes_correctly() {
        let pool = crypto_3pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x66);
        let deadline_ts = 1_800_000_000u64;
        // 50 bps slippage
        let opts = opts_resolved(recipient, deadline_ts, 50);

        // 1000 USDT (6 dec)
        let amount_in_raw = U256::from(1_000_000_000u64);
        // Quoted output: 0.5 WETH (18 dec, synthetic — only slippage math matters
        // for encoder unit test; actual quote is tested by fork proof)
        let quoted_out = AssetAmount::new(weth(), U256::from(500_000_000_000_000_000u64));
        let prepared = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(usdt()),
                    raw: amount_in_raw,
                },
                Currency::Token(weth()),
                &quoted_out,
                &opts,
            )
            .expect("build_swap USDT→WETH must succeed");

        // tx fields
        assert_eq!(
            prepared.tx.to, CRYPTO_POOL_ADDR,
            "tx.to must be the crypto pool address"
        );
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "use_eth=false path is never payable"
        );
        assert_eq!(prepared.tx.chain, chain_id(), "chain must round-trip");

        // Decode calldata — must decode as CryptoUseEth ABI
        let decoded = ICurveCryptoUseEth::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as exchange(uint256,uint256,uint256,uint256,bool)");

        assert_eq!(decoded.i, U256::from(0u64), "i must be 0 (USDT index)");
        assert_eq!(decoded.j, U256::from(2u64), "j must be 2 (WETH index)");
        assert_eq!(decoded.dx, amount_in_raw, "dx must equal amount_in.raw");
        assert!(!decoded.use_eth, "use_eth must be false (ERC-20 WETH path)");

        // min_dy = floor(0.5e18 * 9950/10000) at 50 bps
        let expected_min =
            U256::from(500_000_000_000_000_000u64) * U256::from(9950u64) / U256::from(10000u64);
        assert_eq!(decoded.min_dy, expected_min, "min_dy at 50bps");

        // min_received
        assert_eq!(prepared.min_received.raw, expected_min);
        assert_eq!(prepared.min_received.asset, weth());

        // max_spent == None (exact-in)
        assert!(
            prepared.max_spent.is_none(),
            "max_spent must be None for exact-in"
        );

        // Approval — spender is the pool, token is USDT
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender, CRYPTO_POOL_ADDR,
            "approval.spender must be the crypto pool"
        );
        assert_eq!(
            approval.token,
            usdt(),
            "approval.token must be USDT (the input asset)"
        );
        assert_eq!(
            approval.min_allowance, amount_in_raw,
            "approval.min_allowance must equal amount_in.raw"
        );
        assert!(!approval.reset_first, "reset_first must be false");
    }

    // ── CryptoU256UseEth i/j are U256, not i128 ──────────────────────────────

    /// Verify that the encoded indices for USDT(0)→WBTC(1) are U256 in the
    /// CryptoUseEth calldata (not i128 values that would decode as StableI128).
    #[test]
    fn crypto_indices_are_u256_not_i128() {
        let pool = crypto_3pool();
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x77), 9_999_999, 30);

        let prepared = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(usdt()),
                    raw: U256::from(500_000_000u64),
                },
                Currency::Token(wbtc()),
                &AssetAmount::new(wbtc(), U256::from(1_000_000u64)),
                &opts,
            )
            .expect("build_swap USDT→WBTC must succeed");

        // Must decode as CryptoUseEth (U256 indices), not StableI128
        let decoded = ICurveCryptoUseEth::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("must decode as CryptoUseEth calldata");
        assert_eq!(decoded.i, U256::from(0u64));
        assert_eq!(decoded.j, U256::from(1u64));
        assert!(!decoded.use_eth);

        // Must NOT decode as StableI128 correctly (selectors differ)
        assert!(
            ICurveStableI128::exchangeCall::abi_decode(&prepared.tx.data).is_err(),
            "CryptoUseEth calldata must not decode as StableI128"
        );
    }

    // ── guard: price_limit → UnsupportedProtocol ─────────────────────────────

    #[test]
    fn price_limit_some_returns_unsupported_protocol() {
        use amm_core::primitives::price::Price;
        use amm_core::primitives::ratio::Ratio;

        let pool = stable_3pool();
        let c = ctx();
        let ratio = Ratio::new(U256::from(2u64), U256::from(1u64)).unwrap();
        let price_limit = Price::new(dai(), usdc(), ratio).unwrap();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50)
            .with_price_limit(Some(price_limit));

        let err = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(dai()),
                    raw: U256::from(100u64),
                },
                Currency::Token(usdc()),
                &AssetAmount::new(usdc(), U256::from(100u64)),
                &opts,
            )
            .expect_err("price_limit must return UnsupportedProtocol");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ── guard: native input → UnsupportedProtocol ────────────────────────────

    #[test]
    fn native_input_returns_unsupported_protocol() {
        let pool = stable_3pool();
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);

        let err = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Native,
                    raw: U256::from(100u64),
                },
                Currency::Token(usdc()),
                &AssetAmount::new(usdc(), U256::from(100u64)),
                &opts,
            )
            .expect_err("native input must return UnsupportedProtocol");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ── guard: native output → UnsupportedProtocol ───────────────────────────

    #[test]
    fn native_output_returns_unsupported_protocol() {
        let pool = stable_3pool();
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);

        let err = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(dai()),
                    raw: U256::from(100u64),
                },
                Currency::Native,
                &AssetAmount::new(weth(), U256::from(100u64)),
                &opts,
            )
            .expect_err("native output must return UnsupportedProtocol");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ── guard: exact-out → UnsupportedProtocol ───────────────────────────────

    #[test]
    fn exact_out_returns_unsupported_protocol() {
        let pool = stable_3pool();
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);

        let err = pool
            .build_swap_exact_out(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(usdc()),
                    raw: U256::from(100u64),
                },
                Currency::Token(dai()),
                &AssetAmount::new(dai(), U256::from(100u64)),
                &opts,
            )
            .expect_err("exact-out must return UnsupportedProtocol");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ── guard: no interface (None) → UnsupportedProtocol ─────────────────────

    #[test]
    fn no_interface_returns_unsupported_protocol() {
        // A pool without .with_execution() has interface() == None.
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let bal = e18 * U256::from(1_000_000u64);
        let inner = CurveMathPool::StableSwapV1 {
            balances: vec![bal, bal, bal],
            rates: vec![e18, e18, e18],
            amp: U256::from(2_000u64),
            fee: U256::from(1_000_000u64),
        };
        let pool = CurvePool::new(
            PoolId::new("1:curve:0xno-meta"),
            vec![dai(), usdc(), usdt()],
            inner,
        );
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);

        let err = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(dai()),
                    raw: U256::from(100u64),
                },
                Currency::Token(usdc()),
                &AssetAmount::new(usdc(), U256::from(100u64)),
                &opts,
            )
            .expect_err("no-interface pool must return UnsupportedProtocol");
        assert_eq!(err, BuildError::UnsupportedProtocol);
    }

    // ── StableI128Ng pool fixture ──────────────────────────────────────────────

    /// StableSwapNG address on Ethereum mainnet (USDC/crvUSD pool).
    const NG_POOL_ADDR: Address = address!("4DEcE678ceceb27446b35C672dC7d61F30bAD69E");

    /// Twocrypto-NG pool address on Ethereum mainnet (WETH/TC_NG_TOKEN).
    const CRYPTO_RECV_POOL_ADDR: Address = address!("592878b920101946fb5915ab97961bc546f211cc");

    fn usdc_ng() -> AssetId {
        asset(0x05)
    }
    fn crvusd() -> AssetId {
        asset(0x06)
    }
    fn tc_ng_token() -> AssetId {
        asset(0x07)
    }

    /// A 2-coin StableSwapNG pool with `StableI128Ng` interface.
    fn stable_ng_pool() -> CurvePool {
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let bal = e18 * U256::from(1_000_000u64);
        let inner = CurveMathPool::StableSwapV1 {
            balances: vec![bal, bal],
            rates: vec![e18, e18],
            amp: U256::from(2_000u64),
            fee: U256::from(1_000_000u64),
        };
        CurvePool::new(
            PoolId::new("1:curve:0xstable-ng"),
            vec![usdc_ng(), crvusd()],
            inner,
        )
        .with_execution(NG_POOL_ADDR, CurveInterface::StableI128Ng)
    }

    /// A 2-coin Twocrypto-NG pool with `CryptoU256Receiver` interface.
    fn crypto_receiver_pool() -> CurvePool {
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let inner = CurveMathPool::TwoCryptoStable {
            balances: [e18 * U256::from(100u64), e18 * U256::from(100u64)],
            precisions: [U256::from(1u64), U256::from(1u64)],
            price_scale: e18,
            d: e18 * U256::from(200u64),
            ann: U256::from(400_000u64),
            mid_fee: U256::from(3_000_000u64),
            out_fee: U256::from(30_000_000u64),
            fee_gamma: U256::from(230_000_000_000_000u64),
        };
        CurvePool::new(
            PoolId::new("1:curve:0xtwocrypto-ng"),
            vec![weth(), tc_ng_token()],
            inner,
        )
        .with_execution(CRYPTO_RECV_POOL_ADDR, CurveInterface::CryptoU256Receiver)
    }

    // ── StableI128Ng encodes like StableI128 ──────────────────────────────────

    /// `StableI128Ng` must produce the identical `exchange(int128,int128,uint256,uint256)`
    /// calldata as `StableI128` (same selector `0x3df02124`).
    #[test]
    fn stable_ng_encodes_like_stable_i128() {
        let pool = stable_ng_pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x88);
        let opts = opts_resolved(recipient, 9_999_999, 50);

        let amount_in_raw = U256::from(1_000_000u64); // 1 USDC-NG (6 dec)
        let quoted_out = AssetAmount::new(crvusd(), U256::from(1_000_000_000_000_000_000u64));
        let prepared = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(usdc_ng()),
                    raw: amount_in_raw,
                },
                Currency::Token(crvusd()),
                &quoted_out,
                &opts,
            )
            .expect("build_swap StableI128Ng must succeed");

        // Must decode as StableI128 (same selector + same arg layout).
        let decoded = ICurveStableI128::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("StableI128Ng calldata must decode as exchange(int128,int128,uint256,uint256)");

        assert_eq!(decoded.i, 0i128, "i must be 0 (usdc_ng index)");
        assert_eq!(decoded.j, 1i128, "j must be 1 (crvusd index)");
        assert_eq!(decoded.dx, amount_in_raw, "dx must equal amount_in.raw");

        let expected_min =
            U256::from(1_000_000_000_000_000_000u64) * U256::from(9950u64) / U256::from(10000u64);
        assert_eq!(decoded.min_dy, expected_min, "min_dy must be at 50bps");

        // selector must be 0x3df02124 (same as StableI128).
        assert_eq!(
            &prepared.tx.data[..4],
            &[0x3d, 0xf0, 0x21, 0x24],
            "StableI128Ng selector must be 0x3df02124"
        );

        // tx fields
        assert_eq!(prepared.tx.to, NG_POOL_ADDR, "tx.to must be the NG pool");
        assert_eq!(prepared.tx.value, U256::ZERO, "value must be 0");

        // approval targets the NG pool
        let approval = prepared.approval.expect("approval must be Some");
        assert_eq!(approval.spender, NG_POOL_ADDR);
        assert_eq!(approval.token, usdc_ng());
        assert_eq!(approval.min_allowance, amount_in_raw);
    }

    // ── CryptoU256Receiver encodes with receiver address ─────────────────────

    /// `CryptoU256Receiver` must encode `exchange(uint256,uint256,uint256,uint256,address)`
    /// with the 5th arg = `recipient` (NOT a bool), and the selector must differ
    /// from `0x394747c5` (the burn-in trap guard).
    #[test]
    fn crypto_receiver_encodes_with_receiver() {
        let pool = crypto_receiver_pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0x99);
        let opts = opts_resolved(recipient, 9_999_999, 50);

        let amount_in_raw = U256::from(100_000_000_000_000_000u64); // 0.1 WETH
        let quoted_out = AssetAmount::new(tc_ng_token(), U256::from(1_000_000_000_000_000_000u64));
        let prepared = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(weth()),
                    raw: amount_in_raw,
                },
                Currency::Token(tc_ng_token()),
                &quoted_out,
                &opts,
            )
            .expect("build_swap CryptoU256Receiver must succeed");

        // Decode as ICurveCryptoReceiver.
        let decoded = ICurveCryptoReceiver::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as exchange(uint256,uint256,uint256,uint256,address)");

        assert_eq!(decoded.i, U256::from(0u64), "i must be 0 (weth index)");
        assert_eq!(
            decoded.j,
            U256::from(1u64),
            "j must be 1 (tc_ng_token index)"
        );
        assert_eq!(decoded.dx, amount_in_raw, "dx must equal amount_in.raw");
        assert_eq!(
            decoded.receiver, recipient,
            "receiver must be the resolved recipient"
        );

        let expected_min =
            U256::from(1_000_000_000_000_000_000u64) * U256::from(9950u64) / U256::from(10000u64);
        assert_eq!(decoded.min_dy, expected_min, "min_dy must be at 50bps");

        // Burn-in trap guard: selector must NOT be 0x394747c5 (CryptoUseEth).
        assert_ne!(
            &prepared.tx.data[..4],
            &[0x39u8, 0x47, 0x47, 0xc5],
            "CryptoReceiver selector must differ from CryptoUseEth 0x394747c5"
        );

        // Must NOT decode as UseEth ABI.
        assert!(
            ICurveCryptoUseEth::exchangeCall::abi_decode(&prepared.tx.data).is_err(),
            "CryptoReceiver calldata must not decode as CryptoUseEth"
        );

        // tx fields
        assert_eq!(
            prepared.tx.to, CRYPTO_RECV_POOL_ADDR,
            "tx.to must be the Twocrypto-NG pool"
        );
        assert_eq!(prepared.tx.value, U256::ZERO, "value must be 0");

        // approval targets the pool (Curve pulls via transferFrom).
        let approval = prepared.approval.expect("approval must be Some");
        assert_eq!(approval.spender, CRYPTO_RECV_POOL_ADDR);
        assert_eq!(approval.token, weth());
        assert_eq!(approval.min_allowance, amount_in_raw);
    }

    // ── CryptoU256Receiver passes the resolved recipient (0xCD…) ─────────────

    /// `CryptoU256Receiver` must forward `r.recipient` as the `receiver` field.
    ///
    /// Decodes the produced `exchange(uint256,uint256,uint256,uint256,address)`
    /// calldata and asserts the decoded `receiver` equals the configured
    /// `Recipient::To(0xCD…)`.
    #[test]
    fn twocrypto_ng_passes_the_receiver() {
        let pool = crypto_receiver_pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0xCD);
        let opts = opts_resolved(recipient, 9_999_999, 50);

        let amount_in_raw = U256::from(100_000_000_000_000_000u64); // 0.1 WETH
        let quoted_out = AssetAmount::new(tc_ng_token(), U256::from(1_000_000_000_000_000_000u64));
        let prepared = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(weth()),
                    raw: amount_in_raw,
                },
                Currency::Token(tc_ng_token()),
                &quoted_out,
                &opts,
            )
            .expect("CryptoU256Receiver build_swap must succeed");

        let decoded = ICurveCryptoReceiver::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as exchange(uint256,uint256,uint256,uint256,address)");

        assert_eq!(
            decoded.receiver, recipient,
            "receiver field must equal the resolved recipient (0xCD…)"
        );
    }

    // ── CryptoU256UseEth: native-in sets value, use_eth, skips approval ─────

    /// ETH-in on a `CryptoU256UseEth` pool must set `use_eth=true`, carry
    /// `amount_in.raw` as `tx.value`, and omit the ERC-20 approval (ETH is
    /// sent as value, not pulled via transferFrom).
    ///
    /// Pool coins: USDT(0), WBTC(1), WETH(2). Input: ETH (→ WETH index 2).
    /// Output: WBTC (index 1). `ctx.weth == weth()` so resolve_swap maps
    /// Currency::Native → weth(), coin_indices finds index 2.
    #[test]
    fn eth_in_sets_value_and_use_eth_and_skips_approval() {
        let pool = crypto_3pool();
        let c = ctx(); // ctx.weth == weth() == pool coin index 2
        let recipient = Address::repeat_byte(0xAA);
        let opts = opts_resolved(recipient, 9_999_999, 50);

        let dx = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        let quoted_out = AssetAmount::new(wbtc(), U256::from(5_000_000u64)); // synthetic
        // Route uses weth() as the canonical from-asset (what resolve_swap sees
        // after mapping Native→weth()); the route from/to assets must be members.
        let prepared = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Native,
                    raw: dx,
                },
                Currency::Token(wbtc()),
                &quoted_out,
                &opts,
            )
            .expect("ETH-in CryptoU256UseEth must succeed");

        // tx.value must carry the ETH amount.
        assert_eq!(
            prepared.tx.value, dx,
            "tx.value must equal amount_in.raw for native-in"
        );

        // Calldata: use_eth=true, i=2 (WETH), j=1 (WBTC).
        let decoded = ICurveCryptoUseEth::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as CryptoUseEth exchange");
        assert!(decoded.use_eth, "use_eth must be true for native-in");
        assert_eq!(decoded.i, U256::from(2u64), "i must be 2 (WETH/ETH index)");
        assert_eq!(decoded.j, U256::from(1u64), "j must be 1 (WBTC index)");
        assert_eq!(decoded.dx, dx, "dx must equal amount_in.raw");

        // No ERC-20 approval — ETH is sent as value, not pulled via transferFrom.
        assert!(
            prepared.approval.is_none(),
            "approval must be None for native-in"
        );
    }

    // ── CryptoU256UseEth: native-out sets use_eth, keeps value=0 + approval ──

    /// ETH-out on a `CryptoU256UseEth` pool must set `use_eth=true`, keep
    /// `tx.value=0` (input is an ERC-20 token), and include an ERC-20 approval
    /// for the input token.
    ///
    /// Input: WBTC (index 1, ERC-20). Output: ETH (→ WETH index 2 after
    /// resolve). Pool must unwrap the WETH output before delivery; caller sends
    /// no ETH (value=0) but must approve the pool to pull WBTC.
    #[test]
    fn eth_out_uses_zero_value_and_approves_input() {
        let pool = crypto_3pool();
        let c = ctx();
        let recipient = Address::repeat_byte(0xBB);
        let opts = opts_resolved(recipient, 9_999_999, 50);

        let dx = U256::from(5_000_000u64); // 0.05 WBTC (8 dec)
        let quoted_out = AssetAmount::new(weth(), U256::from(1_000_000_000_000_000_000u64));
        let prepared = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(wbtc()),
                    raw: dx,
                },
                Currency::Native,
                &quoted_out,
                &opts,
            )
            .expect("ETH-out CryptoU256UseEth must succeed");

        // Not payable — input is ERC-20, ETH goes out, not in.
        assert_eq!(
            prepared.tx.value,
            U256::ZERO,
            "tx.value must be 0 for native-out"
        );

        // Calldata: use_eth=true, i=1 (WBTC), j=2 (WETH).
        let decoded = ICurveCryptoUseEth::exchangeCall::abi_decode(&prepared.tx.data)
            .expect("calldata must decode as CryptoUseEth exchange");
        assert!(decoded.use_eth, "use_eth must be true for native-out");
        assert_eq!(decoded.i, U256::from(1u64), "i must be 1 (WBTC index)");
        assert_eq!(decoded.j, U256::from(2u64), "j must be 2 (WETH/ETH index)");
        assert_eq!(decoded.dx, dx, "dx must equal amount_in.raw");

        // ERC-20 approval for the input token (WBTC) — pool pulls via transferFrom.
        let approval = prepared
            .approval
            .expect("approval must be Some for ERC-20 input");
        assert_eq!(
            approval.spender, CRYPTO_POOL_ADDR,
            "spender must be the pool"
        );
        assert_eq!(
            approval.token,
            wbtc(),
            "approval must target the input token"
        );
        assert_eq!(approval.min_allowance, dx, "min_allowance must equal dx");
    }

    // ── Non-use-eth interfaces reject native currency ─────────────────────────

    /// `StableI128` does not accept native ETH on either side. Passing
    /// `Currency::Native` as input must return `UnsupportedProtocol` immediately
    /// (before any coin-lookup), regardless of whether the pool's coins include
    /// the WETH asset.
    #[test]
    fn non_use_eth_interface_rejects_native() {
        let pool = stable_3pool(); // StableI128, coins: DAI/USDC/USDT
        let c = ctx();
        let opts = opts_resolved(Address::repeat_byte(0x55), 9_999_999, 50);

        // Native input → StableI128 must reject.
        let err_in = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Native,
                    raw: U256::from(1_000u64),
                },
                Currency::Token(usdc()),
                &AssetAmount::new(usdc(), U256::from(1_000u64)),
                &opts,
            )
            .expect_err("StableI128 + native-in must return UnsupportedProtocol");
        assert_eq!(err_in, BuildError::UnsupportedProtocol);

        // Native output → StableI128 must reject.
        let err_out = pool
            .build_swap(
                &c,
                CurrencyAmount {
                    currency: Currency::Token(dai()),
                    raw: U256::from(1_000u64),
                },
                Currency::Native,
                &AssetAmount::new(weth(), U256::from(1_000u64)),
                &opts,
            )
            .expect_err("StableI128 + native-out must return UnsupportedProtocol");
        assert_eq!(err_out, BuildError::UnsupportedProtocol);
    }

    // ── as_executable dispatch ────────────────────────────────────────────────

    /// `as_executable` must return `Some` for a `CurvePool`.
    #[test]
    fn as_executable_is_some_for_curve_pool() {
        let pool = stable_3pool();
        assert!(as_executable(&pool as &dyn Pool).is_some());
    }
}
