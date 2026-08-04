# Architecture

`amm-rs` is two crates with a hard boundary between **math** and **I/O**.

| Crate      | Role                                             | Dependencies                     |
|------------|--------------------------------------------------|----------------------------------|
| `amm-core` | Pure quoting: primitives, traits, per-protocol quoters, slippage & path helpers. No network. | `alloy-primitives`, `num-*`, protocol math crates (opt-in) |
| `amm-rpc`  | On-chain state fetching: turns chain state into quotable `amm-core` pools. | `alloy`, `amm-core`              |

A consumer that already has pool state (a subgraph, a database, fixtures) uses
`amm-core` alone and never pulls `alloy` or a network stack. `amm-rpc` exists
only to populate `amm-core`'s pool structs from a live chain.

## The `Pool` trait and its extensions

The core abstraction is an **object-safe** base trait:

```rust
pub trait Pool: Send + Sync {
    fn id(&self) -> &PoolId;
    fn assets(&self) -> &[AssetId];
    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError>;
}
```

Object-safety is deliberate: a router holds a heterogeneous `Vec<Box<dyn Pool>>`
of Uniswap, Curve, and Aerodrome pools and quotes across all of them through one
interface. Adding a new AMM is implementing this trait in *your* crate — there is
no closed enum to fork.

Capabilities beyond a spot exact-in quote are **opt-in extension traits**, each a
supertrait of `Pool`:

| Trait       | Adds                                                    |
|-------------|---------------------------------------------------------|
| `ExactOut`  | `quote_exact_out` — solve for the input that yields a target output. |
| `Pricing`   | `spot_price` — the marginal (infinitesimal) directional price.       |
| `Introspect`| `fee_bps`, `reserve`, `kind` — describe the pool.                     |
| `Limits`    | `max_amount_in`, `quote_with_limit` — bound a swap by a price limit.  |

Splitting these off keeps `Pool` object-safe and lets an implementor provide only
what its AMM supports.

## Typed value objects

Amounts and prices carry their identity, so category errors are compile-time or
`Result` errors rather than silent mis-scaling:

- `AssetId { chain: ChainId, token: B256 }` — a token on a chain.
- `AssetAmount { asset: AssetId, raw: U256 }` — a base-unit amount that knows its
  token. Quoting the wrong token into a pool is `Err(QuoteError::AssetNotInPool)`,
  not a wrong number.
- `Price { base, quote, ratio }` — a **directional** price (an exact
  `num-rational` ratio, not a lossy `f64`).
- `Bps(u16)` — basis points, for fees and slippage.

## Shared concentrated-liquidity engine

Uniswap V3, Uniswap V4, and Aerodrome Slipstream are the same tick-crossing swap
math. That math lives once, in `protocols::concentrated` (a `SwapState` + a single
signed `compute_swap_step` loop), and the three quoters are thin adapters over it:

- **V3** — fee tier per pool.
- **V4** — per-direction effective fee (LP fee compounded with V4's per-direction
  protocol fee) and a hooks classification that refuses pools whose hook alters
  pricing.
- **Slipstream** — a V3 fork with its own `slot0`/`ticks` ABI.

Two-asset direction resolution (`assets[0] → assets[1]` is `zero_for_one`) is also
shared across every two-asset quoter.

## Feature gating

`amm-core`'s default build enables **no** protocols (`default = []`); each is an
opt-in feature (`uniswap-v2`, `uniswap-v3`, `uniswap-v4`, `curve`, `aerodrome`,
`serde`). You compile only the math you use. CI builds `--all-features`.

The `curve` feature is special: it pulls `curve-math`, which is **BSL-1.1**
licensed. It is off by default so the crate stays MIT/Apache unless a consumer
opts in. See [protocols.md](protocols.md#licensing).

## On-chain state fetching (`amm-rpc`)

`amm-rpc` implements one trait:

```rust
#[async_trait]
pub trait StateSource {
    async fn discover(&self, chain: &ChainId, assets: &[AssetId]) -> Result<Vec<PoolKey>, RpcError>;
    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError>;
}
```

- **Batching.** Every source reads through a hand-rolled Multicall3 `aggregate3`
  helper: one block-pinned round trip, per-call revert tolerance (one bad pool
  does not sink the batch).
- **Two-round tick fetch.** Concentrated pools refresh in two dependent,
  same-block rounds: first `slot0`/`liquidity`/`fee`, then — now that the active
  tick is known — a bounded window of `ticks(t)` around it. The window bounds the
  fetch; a swap large enough to cross beyond it is under-represented (this is the
  only source of divergence from the on-chain contract — see
  [protocols.md](protocols.md#fidelity)).
- **Reverts are data.** A pool whose reads revert is omitted from the refresh
  result rather than failing the whole batch.

Correctness is proven two ways: deterministic golden-vector unit tests (no
network), and a gated live **differential** harness that asserts our quote equals
the deployed contract's own quote at a pinned block for every exchange
(`amm-rpc/tests/differential.rs`).
