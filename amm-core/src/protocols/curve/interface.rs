/// Which on-chain `exchange` ABI a Curve pool uses. Mapped from the source's
/// 12-way variant; the four families differ in index type, receiver param,
/// return value, and native handling (spec §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CurveInterface {
    /// `exchange(int128,int128,uint256,uint256)` — void return, no receiver param.
    StableI128,
    /// `exchange(int128,int128,uint256,uint256[,receiver])` — returns `uint256`.
    StableI128Ng,
    /// `exchange(uint256,uint256,uint256,uint256,bool[,recv])` — payable, has `use_eth`.
    CryptoU256UseEth,
    /// `exchange(uint256,uint256,uint256,uint256[,receiver])` — no `use_eth` param.
    CryptoU256Receiver,
}

#[cfg(test)]
mod tests {
    use super::CurveInterface;

    #[test]
    fn variants_are_distinct() {
        assert_ne!(CurveInterface::StableI128, CurveInterface::StableI128Ng);
        assert_ne!(
            CurveInterface::CryptoU256UseEth,
            CurveInterface::CryptoU256Receiver
        );
    }
}
