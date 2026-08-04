//! Domain value types: token identity, amounts, exact-rational prices, and
//! pool descriptors. All are immutable, and the hot-path identity/amount types
//! are `Copy`.

pub mod asset;
pub mod pool;
pub mod price;
pub mod ratio;
