//! Curve on-chain state source, covering all 12 `curve-math` variants.
//!
//! Config-driven: Curve's registry topology is intricate, so each pool's
//! address, variant, coins, and decimals come from a [`CurvePoolConfig`] rather
//! than on-chain enumeration. `refresh` lays out the variant-specific reads a
//! pool needs, batches them through one block-pinned Multicall3, decodes them
//! into a [`RawPoolState`], and delegates the build to `curve_adapter`.
//!
//! Different variant families need different reads:
//! - StableSwap plain (V0/V1/V2/STETH): `future_A`/`A`, `fee`, `balances`
//! - StableSwap Meta: plain + base-pool `get_virtual_price` (the LP coin's rate)
//! - StableSwap ALend: plain + `offpeg_fee_multiplier`
//! - StableSwap-NG: plain + `offpeg_fee_multiplier` + `stored_rates`
//! - CryptoSwap (Two/Tri): `A`, `balances`, `mid_fee`, `out_fee`, `fee_gamma`,
//!   `D`, `price_scale` (+ `gamma` except TwoCryptoStable)

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use amm_core::primitives::asset::AssetId;
use amm_core::primitives::pool::PoolKey;
use amm_core::protocols::curve::pool::CurvePool;
use amm_core::traits::pool::Pool;
use curve_adapter::{CurveVariant, RawPoolState, build_pool};

use crate::error::RpcError;
use crate::multicall::{self, Call, CallResult};
use crate::source::StateSource;

sol! {
    /// Every Curve getter that returns a single `uint256` (decoded uniformly).
    interface ICurveScalar {
        function A() external view returns (uint256);
        function future_A() external view returns (uint256);
        function fee() external view returns (uint256);
        function mid_fee() external view returns (uint256);
        function out_fee() external view returns (uint256);
        function fee_gamma() external view returns (uint256);
        function gamma() external view returns (uint256);
        function D() external view returns (uint256);
        function offpeg_fee_multiplier() external view returns (uint256);
        function get_virtual_price() external view returns (uint256);
        function price_scale() external view returns (uint256);
    }
    /// Older StableSwap (V0-era) pools index `balances` by `int128`.
    interface ICurveBalancesInt {
        function balances(int128 i) external view returns (uint256);
    }
    /// NG / CryptoSwap pools index `balances` by `uint256`.
    interface ICurveBalancesUint {
        function balances(uint256 i) external view returns (uint256);
    }
    /// StableSwap-NG per-token rates.
    interface ICurveStoredRates {
        function stored_rates() external view returns (uint256[] memory);
    }
    /// TriCrypto indexes `price_scale` by coin.
    interface ICurvePriceScaleIdx {
        function price_scale(uint256 i) external view returns (uint256);
    }
}

/// The read/build strategy shared by variants with the same on-chain shape.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Plain,
    Meta,
    ALend,
    Ng,
    TwoCrypto,
    TriCrypto,
}

impl Family {
    fn of(variant: CurveVariant) -> Self {
        match variant {
            CurveVariant::StableSwapV0
            | CurveVariant::StableSwapV1
            | CurveVariant::StableSwapV2
            | CurveVariant::StableSwapSTETH => Family::Plain,
            CurveVariant::StableSwapMeta => Family::Meta,
            CurveVariant::StableSwapALend => Family::ALend,
            CurveVariant::StableSwapNG => Family::Ng,
            CurveVariant::TwoCryptoV1
            | CurveVariant::TwoCryptoNG
            | CurveVariant::TwoCryptoStable => Family::TwoCrypto,
            CurveVariant::TriCryptoV1 | CurveVariant::TriCryptoNG => Family::TriCrypto,
        }
    }
}

/// A configured Curve pool: where it is, which variant, its coins/decimals, and
/// the extra references some variants need.
#[derive(Clone, Debug)]
pub struct CurvePoolConfig {
    /// The pool's on-chain address.
    pub address: Address,
    /// Which `curve-math` variant this pool is.
    pub variant: CurveVariant,
    /// The pool's coins, in coin-index order.
    pub coins: Vec<AssetId>,
    /// Each coin's decimal count, index-aligned with `coins`.
    pub decimals: Vec<u8>,
    /// Meta pools: the base pool whose `get_virtual_price` is the LP coin's rate.
    pub base_pool: Option<Address>,
    /// `TwoCryptoV1` only: whether it is the WETH (ETH-variant) Newton solver.
    pub eth_variant: Option<bool>,
}

