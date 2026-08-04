# amm-rs

**Wei-exact swap quoting across Uniswap V2/V3/V4, Curve, and Aerodrome — behind one open, object-safe `Pool` trait.**

[![Crates.io][crates-badge]][crates-url]
[![Docs.rs][docs-badge]][docs-url]
[![CI][ci-badge]][ci-url]
[![MSRV][msrv-badge]][msrv-url]
[![License][license-badge]][license-url]

[Documentation](docs/) · [Examples](#examples) · [Supported protocols](#supported-protocols) · [Architecture](docs/architecture.md)

`amm-rs` computes AMM swap quotes that reproduce the on-chain contract **to the
wei**. Every protocol sits behind the same object-safe `Pool` trait, so a router
holds Uniswap, Curve, and Aerodrome pools in one `Vec<Box<dyn Pool>>` and adds a
new AMM by implementing the trait in its own crate — no closed enum to fork.

> ⚠️ **Not audited. Quoting only.** `amm-rs` computes swap quotes; it does not
> execute trades or hold funds. Quotes are best-effort reproductions of on-chain
> contract math and can diverge from live results (MEV, state changes between
> block and execution, unsupported edge cases). **Verify against the chain before
> acting on any quote.** No warranty; use at your own risk.

## Installation

```toml
[dependencies]
# Pure quoting math (no network). Enable only the protocols you use.
amm-core = { version = "0.1", features = ["uniswap-v2", "uniswap-v3", "uniswap-v4", "aerodrome"] }
# Optional: alloy-backed on-chain state fetching.
amm-rpc  = "0.1"
```

The default `amm-core` build enables **no** protocols; each is an opt-in feature.
The `curve` feature is off by default because it pulls BSL-1.1 math — see
[Licensing](docs/protocols.md#licensing).

## Quickstart

Quote a pool you already have state for — pure `amm-core`, no network:

```rust
use amm_core::primitives::asset::{AssetAmount, AssetId, ChainId};
use amm_core::primitives::pool::PoolId;
use amm_core::primitives::ratio::Bps;
use amm_core::protocols::uniswap::v2::UniswapV2Pool;
use amm_core::slippage::Slippage;
use amm_core::traits::{exact_out::ExactOut, pool::Pool};
use alloy_primitives::{U256, address};

let usdc = AssetId::new(ChainId(1), address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").into_word());
let weth = AssetId::new(ChainId(1), address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").into_word());

let pool = UniswapV2Pool::new(
    PoolId::new("1:uniswap-v2:0xB4e1…C9Dc"),
    [usdc, weth],                                    // [token0, token1]
    [U256::from(30_000_000_000_000u128), U256::from(10_000u128) * U256::from(10u128).pow(U256::from(18))],
    30,                                              // 0.30% fee
);

// `AssetAmount` carries its token, so a wrong-token input is a typed error,
// never a silent mispricing.
let out = pool.quote(&AssetAmount::new(usdc, U256::from(1_000_000_000u64)), &weth)?; // 1000 USDC -> WETH
let min_out = Slippage::from_bps(Bps(50)).min_amount_out(&out);                       // 0.5% tolerance

// Exact-out: solve for the input that yields a target output.
let needed = pool.quote_exact_out(&AssetAmount::new(weth, U256::from(10u128).pow(U256::from(18))), &usdc)?;
```

Fetch a live pool from a chain with `amm-rpc`:

```rust,ignore
use amm_rpc::{make_provider, StateSource};
use amm_rpc::protocols::uniswap_v3::UniswapV3Source;
use alloy::eips::BlockId;

let source = UniswapV3Source::new(make_provider("https://ethereum-rpc.publicnode.com")?);
let pools = source.refresh(&[key], BlockId::latest()).await?; // one block-pinned batch
let out = pools[0].quote(&AssetAmount::new(usdc, U256::from(1_000_000_000u64)), &weth)?;
```

Runnable versions of both are in [`examples/`](#examples).

## Supported protocols

Every family has a pure quoter (`amm-core`) and an on-chain state source (`amm-rpc`).

| Protocol   | Variants                                   | exact-in | exact-out | on-chain fetch |
|------------|--------------------------------------------|:--------:|:---------:|:--------------:|
| Uniswap    | V2, V3, V4                                  |    ✅    |    ✅     |      ✅        |
| Curve      | all 12 (StableSwap + CryptoSwap)            |    ✅    |    ✅     |      ✅        |
| Aerodrome  | volatile (vAMM), stable (sAMM), Slipstream  |    ✅    |    ✅     |      ✅        |

Constant-product and stableswap quotes are wei-exact. Concentrated-liquidity
quotes are exact over the tick data supplied; when fetched via `amm-rpc` they use
a bounded tick window, exact for in-window sizes. See
[docs/protocols.md](docs/protocols.md).

## Core concepts

- **Open, object-safe `Pool` trait.** `id` / `assets` / `quote`. Hold any mix of
  AMMs as `Box<dyn Pool>`; extend with your own by implementing the trait.
- **Opt-in extension traits.** `ExactOut` (solve for input), `Pricing`
  (marginal price), `Introspect` (fee/reserve/kind), `Limits` (price-bounded
  swaps) — a pool provides only what its AMM supports.
- **Typed value objects.** `AssetAmount` carries its token; `Price` is a
  directional exact ratio (no lossy `f64`); wrong-token/wrong-direction quotes
  are `Result` errors.
- **Wei-exact.** Integer math throughout — quotes reproduce the deployed
  contract's arithmetic, not an approximation.
- **Slippage & multi-hop paths.** `Slippage` bounds (with `compound` for
  multi-hop) and `path::quote_path` for chained routes.

## Crate layout

- **`amm-core`** — pure quoting: primitives, traits, per-protocol quoters,
  slippage & path helpers. Minimal dependencies, no network.
- **`amm-rpc`** — optional, `async`: `alloy`-backed on-chain state fetching that
  turns chain state into quotable `amm-core` pools via one `StateSource` trait.

## Extending: add your own AMM

Implement `Pool` (and any extension traits you can support) for your type in your
own crate. Nothing in `amm-rs` needs to change, and your pool drops straight into
any router that consumes `Box<dyn Pool>`.

## Correctness

Two layers, both in CI:

- **Golden-vector unit tests** — deterministic, no network; each quoter is
  checked against known on-chain values.
- **Live differential tests** ([`amm-rpc/tests/differential.rs`](amm-rpc/tests/differential.rs))
  — refresh a real pool and assert our quote equals the deployed contract's own
  quote (`get_dy` / `getAmountOut` / a Quoter) at the same block, for every
  exchange. Gated on an RPC endpoint.

## Examples

```bash
# Offline quoting (no network):
cargo run -p amm-core --example quote_offline --features uniswap-v2

# Live on-chain refresh (set AMM_RPC_URL, or a public node is used):
cargo run -p amm-rpc --example refresh_onchain
```

## Minimum supported Rust version (MSRV)

Rust **1.85** (edition 2024). Raising the MSRV is a minor-version change.

## Contributing

Issues and PRs welcome. Please run `cargo fmt`, `cargo clippy --all-features`, and
`cargo test --all-features` before opening a PR.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option. The optional `curve` feature additionally pulls BSL-1.1 math; see
[Licensing](docs/protocols.md#licensing).

<!-- badges -->
[crates-badge]: https://img.shields.io/crates/v/amm-core.svg
[crates-url]: https://crates.io/crates/amm-core
[docs-badge]: https://img.shields.io/docsrs/amm-core
[docs-url]: https://docs.rs/amm-core
[ci-badge]: https://github.com/21r21a33333/amm-rs/actions/workflows/ci.yml/badge.svg
[ci-url]: https://github.com/21r21a33333/amm-rs/actions/workflows/ci.yml
[msrv-badge]: https://img.shields.io/badge/MSRV-1.85-blue.svg
[msrv-url]: #minimum-supported-rust-version-msrv
[license-badge]: https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg
[license-url]: #license
