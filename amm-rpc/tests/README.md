# `amm-rpc` integration tests

Two independent suites, each with its own responsibility:

- **Execution matrix** (`execution_*.rs` + `support/`) — proves that the calldata
  we build executes on-chain and delivers the **wei-exact** output our `quote()`
  predicted. Runs in **revm** (the EVM `reth` runs) against a forked RPC.
- **Differential** (`differential.rs`) — a separate quote-precision harness that
  compares our `quote()` against each protocol's own on-chain quoter and reports
  PPM drift. No revm, no calldata — a distinct concern from execution.

## The standing rule

**Every new pool adapter or execution feature ships with:**

1. **Offline tests** — calldata/decode unit tests in `src/execution/protocols/…`
   (structure, command bytes, sentinels), and
2. **A fork proof** — `Case` rows added to the matrix that turn green against a
   mainnet/Base fork *before merge*.

Adding a protocol is adding **data**, not a new test file: register its pools in
`support/fixtures.rs`, write `fn <proto>_cases() -> Vec<Case>`, and loop them
through `support::run_case`. The harness and the runner are reused unchanged.

## The table-driven model (`support/`)

The matrix is data. One engine executes any case.

| Module | Responsibility |
|---|---|
| `support/fork.rs` | revm fork mechanics: fund / approve / submit / snapshot / balance reads. Knows nothing about the matrix. |
| `support/fixtures.rs` | the pool catalog — address, tokens, decimals, balance slot, per-protocol refresh recipe. One source of truth. |
| `support/case.rs` | `Case` + `Trade` / `Direction` / `Expect` / `RecipientKind` / `ApprovalKind` — the matrix vocabulary. |
| `support/runner.rs` | `run_case` — builds the swap, funds, approves, submits, and asserts wei-exact output + zero router residue. |
| `support/dsl.rs` | `asset`, `exec_opts`, chain-config builders, chain-infra addresses. |
| `support/env.rs` | `open_fork` (env-gated, `None` when unset) + `fork_block_or`. |
| `support/route.rs` | `make_route` / `make_uniswap_span` / `refresh_pools` — multi-hop fixtures (Plan 3). |

### A `Case` row

```rust
Case {
    name: "v3_usdc_weth_exact_in",
    chain: ChainId(1),
    pools: &["usdc_weth_v3_005"],   // fixture names; 1 = single-hop
    direction: Direction::Forward,   // Forward = token0 -> token1
    trade: Trade::ExactIn { amount_in: U256::from(1_000_000_000u64) },
    native_in: false, native_out: false,
    recipient: RecipientKind::Sender,
    approval: ApprovalKind::Erc20,
    expect: Expect::WeiExact,        // or Exact / AtLeast / RejectBuild(kind)
}
```

**Assertion policy — exactness, no tolerance.** `WeiExact` (exact-in: out delta
`== quoted`), `Exact` (exact-out Strict: out `== target`, in `<= max`),
`AtLeast` (exact-out OrBetter: out `>= target`). A path that proves non-exact
under fork is a **real defect in the wei-exact guarantee** to surface and fix —
never absorbed by a PPM tolerance. (Quote-vs-quoter PPM drift belongs in
`differential.rs`, not here.)

## Running the fork proofs

Fork tests are `#[ignore]` and gated on env vars, so offline CI skips them.

Environment variables (unified across both suites):

| Var | Chain |
|---|---|
| `AMM_RPC_FORK_URL` | Ethereum mainnet |
| `AMM_RPC_FORK_URL_BASE` | Base |
| `AMM_FORK_BLOCK` | optional block override (both chains) |

```sh
# All mainnet matrices against an archive RPC at each fixture's pinned block:
AMM_RPC_FORK_URL=<eth-archive-rpc> \
  cargo test -p amm-rpc --all-features \
  --test execution_v2 --test execution_v3 --test execution_v4 --test execution_curve \
  -- --ignored

# Base matrices:
AMM_RPC_FORK_URL_BASE=<base-archive-rpc> \
  cargo test -p amm-rpc --all-features \
  --test execution_aerodrome --test execution_slipstream -- --ignored
```

**Keyless runs at a recent block.** `run_case` refreshes each pool at the fork's
own block, so a keyless full node (e.g. `https://ethereum-rpc.publicnode.com`,
`https://base-rpc.publicnode.com`) works when `AMM_FORK_BLOCK` is pinned a few
blocks behind head (state a non-archive node still serves):

```sh
PIN=$(( $(cast block-number --rpc-url https://ethereum-rpc.publicnode.com) - 30 ))
AMM_RPC_FORK_URL=https://ethereum-rpc.publicnode.com AMM_FORK_BLOCK=$PIN \
  cargo test -p amm-rpc --all-features --test execution_v3 v3_matrix -- --ignored
```

The `differential.rs` suite pins each fixture at its own block and reads via the
provider (no revm), so it needs an **archive** RPC for its pinned blocks.

## Balance slots

Fixtures fund the input token by stuffing its `balanceOf` storage slot. Known
slots: USDC `9`, DAI `2`, USDT `2`, WETH `3`, WBTC `0`. A wrong slot fails fast
via `fund_erc20_verified` (funds, then asserts the read-back). Some tokens
(e.g. Base USDbC — a proxy with a non-standard layout) cannot be slot-stuffed and
are usable only as swap **output**.

## Scope not yet covered

Multi-hop (`RouterSpan`) and cross-router fork proofs land with **Plan 3**'s
public `Plan`/executor (the span builders are `pub(crate)` today, so integration
tests can't call them yet). `support/route.rs` already provides the builders
Plan 3 will use.
