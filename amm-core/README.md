# amm-core

**Wei-exact AMM quoting primitives and traits — zero network dependencies.**

The math half of [`amm-rs`](https://github.com/21r21a33333/amm-rs). It exposes an
open, object-safe `Pool` trait, typed value objects that carry their token
identity (`AssetId`, `AssetAmount`), and per-protocol pure quoters that reproduce
each deployed contract's own arithmetic **to the wei**. On-chain state fetching
lives in the separate [`amm-rpc`](../amm-rpc) crate; if you already hold pool
state (subgraph, DB, your own reader) you can use this crate alone.

## Quickstart

```rust
use amm_core::{AssetAmount, AssetId, ChainId, Bps, Pool, PoolId, Slippage};
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use alloy_primitives::{U256, address};

let chain = ChainId(1);
let usdc = AssetId::new(chain, address!("0xa0b8...").into_word());
let weth = AssetId::new(chain, address!("0xC02a...").into_word());

let pool = UniswapV2Pool::new(PoolId::new("1:univ2:usdc-weth"), [usdc, weth],
                              [reserve_usdc, reserve_weth], 30);

let out = pool.quote(&AssetAmount::new(usdc, amount_in), &weth)?;   // exact-in
let min = Slippage::from_bps(Bps(50)).min_amount_out(&out);        // 0.50% floor
```

`Pool::quote` (exact-in) and `ExactOut::quote_exact_out` (exact-out) are the
core entry points; both traits are re-exported at the crate root. Multi-hop
quoting lives in `amm_core::{quote_path, quote_path_amounts, quote_path_exact_out}`.

## Protocols (opt-in Cargo features)

`uniswap-v2`, `uniswap-v3`, `uniswap-v4`, `curve` (BSL-1.1), `aerodrome`
(Solidly volatile + stable, and Slipstream concentrated). The default build
enables none — pick what you need. The V3/V4/Aerodrome tick math shares one
underlying engine, so enabling any of them pulls it in.

## License

MIT OR Apache-2.0 (the `curve` feature pulls BSL-1.1 dependencies).
