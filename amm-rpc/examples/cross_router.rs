//! Cross-router execution — a route whose hops settle through DIFFERENT routers,
//! so it CANNOT be one atomic transaction. No RPC.
//!
//! Route: A →(Curve stableswap)→ B →(Uniswap V2)→ C. The planner partitions this
//! into two spans — one Curve `exchange`, one Universal Router `execute` — and
//! `is_atomic()` is false with `tx_count() == 2`. Each earlier span delivers to
//! the sender, whose on-chain output you thread back into `next_tx` as the next
//! span's real input. (A same-router multi-hop, by contrast, is a single atomic
//! tx — see `multihop_execution`.)
//!
//! Requires the `curve` feature:
//! `cargo run -p amm-rpc --example cross_router --features curve`

use alloy::primitives::{Address, U256, address};
use amm_core::protocols::curve::interface::CurveInterface;
use amm_core::protocols::curve::pool::CurvePool;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_rpc::execution::routing::Route;
use amm_rpc::execution::{
    ExactOutPolicy, ExecutionOptions, NativeEdge, Recipient, TradeType, chains, plan, resolve,
};
use amm_rpc::{AssetAmount, AssetId, Bps, ChainId, Pool, PoolId, Slippage};
use curve_math::Pool as CurveMathPool;
use std::time::{SystemTime, UNIX_EPOCH};

fn asset(addr: Address) -> AssetId {
    AssetId::new(ChainId(1), addr.into_word())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let e18 = U256::from(10u128).pow(U256::from(18u8));
    // Synthetic 18-dp coins A < B < C < D (address-sorted).
    let a = asset(address!("0x1111111111111111111111111111111111111111"));
    let b = asset(address!("0x2222222222222222222222222222222222222222"));
    let c = asset(address!("0x3333333333333333333333333333333333333333"));
    let d = asset(address!("0x4444444444444444444444444444444444444444"));

    // Curve 3-coin stableswap [A, B, D], 1M each. `with_execution` attaches the
    // on-chain pool address + ABI family the calldata encoder needs.
    let bal = U256::from(1_000_000u128) * e18;
    let curve = CurvePool::new(
        PoolId::new("1:curve:abc"),
        vec![a, b, d],
        CurveMathPool::StableSwapV1 {
            balances: vec![bal, bal, bal],
            rates: vec![e18, e18, e18],
            amp: U256::from(2_000u64),
            fee: U256::from(1_000_000u64),
        },
    )
    .with_execution(
        address!("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
        CurveInterface::StableI128,
    );
    // Uniswap V2 [B, C], 1M/1M.
    let univ2 = UniswapV2Pool::new(PoolId::new("1:univ2:bc"), [b, c], [bal, bal], 30);

    let route = Route {
        pools: vec![&curve, &univ2],
        path: vec![a, b, c],
        trade_type: TradeType::ExactIn,
    };

    let cfg = chains::ethereum();
    let sender = address!("0x1111111111111111111111111111111111111111");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let opts = resolve(
        ExecutionOptions::new(Slippage::from_bps(Bps(50))).with_recipient(Recipient::To(sender)),
        now,
        sender,
    );

    let amount_in = U256::from(1_000u128) * e18;
    let mut plan = plan(
        &cfg,
        &route,
        amount_in,
        &opts,
        sender,
        NativeEdge::None,
        ExactOutPolicy::Strict,
    )?;
    println!(
        "atomic: {}   transactions: {}   (route crosses Curve + Uniswap)",
        plan.is_atomic(),
        plan.tx_count()
    );

    // Drive both spans. The intermediate (B) landing on the sender after span 1
    // is the real input to span 2 — here we quote it locally to stand in for the
    // on-chain receipt you'd read in a live integration.
    let mut observed = None;
    for i in 0..plan.tx_count() {
        let prepared = plan.next_tx(observed)?.expect("span");
        println!(
            "\ntx #{i}: to {}  ({} calldata bytes)  min out {} wei",
            prepared.tx.to,
            prepared.tx.data.len(),
            prepared.min_received.raw
        );
        // Stand-in for the next span's real input: quote hop 0 (A -> B) locally.
        observed = Some(curve.quote(&AssetAmount::new(a, amount_in), &b)?);
    }
    Ok(())
}
