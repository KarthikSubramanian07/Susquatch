//! YES-space limit prices as integer pips (1 pip = 1e-4).
//!
//! The platform quotes decimals on a 0.005 tick in `[0.005, 0.995]`. Floats only exist at the API
//! boundary: everything inside rust-live is a [`Pips`], which can only hold a valid, on-tick price.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Pips in one unit of price (a winning share pays 1 = 10 000 pips).
pub const PIPS_PER_UNIT: u32 = 10_000;
/// Minimum price increment: 0.005.
pub const TICK: u32 = 50;
/// Lowest valid limit price: 0.005.
pub const MIN_PIPS: u32 = TICK;
/// Highest valid limit price: 0.995.
pub const MAX_PIPS: u32 = PIPS_PER_UNIT - TICK;

/// Why a raw value is not a valid limit price.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PriceError {
    #[error("price {0} pips is off the {TICK}-pip tick")]
    OffTick(u32),
    #[error("price {0} pips is outside [{MIN_PIPS}, {MAX_PIPS}]")]
    OutOfRange(u32),
    #[error("price {0} is not a finite decimal in [0, 1]")]
    NotFinite(String),
    #[error("price {0} is not a whole number of pips")]
    SubPip(String),
}

/// A valid limit price: on tick and inside `[0.005, 0.995]`.
///
/// Construction is always checked, so holding a `Pips` is proof the platform will accept the price.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Pips(u32);

impl Pips {
    /// 0.005
    pub const MIN: Pips = Pips(MIN_PIPS);
    /// 0.995
    pub const MAX: Pips = Pips(MAX_PIPS);

    /// Checked construction from raw pips.
    pub fn new(raw: u32) -> Result<Self, PriceError> {
        if !(MIN_PIPS..=MAX_PIPS).contains(&raw) {
            return Err(PriceError::OutOfRange(raw));
        }
        if raw % TICK != 0 {
            return Err(PriceError::OffTick(raw));
        }
        Ok(Pips(raw))
    }

    /// Raw pips.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Number of ticks above zero (0.005 → 1).
    pub const fn ticks(self) -> u32 {
        self.0 / TICK
    }

    /// `1 - p`. Always valid because the range is symmetric around 0.5.
    pub const fn complement(self) -> Pips {
        Pips(PIPS_PER_UNIT - self.0)
    }

    /// Move by `n` ticks; `None` if that leaves the valid range.
    pub fn checked_add_ticks(self, n: i32) -> Option<Pips> {
        let raw = i64::from(self.0) + i64::from(n) * i64::from(TICK);
        u32::try_from(raw).ok().and_then(|r| Pips::new(r).ok())
    }

    /// Largest valid price `<= raw` pips (round down to tick), or `None` if below 0.005.
    /// Clamps values above 0.995 down to 0.995.
    pub fn floor_from_raw(raw: i64) -> Option<Pips> {
        if raw < i64::from(MIN_PIPS) {
            return None;
        }
        let clamped = raw.min(i64::from(MAX_PIPS));
        let floored = clamped - clamped % i64::from(TICK);
        u32::try_from(floored).ok().and_then(|r| Pips::new(r).ok())
    }

    /// Smallest valid price `>= raw` pips (round up to tick), or `None` if above 0.995.
    /// Clamps values below 0.005 up to 0.005.
    pub fn ceil_from_raw(raw: i64) -> Option<Pips> {
        if raw > i64::from(MAX_PIPS) {
            return None;
        }
        let clamped = raw.max(i64::from(MIN_PIPS));
        let tick = i64::from(TICK);
        let ceiled = (clamped + tick - 1) / tick * tick;
        u32::try_from(ceiled).ok().and_then(|r| Pips::new(r).ok())
    }

    /// Largest valid price `<= p` for a probability `p` (e.g. a fair value), if any.
    pub fn floor_from_prob(p: f64) -> Option<Pips> {
        if !p.is_finite() {
            return None;
        }
        Self::floor_from_raw((p * f64::from(PIPS_PER_UNIT) + 1e-6).floor() as i64)
    }

    /// Smallest valid price `>= p` for a probability `p`, if any.
    pub fn ceil_from_prob(p: f64) -> Option<Pips> {
        if !p.is_finite() {
            return None;
        }
        Self::ceil_from_raw((p * f64::from(PIPS_PER_UNIT) - 1e-6).ceil() as i64)
    }

    /// Parse an API decimal (e.g. `0.455`). Rejects anything that is not exactly on a pip, on tick
    /// and in range, so a malformed price from the wire can never become an order price.
    pub fn from_f64(x: f64) -> Result<Self, PriceError> {
        if !x.is_finite() || !(0.0..=1.0).contains(&x) {
            return Err(PriceError::NotFinite(x.to_string()));
        }
        let scaled = x * f64::from(PIPS_PER_UNIT);
        let rounded = scaled.round();
        if (scaled - rounded).abs() > 1e-6 {
            return Err(PriceError::SubPip(x.to_string()));
        }
        // In range [0, 10000] by the check above, so the cast is exact.
        Pips::new(rounded as u32)
    }

    /// The decimal sent to the API. `serde_json` prints the shortest round-trip representation,
    /// which equals [`Pips::to_decimal_string`] for every valid price (property-tested).
    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / f64::from(PIPS_PER_UNIT)
    }

    /// Probability view of the price in `[0, 1]`.
    pub fn prob(self) -> f64 {
        self.as_f64()
    }

    /// Exact decimal text with trailing zeros removed: 4550 → `"0.455"`, 5000 → `"0.5"`.
    pub fn to_decimal_string(self) -> String {
        let digits = format!("{:04}", self.0);
        let trimmed = digits.trim_end_matches('0');
        format!("0.{trimmed}")
    }
}

