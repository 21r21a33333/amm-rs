//! Curve pool quoter backed by the `curve-math` engine, covering every
//! StableSwap and CryptoSwap variant. This wrapper adapts the chain-agnostic
//! [`AssetId`] interface onto `curve_math::Pool`'s index-based math.

use alloy_primitives::{Address, U256};
use curve_math::Pool as CurveMathPool;

use super::coin_indices;
use super::interface::CurveInterface;
use crate::error::QuoteError;
use crate::primitives::asset::{AssetAmount, AssetId};
use crate::primitives::pool::{PoolId, PoolKind};
use crate::primitives::price::Price;
use crate::primitives::ratio::{Bps, Ratio};
use crate::traits::exact_out::ExactOut;
use crate::traits::introspect::Introspect;
use crate::traits::pool::Pool;
use crate::traits::pricing::Pricing;

/// One basis point in `curve-math`'s raw fee units. Its fee denominator is
/// `1e10`, so `1e10 / 1e4 = 1e6` raw units make one bp; divide a raw fee by this.
const FEE_PER_BP: u64 = 1_000_000;

/// A Curve pool of any variant.
///
/// `assets` is ordered to match the coin indices of `inner`, so an
/// `(input, output)` asset pair resolves to the `(i, j)` indices `curve_math`
/// expects.
#[derive(Clone)]
pub struct CurvePool {
    id: PoolId,
    assets: Vec<AssetId>,
    inner: CurveMathPool,
    pool_address: Option<Address>,
    interface: Option<CurveInterface>,
}

impl CurvePool {
    /// Wrap a built `curve_math::Pool` with its coin ordering (`assets[k]` is
    /// coin index `k`).
    pub fn new(id: PoolId, assets: Vec<AssetId>, inner: CurveMathPool) -> Self {
        Self {
            id,
            assets,
            inner,
            pool_address: None,
            interface: None,
        }
    }

    /// Attach the on-chain address and `exchange` ABI family the execution layer
    /// needs. Called by the state source after `curve_adapter::build_pool`, which
    /// cannot carry this metadata itself.
    pub fn with_execution(mut self, address: Address, interface: CurveInterface) -> Self {
        self.pool_address = Some(address);
        self.interface = Some(interface);
        self
    }

    /// The pool's on-chain address, if execution metadata has been attached.
    pub fn pool_address(&self) -> Option<Address> {
        self.pool_address
    }

    /// The `exchange` ABI family for this pool, if execution metadata has been attached.
    pub fn interface(&self) -> Option<CurveInterface> {
        self.interface
    }

    /// Coin positions `(i, j)` for a `from -> to` swap, or `None` if either coin is
    /// absent. Public wrapper over the internal index resolution.
    pub fn coin_indices(&self, from: &AssetId, to: &AssetId) -> Option<(usize, usize)> {
        super::coin_indices(&self.assets, from, to)
    }

    /// Resolve `(i, j)` for a `from -> to` swap, or the not-in-pool error.
    ///
    /// Also rejects indices past the wrapped pool's coin count: `curve_math`
    /// indexes its coin arrays directly, so an `assets` list longer than the
    /// pool would otherwise panic there.
    fn indices(&self, from: &AssetId, to: &AssetId) -> Result<(usize, usize), QuoteError> {
        let coins = self.inner.balances().len();
        match coin_indices(&self.assets, from, to) {
            Some((i, j)) if i < coins && j < coins => Ok((i, j)),
            _ => Err(QuoteError::AssetNotInPool {
                input: *from,
                output: *to,
            }),
        }
    }
}

impl Pool for CurvePool {
    fn id(&self) -> &PoolId {
        &self.id
    }

    fn assets(&self) -> &[AssetId] {
        &self.assets
    }

    fn quote(&self, amount_in: &AssetAmount, to: &AssetId) -> Result<AssetAmount, QuoteError> {
        let (i, j) = self.indices(&amount_in.asset, to)?;
        match amount_in.raw.is_zero() {
            // curve-math returns None for a zero input; a zero swap is 0 out, not an error.
            true => Ok(AssetAmount::new(*to, U256::ZERO)),
            false => {
                let out = self
                    .inner
                    .get_amount_out(i, j, amount_in.raw)
                    .ok_or(QuoteError::InsufficientLiquidity)?;
                Ok(AssetAmount::new(*to, out))
            }
        }
    }

    fn as_exact_out(&self) -> Option<&dyn ExactOut> {
        Some(self)
    }
    fn as_pricing(&self) -> Option<&dyn Pricing> {
        Some(self)
    }
    fn as_introspect(&self) -> Option<&dyn Introspect> {
        Some(self)
    }
}

