# Usage

## Quote a pool you already have state for (`amm-core` only)

```rust
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::PoolId;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_core::traits::pool::Pool;
use alloy_primitives::{U256, address};

let usdc = AssetId::new(ChainId(1), address!("0xa0b8...eb48").into_word());
let weth = AssetId::new(ChainId(1), address!("0xc02a...cc2").into_word());

// reserves are [reserve0, reserve1], index-aligned with [token0, token1]
let pool = UniswapV2Pool::new(
    PoolId::new("1:uniswap-v2:0xB4e1...C9Dc"),
    [usdc, weth],
    [U256::from(30_000_000_000_000u128), U256::from(10_000_000_000_000_000_000u128)],
    30, // fee bps
);

let out = pool.quote(&AssetAmount::new(usdc, U256::from(1_000_000_000u64)), &weth)?;
println!("1000 USDC -> {} WETH wei", out.raw);
```

`quote` returns `Result<AssetAmount, QuoteError>`; quoting a token the pool does
not hold is `Err(QuoteError::AssetNotInPool)`, never a wrong number.

## Exact-out: solve for the input

Pools that implement `ExactOut` answer "how much input for this output?":

```rust
use amm_core::traits::exact_out::ExactOut;

let want = AssetAmount::new(weth, U256::from(1_000_000_000_000_000_000u128)); // 1 WETH
let needed = pool.quote_exact_out(&want, &usdc)?;
```

The exact-out solver is conservative: the input it returns is guaranteed to
deliver **at least** the requested output.

## Prices and slippage

```rust
use amm_core::traits::pricing::Pricing;
use amm_core::primitives::ratio::Bps;
use amm_core::slippage::Slippage;

let price = pool.spot_price(&usdc, &weth)?;          // directional, exact ratio

let quoted = pool.quote(&AssetAmount::new(usdc, U256::from(1_000_000_000u64)), &weth)?;
let min_out = Slippage::from_bps(Bps(50)).min_amount_out(&quoted); // 0.5% tolerance
```

`Slippage` also gives `max_amount_in` (for exact-out) and `compound(hops)` for
multi-hop tolerance.

## Multi-hop paths

`path::quote_path` chains quotes across a sequence of pools, threading each hop's
output into the next:

```rust
use amm_core::path::{quote_path, Hop};

let start = AssetAmount::new(usdc, U256::from(1_000_000_000u64));
let out = quote_path(&start, &[
    Hop { pool: &pool_a, to: weth },
    Hop { pool: &pool_b, to: dai },
])?;
```

## Fetch live state from a chain (`amm-rpc`)

```rust
use amm_rpc::{make_provider, StateSource};
use amm_rpc::protocols::uniswap_v3::UniswapV3Source;
use amm_core::primitives::pool::{ExchangeId, PoolKey};
use alloy::eips::BlockId;

let provider = make_provider("https://ethereum-rpc.publicnode.com")?;
let source = UniswapV3Source::new(provider);

let key = PoolKey {
    exchange: ExchangeId::new("uniswap-v3"),
    chain: ChainId(1),
    address: "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(),
    assets: vec![usdc, weth],
    fee_bps: None,
};

// One block-pinned refresh; pools that decode become quotable trait objects.
let pools = source.refresh(&[key], BlockId::latest()).await?;
let out = pools[0].quote(&AssetAmount::new(usdc, U256::from(1_000_000_000u64)), &weth)?;
```

`refresh` reads all pools in one block-pinned Multicall3 batch and omits any whose
reads revert. `assets` in a `PoolKey` must be in address-sorted `[token0, token1]`
order.

## Precision and rounding

All amounts are base-unit `U256` and all arithmetic is integer — no floats in the
quote path. Prices are exact `num-rational` ratios. Rounding follows the
protocol's own convention (e.g. constant-product truncates toward zero), so
quotes match the on-chain contract rather than a re-derived approximation.

> **Disclaimer.** `amm-rs` computes quotes; it is not audited and does not execute
> trades. Treat quotes as best-effort estimates and verify on-chain before acting
> on them.
