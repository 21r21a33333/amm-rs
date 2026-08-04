# Supported protocols

Every protocol has a pure `amm-core` quoter and an `amm-rpc` on-chain state
source.

| Family                     | `amm-core` quoter          | `amm-rpc` source         | Fee model                        | Fidelity |
|----------------------------|----------------------------|--------------------------|----------------------------------|----------|
| Uniswap V2 (+ forks)       | `UniswapV2Pool`            | `UniswapV2Source`        | fee-on-input, constant product   | wei-exact |
| Uniswap V3                 | `UniswapV3Pool`            | `UniswapV3Source`        | fee tier                         | bounded-ppm¹ |
| Uniswap V4                 | `UniswapV4Pool`            | `UniswapV4Source`        | per-direction LP+protocol fee    | bounded-ppm¹ |
| Curve (12 variants)        | `CurvePool`                | `CurveSource`            | StableSwap / CryptoSwap          | wei-exact |
| Aerodrome volatile (vAMM)  | `AerodromeVolatilePool`    | `AerodromeSource`        | fee-on-input, constant product   | wei-exact |
| Aerodrome stable (sAMM)    | `AerodromeStablePool`      | `AerodromeSource`        | Solidly `x³y + y³x`              | wei-exact |
| Aerodrome Slipstream       | `AerodromeSlipstreamPool`  | `SlipstreamSource`       | on-chain `fee()` (gauge-settable)| bounded-ppm¹ |

¹ The **math** is exact given full tick liquidity. `amm-rpc` fetches a bounded
tick window (`±50` spacings around the active tick), so a swap large enough to
cross beyond the window is under-represented. For in-window sizes the quote is
wei-exact; the live differential harness bounds the divergence for larger sizes.
If you supply full tick data yourself (constructing the `amm-core` pool directly),
the quote is exact at any size.

## Curve variant coverage

All twelve `curve-math` variants are covered end-to-end (each has a live
differential test against the pool's own `get_dy`):

- **StableSwap**: V0, V1, V2, stETH, Meta, aLend (Aave-lending), NG (next-gen).
- **CryptoSwap**: TwoCrypto V1, TwoCrypto-NG, TwoCrypto-Stable, TriCrypto V1,
  TriCrypto-NG.

Curve's registry topology is intricate, so `CurveSource` is **config-driven**:
each pool's address, variant, coins, and decimals come from a `CurvePoolConfig`
rather than on-chain enumeration.

## Uniswap V4 specifics

V4 collapses every pool into one `PoolManager`. A pool is identified by
`pool_id = keccak256(abi.encode(poolKey))`, and its state is read with
`extsload(bytes32)` against hand-derived `StateLibrary` storage slots. `V4Source`
is config-driven (there is no factory to enumerate). Pools whose **hook** alters
the swap curve or sets a dynamic fee cannot be reproduced from static state and
are refused (`QuoteError::Unsupported`).

## Fidelity

- **Constant-product and stableswap** (V2, Curve, Aerodrome v2): wei-exact —
  the quoter reproduces the deployed contract's integer arithmetic to the wei,
  proven by golden vectors and by the differential harness against the contract's
  own quote function.
- **Concentrated liquidity** (V3, V4, Slipstream): exact math over the tick data
  it is given; the only divergence is the bounded fetch window described above.

## Licensing

`amm-core` and `amm-rpc` are `MIT OR Apache-2.0`. The **`curve` feature** pulls
[`curve-math`](https://github.com/sunce86/curve-math), which is **BSL-1.1**
(free for research, testing, and audit; commercial use needs a paid license). It
is off by default; enabling it is an explicit opt-in, and the rest of the library
stays MIT/Apache.