/// A [`StateSource`] for a configured set of Curve pools over a provider `P`.
///
/// `refresh` reads and builds the configured pools whose keys are requested;
/// `discover` returns every configured pool as a key (Curve discovery is
/// config-driven, not on-chain).
pub struct CurveSource<P> {
    provider: P,
    pools: Vec<CurvePoolConfig>,
}

impl<P: Provider> CurveSource<P> {
    /// Wrap a provider and the set of configured Curve pools.
    pub fn new(provider: P, pools: Vec<CurvePoolConfig>) -> Self {
        Self { provider, pools }
    }

    fn config_for(&self, key: &PoolKey) -> Option<&CurvePoolConfig> {
        let addr = key.address.parse::<Address>().ok()?;
        self.pools.iter().find(|p| p.address == addr)
    }
}

/// The ordered read calls a pool needs, by family.
fn calls_for(config: &CurvePoolConfig) -> Vec<Call> {
    let addr = config.address;
    let n = config.coins.len();
    let mut calls = Vec::new();

    let scalar = |data: Vec<u8>| Call {
        target: addr,
        call_data: data.into(),
    };
    // Only V0-era pools index balances by int128; every later pool uses uint256.
    let bal = |i: usize| match config.variant == CurveVariant::StableSwapV0 {
        true => scalar(ICurveBalancesInt::balancesCall { i: i as i128 }.abi_encode()),
        false => scalar(ICurveBalancesUint::balancesCall { i: U256::from(i) }.abi_encode()),
    };

    match Family::of(config.variant) {
        Family::Plain => {
            calls.push(scalar(ICurveScalar::future_ACall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::feeCall {}.abi_encode()));
            (0..n).for_each(|i| calls.push(bal(i)));
        }
        Family::Meta => {
            calls.push(scalar(ICurveScalar::future_ACall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::feeCall {}.abi_encode()));
            (0..n).for_each(|i| calls.push(bal(i)));
            if let Some(base) = config.base_pool {
                calls.push(Call {
                    target: base,
                    call_data: ICurveScalar::get_virtual_priceCall {}.abi_encode().into(),
                });
            }
        }
        Family::ALend => {
            calls.push(scalar(ICurveScalar::future_ACall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::feeCall {}.abi_encode()));
            (0..n).for_each(|i| calls.push(bal(i)));
            calls.push(scalar(
                ICurveScalar::offpeg_fee_multiplierCall {}.abi_encode(),
            ));
        }
        Family::Ng => {
            calls.push(scalar(ICurveScalar::future_ACall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::feeCall {}.abi_encode()));
            (0..n).for_each(|i| calls.push(bal(i)));
            calls.push(scalar(
                ICurveScalar::offpeg_fee_multiplierCall {}.abi_encode(),
            ));
            calls.push(scalar(ICurveStoredRates::stored_ratesCall {}.abi_encode()));
        }
        Family::TwoCrypto | Family::TriCrypto => {
            calls.push(scalar(ICurveScalar::ACall {}.abi_encode()));
            (0..n).for_each(|i| calls.push(bal(i)));
            calls.push(scalar(ICurveScalar::mid_feeCall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::out_feeCall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::fee_gammaCall {}.abi_encode()));
            calls.push(scalar(ICurveScalar::DCall {}.abi_encode()));
            if config.variant != CurveVariant::TwoCryptoStable {
                calls.push(scalar(ICurveScalar::gammaCall {}.abi_encode()));
            }
            match Family::of(config.variant) {
                Family::TriCrypto => {
                    calls.push(scalar(
                        ICurvePriceScaleIdx::price_scaleCall { i: U256::from(0) }.abi_encode(),
                    ));
                    calls.push(scalar(
                        ICurvePriceScaleIdx::price_scaleCall { i: U256::from(1) }.abi_encode(),
                    ));
                }
                _ => calls.push(scalar(ICurveScalar::price_scaleCall {}.abi_encode())),
            }
        }
    }
    calls
}

