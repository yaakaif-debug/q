//! Модель издержек входа и выхода (SPEC.md, S1 «Cost»): комиссии обеих ног на
//! уровне аккаунта, проскальзывание по стакану на целевом размере, ожидаемый спред
//! при разворачивании, доля переводов на перебалансировку и конвертация обеспечения.

use rust_decimal::Decimal;
use rust_decimal::dec;
use serde::{Deserialize, Serialize};
use sh_core::{Qty, Side, Usd};
use sh_md::OrderBook;

use crate::fees::FeeRates;

/// Секция `[costs]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct CostParams {
    /// Мейкер-первый вход и выход (S5): одна нога — мейкер без проскальзывания.
    pub maker_first: bool,
    /// Множитель проскальзывания выхода относительно текущего стакана (ожидаемое расширение спреда).
    pub unwind_spread_multiplier: Decimal,
    /// Доля переводов на перебалансировку S1 на пару, $ (UNVERIFIED: ~$1 за вывод с HL плюс газ).
    pub rebalance_usd_s1: Decimal,
    /// То же для S2 (внутренние переводы HyperCore бесплатны).
    pub rebalance_usd_s2: Decimal,
    /// Конвертация стейблкоина обеспечения на споте HL, bp от маржи ноги в каждую сторону (UNVERIFIED).
    pub collateral_conversion_bps: Decimal,
}

impl Default for CostParams {
    fn default() -> Self {
        Self {
            maker_first: true,
            unwind_spread_multiplier: dec!(1.5),
            rebalance_usd_s1: dec!(4),
            rebalance_usd_s2: dec!(0),
            collateral_conversion_bps: dec!(2),
        }
    }
}

/// Входные данные по ноге.
#[derive(Debug, Clone, Copy)]
pub struct LegCostInput<'a> {
    /// Стакан ноги.
    pub book: &'a OrderBook,
    /// Комиссии ноги.
    pub fees: FeeRates,
    /// Сторона входа: короткая нога продаёт, длинная покупает.
    pub entry_side: Side,
    /// Плечо ноги.
    pub leverage: Decimal,
    /// Обеспечение ноги требует конвертации из USDC (HIP-3 дексы на USDH/USDE/USDT0).
    pub needs_conversion: bool,
}

/// Разбивка издержек входа и выхода, $.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CostBreakdown {
    /// Комиссии входа.
    pub fees_in: Usd,
    /// Комиссии выхода.
    pub fees_out: Usd,
    /// Проскальзывание входа.
    pub slip_in: Usd,
    /// Проскальзывание выхода (с множителем расширения спреда).
    pub slip_out: Usd,
    /// Перебалансировка.
    pub rebalance: Usd,
    /// Конвертация обеспечения.
    pub conversion: Usd,
    /// Итого.
    pub total: Usd,
    /// Базовое количество на ногу.
    pub qty: Qty,
    /// Какая нога мейкер (`Some(true)` — короткая), если мейкер-первый вход.
    pub maker_is_short: Option<bool>,
}

/// Проскальзывание от mid при агрессивном исполнении `qty` стороной `taker`; `None` при нехватке глубины.
pub fn walk_slippage(book: &OrderBook, taker: Side, qty: Qty) -> Option<Usd> {
    let mid = book.mid()?;
    let w = book.walk(taker, qty)?;
    if w.filled < qty {
        return None;
    }
    let per_unit = match taker {
        Side::Buy => w.avg_px.0 - mid.0,
        Side::Sell => mid.0 - w.avg_px.0,
    };
    Some(Usd(per_unit.max(Decimal::ZERO) * qty.0))
}

