//! Денежные новые типы поверх `rust_decimal::Decimal`.
//! `f64` в денежных путях не используется: только эти типы.

use std::fmt;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// Округляет вниз (к нулю для положительных) до кратного `step`.
/// Шаг берётся из метаданных площадки, не из констант.
pub fn floor_to_step(value: Decimal, step: Decimal) -> Result<Decimal, CoreError> {
    if step <= Decimal::ZERO {
        return Err(CoreError::NonPositiveStep(step.to_string()));
    }
    // floor() корректно работает и для отрицательных значений
    Ok((value / step).floor() * step)
}

macro_rules! decimal_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Decimal);

        impl $name {
            /// Ноль.
            pub const ZERO: Self = Self(Decimal::ZERO);

            /// Создаёт значение из `Decimal`.
            pub const fn new(value: Decimal) -> Self {
                Self(value)
            }

            /// Внутреннее десятичное значение.
            pub const fn inner(self) -> Decimal {
                self.0
            }

            /// Истина, если значение равно нулю.
            pub fn is_zero(self) -> bool {
                self.0.is_zero()
            }

            /// Истина, если значение строго больше нуля.
            pub fn is_positive(self) -> bool {
                self.0 > Decimal::ZERO
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                // Площадки иногда присылают экспоненциальную запись; поддерживаем обе формы
                Decimal::from_str(s)
                    .or_else(|_| Decimal::from_scientific(s))
                    .map(Self)
                    .map_err(|_| CoreError::InvalidDecimal(s.to_owned()))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.normalize(), f)
            }
        }

        impl From<Decimal> for $name {
            fn from(value: Decimal) -> Self {
                Self(value)
            }
        }

        impl std::ops::Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self {
                Self(self.0 + rhs.0)
            }
        }

        impl std::ops::Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self {
                Self(self.0 - rhs.0)
            }
        }
    };
}

decimal_newtype!(
    /// Цена в единицах котируемого актива.
    Px
);
decimal_newtype!(
    /// Количество базового актива (размер позиции или уровня стакана).
    Qty
);
decimal_newtype!(
    /// Сумма в долларах (номинал, комиссии, PnL).
    Usd
);
decimal_newtype!(
    /// Базисные пункты: 1 bp = 0.01%.
    Bps
);

impl Qty {
    /// Округляет количество вниз до шага лота площадки.
    /// Остаток не «добивается»: вызывающий код логирует его сам.
    pub fn floor_to_lot(self, lot: Qty) -> Result<Qty, CoreError> {
        floor_to_step(self.0, lot.0).map(Qty)
    }
}

impl Px {
    /// Округляет цену вниз до тика площадки.
    pub fn floor_to_tick(self, tick: Px) -> Result<Px, CoreError> {
        floor_to_step(self.0, tick.0).map(Px)
    }

    /// Номинал `self * qty` в долларах (для котировки в стейблкоине).
    pub fn notional(self, qty: Qty) -> Usd {
        Usd(self.0 * qty.0)
    }
}

impl Bps {
    /// Переводит долю (0.0001 = 1 bp) в базисные пункты.
    pub fn from_fraction(fraction: Decimal) -> Bps {
        Bps(fraction * Decimal::from(10_000))
    }