/// Decode one pool's result slice into a [`RawPoolState`] and build it. `None`
/// if a required read reverted or the adapter rejects the state.
fn build_curve_pool(config: &CurvePoolConfig, results: &[CallResult]) -> Option<Box<dyn Pool>> {
    let n = config.coins.len();
    let mut cur = Cursor::new(results);

    let raw = match Family::of(config.variant) {
        Family::Plain => RawPoolState {
            variant: config.variant,
            amp: cur.amp()?,
            fee: Some(cur.next()?),
            balances: cur.take(n)?,
            token_decimals: config.decimals.clone(),
            ..Default::default()
        },
        Family::Meta => {
            let amp = cur.amp()?;
            let fee = cur.next()?;
            let balances = cur.take(n)?;
            let virtual_price = cur.next()?;
            // The last coin is the base-pool LP token; its rate is the base pool's
            // virtual price. Every other coin prices off its decimals.
            let mut rates: Vec<Option<U256>> = vec![None; n];
            rates[n - 1] = Some(virtual_price);
            RawPoolState {
                variant: config.variant,
                amp,
                fee: Some(fee),
                balances,
                token_decimals: config.decimals.clone(),
                dynamic_rates: Some(rates),
                ..Default::default()
            }
        }
        Family::ALend => RawPoolState {
            variant: config.variant,
            amp: cur.amp()?,
            fee: Some(cur.next()?),
            balances: cur.take(n)?,
            token_decimals: config.decimals.clone(),
            offpeg_fee_multiplier: Some(cur.next()?),
            ..Default::default()
        },
        Family::Ng => {
            let amp = cur.amp()?;
            let fee = cur.next()?;
            let balances = cur.take(n)?;
            // Tolerant: crvUSD factory pools lack offpeg, and plain-token pools
            // have no meaningful stored_rates (the adapter falls back to decimals).
            let offpeg = cur.next();
            let dynamic_rates = cur
                .next_array()
                .map(|rates| rates.into_iter().map(Some).collect());
            RawPoolState {
                variant: config.variant,
                amp,
                fee: Some(fee),
                balances,
                token_decimals: config.decimals.clone(),
                offpeg_fee_multiplier: offpeg,
                dynamic_rates,
                ..Default::default()
            }
        }
        Family::TwoCrypto | Family::TriCrypto => {
            let amp = cur.next()?;
            let balances = cur.take(n)?;
            let mid_fee = cur.next()?;
            let out_fee = cur.next()?;
            let fee_gamma = cur.next()?;
            let d = cur.next()?;
            let gamma = match config.variant {
                CurveVariant::TwoCryptoStable => None,
                _ => Some(cur.next()?),
            };
            let price_scale = match Family::of(config.variant) {
                Family::TriCrypto => vec![cur.next()?, cur.next()?],
                _ => vec![cur.next()?],
            };
            RawPoolState {
                variant: config.variant,
                amp,
                balances,
                token_decimals: config.decimals.clone(),
                mid_fee: Some(mid_fee),
                out_fee: Some(out_fee),
                fee_gamma: Some(fee_gamma),
                d: Some(d),
                gamma,
                price_scale: Some(price_scale),
                eth_variant: config.eth_variant,
                ..Default::default()
            }
        }
    };

    let inner = build_pool(&raw).ok()?;
    Some(Box::new(CurvePool::new(
        amm_core::primitives::pool::PoolId::new(&config.address.to_string()),
        config.coins.clone(),
        inner,
    )))
}

/// A sequential reader over a pool's result slice, mirroring `calls_for`'s order.
struct Cursor<'a> {
    results: &'a [CallResult],
    idx: usize,
}

impl<'a> Cursor<'a> {
    fn new(results: &'a [CallResult]) -> Self {
        Self { results, idx: 0 }
    }

    /// The next single-`uint256` read, or `None` if it reverted / is missing.
    fn next(&mut self) -> Option<U256> {
        let value = self.results.get(self.idx).and_then(decode_scalar);
        self.idx += 1;
        value
    }

    /// The amplification coefficient: `future_A()` preferred over `A()`. `A()`
    /// truncates by `A_PRECISION`; `future_A()` is the raw value the math uses
    /// while a pool is idle. Falls back to `A()` for V0-era pools.
    fn amp(&mut self) -> Option<U256> {
        let future = self.next();
        let current = self.next();
        future.or(current)
    }

    /// The next `n` single-`uint256` reads, or `None` if any reverted.
    fn take(&mut self, n: usize) -> Option<Vec<U256>> {
        (0..n).map(|_| self.next()).collect()
    }

    /// The next `uint256[]` read (e.g. `stored_rates`), or `None`.
    fn next_array(&mut self) -> Option<Vec<U256>> {
        let value = self.results.get(self.idx).and_then(decode_array);
        self.idx += 1;
        value
    }
}