impl ExactOut for CurvePool {
    fn quote_exact_out(
        &self,
        amount_out: &AssetAmount,
        from: &AssetId,
    ) -> Result<AssetAmount, QuoteError> {
        let (i, j) = self.indices(from, &amount_out.asset)?;
        // curve-math's `get_amount_in` rounds the input up for safety; tighten it
        // to the exact minimum against the forward `get_amount_out` (wei-exact vs
        // the chain). Minimal, and never under-delivers.
        let candidate = self
            .inner
            .get_amount_in(i, j, amount_out.raw)
            .ok_or(QuoteError::InsufficientLiquidity)?;
        let needed = crate::protocols::minimal_exact_out_input(candidate, amount_out.raw, |dx| {
            self.inner.get_amount_out(i, j, dx)
        });
        Ok(AssetAmount::new(*from, needed))
    }
}

impl Pricing for CurvePool {
    fn spot_price(&self, base: &AssetId, quote: &AssetId) -> Result<Price, QuoteError> {
        let (i, j) = self.indices(base, quote)?;
        // curve-math returns dy/dx as (numerator, denominator): coin j per coin i.
        // Its analytic form is exact for StableSwap, but some CryptoSwap variants
        // probe with a decimal-blind fixed dx that overflows low-decimal coins and
        // returns `None`. Fall back to a reserve-relative marginal probe: dy/dx for
        // a tiny dx sized off the input reserve (decimal-agnostic).
        let (num, den) = match self.inner.spot_price(i, j) {
            Some(p) => p,
            None => {
                let reserve = self
                    .inner
                    .balances()
                    .get(i)
                    .copied()
                    .filter(|b| !b.is_zero())
                    .ok_or(QuoteError::InsufficientLiquidity)?;
                let dx = (reserve / U256::from(1_000_000u64)).max(U256::from(1u64));
                let dy = self
                    .inner
                    .get_amount_out(i, j, dx)
                    .filter(|d| !d.is_zero())
                    .ok_or(QuoteError::InsufficientLiquidity)?;
                (dy, dx)
            }
        };
        let ratio = Ratio::new(num, den).ok_or(QuoteError::InsufficientLiquidity)?;
        Price::new(*base, *quote, ratio).ok_or(QuoteError::InsufficientLiquidity)
    }
}

impl Introspect for CurvePool {
    fn fee_bps(&self, source: &AssetId, destination: &AssetId) -> Option<Bps> {
        coin_indices(&self.assets, source, destination)?;
        // StableSwap exposes a single static fee. CryptoSwap's fee is dynamic
        // (mid_fee at balance rising toward out_fee as the pool skews), so report
        // its `mid_fee` — the representative near-balance rate. Both share the 1e10
        // denominator.
        let fee = self
            .inner
            .fee()
            .or_else(|| self.inner.crypto_fees().map(|(mid_fee, _, _)| mid_fee))?;
        let bps = fee / U256::from(FEE_PER_BP);
        Some(Bps(u16::try_from(bps).unwrap_or(u16::MAX)))
    }

    fn reserve(&self, asset: &AssetId) -> Option<AssetAmount> {
        let i = self.assets.iter().position(|c| c == asset)?;
        self.inner
            .balances()
            .get(i)
            .map(|&bal| AssetAmount::new(*asset, bal))
    }

