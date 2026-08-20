//! The advanced trade surface: exact-out (Strict vs OrBetter) and native-ETH
//! edges — no RPC.
//!
//! * **Exact-out Strict** — deliver EXACTLY the requested output; the input is
//!   bounded by `max_spent`. Only families with a true reverse router entry
//!   (Uniswap, Slipstream) support this; others reject.
//! * **Exact-out OrBetter** — deliver AT LEAST the requested output. The executor
//!   backward-solves the input, then runs an exact-in plan with the final floor
//!   pinned to the target. Works for every family (it degrades to exact-in).
//! * **Native edges** — `NativeEdge::Output` unwraps the final WETH to native ETH
//!   for the recipient (and `Input` wraps ETH the caller sends). Native only ever
//!   appears at a route endpoint.
//!
//! Run: `cargo run -p amm-rpc --example exact_out_and_native`

use alloy::primitives::{U256, address};
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_rpc::execution::routing::Route;
use amm_rpc::execution::{
    self, ExactOutPolicy, ExecutionOptions, NativeEdge, Plan, Recipient, TradeType, chains, resolve,
};
use amm_rpc::{AssetId, Bps, ChainId, PoolId, Slippage};
use std::time::{SystemTime, UNIX_EPOCH};

fn asset(hex: &str) -> AssetId {
    AssetId::new(
        ChainId(1),
        hex.parse::<alloy::primitives::Address>()
            .unwrap()
            .into_word(),
    )
}

fn report(label: &str, mut plan: Plan<'_>) -> Result<(), Box<dyn std::error::Error>> {
    println!("── {label} ──");
    let prepared = plan.next_tx(None)?.expect("one span");
    println!(
        "  min received : {} wei {}",
        prepared.min_received.raw, prepared.min_received.asset
    );
    match &prepared.max_spent {
        Some(m) => println!(
            "  max spent    : {} wei {} (exact-out bound)",
            m.raw, m.asset
        ),
        None => println!("  max spent    : unbounded (exact-in / OrBetter)"),
    }
    println!(
        "  calldata     : {} bytes to {}\n",
        prepared.tx.data.len(),
        prepared.tx.to
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let e18 = U256::from(10u128).pow(U256::from(18u8));
    let usdc = asset("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let weth = asset("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let dai = asset("0x6B175474E89094C44Da98b954EedeAC495271d0F");

    let usdc_weth = UniswapV2Pool::new(
        PoolId::new("1:univ2:usdc-weth"),
        [usdc, weth],
        [
            U256::from(50_000_000_000_000u128),
            U256::from(15_000u128) * e18,
        ],
        30,
    );
    let weth_dai = UniswapV2Pool::new(
        PoolId::new("1:univ2:weth-dai"),
        [weth, dai],
        [
            U256::from(15_000u128) * e18,
            U256::from(50_000_000u128) * e18,
        ],
        30,
    );

    let cfg = chains::ethereum();
    let sender = address!("0x1111111111111111111111111111111111111111");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let opts = resolve(
        ExecutionOptions::new(Slippage::from_bps(Bps(50))).with_recipient(Recipient::To(sender)),
        now,
        sender,
    );

    // 1) Exact-out STRICT: spend USDC to receive EXACTLY 1 WETH.
    let route = Route {
        pools: vec![&usdc_weth],
        path: vec![usdc, weth],
        trade_type: TradeType::ExactOut,
    };
    let p = execution::plan(
        &cfg,
        &route,
        e18,
        &opts,
        sender,
        NativeEdge::None,
        ExactOutPolicy::Strict,
    )?;
    report("exact-out Strict: receive exactly 1 WETH", p)?;

    // 2) Exact-out OrBetter over 2 hops: receive AT LEAST 1,000 DAI.
    let route2 = Route {
        pools: vec![&usdc_weth, &weth_dai],
        path: vec![usdc, weth, dai],
        trade_type: TradeType::ExactOut,
    };
    let p = execution::plan(
        &cfg,
        &route2,
        U256::from(1_000u128) * e18,
        &opts,
        sender,
        NativeEdge::None,
        ExactOutPolicy::OrBetter,
    )?;
    report("exact-out OrBetter: receive at least 1,000 DAI", p)?;

    // 3) Native output: swap USDC and receive native ETH (the router unwraps WETH).
    let route3 = Route {
        pools: vec![&usdc_weth],
        path: vec![usdc, weth],
        trade_type: TradeType::ExactIn,
    };
    let p = execution::plan(
        &cfg,
        &route3,
        U256::from(1_000_000_000u64),
        &opts,
        sender,
        NativeEdge::Output,
        ExactOutPolicy::Strict,
    )?;
    report("native-out: 1,000 USDC -> ETH (unwrapped)", p)?;
    Ok(())
}