/// Decode a successful single-`uint256` result (every scalar getter shares this shape).
fn decode_scalar(result: &CallResult) -> Option<U256> {
    match result.success {
        true => ICurveScalar::ACall::abi_decode_returns(&result.return_data).ok(),
        false => None,
    }
}

/// Decode a successful `uint256[]` result.
fn decode_array(result: &CallResult) -> Option<Vec<U256>> {
    match result.success {
        true => ICurveStoredRates::stored_ratesCall::abi_decode_returns(&result.return_data).ok(),
        false => None,
    }
}

#[async_trait::async_trait]
impl<P: Provider + Send + Sync> StateSource for CurveSource<P> {
    async fn discover(
        &self,
        chain: &amm_core::primitives::asset::ChainId,
        _assets: &[AssetId],
    ) -> Result<Vec<PoolKey>, RpcError> {
        // Config-driven: every configured pool on this chain is a key.
        Ok(self
            .pools
            .iter()
            .map(|pool| PoolKey {
                exchange: amm_core::primitives::pool::ExchangeId::new("curve"),
                chain: *chain,
                address: pool.address.to_string(),
                assets: pool.coins.clone(),
                fee_bps: None,
            })
            .collect())
    }

    async fn refresh(&self, keys: &[PoolKey], at: BlockId) -> Result<Vec<Box<dyn Pool>>, RpcError> {
        // Lay every configured pool's variant-specific reads into one batch,
        // remembering which result slice belongs to which pool.
        let mut calls: Vec<Call> = Vec::new();
        let mut plans: Vec<(&CurvePoolConfig, std::ops::Range<usize>)> = Vec::new();
        for key in keys {
            if let Some(config) = self.config_for(key) {
                let start = calls.len();
                calls.extend(calls_for(config));
                plans.push((config, start..calls.len()));
            }
        }

        let results = multicall::aggregate3(&self.provider, calls, at).await?;

        let pools = plans
            .into_iter()
            .filter_map(|(config, range)| build_curve_pool(config, results.get(range)?))
            .collect();
        Ok(pools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, address};
    use amm_core::primitives::asset::{AssetAmount, ChainId};

    fn coin(byte: u8) -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[byte]))
    }

    /// A `CallResult` carrying a single `uint256` return.
    fn scalar(value: U256) -> CallResult {
        CallResult {
            success: true,
            return_data: value.to_be_bytes::<32>().to_vec().into(),
        }
    }

    fn stable_v1_config() -> CurvePoolConfig {
        CurvePoolConfig {
            address: address!("0x00000000000000000000000000000000000000a1"),
            variant: CurveVariant::StableSwapV1,
            coins: vec![coin(0x01), coin(0x02), coin(0x03)],
            decimals: vec![18, 18, 18],
            base_pool: None,
            eth_variant: None,
        }
    }

    #[test]
    fn calls_for_plain_emits_future_a_a_fee_and_one_balance_per_coin() {
        // Plain family: future_A, A, fee, then n balances.
        assert_eq!(calls_for(&stable_v1_config()).len(), 3 + 3);
    }

    #[test]
    fn build_plain_stableswap_from_reads_quotes_near_parity() {
        // Balanced 3pool (1M each, 18-dec), amp 2000, fee 1e6 (0.01%). Results in
        // calls_for order: future_A, A, fee, bal0, bal1, bal2.
        let e18 = U256::from(1_000_000_000_000_000_000u128);
        let bal = e18 * U256::from(1_000_000u64);
        let results = [
            scalar(U256::from(2000u64)),      // future_A
            scalar(U256::from(2000u64)),      // A
            scalar(U256::from(1_000_000u64)), // fee (1e10 denom → 0.01%)
            scalar(bal),
            scalar(bal),
            scalar(bal),
        ];
        let pool = build_curve_pool(&stable_v1_config(), &results).expect("builds");
        // 1 coin in → just under 1 coin out (fee + curvature), near parity.
        let out = pool
            .quote(&AssetAmount::new(coin(0x01), e18), &coin(0x02))
            .unwrap();
        assert!(out.raw < e18 && out.raw > e18 * U256::from(99u64) / U256::from(100u64));
    }

    #[test]
    fn build_returns_none_when_a_required_read_reverted() {
        let reverted = CallResult {
            success: false,
            return_data: Default::default(),
        };
        let results = [
            reverted.clone(),
            reverted.clone(),
            reverted.clone(),
            reverted.clone(),
            reverted.clone(),
            reverted,
        ];
        assert!(build_curve_pool(&stable_v1_config(), &results).is_none());
    }
}
