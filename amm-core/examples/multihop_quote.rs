//! Multi-hop quoting with the path helpers — no execution, no network.
//!
//! Chains two constant-product pools (USDC → WETH → DAI) and quotes the route
//! both ways:
//!   * `quote_path_amounts` — forward: given the input, what comes out of each hop.
//!   * `quote_path_exact_out` — backward: given the desired final output, how much
//!     input each hop needs (the "how much do I spend to receive exactly X?" solve).
//!
//! Run: `cargo run -p amm-core --example multihop_quote --features uniswap-v2`

use alloy_primitives::U256;
use amm_core::path::{Hop, quote_path_amounts, quote_path_exact_out};
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::PoolId;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_core::traits::pool::Pool;

fn asset(hex: &str) -> AssetId {
    AssetId::new(
        ChainId(1),
        hex.parse::<alloy_primitives::Address>()
            .unwrap()
            .into_word(),
    )
}

fn main() {
    let e18 = U256::from(10u128).pow(U256::from(18u8));
    let usdc = asset("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let weth = asset("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let dai = asset("0x6B175474E89094C44Da98b954EedeAC495271d0F");

    // Pool A: USDC/WETH — 50M USDC (6 dp) / 15k WETH (18 dp).
    let a = UniswapV2Pool::new(
        PoolId::new("1:univ2:usdc-weth"),
        [usdc, weth],
        [
            U256::from(50_000_000_000_000u128),
            U256::from(15_000u128) * e18,
        ],
        30,
    );
    // Pool B: WETH/DAI — 15k WETH / 50M DAI (both 18 dp).
    let b = UniswapV2Pool::new(
        PoolId::new("1:univ2:weth-dai"),
        [weth, dai],
        [
            U256::from(15_000u128) * e18,
            U256::from(50_000_000u128) * e18,
        ],
        30,
    );

    // A path is an ordered list of hops; each hop names the pool and its OUTPUT
    // asset (which becomes the next hop's input). The pools are borrowed as
    // `&dyn Pool`, so any protocol can appear at any hop.
    let hops = [
        Hop {
            pool: &a as &dyn Pool,
            to: weth,
        },
        Hop {
            pool: &b as &dyn Pool,
            to: dai,
        },
    ];

    // ── Forward: exact-in ─────────────────────────────────────────────────────
    // 1,000 USDC in. `amounts` is [in, after-hop-1, after-hop-2].
    let start = AssetAmount::new(usdc, U256::from(1_000_000_000u64));
    let amounts = quote_path_amounts(&start, &hops).expect("forward quote");
    println!("exact-in  1000 USDC ->");
    println!("  hop 1 (USDC->WETH): {} wei WETH", amounts[1].raw);
    println!("  hop 2 (WETH->DAI) : {} wei DAI", amounts[2].raw);

    // ── Backward: exact-out ───────────────────────────────────────────────────
    // "I want exactly 1,000 DAI out — how much of each asset must enter each hop?"
    // `required` is [input-USDC, input-WETH, target-DAI].
    let target = AssetAmount::new(dai, U256::from(1_000u128) * e18);
    let required = quote_path_exact_out(&target, usdc, &hops).expect("backward solve");
    println!("exact-out 1000 DAI <-");
    println!("  needs {} wei USDC in", required[0].raw);
    println!("  (via {} wei WETH mid-hop)", required[1].raw);
}