fn opposite(s: Side) -> Side {
    match s {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

/// Полные издержки входа и выхода на номинал `notional` на ногу.
/// Количество одинаково на обеих ногах: номинал по среднему из двух mid.
pub fn round_trip_cost(
    short: &LegCostInput<'_>,
    long: &LegCostInput<'_>,
    notional: Usd,
    rebalance: Usd,
    params: &CostParams,
) -> Option<CostBreakdown> {
    let mid = (short.book.mid()?.0 + long.book.mid()?.0) / Decimal::TWO;
    if mid <= Decimal::ZERO || notional.0 <= Decimal::ZERO {
        return None;
    }
    let qty = Qty(notional.0 / mid);
    // мейкер — нога с меньшей мейкер-комиссией; при равенстве — короткая
    let maker_is_short = params.maker_first.then_some(short.fees.maker <= long.fees.maker);
    let mut c = CostBreakdown { qty, maker_is_short, rebalance, ..CostBreakdown::default() };
    for (leg, is_short) in [(short, true), (long, false)] {
        let is_maker = maker_is_short == Some(is_short);
        let fee = if is_maker { leg.fees.maker } else { leg.fees.taker };
        c.fees_in.0 += fee * notional.0;
        c.fees_out.0 += fee * notional.0;
        if !is_maker {
            c.slip_in.0 += walk_slippage(leg.book, leg.entry_side, qty)?.0;
            c.slip_out.0 += walk_slippage(leg.book, opposite(leg.entry_side), qty)?.0 * params.unwind_spread_multiplier;
        }
        if leg.needs_conversion && leg.leverage > Decimal::ZERO {
            // конвертация маржи туда и обратно
            c.conversion.0 += Decimal::TWO * params.collateral_conversion_bps / dec!(10000) * notional.0 / leg.leverage;
        }
    }
    c.total = Usd(c.fees_in.0 + c.fees_out.0 + c.slip_in.0 + c.slip_out.0 + c.rebalance.0 + c.conversion.0);
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use sh_core::{Level, Px};

    fn book(mid: Decimal, half_spread: Decimal, depth_each: Decimal, levels: usize) -> OrderBook {
        let mut b = OrderBook::new();
        let bids: Vec<Level> =
            (0..levels).map(|i| Level::new(Px(mid - half_spread - Decimal::from(i as u64)), Qty(depth_each))).collect();
        let asks: Vec<Level> =
            (0..levels).map(|i| Level::new(Px(mid + half_spread + Decimal::from(i as u64)), Qty(depth_each))).collect();
        b.replace(&bids, &asks);
        b
    }

    fn leg(b: &OrderBook, side: Side, maker: Decimal, taker: Decimal) -> LegCostInput<'_> {
        LegCostInput {
            book: b,
            fees: FeeRates { maker, taker },
            entry_side: side,
            leverage: dec!(3),
            needs_conversion: false,
        }
    }

    #[test]
    fn taker_taker_cost() {
        let a = book(dec!(100), dec!(0.5), dec!(1000), 5);
        let b = book(dec!(100), dec!(0.5), dec!(1000), 5);
        let p = CostParams { maker_first: false, unwind_spread_multiplier: dec!(1), ..CostParams::default() };
        let c = round_trip_cost(
            &leg(&a, Side::Sell, dec!(0), dec!(0.0004)),
            &leg(&b, Side::Buy, dec!(0), dec!(0.0004)),
            Usd(dec!(10000)),
            Usd(dec!(4)),
            &p,
        )
        .unwrap();
        // 100 контрактов, полспреда 0.5 на каждой ноге и в каждую сторону: 4 * 50 = 200
        assert_eq!(c.qty, Qty(dec!(100)));
        assert_eq!(c.fees_in, Usd(dec!(8)));
        assert_eq!(c.slip_in, Usd(dec!(100)));
        assert_eq!(c.slip_out, Usd(dec!(100)));
        assert_eq!(c.total, Usd(dec!(220)));
        assert_eq!(c.maker_is_short, None);
    }

    #[test]
    fn maker_first_picks_cheaper_maker_leg() {
        let a = book(dec!(100), dec!(0.5), dec!(1000), 5);
        let b = book(dec!(100), dec!(0.5), dec!(1000), 5);
        let p = CostParams { unwind_spread_multiplier: dec!(1), rebalance_usd_s1: dec!(0), ..CostParams::default() };
        // длинная нога дешевле как мейкер
        let c = round_trip_cost(
            &leg(&a, Side::Sell, dec!(0.0002), dec!(0.0004)),
            &leg(&b, Side::Buy, dec!(0), dec!(0.0004)),
            Usd(dec!(10000)),
            Usd::ZERO,
            &p,
        )
        .unwrap();
        assert_eq!(c.maker_is_short, Some(false));
        assert_eq!(c.fees_in, Usd(dec!(4)));
        assert_eq!(c.slip_in, Usd(dec!(50)));
        assert_eq!(c.total, Usd(dec!(108)));
    }

    #[test]
    fn insufficient_depth_is_none_and_conversion_counts() {
        let thin = book(dec!(100), dec!(0.5), dec!(1), 2);
        let deep = book(dec!(100), dec!(0.5), dec!(1000), 5);
        let p = CostParams { maker_first: false, ..CostParams::default() };
        assert!(
            round_trip_cost(
                &leg(&thin, Side::Sell, dec!(0), dec!(0)),
                &leg(&deep, Side::Buy, dec!(0), dec!(0)),
                Usd(dec!(10000)),
                Usd::ZERO,
                &p
            )
            .is_none()
        );
        let mut conv = leg(&deep, Side::Sell, dec!(0), dec!(0));
        conv.needs_conversion = true;
        let c =
            round_trip_cost(&conv, &leg(&deep, Side::Buy, dec!(0), dec!(0)), Usd(dec!(30000)), Usd::ZERO, &p).unwrap();
        // 2 * 2bp * 30000 / 3 = 4
        assert_eq!(c.conversion, Usd(dec!(4)));
    }

    proptest! {
        #[test]
        fn cost_grows_with_size(n1 in 1_000i64..50_000, extra in 1i64..50_000, spread_bp in 1i64..50) {
            let half = Decimal::new(spread_bp, 2);
            let a = book(dec!(100), half, dec!(50), 40);
            let b = book(dec!(100), half, dec!(50), 40);
            let p = CostParams { maker_first: false, ..CostParams::default() };
            let s = leg(&a, Side::Sell, dec!(0.0001), dec!(0.0004));
            let l = leg(&b, Side::Buy, dec!(0.0001), dec!(0.0004));
            let small = round_trip_cost(&s, &l, Usd(Decimal::from(n1)), Usd::ZERO, &p);
            let large = round_trip_cost(&s, &l, Usd(Decimal::from(n1 + extra)), Usd::ZERO, &p);
            if let (Some(sm), Some(lg)) = (small, large) {
                prop_assert!(lg.total >= sm.total);
                // издержки на доллар номинала тоже не убывают (стакан выпуклый)
                prop_assert!(lg.total.0 / Decimal::from(n1 + extra) >= sm.total.0 / Decimal::from(n1) - dec!(0.0000001));
            }
        }
    }
}
