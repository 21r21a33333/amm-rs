# amm-rs

**Wei-exact swap quoting and calldata construction across Uniswap V2/V3/V4, Curve, and Aerodrome — behind one open, object-safe `Pool` trait.**

[![Crates.io][crates-badge]][crates-url]
[![Docs.rs][docs-badge]][docs-url]
[![CI][ci-badge]][ci-url]
[![MSRV][msrv-badge]][msrv-url]
[![License][license-badge]][license-url]

[Documentation](docs/) · [Examples](#examples) · [Supported protocols](#supported-protocols) · [Architecture](docs/architecture.md)

`amm-rs` computes AMM swap quotes that reproduce the on-chain contract **to the
wei**, and builds the swap calldata to act on them. Every protocol sits behind
the same object-safe `Pool` trait, so a router holds Uniswap, Curve, and
Aerodrome pools in one `Vec<Box<dyn Pool>>` and adds a new AMM by implementing
the trait in its own crate — no closed enum to fork.

> ✅ **Verified to the wei, on-chain.** Every quoter reproduces the deployed
> contract's own math, and every calldata builder is proven on a mainnet/Base
> fork to settle to the **exact wei** against the real on-chain balance delta —
> for the execution layer, with delivery to the resolved recipient and **no funds
> left in the router**. Four independent test layers back this: golden vectors,
> property-based invariants, live differential (our quote vs the contract's own
> quoter at the same block), and execution fork proofs. All six protocol families
> pass across both directions and exact-in/exact-out.
>
> ⚠️ **Scope & safety.** `amm-rs` computes quotes and builds calldata — it never
> holds funds or submits transactions; you sign and send. It has **not had an
> independent security audit**, and a quote can still diverge from live execution
> (MEV, state drift between block and send, unsupported edge cases). **Verify
> against the chain and review the calldata before you sign.** No warranty; use at
> your own risk.

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

Turn a quote into ready-to-sign calldata:

```rust,ignore
use amm_rpc::execution::{
    as_executable, ChainConfig, Currency, CurrencyAmount, Deadline, ExecutionOptions,
    Recipient,
};

let opts = ExecutionOptions::new(Slippage::from_bps(Bps(50)))   // 0.5% floor
    .with_recipient(Recipient::To(sender))
    .with_deadline(Deadline::AtTimestamp(deadline));

let prepared = as_executable(pools[0].as_ref())?.build_swap(
    &cfg,                                                       // ChainConfig with router addresses
    CurrencyAmount { currency: Currency::Token(usdc), raw: amount_in },
    Currency::Token(weth),
    &out,                                                       // the quote, for the slippage floor
    &opts,
)?;
// prepared.tx       — the transaction (to / value / data) to sign and send
// prepared.approval — any Permit2 / ERC-20 approval to grant first
```

Runnable versions are in [`examples/`](#examples).

## Supported protocols

Every family has a pure quoter (`amm-core`), an on-chain state source, and a
swap-calldata builder (`amm-rpc`).

| Protocol   | Variants                                   | exact-in | exact-out | on-chain fetch | calldata |
|------------|--------------------------------------------|:--------:|:---------:|:--------------:|:--------:|
| Uniswap    | V2, V3, V4                                  |    ✅    |    ✅     |      ✅        |    ✅    |
| Curve      | all 12 (StableSwap + CryptoSwap)            |    ✅    |    ✅     |      ✅        |    ✅    |
| Aerodrome  | volatile (vAMM), stable (sAMM), Slipstream  |    ✅    |    ✅     |      ✅        |    ✅    |

Constant-product and stableswap quotes are wei-exact at every size; exact-out
returns the wei-minimal input. Concentrated-liquidity quotes are wei-exact over
the tick data supplied; when fetched via `amm-rpc` they use a bounded tick window
(depth configurable per source), and a swap large enough to cross beyond it is
**refused** rather than extrapolated — so a returned quote is never an
over-estimate. Every calldata builder is proven to settle on-chain to the wei on
mainnet/Base forks.

The calldata layer also handles the execution details that break naive builders:
output is delivered to your chosen **recipient** (not silently to the sender),
**native ETH** is swapped directly on the pools that support it (Curve `use_eth`,
Uniswap V4 `address(0)`) and **wrapped/unwrapped** around WETH-currency V4 pools
with no ETH or WETH stranded, and each protocol's Permit2 / ERC-20 approval is
returned alongside the transaction. See [docs/protocols.md](docs/protocols.md).

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
- **Executable calldata.** `amm-rpc` turns a quote into a ready-to-sign
  transaction — recipient, deadline, slippage floor, and any Permit2/ERC-20
  approval — routed through each protocol's canonical router (Universal Router,
  SwapRouter02, the Curve pool, …). You sign and send.

## Crate layout

- **`amm-core`** — pure quoting: primitives, traits, per-protocol quoters,
  slippage & path helpers. Minimal dependencies, no network.
- **`amm-rpc`** — optional, `async`: `alloy`-backed on-chain state fetching
  (`StateSource`) that turns chain state into quotable `amm-core` pools, plus a
  swap-calldata builder that turns a quote into a ready-to-sign transaction.

## Extending: add your own AMM

Implement `Pool` (and any extension traits you can support) for your type in your
own crate. Nothing in `amm-rs` needs to change, and your pool drops straight into
any router that consumes `Box<dyn Pool>`.

## Ecosystem — the off-chain router

`amm-rs` is the quoting + calldata engine; [**`arb-router`**](../arb-router) is
the companion off-chain **universal router** built on top of it. Per chain it
discovers and refreshes pools into `amm-core` pools (via `amm-rpc`'s
`StateSource`), models the market as a graph of assets and pools, enumerates and
quotes candidate paths against the local pool math, and ranks opportunities over
a read API — then executes the chosen route through this library's `plan()`
builder. It is the reference consumer that demonstrates the full path from raw
chain state to a signed-ready multi-hop swap.

## Correctness

Four layers:

- **Golden-vector unit tests** — deterministic, no network; each quoter is
  checked against known on-chain values.
- **Property-based invariants** ([`amm-core/tests/properties.rs`](amm-core/tests/properties.rs))
  — over generated pool states: round-trip (`exact_out(exact_in(x)) ≤ x`, and
  minimal to the wei), monotonicity, output bounded by liquidity, multi-tick
  crossing, and slippage-guard rounding. Reaches the corners fixed fixtures miss.
- **Live differential tests** ([`amm-rpc/tests/differential.rs`](amm-rpc/tests/differential.rs))
  — refresh a real pool and assert our quote equals the deployed contract's own
  quote (`get_dy` / `getAmountOut` / a Quoter) at the same block, swept by
  liquidity fraction, for every exchange. Gated on an RPC endpoint.
- **Execution fork proofs** ([`amm-rpc/tests/execution_*.rs`](amm-rpc/tests))
  — build the calldata, run it against a mainnet/Base fork, and assert the
  settled on-chain balance delta equals the quote to the wei, delivered to a
  distinct recipient, with no ETH/WETH left in the router. Covers every family
  in both directions and exact-in/exact-out, including native-ETH and the V4
  WETH-wrap paths. Gated on an RPC endpoint (`$AMM_RPC_FORK_URL` /
  `$AMM_RPC_BASE_FORK_URL`).

## Examples

A progression from a single offline quote to cross-router execution. Every one
is self-contained and runnable (only `refresh_onchain` touches the network).

```bash
# 1. Quote one pool offline — Pool / ExactOut / Slippage (amm-core):
cargo run -p amm-core --example quote_offline --features uniswap-v2

# 2. Multi-hop quoting — quote_path_amounts + quote_path_exact_out (amm-core):
cargo run -p amm-core --example multihop_quote --features uniswap-v2

# 3. One trait, many protocols — heterogeneous Vec<Box<dyn Pool>>, Pricing/Introspect:
cargo run -p amm-core --example multi_protocol --features "uniswap-v2 aerodrome"

# 4. Build sign-ready calldata for a single swap (amm-rpc):
cargo run -p amm-rpc --example build_calldata

# 5. Multi-hop atomic execution — plan() + next_tx loop (amm-rpc):
cargo run -p amm-rpc --example multihop_execution

# 6. Exact-out (Strict / OrBetter) and native-ETH edges (amm-rpc):
cargo run -p amm-rpc --example exact_out_and_native

# 7. Cross-router (Curve + Uniswap) — non-atomic, two transactions (amm-rpc):
cargo run -p amm-rpc --example cross_router --features curve

# 8. Live on-chain refresh (set AMM_RPC_URL, or a public node is used):
cargo run -p amm-rpc --example refresh_onchain
```

## Minimum supported Rust version (MSRV)

Rust **1.86** (edition 2024). Raising the MSRV is a minor-version change.

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
[msrv-badge]: https://img.shields.io/badge/MSRV-1.86-blue.svg
[msrv-url]: #minimum-supported-rust-version-msrv
[license-badge]: https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg
[license-url]: #license
