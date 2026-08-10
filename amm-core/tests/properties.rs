//! Property-based invariants for the pure quoters.
//!
//! Unlike the fork proofs (a handful of real pools) and unit tests (fixed
//! fixtures), these generate pool states directly and assert the laws every
//! constant-function market maker must obey. A pure library can synthesise valid
//! states cheaply, so `proptest` reaches the corners a fixture set never does —
//! which is where rounding-direction and solver bugs hide.
//!
//! Invariants asserted, direction-agnostic, for every generated `(pool, size)`:
//! - **determinism** — the same quote twice is identical.
//! - **monotonicity** — a larger input never yields a smaller output.
//! - **output bounded** — a reserve pool never pays out more than it holds.
//! - **round-trip** — the exact-out input for `quote(x)` never exceeds `x`, its
//!   quote covers the target, and one wei less under-fills (exact minimality).

#![cfg(any(
    feature = "uniswap-v2",
    feature = "uniswap-v3",
    feature = "curve",
    feature = "aerodrome"
))]

use alloy_primitives::{B256, U256};
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::PoolId;
use amm_core::traits::pool::Pool;
use proptest::prelude::*;

fn a() -> AssetId {
    AssetId::new(ChainId(1), B256::left_padding_from(&[0x0a]))
}
fn b() -> AssetId {
    AssetId::new(ChainId(1), B256::left_padding_from(&[0x0b]))
}

/// Assert the quote invariants for one pool at two ordered input sizes.
/// `lo ≤ hi`. Checks that only make sense on a successful quote are skipped when
/// the pool refuses the size (insufficient liquidity / window exceeded), so a
/// refusal is never a false failure.
fn check_invariants(
    pool: &dyn Pool,
    from: AssetId,
    to: AssetId,
    lo: U256,
    hi: U256,
) -> Result<(), TestCaseError> {
    let quote = |x: U256| pool.quote(&AssetAmount::new(from, x), &to).map(|q| q.raw);

    // determinism: the same input quotes identically.
    prop_assert_eq!(quote(lo), quote(lo));

    // monotonicity: a larger input never returns a smaller output.
    if let (Ok(out_lo), Ok(out_hi)) = (quote(lo), quote(hi)) {
        prop_assert!(
            out_hi >= out_lo,
            "monotonicity: quote({hi}) = {out_hi} < quote({lo}) = {out_lo}"
        );
    }

    let Ok(out) = quote(hi) else {
        return Ok(());
    };

    // output bounded: a reserve pool never pays out more than it holds.
    if let Some(reserve) = pool.as_introspect().and_then(|i| i.reserve(&to)) {
        prop_assert!(
            out < reserve.raw,
            "output {out} ≥ reserve {} of {to}",
            reserve.raw
        );
    }

    // round-trip + exact-out minimality (only when there is a nonzero output to
    // reverse-solve for).
    if let (Some(exact_out), true) = (pool.as_exact_out(), !out.is_zero()) {
        if let Ok(need) = exact_out.quote_exact_out(&AssetAmount::new(to, out), &from) {
            // The input to receive `out` never exceeds the input that produced it.
            prop_assert!(need.raw <= hi, "round-trip: need {} > input {hi}", need.raw);
            // That input actually covers the target...
            prop_assert!(
                quote(need.raw).is_ok_and(|q| q >= out),
                "exact-out under-covers: need {} -> {:?} < {out}",
                need.raw,
                quote(need.raw)
            );
            // ...and is minimal — one wei less under-fills.
            if !need.raw.is_zero() {
                prop_assert!(
                    quote(need.raw - U256::from(1u64)).is_ok_and(|q| q < out),
                    "exact-out not minimal: need-1 still delivers ≥ {out}"
                );
            }
        }
    }
    Ok(())
}

fn e18(mul: u64) -> U256 {
    U256::from(mul) * U256::from(1_000_000_000_000_000_000u64)
}

// ─── Uniswap V2 (constant product) ───────────────────────────────────────────

