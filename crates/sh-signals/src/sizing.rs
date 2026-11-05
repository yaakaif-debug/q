//! Размер позиции (SPEC.md, «Sizing»): минимум из капитала пары, 1% меньшего ОИ,
//! 25% глубины в пределах 10 bp от mid на более тонкой стороне и остатка лимитов
//! площадок. Количество округляется вниз до кратного лотам обеих площадок; остаток
//! только сообщается, никогда не добивается.

use rust_decimal::Decimal;
use rust_decimal::dec;
use serde::{Deserialize, Serialize};
use sh_core::{Px, Qty, Usd};

/// Что ограничило размер.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeConstraint {
    /// Капитал пары.
    Capital,
    /// Открытый интерес.
    OpenInterest,
    /// Глубина стакана.
    Depth,
    /// Лимит площадки.
    VenueCap,
}

/// Входные данные.
#[derive(Debug, Clone, Copy)]
pub struct SizingInputs {
    /// Номинал на ногу, допустимый капиталом пары.
    pub capital_notional: Usd,
    /// ОИ одной стороны на площадках ног, $.
    pub oi_usd: [Usd; 2],
    /// Глубина в пределах 10 bp на нужных сторонах ног, $.
    pub depth_usd: [Usd; 2],
    /// Остаток лимита площадок в номинале, $.
    pub venue_room: [Usd; 2],
    /// Цена для перевода номинала в количество.
    pub price: Px,
    /// Лоты площадок.
    pub lots: [Qty; 2],
}

/// Решение о размере.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeDecision {
    /// Номинал на ногу после округления.
    pub notional: Usd,
    /// Количество на ногу.
    pub qty: Qty,
    /// Отброшенный остаток количества.
    pub residual: Qty,
    /// Связывающее ограничение.
    pub binding: SizeConstraint,
}

/// Доля ОИ на позицию.
pub const OI_FRACTION: Decimal = dec!(0.01);
/// Доля глубины на позицию.
pub const DEPTH_FRACTION: Decimal = dec!(0.25);

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Наименьший общий шаг (НОК) двух десятичных лотов; `None` для неположительных.
pub fn common_lot(a: Qty, b: Qty) -> Option<Qty> {
    if !a.is_positive() || !b.is_positive() {
        return None;
    }
    let (a, b) = (a.0.normalize(), b.0.normalize());
    let scale = a.scale().max(b.scale());
    let to_int = |d: Decimal| -> Option<u128> {
        let mut d = d;
        d.rescale(scale);
        u128::try_from(d.mantissa()).ok()
    };
    let (ia, ib) = (to_int(a)?, to_int(b)?);
    let l = ia / gcd(ia, ib) * ib;
    let m = i128::try_from(l).ok()?;
    Some(Qty(Decimal::try_from_i128_with_scale(m, scale).ok()?))
}

/// Размер позиции; `None`, если после округления количество нулевое.
pub fn size_position(inp: &SizingInputs) -> Option<SizeDecision> {
    if inp.price.0 <= Decimal::ZERO {
        return None;
    }
    let candidates = [
        (inp.capital_notional.0, SizeConstraint::Capital),
        (inp.oi_usd[0].0.min(inp.oi_usd[1].0) * OI_FRACTION, SizeConstraint::OpenInterest),
        (inp.depth_usd[0].0.min(inp.depth_usd[1].0) * DEPTH_FRACTION, SizeConstraint::Depth),
        (inp.venue_room[0].0.min(inp.venue_room[1].0), SizeConstraint::VenueCap),
    ];
    let (limit, binding) = candidates
        .iter()
        .copied()
        .fold((Decimal::MAX, SizeConstraint::Capital), |acc, c| if c.0 < acc.0 { c } else { acc });
    if limit <= Decimal::ZERO {
        return None;
    }
    let raw_qty = limit / inp.price.0;
    let lot = common_lot(inp.lots[0], inp.lots[1])?;
    let qty = Qty(raw_qty).floor_to_lot(lot).ok()?;
    if !qty.is_positive() {
        return None;
    }
    Some(SizeDecision { notional: Usd(qty.0 * inp.price.0), qty, residual: Qty(raw_qty - qty.0), binding })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn lcm_of_lots() {
        assert_eq!(common_lot(Qty(dec!(0.001)), Qty(dec!(0.01))), Some(Qty(dec!(0.01))));
        assert_eq!(common_lot(Qty(dec!(0.02)), Qty(dec!(0.03))), Some(Qty(dec!(0.06))));
        assert_eq!(common_lot(Qty(dec!(1)), Qty(dec!(0.5))), Some(Qty(dec!(1))));
        assert_eq!(common_lot(Qty::ZERO, Qty(dec!(1))), None);
    }

    fn inputs() -> SizingInputs {
        SizingInputs {
            capital_notional: Usd(dec!(25000)),
            oi_usd: [Usd(dec!(50000000)), Usd(dec!(10000000))],
            depth_usd: [Usd(dec!(1000000)), Usd(dec!(1000000))],
            venue_room: [Usd(dec!(1000000)), Usd(dec!(1000000))],
            price: Px(dec!(3000)),
            lots: [Qty(dec!(0.001)), Qty(dec!(0.01))],
        }
    }

    #[test]
    fn binding_constraints() {
        let d = size_position(&inputs()).unwrap();
        // 1% от 10M = 100k > 25k, глубина 250k → капитал
        assert_eq!(d.binding, SizeConstraint::Capital);
        assert_eq!(d.qty, Qty(dec!(8.33)));
        assert_eq!(d.notional, Usd(dec!(24990)));
        let mut thin = inputs();
        thin.depth_usd[1] = Usd(dec!(40000));
        assert_eq!(size_position(&thin).unwrap().binding, SizeConstraint::Depth);
        let mut small_oi = inputs();
        small_oi.oi_usd[0] = Usd(dec!(1000000));
        assert_eq!(size_position(&small_oi).unwrap().binding, SizeConstraint::OpenInterest);
        let mut capped = inputs();
        capped.venue_room[0] = Usd::ZERO;
        assert!(size_position(&capped).is_none());
    }

    proptest! {
        #[test]
        fn size_never_exceeds_any_limit(cap in 0i64..1_000_000, oi in 0i64..100_000_000, depth in 0i64..10_000_000, room in 0i64..1_000_000, px in 1i64..100_000, la in 0u32..6, lb in 0u32..6) {
            let inp = SizingInputs {
                capital_notional: Usd(Decimal::from(cap)),
                oi_usd: [Usd(Decimal::from(oi)), Usd(Decimal::from(oi) * dec!(2))],
                depth_usd: [Usd(Decimal::from(depth)), Usd(Decimal::from(depth))],
                venue_room: [Usd(Decimal::from(room)), Usd(Decimal::from(room))],
                price: Px(Decimal::from(px)),
                lots: [Qty(Decimal::new(1, la)), Qty(Decimal::new(1, lb))],
            };
            if let Some(d) = size_position(&inp) {
                let n = d.notional.0;
                prop_assert!(n <= Decimal::from(cap));
                prop_assert!(n <= Decimal::from(oi) * OI_FRACTION);
                prop_assert!(n <= Decimal::from(depth) * DEPTH_FRACTION);
                prop_assert!(n <= Decimal::from(room));
                let lot = common_lot(inp.lots[0], inp.lots[1]).unwrap();
                prop_assert!((d.qty.0 / lot.0).fract().is_zero());
                prop_assert!(d.residual.0 >= Decimal::ZERO && d.residual.0 < lot.0);
            }
        }
    }
}