impl TryFrom<u32> for Pips {
    type Error = PriceError;
    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        Pips::new(raw)
    }
}

impl From<Pips> for u32 {
    fn from(p: Pips) -> u32 {
        p.0
    }
}

impl fmt::Display for Pips {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_decimal_string())
    }
}

impl fmt::Debug for Pips {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pips({})", self.to_decimal_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn any_pips() -> impl Strategy<Value = Pips> {
        (1u32..=199).prop_map(|t| Pips::new(t * TICK).expect("on tick"))
    }

    #[test]
    fn bounds_and_tick() {
        assert_eq!(Pips::new(50).map(Pips::get), Ok(50));
        assert_eq!(Pips::new(9950).map(Pips::get), Ok(9950));
        assert_eq!(Pips::new(0), Err(PriceError::OutOfRange(0)));
        assert_eq!(Pips::new(10_000), Err(PriceError::OutOfRange(10_000)));
        assert_eq!(Pips::new(4551), Err(PriceError::OffTick(4551)));
    }

    #[test]
    fn decimal_formatting_is_exact() {
        let p = |r| Pips::new(r).expect("valid");
        assert_eq!(p(4550).to_decimal_string(), "0.455");
        assert_eq!(p(5000).to_decimal_string(), "0.5");
        assert_eq!(p(50).to_decimal_string(), "0.005");
        assert_eq!(p(9950).to_decimal_string(), "0.995");
        assert_eq!(p(1450).to_decimal_string(), "0.145");
        assert_eq!(p(100).to_decimal_string(), "0.01");
    }

    #[test]
    fn from_f64_rejects_garbage() {
        assert!(Pips::from_f64(f64::NAN).is_err());
        assert!(Pips::from_f64(1.5).is_err());
        assert!(Pips::from_f64(0.4551).is_err());
        assert!(Pips::from_f64(0.45505).is_err());
        assert!(Pips::from_f64(0.0).is_err());
        assert!(Pips::from_f64(1.0).is_err());
        assert_eq!(Pips::from_f64(0.455).map(Pips::get), Ok(4550));
        // Classic float noise still lands on the pip.
        assert_eq!(Pips::from_f64(0.1 + 0.2).map(Pips::get), Ok(3000));
    }

    #[test]
    fn rounding_helpers() {
        assert_eq!(Pips::floor_from_raw(4599).map(Pips::get), Some(4550));
        assert_eq!(Pips::ceil_from_raw(4501).map(Pips::get), Some(4550));
        assert_eq!(Pips::floor_from_raw(49), None);
        assert_eq!(Pips::ceil_from_raw(9951), None);
        assert_eq!(Pips::floor_from_raw(20_000).map(Pips::get), Some(9950));
        assert_eq!(Pips::ceil_from_raw(-5).map(Pips::get), Some(50));
        assert_eq!(Pips::floor_from_prob(0.62).map(Pips::get), Some(6200));
        assert_eq!(Pips::ceil_from_prob(0.62).map(Pips::get), Some(6200));
        assert_eq!(Pips::floor_from_prob(0.6249).map(Pips::get), Some(6200));
        assert_eq!(Pips::ceil_from_prob(0.6201).map(Pips::get), Some(6250));
    }

    #[test]
    fn serde_rejects_invalid_raw() {
        assert!(serde_json::from_str::<Pips>("4551").is_err());
        assert_eq!(serde_json::from_str::<Pips>("4550").ok(), Pips::new(4550).ok());
    }

    proptest! {
        #[test]
        fn complement_is_an_involution_and_valid(p in any_pips()) {
            prop_assert_eq!(p.complement().complement(), p);
            prop_assert!(Pips::new(p.complement().get()).is_ok());
            prop_assert_eq!(p.get() + p.complement().get(), PIPS_PER_UNIT);
        }

        #[test]
        fn f64_round_trip_is_exact(p in any_pips()) {
            prop_assert_eq!(Pips::from_f64(p.as_f64()), Ok(p));
            // The JSON number we send is byte-identical to the exact decimal.
            prop_assert_eq!(serde_json::to_string(&p.as_f64()).expect("json"), p.to_decimal_string());
            let parsed: f64 = p.to_decimal_string().parse().expect("decimal");
            prop_assert_eq!(Pips::from_f64(parsed), Ok(p));
        }

        #[test]
        fn tick_moves_stay_valid(p in any_pips(), n in -250i32..250) {
            match p.checked_add_ticks(n) {
                Some(q) => {
                    prop_assert_eq!(i64::from(q.get()) - i64::from(p.get()), i64::from(n) * i64::from(TICK));
                }
                None => {
                    let raw = i64::from(p.get()) + i64::from(n) * i64::from(TICK);
                    prop_assert!(raw < i64::from(MIN_PIPS) || raw > i64::from(MAX_PIPS));
                }
            }
        }

        #[test]
        fn floor_and_ceil_bracket(raw in -1000i64..11_000) {
            if let Some(f) = Pips::floor_from_raw(raw) {
                prop_assert!(i64::from(f.get()) <= raw.max(i64::from(MIN_PIPS)));
                prop_assert!(raw >= i64::from(MAX_PIPS) || raw - i64::from(f.get()) < i64::from(TICK));
            }
            if let Some(c) = Pips::ceil_from_raw(raw) {
                prop_assert!(i64::from(c.get()) >= raw.min(i64::from(MAX_PIPS)));
                prop_assert!(raw <= i64::from(MIN_PIPS) || i64::from(c.get()) - raw < i64::from(TICK));
            }
        }

        #[test]
        fn arbitrary_floats_never_produce_invalid_pips(x in proptest::num::f64::ANY) {
            if let Ok(p) = Pips::from_f64(x) {
                prop_assert!(Pips::new(p.get()).is_ok());
            }
        }
    }
}