#[cfg(feature = "uniswap-v2")]
proptest! {
    #[test]
    fn v2_invariants(
        r0 in 1_000u64..1_000_000_000u64,
        r1 in 1_000u64..1_000_000_000u64,
        fee in 0u32..=1_000u32,
        x0 in 1u64..500_000_000u64,
        x1 in 1u64..500_000_000u64,
    ) {
        use amm_core::protocols::uniswap::v2::UniswapV2Pool;
        let pool = UniswapV2Pool::new(
            PoolId::new("1:v2:0x0"),
            [a(), b()],
            [e18(r0), e18(r1)],
            fee,
        );
        let (lo, hi) = (e18(x0.min(x1)), e18(x0.max(x1)));
        check_invariants(&pool, a(), b(), lo, hi)?;
    }

    /// Fee monotonicity: a strictly higher fee never returns more output.
    #[test]
    fn v2_higher_fee_never_pays_more(
        r0 in 1_000u64..1_000_000_000u64,
        r1 in 1_000u64..1_000_000_000u64,
        f_lo in 0u32..500u32,
        bump in 1u32..500u32,
        x in 1u64..100_000_000u64,
    ) {
        use amm_core::protocols::uniswap::v2::UniswapV2Pool;
        let mk = |fee| UniswapV2Pool::new(PoolId::new("1:v2:0x0"), [a(), b()], [e18(r0), e18(r1)], fee);
        let inp = AssetAmount::new(a(), e18(x));
        let out_lo = mk(f_lo).quote(&inp, &b()).unwrap().raw;
        let out_hi = mk(f_lo + bump).quote(&inp, &b()).unwrap().raw;
        prop_assert!(out_hi <= out_lo, "higher fee paid more: {out_hi} > {out_lo}");
    }
}

// ─── Aerodrome volatile (constant product, fee-first) ────────────────────────

#[cfg(feature = "aerodrome")]
proptest! {
    #[test]
    fn aerodrome_volatile_invariants(
        r0 in 1_000u64..1_000_000_000u64,
        r1 in 1_000u64..1_000_000_000u64,
        fee in 0u32..=1_000u32,
        x0 in 1u64..500_000_000u64,
        x1 in 1u64..500_000_000u64,
    ) {
        use amm_core::protocols::aerodrome::volatile::AerodromeVolatilePool;
        let pool = AerodromeVolatilePool::new(
            PoolId::new("1:aero-v:0x0"),
            [a(), b()],
            [e18(r0), e18(r1)],
            fee,
        );
        let (lo, hi) = (e18(x0.min(x1)), e18(x0.max(x1)));
        check_invariants(&pool, a(), b(), lo, hi)?;
    }
}

// ─── Aerodrome stable (x³y + y³x) ────────────────────────────────────────────

#[cfg(feature = "aerodrome")]
proptest! {
    #[test]
    fn aerodrome_stable_invariants(
        r0 in 1_000u64..100_000_000u64,
        r1 in 1_000u64..100_000_000u64,
        dec0 in 6u8..=18u8,
        dec1 in 6u8..=18u8,
        fee in 0u32..=1_000u32,
        x0 in 1u64..50_000_000u64,
        x1 in 1u64..50_000_000u64,
    ) {
        use amm_core::protocols::aerodrome::stable::AerodromeStablePool;
        let unit = |d: u8| U256::from(10u128).pow(U256::from(d));
        let pool = AerodromeStablePool::new(
            PoolId::new("1:aero-s:0x0"),
            [a(), b()],
            [U256::from(r0) * unit(dec0), U256::from(r1) * unit(dec1)],
            [dec0, dec1],
            fee,
        );
        let (x_lo, x_hi) = (x0.min(x1), x0.max(x1));
        check_invariants(&pool, a(), b(), U256::from(x_lo) * unit(dec0), U256::from(x_hi) * unit(dec0))?;
    }
}

// ─── Curve StableSwap ────────────────────────────────────────────────────────