    fn kind(&self) -> PoolKind {
        // Discriminate on CryptoSwap fee parameters, not `gamma()`: TwoCryptoStable
        // is a CryptoSwap-interface pool that uses StableSwap math and exposes no
        // gamma, but does carry crypto fees.
        match self.inner.crypto_fees().is_some() {
            true => PoolKind::CurveCrypto,
            false => PoolKind::CurveStable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::asset::ChainId;
    use alloy_primitives::B256;

    fn dai() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x01]))
    }
    fn usdc() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x02]))
    }
    fn usdt() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x03]))
    }
    fn weth() -> AssetId {
        AssetId::new(ChainId(1), B256::left_padding_from(&[0x04]))
    }

    /// A balanced 3-coin StableSwap (each coin normalised to 1e18 via `rates`),
    /// 1,000,000 units of each. `fee = 1e6` over the 1e10 denominator = 1 bp.
    fn stable_3pool() -> CurvePool {
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let bal = e18 * U256::from(1_000_000u64);
        let inner = CurveMathPool::StableSwapV1 {
            balances: vec![bal, bal, bal],
            rates: vec![e18, e18, e18],
            amp: U256::from(2_000u64),
            fee: U256::from(1_000_000u64),
        };
        CurvePool::new(
            PoolId::new("1:curve:0x3pool"),
            vec![dai(), usdc(), usdt()],
            inner,
        )
    }

    #[test]
    fn quote_between_coins_is_near_parity_minus_fee() {
        let one = U256::from(1_000_000_000_000_000_000u64); // 1 coin (1e18)
        let out = stable_3pool()
            .quote(&AssetAmount::new(dai(), one), &usdc())
            .unwrap();
        assert_eq!(out.asset, usdc());
        assert!(out.raw < one, "fee/curvature must reduce output");
        assert!(
            out.raw > one * U256::from(99u64) / U256::from(100u64),
            "near parity"
        );
    }

    #[test]
    fn exact_out_input_delivers_at_least_the_target() {
        let pool = stable_3pool();
        let want = U256::from(500_000_000_000_000_000u64); // 0.5 coin
        let needed = pool
            .quote_exact_out(&AssetAmount::new(usdc(), want), &dai())
            .unwrap();
        assert_eq!(needed.asset, dai());
        let delivered = pool
            .quote(&AssetAmount::new(dai(), needed.raw), &usdc())
            .unwrap();
        assert!(
            delivered.raw >= want,
            "exact-out input must cover the target"
        );
        // ...and be minimal: one wei less must under-fill.
        let under = pool
            .quote(
                &AssetAmount::new(dai(), needed.raw - U256::from(1u64)),
                &usdc(),
            )
            .unwrap();
        assert!(
            under.raw < want,
            "exact-out input must be minimal (one wei less under-fills)"
        );
    }

    #[test]
    fn spot_price_of_balanced_stable_pool_is_near_one() {
        let price = stable_3pool().spot_price(&dai(), &usdc()).unwrap();
        let lo = Ratio::new(U256::from(99u64), U256::from(100u64)).unwrap();
        let hi = Ratio::new(U256::from(101u64), U256::from(100u64)).unwrap();
        assert!(price.ratio() > &lo && price.ratio() < &hi);
    }

    #[test]
    fn quote_unknown_coin_errors() {
        assert!(matches!(
            stable_3pool().quote(&AssetAmount::new(weth(), U256::from(1u64)), &usdc()),
            Err(QuoteError::AssetNotInPool { .. })
        ));
    }

    #[test]
    fn quote_same_coin_errors() {
        assert!(matches!(
            stable_3pool().quote(&AssetAmount::new(dai(), U256::from(1u64)), &dai()),
            Err(QuoteError::AssetNotInPool { .. })
        ));
    }

    #[test]
    fn introspection_reports_fee_reserve_and_kind() {
        let pool = stable_3pool();
        assert_eq!(pool.fee_bps(&dai(), &usdc()), Some(Bps(1))); // 1e6 / 1e6 = 1 bp
        assert_eq!(pool.fee_bps(&dai(), &weth()), None); // not this pool's pair
        assert_eq!(
            pool.reserve(&dai()),
            Some(AssetAmount::new(
                dai(),
                U256::from(1_000_000_000_000_000_000u64) * U256::from(1_000_000u64)
            ))
        );
        assert_eq!(pool.reserve(&weth()), None);
        assert_eq!(pool.kind(), PoolKind::CurveStable);
    }

    #[test]
    fn execution_metadata_round_trips() {
        use alloy_primitives::address;
        let addr = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
        let pool = stable_3pool().with_execution(addr, CurveInterface::StableI128);
        assert_eq!(pool.pool_address(), Some(addr));
        assert_eq!(pool.interface(), Some(CurveInterface::StableI128));
        // coin_indices resolves (i, j) from the two coins:
        let (i, j) = pool
            .coin_indices(&dai(), &usdc())
            .expect("dai/usdc are coins");
        assert_eq!((i, j), (0, 1));
    }

    #[test]
    fn cryptoswap_pool_reports_crypto_kind_and_mid_fee() {
        // TwoCryptoStable is a CryptoSwap-interface pool that uses StableSwap math
        // and exposes no `gamma()`. It must still classify as CurveCrypto (via the
        // crypto fee params) and report its `mid_fee` as the representative bps —
        // the case that a `gamma()`-based discriminator would misclassify.
        let e18 = U256::from(1_000_000_000_000_000_000u64);
        let inner = CurveMathPool::TwoCryptoStable {
            balances: [e18, e18],
            precisions: [U256::from(1u64), U256::from(1u64)],
            price_scale: e18,
            d: e18 * U256::from(2u64),
            ann: U256::from(400_000u64),
            mid_fee: U256::from(3_000_000u64),
            out_fee: U256::from(30_000_000u64),
            fee_gamma: U256::from(230_000_000_000_000u64),
        };
        let pool = CurvePool::new(PoolId::new("1:curve:0x2crypto"), vec![dai(), usdc()], inner);
        assert_eq!(pool.kind(), PoolKind::CurveCrypto);
        // mid_fee 3_000_000 / 1e6 = 3 bps (the near-balance rate).
        assert_eq!(pool.fee_bps(&dai(), &usdc()), Some(Bps(3)));
    }
}
