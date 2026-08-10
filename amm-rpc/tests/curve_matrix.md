# Curve execution test matrix (E4)

The four `CurveInterface` ABI families, each pinned to one representative real
mainnet pool + the swap direction its fork proof exercises. Curve has **no
exact-out** and rejects `price_limit`; `tx.to` is the pool itself; the input
token is approved to the pool (Curve pulls via `transferFrom`).

`i`/`j` = coin positions in `coins()` order; `min_dy` (4th arg, universal) =
the slippage floor; `dx` = amountIn.

| Family (`CurveInterface`) | on-chain `exchange` signature | Repr. pool | coins (index: addr, balance-slot) | proof direction |
|---|---|---|---|---|
| **StableI128** | `exchange(int128 i, int128 j, uint256 dx, uint256 min_dy)` — void (`0x3df02124`) | 3pool `0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7` (StableSwapV1) | 0: DAI `0x6B175474E89094C44Da98b954EedeAC495271d0F` (slot 2); 1: USDC `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48` (slot 9); 2: USDT `0xdAC17F958D2ee523a2206206994597C13D831ec7` (slot 2) | DAI→USDC (i=0,j=1) — E4.2 |
| **CryptoU256UseEth** | `exchange(uint256 i, uint256 j, uint256 dx, uint256 min_dy, bool use_eth)` — payable (`0x394747c5`) | tricrypto2 `0xD51a44d3FaE010294C616388b506AcdA1bfAAE46` (TriCryptoV1) | 0: USDT `0xdAC1…ec7` (slot 2); 1: WBTC `0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599`; 2: WETH `0xC02a…Cc2` | USDT→WETH (i=0,j=2), `use_eth=false` — E4.3 |
| **StableI128Ng** | `exchange(int128 i, int128 j, uint256 dx, uint256 min_dy)` → returns `uint256` (StableSwap-NG; `0x3df02124`, or `exchange_received`/receiver form `0xddc1f59d`) | resolve a crvUSD/USDC-class NG pool at E4.4 | (resolve at E4.4) | stable NG pair — E4.4 |
| **CryptoU256Receiver** | `exchange(uint256 i, uint256 j, uint256 dx, uint256 min_dy, address receiver)` — **NO `use_eth`** (`0x5b41b908`/`0xa64833a0`) | resolve a Twocrypto-NG pool at E4.4 | (resolve at E4.4) | 2-coin crypto NG pair — E4.4 |

## Burn-in traps (from spec §4.4 / the Phase-0 `interface_of` map)
- **Twocrypto-NG (`CryptoU256Receiver`) dropped `use_eth`** — its 5th arg is `receiver`, not `bool`. Encoding it with the `CryptoU256UseEth` builder emits the wrong selector and reverts.
- **Classic Tricrypto (`CryptoU256UseEth`) has NO receiver** (only `use_eth`); Tricrypto-NG maps to `CryptoU256UseEth` in our classification too.
- **StableSwap is never payable**; `use_eth` only applies to crypto pools. Our encoder uses `use_eth=false` (trade WETH as ERC-20) — native/payable is deferred.

## Assertion precision
Curve StableSwap and CryptoSwap quotes are computed from full pool state (no
finite tick window), so proofs assert **wei-exact** `delta == quoted.raw`
(fall back to bounded-ppm only if a specific crypto pool's Newton-solver
rounding is shown to diverge, with a logged reason).

## Dimensions covered vs deferred
- **Covered (E4.2–E4.4):** StableI128 (plain), CryptoU256UseEth (classic tri/crypto), StableI128Ng, CryptoU256Receiver (Twocrypto-NG).
- **Deferred (return `UnsupportedProtocol`, logged in E4.5):** `exchange_underlying` (meta/lending — different index space); any `Currency::Native`/payable path (`use_eth=true`).