#[cfg(feature = "curve")]
proptest! {
    #[test]
    fn curve_stableswap_invariants(
        b0 in 1_000u64..100_000_000u64,
        b1 in 1_000u64..100_000_000u64,
        amp in 1u64..100_000u64,
        fee in 0u64..100_000_000u64,
        x0 in 1u64..50_000_000u64,
        x1 in 1u64..50_000_000u64,
    ) {
        use amm_core::protocols::curve::pool::CurvePool;
        use curve_math::Pool as CurveMathPool;
        let inner = CurveMathPool::StableSwapV1 {
            balances: vec![e18(b0), e18(b1)],
            rates: vec![e18(1), e18(1)],
            amp: U256::from(amp),
            fee: U256::from(fee),
        };
        let pool = CurvePool::new(PoolId::new("1:curve:0x0"), vec![a(), b()], inner);
        let (lo, hi) = (e18(x0.min(x1)), e18(x0.max(x1)));
        check_invariants(&pool, a(), b(), lo, hi)?;
    }
}

// ─── Uniswap V3 (concentrated, full-range at price 1) ────────────────────────

#[cfg(feature = "uniswap-v3")]
proptest! {
    #[test]
    fn v3_full_range_invariants(
        liq in 1u64..10_000_000u64,
        fee in 100u32..=10_000u32,
        x0 in 1u64..1_000_000u64,
        x1 in 1u64..1_000_000u64,
    ) {
        use amm_core::protocols::uniswap::v3::{TickData, TickInfo, UniswapV3Pool};
        // sqrt price 1 (2^96), tick 0, a single full-range position.
        const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;
        let l = e18(liq).to::<u128>();
        let (lo_tick, hi_tick) = (-887_220i32, 887_220i32);
        let ticks = TickData::from_ticks(
            60,
            [
                (lo_tick, TickInfo { liquidity_net: l as i128, initialized: true }),
                (hi_tick, TickInfo { liquidity_net: -(l as i128), initialized: true }),
            ],
        );
        let pool = UniswapV3Pool::new(
            PoolId::new("1:v3:0x0"),
            [a(), b()],
            U256::from(SQRT_1_1),
            l,
            0,
            fee,
            ticks,
        );
        let (lo, hi) = (e18(x0.min(x1)), e18(x0.max(x1)));
        check_invariants(&pool, a(), b(), lo, hi)?;
    }

    /// Nested positions of increasing width around tick 0 (price 1), so active
    /// liquidity steps down as a swap crosses each boundary. A large enough input
    /// crosses several initialized ticks — exercising the tick-transition
    /// accounting a single-region full-range pool never touches.
    #[test]
    fn v3_multi_tick_invariants(
        l0 in 1u64..5_000_000u64,
        l1 in 1u64..5_000_000u64,
        l2 in 1u64..5_000_000u64,
        fee in 100u32..=10_000u32,
        x0 in 1u64..2_000_000u64,
        x1 in 1u64..2_000_000u64,
    ) {
        use amm_core::protocols::uniswap::v3::{TickData, TickInfo, UniswapV3Pool};
        const SQRT_1_1: u128 = 79_228_162_514_264_337_593_543_950_336;
        const S: i32 = 60;
        let (q0, q1, q2) = (e18(l0).to::<u128>(), e18(l1).to::<u128>(), e18(l2).to::<u128>());
        let tick = |t: i32, net: i128| (t, TickInfo { liquidity_net: net, initialized: true });
        // position k spans [-(k+1)·S, (k+1)·S]: adds liquidity at its lower bound,
        // removes it at its upper. Active liquidity at tick 0 is the sum.
        let ticks = TickData::from_ticks(
            S,
            [
                tick(-3 * S, q2 as i128),
                tick(-2 * S, q1 as i128),
                tick(-S, q0 as i128),
                tick(S, -(q0 as i128)),
                tick(2 * S, -(q1 as i128)),
                tick(3 * S, -(q2 as i128)),
            ],
        );
        let pool = UniswapV3Pool::new(
            PoolId::new("1:v3:0x0"),
            [a(), b()],
            U256::from(SQRT_1_1),
            q0 + q1 + q2,
            0,
            fee,
            ticks,
        );
        let (lo, hi) = (e18(x0.min(x1)), e18(x0.max(x1)));
        check_invariants(&pool, a(), b(), lo, hi)?;
    }
}
