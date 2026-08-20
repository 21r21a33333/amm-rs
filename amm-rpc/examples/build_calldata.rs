//! Quote a swap and build sign-ready calldata — the `amm-rpc` execution half.
//!
//! Self-contained (no RPC): it constructs a USDC/WETH Uniswap V2 pool from
//! literal reserves, quotes USDC → WETH, and builds the Universal Router
//! transaction + Permit2 approval using the bundled mainnet [`chains`] preset.
//!
//! Run: `cargo run -p amm-rpc --example build_calldata`
//!
//! [`chains`]: amm_rpc::execution::chains

use alloy::primitives::{U256, address};
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_rpc::execution::{
    Currency, CurrencyAmount, ExecutionOptions, Recipient, as_executable, chains, resolve,
};
use amm_rpc::{AssetAmount, AssetId, Bps, ChainId, Pool, PoolId, Slippage};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let chain = ChainId(1);
    // Address-sorted [token0, token1]: USDC (0xa0b8…) < WETH (0xC02a…).
    let usdc = AssetId::new(
        chain,
        address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").into_word(),
    );
    let weth = AssetId::new(
        chain,
        address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").into_word(),
    );

    // A USDC/WETH V2 pool: 50M USDC (6 dp) and 15k WETH (18 dp), 30 bps fee.
    let pool = UniswapV2Pool::new(
        PoolId::new("1:univ2:usdc-weth"),
        [usdc, weth],
        [
            U256::from(50_000_000_000_000u128), // 50,000,000 USDC
            U256::from(15_000u128) * U256::from(10u128).pow(U256::from(18u8)), // 15,000 WETH
        ],
        30,
    );

    // Quote 1,000 USDC → WETH.
    let amount_in = U256::from(1_000_000_000u64); // 1,000 USDC
    let quoted: AssetAmount = pool.quote(&AssetAmount::new(usdc, amount_in), &weth)?;
    println!("quote: 1000 USDC -> {} wei WETH", quoted.raw);

    // Build calldata against the bundled mainnet config (Universal Router + Permit2).
    let cfg = chains::ethereum();
    let sender = address!("0x1111111111111111111111111111111111111111");
    // Resolve relative options (deadline/sender) to absolutes at the edge — the
    // build layer rejects an unresolved `Deadline::FromNow`.
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let opts = resolve(
        ExecutionOptions::new(Slippage::from_bps(Bps(50))) // 0.50% slippage
            .with_recipient(Recipient::To(sender)),
        now,
        sender,
    );

    let prepared = as_executable(&pool)
        .ok_or("pool has no swap encoder")?
        .build_swap(
            &cfg,
            CurrencyAmount {
                currency: Currency::Token(usdc),
                raw: amount_in,
            },
            Currency::Token(weth),
            &quoted,
            &opts,
        )?;

    println!("tx.to     = {}", prepared.tx.to);
    println!("tx.value  = {}", prepared.tx.value);
    println!(
        "tx.data   = {} ({} bytes)",
        prepared.tx.data,
        prepared.tx.data.len()
    );
    println!("min_out   = {} wei WETH", prepared.min_received.raw);
    if let Some(a) = &prepared.approval {
        println!(
            "approval  = approve {} to spend {} of {}",
            a.spender, a.min_allowance, a.token
        );
    }
    Ok(())
}
