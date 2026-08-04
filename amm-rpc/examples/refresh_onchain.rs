//! Fetch a live Uniswap V3 pool from an RPC and quote it — the `amm-rpc` half of
//! the library populating an `amm-core` pool from chain state.
//!
//! Run (a public node is used if `AMM_RPC_URL` is unset):
//!   `AMM_RPC_URL=https://your-node cargo run -p amm-rpc --example refresh_onchain`

use alloy::eips::BlockId;
use alloy::primitives::{U256, address};
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::{ExchangeId, PoolKey};
use amm_rpc::protocols::uniswap_v3::UniswapV3Source;
use amm_rpc::{StateSource, make_provider};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("AMM_RPC_URL")
        .unwrap_or_else(|_| "https://ethereum-rpc.publicnode.com".to_string());
    let provider = make_provider(&url)?;
    let source = UniswapV3Source::new(provider);

    let usdc = AssetId::new(
        ChainId(1),
        address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").into_word(),
    );
    let weth = AssetId::new(
        ChainId(1),
        address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").into_word(),
    );

    // Mainnet USDC/WETH 0.05% pool. `refresh` reads price + liquidity + a tick
    // window in one block-pinned batch and hands back a quotable pool.
    let key = PoolKey {
        exchange: ExchangeId::new("uniswap-v3"),
        chain: ChainId(1),
        address: "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(),
        assets: vec![usdc, weth],
        fee_bps: None,
    };

    let pools = source.refresh(&[key], BlockId::latest()).await?;
    let pool = pools.first().ok_or("pool failed to refresh")?;
    let out = pool.quote(&AssetAmount::new(usdc, U256::from(1_000_000_000u64)), &weth)?;
    println!("live 1000 USDC -> {} WETH wei", out.raw);
    Ok(())
}
