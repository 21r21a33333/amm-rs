//! Multi-hop execution with the `plan()` executor — no RPC.
//!
//! Builds a 2-hop route (USDC → WETH → DAI) across two Uniswap pools and plans
//! it. Because both pools settle through the Uniswap Universal Router, the whole
//! route is ONE atomic span → ONE transaction (`is_atomic() == true`). The
//! `next_tx` loop is the general driver: it yields one sign-ready transaction per
//! span, and you thread each span's observed on-chain output back into the next
//! call (irrelevant here with a single span, shown for the cross-router case).
//!
//! Run: `cargo run -p amm-rpc --example multihop_execution`

use alloy::primitives::{U256, address};
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_rpc::execution::routing::Route;
use amm_rpc::execution::{
    ExactOutPolicy, ExecutionOptions, NativeEdge, Recipient, TradeType, chains, plan, resolve,
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

    // A Route is an ordered pool list + the token path (one longer than the pool
    // list) + the trade direction. Pools are borrowed as `&dyn Pool`.
    let route = Route {
        pools: vec![&usdc_weth, &weth_dai],
        path: vec![usdc, weth, dai],
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

    // Plan 1,000 USDC → DAI, exact-in, no native edges.
    let mut plan = plan(
        &cfg,
        &route,
        U256::from(1_000_000_000u64),
        &opts,
        sender,
        NativeEdge::None,
        ExactOutPolicy::Strict,
    )?;

    println!(
        "atomic: {}   transactions: {}   spans: {}",
        plan.is_atomic(),
        plan.tx_count(),
        plan.spans().len()
    );

    // Drive the plan. `next_tx` returns one PreparedSwap per span; feed the
    // previous span's observed output back in (None for the first span).
    let mut observed = None;
    let mut i = 0;
    while let Some(prepared) = plan.next_tx(observed)? {
        println!("\ntx #{i}:");
        println!("  to        {}", prepared.tx.to);
        println!("  calldata  {} bytes", prepared.tx.data.len());
        println!(
            "  min out   {} wei {}",
            prepared.min_received.raw, prepared.min_received.asset
        );
        if let Some(a) = &prepared.approval {
            println!(
                "  approval  spend {} of {} via {}",
                a.min_allowance, a.token, a.spender
            );
        }
        // In a real integration you'd sign+send `prepared.tx`, read the actual
        // output from the receipt, and pass it back as `observed`.
        observed = None;
        i += 1;
    }
    Ok(())
}
