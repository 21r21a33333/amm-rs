# amm-rs

A wei-exact, `no_std`-capable Rust library for quoting swaps across AMM
protocols behind one open, object-safe `Pool` trait — the "one-stop AMM
library" any crypto dev can use and extend without forking.

## Why

Existing Rust AMM crates force a trade-off: `amms-rs` abstracts across
protocols but behind a **closed enum** (fork to add a pool), returns bare
`U256` amounts and lossy `f64` prices, and can't run `no_std`;
`uniswap-sdk-core-rust` has typed amounts/prices but no pools. `amm-rs`
combines them:

- **Open, object-safe `Pool` trait** — add your own AMM in your own crate.
- **Typed value objects** — `AssetAmount` carries its token; `Price` carries
  base + quote; wrong-token/wrong-direction quotes are `Result` errors.
- **Wei-exact** — quoters reproduce the on-chain contract to the wei, proven
  by golden vectors.
- **`no_std + alloc` core** — runs off-chain, in wasm, and in enclaves.

## Workspace

- `amm-core` — `no_std + alloc`, zero network deps: primitives, traits,
  per-protocol quoters, slippage/path helpers.
- `amm-rpc` — optional, `std + async`: `alloy`-backed on-chain state fetching.

## Status

v1 in progress. See `docs/specs/` for the design and `docs/plans/` for the
implementation plan.

## License

Licensed under either of MIT or Apache-2.0 at your option.