    /// Доля, соответствующая базисным пунктам.
    pub fn to_fraction(self) -> Decimal {
        self.0 / Decimal::from(10_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::dec;

    #[test]
    fn parses_plain_and_scientific() {
        assert_eq!("1.25".parse::<Px>().ok(), Some(Px(dec!(1.25))));
        assert_eq!("1e-5".parse::<Qty>().ok(), Some(Qty(dec!(0.00001))));
        assert!("abc".parse::<Usd>().is_err());
    }

    #[test]
    fn floor_to_lot_examples() {
        let q = Qty(dec!(1.23456));
        assert_eq!(q.floor_to_lot(Qty(dec!(0.001))).ok(), Some(Qty(dec!(1.234))));
        assert_eq!(q.floor_to_lot(Qty(dec!(1))).ok(), Some(Qty(dec!(1))));
        assert!(q.floor_to_lot(Qty::ZERO).is_err());
    }

    #[test]
    fn bps_roundtrip() {
        assert_eq!(Bps::from_fraction(dec!(0.00045)), Bps(dec!(4.5)));
        assert_eq!(Bps(dec!(4.5)).to_fraction(), dec!(0.00045));
    }

    #[test]
    fn display_is_normalized() {
        assert_eq!(Px(dec!(100.500)).to_string(), "100.5");
    }

    fn arb_step() -> impl Strategy<Value = Decimal> {
        // шаги вида k * 10^-e, как у реальных площадок
        (1u32..=50, 0u32..=8).prop_map(|(k, e)| Decimal::new(i64::from(k), e))
    }

    proptest! {
        #[test]
        fn lot_rounding_invariants(raw in 0i64..10_000_000_000, scale in 0u32..=10, step in arb_step()) {
            let value = Decimal::new(raw, scale);
            let rounded = floor_to_step(value, step).unwrap_or(Decimal::MIN);
            // не больше исходного, кратно шагу, отличие меньше одного шага
            prop_assert!(rounded <= value);
            prop_assert!((rounded / step).fract().is_zero());
            prop_assert!(value - rounded < step);
            prop_assert!(rounded >= Decimal::ZERO);
        }
    }
}

/// Результат применения сделки к позиции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionChange {
    /// Новое количество со знаком.
    pub qty: Decimal,
    /// Новая средняя цена входа (0 для нулевой позиции).
    pub entry_px: Decimal,
    /// Реализованный результат закрытой части.
    pub realized: Decimal,
}

/// Применяет сделку `dq` (со знаком) по цене `px` к позиции `qty` со средней ценой `entry_px`.
/// Увеличение — средневзвешенная цена; сокращение — реализация по старой цене входа;
/// переворот — остаток открывается по цене сделки.
pub fn apply_fill(qty: Decimal, entry_px: Decimal, dq: Decimal, px: Decimal) -> PositionChange {
    let same = qty.is_zero() || (qty > Decimal::ZERO) == (dq > Decimal::ZERO);
    let new = qty + dq;
    if dq.is_zero() {
        return PositionChange { qty, entry_px, realized: Decimal::ZERO };
    }
    if same {
        let entry = if new.is_zero() { Decimal::ZERO } else { (entry_px * qty.abs() + px * dq.abs()) / new.abs() };
        return PositionChange { qty: new, entry_px: entry, realized: Decimal::ZERO };
    }
    let closing = dq.abs().min(qty.abs());
    let dir = if qty > Decimal::ZERO { Decimal::ONE } else { -Decimal::ONE };
    let realized = closing * (px - entry_px) * dir;
    let entry = if new.is_zero() {
        Decimal::ZERO
    } else if (new > Decimal::ZERO) != (qty > Decimal::ZERO) {
        px
    } else {
        entry_px
    };
    PositionChange { qty: new, entry_px: entry, realized }
}

#[cfg(test)]
mod position_tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn open_add_reduce_flip() {
        let a = apply_fill(dec!(0), dec!(0), dec!(2), dec!(100));
        assert_eq!((a.qty, a.entry_px), (dec!(2), dec!(100)));
        let b = apply_fill(a.qty, a.entry_px, dec!(2), dec!(110));
        assert_eq!(b.entry_px, dec!(105));
        let c = apply_fill(b.qty, b.entry_px, dec!(-1), dec!(115));
        assert_eq!((c.qty, c.entry_px, c.realized), (dec!(3), dec!(105), dec!(10)));
        let d = apply_fill(c.qty, c.entry_px, dec!(-5), dec!(100));
        assert_eq!((d.qty, d.entry_px, d.realized), (dec!(-2), dec!(100), dec!(-15)));
        let e = apply_fill(d.qty, d.entry_px, dec!(2), dec!(90));
        assert_eq!((e.qty, e.entry_px, e.realized), (dec!(0), dec!(0), dec!(20)));
    }
}
