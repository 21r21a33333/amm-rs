//! One trait, many protocols — quote the same trade across different AMM math
//! through a uniform `&dyn Pool`, and read the auxiliary `Pricing` / `Introspect`
//! facets each pool exposes.
//!
//! Every pool type in `amm-core` (Uniswap V2/V3/V4, Curve, Aerodrome volatile &
//! stable, Slipstream) implements the same object-safe `Pool` trait, so a router
//! can hold a heterogeneous `Vec<Box<dyn Pool>>` and treat them identically.
//! This example contrasts the constant-product invariant (`x·y = k`) with the
//! Solidly stable invariant (`x³y + y³x = k`) on the SAME reserves and fee — the
//! stable curve gives far less slippage on a large like-for-like trade.
//!
//! Run: `cargo run -p amm-core --example multi_protocol --features "uniswap-v2 aerodrome"`

use alloy_primitives::{Address, U256, address};
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::PoolId;
use amm_core::protocols::aerodrome::stable::AerodromeStablePool;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_core::traits::pool::Pool;

fn asset(addr: Address) -> AssetId {
    AssetId::new(ChainId(1), addr.into_word())
}

fn main() {
    let e18 = U256::from(10u128).pow(U256::from(18u8));
    // Two synthetic 18-dp stablecoins, address-sorted (USDA < USDB).
    let usda = asset(address!("0x1111111111111111111111111111111111111111"));
    let usdb = asset(address!("0x2222222222222222222222222222222222222222"));
    let reserves = [
        U256::from(1_000_000u128) * e18,
        U256::from(1_000_000u128) * e18,
    ];
    let fee_bps = 5; // same fee on both, so the difference is purely the invariant

    // Heterogeneous pools behind one trait object type.
    let pools: Vec<(&str, Box<dyn Pool>)> = vec![
        (
            "Uniswap V2 (x·y=k)",
            Box::new(UniswapV2Pool::new(
                PoolId::new("1:univ2:usda-usdb"),
                [usda, usdb],
                reserves,
                fee_bps,
            )),
        ),
        (
            "Aerodrome stable (x³y+y³x)",
            Box::new(AerodromeStablePool::new(
                PoolId::new("1:aero-stable:usda-usdb"),
                [usda, usdb],
                reserves,
                [18, 18],
                fee_bps,
            )),
        ),
    ];

    // A large trade: 100k of a 1M-deep pool (10%) — slippage is very visible.
    let amount_in = AssetAmount::new(usda, U256::from(100_000u128) * e18);
    println!("swap 100,000 USDA -> USDB  (both pools: 1M/1M reserves, 5 bps fee)\n");
    for (name, pool) in &pools {
        let out = pool.quote(&amount_in, &usdb).expect("quote");
        // `price_impact` and `fee_bps` are optional facets a pool may expose.
        let impact = pool
            .as_pricing()
            .and_then(|p| p.price_impact(&amount_in, &usdb).ok())
            .map(|bps| format!("{} bps", bps.0))
            .unwrap_or_else(|| "n/a".into());
        let fee = pool
            .as_introspect()
            .and_then(|i| i.fee_bps(&usda, &usdb))
            .map(|bps| format!("{} bps", bps.0))
            .unwrap_or_else(|| "n/a".into());
        println!(
            "{name:<28} out = {:>24} wei   price-impact {impact:<10} fee {fee}",
            out.raw
        );
    }
    println!("\nThe stable curve delivers far more output on a like-for-like trade.");
}
