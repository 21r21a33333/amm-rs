# amm-rpc

**On-chain state fetching and swap calldata for [`amm-core`](../amm-core) pools.**

The execution half of [`amm-rs`](https://github.com/21r21a33333/amm-rs). Two
capabilities, usable independently:

1. **State fetching** — the `StateSource` trait discovers pools for a set of
   assets and refreshes their on-chain state into quotable
   `Box<dyn amm_core::Pool>` values (alloy-backed).
2. **Execution / calldata** — the `execution` module turns a quoted route into
   sign-ready transactions: `execution::plan` for multi-hop routes, or
   `execution::as_executable(pool).build_swap(..)` for a single pool. Wei-exact
   and fork-proven across Uniswap V2/V3/V4, Curve, Aerodrome, and Slipstream.

## Quickstart (single-pool calldata)

```rust
use amm_rpc::execution::{as_executable, chains, resolve, Currency, CurrencyAmount, ExecutionOptions, Recipient};
use amm_rpc::{AssetAmount, Bps, Slippage};

let cfg = chains::ethereum();                       // bundled router/Permit2 addresses
let opts = resolve(                                 // pin relative deadline/sender at the edge
    ExecutionOptions::new(Slippage::from_bps(Bps(50))).with_recipient(Recipient::To(sender)),
    now_unix_secs, sender,
);
let prepared = as_executable(&pool)
    .ok_or("no encoder")?
    .build_swap(&cfg, CurrencyAmount { currency: Currency::Token(usdc), raw: amount_in },
                Currency::Token(weth), &quoted, &opts)?;
// prepared.tx — sign & send;  prepared.approval — grant first if Some.
```

See [`examples/build_calldata.rs`](examples/build_calldata.rs) for a runnable
end-to-end flow, and [`examples/refresh_onchain.rs`](examples/refresh_onchain.rs)
for live state fetching.

## Multi-hop

`execution::plan(..)` partitions a `Route` into same-router spans (atomic per
protocol) and cross-router sequences, and drives them via `Plan::next_tx`. It
handles exact-in/exact-out (Strict + OrBetter), native ETH edges, slippage
compounding, and per-span approvals (Permit2 for Uniswap, direct for the rest).

## Features

- `curve` — Curve state fetching + calldata (pulls BSL-1.1 deps; off by default).

## License

MIT OR Apache-2.0 (the `curve` feature pulls BSL-1.1 dependencies).
