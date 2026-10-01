//! Consensus-critical primitive types for DarkQuant.
//!
//! Everything on the deterministic state-transition path is integer-only:
//! prices are expressed in **ticks**, quantities in **lots**. Floating point
//! never touches matching, fees, or indicators, so every replica that replays
//! the same input log produces byte-identical state.
//!
//! Unit conventions (per market, see [`MarketSpec`]):
//! - `1 tick` = `tick_size` quote atoms **per lot**.
//! - notional (quote atoms) = `price_ticks * qty_lots * tick_size`.

#![no_std]

use core::fmt;

/// Internal account index, assigned at registration. Public keys map to this.
pub type AccountId = u64;
/// Market identifier.
pub type MarketId = u32;
/// Engine-assigned, strictly monotonic order identifier (per book).
pub type OrderId = u64;
/// Milliseconds since Unix epoch, as stamped by the sequencer (monotonic).
pub type TimestampMs = u64;

/// Limit price in ticks. `0` is never a valid price.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub u64);

/// Quantity in lots. `0` is never a valid order size.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(pub u64);

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}t", self.0)
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}l", self.0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Side {
    Bid = 0,
    Ask = 1,
}

impl Side {
    #[inline]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn from_u8(v: u8) -> Option<Side> {
        match v {
            0 => Some(Side::Bid),
            1 => Some(Side::Ask),
            _ => None,
        }
    }
}

/// Static market parameters. Validated once at listing time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MarketSpec {
    pub id: MarketId,
    /// Quote atoms per lot per tick.
    pub tick_size: u64,
    /// Base atoms per lot.
    pub lot_size: u64,
    pub min_qty: Qty,
    pub max_qty: Qty,
    pub max_price: Price,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SpecError {
    ZeroTickSize,
    ZeroLotSize,
    BadQtyBounds,
    ZeroMaxPrice,
    /// `max_price * max_qty * tick_size` must fit in `u128` with headroom for
    /// fee multiplication (`< 2^108`), so no single order can overflow notional math.
    NotionalOverflow,
}

/// Upper bound for single-order notional so `notional * fee_pips` cannot overflow u128.
pub const MAX_ORDER_NOTIONAL_BITS: u32 = 108;

impl MarketSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.tick_size == 0 {
            return Err(SpecError::ZeroTickSize);
        }
        if self.lot_size == 0 {
            return Err(SpecError::ZeroLotSize);
        }
        if self.min_qty.0 == 0 || self.min_qty > self.max_qty {
            return Err(SpecError::BadQtyBounds);
        }
        if self.max_price.0 == 0 {
            return Err(SpecError::ZeroMaxPrice);
        }
        let worst = (self.max_price.0 as u128)
            .checked_mul(self.max_qty.0 as u128)
            .and_then(|x| x.checked_mul(self.tick_size as u128))
            .ok_or(SpecError::NotionalOverflow)?;
        if worst >> MAX_ORDER_NOTIONAL_BITS != 0 {
            return Err(SpecError::NotionalOverflow);
        }
        Ok(())
    }

    /// Quote-atom notional of `qty` at `price`. Infallible for any
    /// `price <= max_price`, `qty <= max_qty` on a validated spec.
    #[inline]
    pub fn notional(&self, price: Price, qty: Qty) -> u128 {
        (price.0 as u128) * (qty.0 as u128) * (self.tick_size as u128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MarketSpec {
        MarketSpec {
            id: 1,
            tick_size: 10,
            lot_size: 1_000,
            min_qty: Qty(1),
            max_qty: Qty(1 << 40),
            max_price: Price(1 << 40),
        }
    }

    #[test]
    fn validates_and_computes_notional() {
        let s = spec();
        assert_eq!(s.validate(), Ok(()));
        assert_eq!(s.notional(Price(250), Qty(4)), 10_000);
    }

    #[test]
    fn rejects_overflowing_spec() {
        let s = MarketSpec {
            max_qty: Qty(u64::MAX),
            max_price: Price(u64::MAX),
            ..spec()
        };
        assert_eq!(s.validate(), Err(SpecError::NotionalOverflow));
    }

    #[test]
    fn side_roundtrip() {
        assert_eq!(Side::from_u8(Side::Ask as u8), Some(Side::Ask));
        assert_eq!(Side::Bid.opposite(), Side::Ask);
        assert_eq!(Side::from_u8(2), None);
    }
}
