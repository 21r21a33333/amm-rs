//! Offline quoting — build a Uniswap V2 pool from reserves and quote it with no
//! network and no `amm-rpc`. This is the pure-math core in isolation.
//!
//! Run: `cargo run -p amm-core --example quote_offline --features uniswap-v2`

use alloy_primitives::{U256, address};
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::PoolId;
use amm_core::primitives::ratio::Bps;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_core::slippage::Slippage;
use amm_core::traits::exact_out::ExactOut;
use amm_core::traits::pool::Pool;

fn main() {
    let usdc = AssetId::new(
        ChainId(1),
        address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").into_word(),
    );
    let weth = AssetId::new(
        ChainId(1),
        address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").into_word(),
    );

    // Reserves are index-aligned with [token0, token1]: ~30M USDC / ~10k WETH.
    let e18 = U256::from(1_000_000_000_000_000_000u128);
    let pool = UniswapV2Pool::new(
        PoolId::new("1:uniswap-v2:0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
        [usdc, weth],
        [
            U256::from(30_000_000_000_000u128),
            U256::from(10_000u128) * e18,
        ],
        30, // 0.30% fee
    );

    // Exact-in: 1000 USDC -> WETH. `AssetAmount` carries its token, so the output
    // is unambiguous and a wrong-token input is a typed error (see below).
    let amount_in = AssetAmount::new(usdc, U256::from(1_000_000_000u64)); // 1000 USDC (6 dp)
    let out = pool.quote(&amount_in, &weth).expect("quote");
    let min_out = Slippage::from_bps(Bps(50)).min_amount_out(&out); // 0.5% tolerance
    println!(
        "exact-in : 1000 USDC -> {} WETH wei  (min @0.5% slippage: {})",
        out.raw, min_out.raw
    );

    // Exact-out: how much USDC to receive exactly 1 WETH?
    let want = AssetAmount::new(weth, e18);
    let needed = pool.quote_exact_out(&want, &usdc).expect("exact-out");
    println!("exact-out: {} USDC wei -> 1 WETH", needed.raw);

    // A token this pool does not hold is a typed error, never a wrong number.
    let dai = AssetId::new(
        ChainId(1),
        address!("0x6b175474e89094c44da98b954eedeac495271d0f").into_word(),
    );
    assert!(
        pool.quote(&AssetAmount::new(dai, U256::from(1u64)), &weth)
            .is_err()
    );
    println!("wrong-token quote correctly rejected");
}
