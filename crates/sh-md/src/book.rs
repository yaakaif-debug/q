//! Простой L2-стакан на `BTreeMap`: уровни с нулевым количеством удаляются.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use sh_core::{Level, Px, Qty, Side};

/// L2-стакан одной площадки и одного инструмента.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderBook {
    bids: BTreeMap<Px, Qty>,
    asks: BTreeMap<Px, Qty>,
    /// Монотонное время последнего изменения (нс), для оценки устаревания.
    last_update_mono_ns: Option<u64>,
    /// Время площадки последнего изменения (мс), если площадка его сообщает.
    venue_ts_ms: Option<i64>,
}

/// Результат прохода по глубине стакана на заданный объём.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkResult {
    /// Исполненный объём (может быть меньше запрошенного, если глубины не хватило).
    pub filled: Qty,
    /// Средневзвешенная цена исполнения.
    pub avg_px: Px,
    /// Худшая затронутая цена.
    pub worst_px: Px,
    /// Номинал исполнения в котируемом активе.
    pub notional: Decimal,
}

impl OrderBook {
    /// Пустой стакан.
    pub fn new() -> Self {
        Self::default()
    }

    /// Очищает стакан (например, при разрыве соединения).
    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.last_update_mono_ns = None;
        self.venue_ts_ms = None;
    }

    /// Истина, если в стакане нет уровней.
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// Устанавливает абсолютное количество на уровне; ноль удаляет уровень.
    pub fn set_level(&mut self, side: Side, level: Level) {
        let map = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        if level.qty.is_zero() {
            map.remove(&level.px);
        } else {
            map.insert(level.px, level.qty);
        }
    }

    /// Применяет набор абсолютных обновлений обеих сторон.
    pub fn apply_levels(&mut self, bids: &[Level], asks: &[Level]) {
        for l in bids {
            self.set_level(Side::Buy, *l);
        }
        for l in asks {
            self.set_level(Side::Sell, *l);
        }
    }

    /// Полностью заменяет содержимое стакана снимком.
    pub fn replace(&mut self, bids: &[Level], asks: &[Level]) {
        self.bids.clear();
        self.asks.clear();
        self.apply_levels(bids, asks);
    }

    /// Отмечает время обновления (монотонное и, если есть, время площадки).
    pub fn touch(&mut self, mono_ns: u64, venue_ts_ms: Option<i64>) {
        self.last_update_mono_ns = Some(mono_ns);
        if venue_ts_ms.is_some() {
            self.venue_ts_ms = venue_ts_ms;
        }
    }

    /// Возраст последнего обновления в миллисекундах относительно `now_mono_ns`.
    pub fn staleness_ms(&self, now_mono_ns: u64) -> Option<u64> {
        self.last_update_mono_ns.map(|t| now_mono_ns.saturating_sub(t) / 1_000_000)
    }

    /// Время площадки последнего обновления, мс.
    pub fn venue_ts_ms(&self) -> Option<i64> {
        self.venue_ts_ms
    }

    /// Лучший bid.
    pub fn best_bid(&self) -> Option<Level> {
        self.bids.iter().next_back().map(|(p, q)| Level::new(*p, *q))
    }

    /// Лучший ask.
    pub fn best_ask(&self) -> Option<Level> {
        self.asks.iter().next().map(|(p, q)| Level::new(*p, *q))
    }

    /// Средняя цена между лучшими bid и ask.
    pub fn mid(&self) -> Option<Px> {
        let (b, a) = (self.best_bid()?, self.best_ask()?);
        Some(Px((b.px.0 + a.px.0) / Decimal::TWO))
    }

    /// Истина, если стакан «перекрещен» (bid >= ask) — признак рассинхронизации.
    pub fn is_crossed(&self) -> bool {
        matches!((self.best_bid(), self.best_ask()), (Some(b), Some(a)) if b.px >= a.px)
    }

    /// Верхние `n` уровней стороны: bids по убыванию цены, asks по возрастанию.
    pub fn top(&self, side: Side, n: usize) -> Vec<Level> {
        match side {
            Side::Buy => self.bids.iter().rev().take(n).map(|(p, q)| Level::new(*p, *q)).collect(),
            Side::Sell => self.asks.iter().take(n).map(|(p, q)| Level::new(*p, *q)).collect(),
        }
    }

    /// Число уровней на стороне.
    pub fn depth(&self, side: Side) -> usize {
        match side {
            Side::Buy => self.bids.len(),
            Side::Sell => self.asks.len(),
        }
    }

    /// Проходит по стакану для агрессора стороны `taker` на объём `qty`:
    /// покупка забирает asks, продажа — bids.
    pub fn walk(&self, taker: Side, qty: Qty) -> Option<WalkResult> {
        if !qty.is_positive() {
            return None;
        }
        let levels: Box<dyn Iterator<Item = (&Px, &Qty)>> = match taker {
            Side::Buy => Box::new(self.asks.iter()),
            Side::Sell => Box::new(self.bids.iter().rev()),
        };
        let mut remaining = qty.0;
        let mut notional = Decimal::ZERO;
        let mut worst = None;
        for (px, avail) in levels {
            if remaining.is_zero() {
                break;
            }
            let take = remaining.min(avail.0);
            notional += take * px.0;
            remaining -= take;
            worst = Some(*px);
        }
        let worst_px = worst?;
        let filled = qty.0 - remaining;
        Some(WalkResult { filled: Qty(filled), avg_px: Px(notional / filled), worst_px, notional })
    }

    /// Суммарное количество на стороне в пределах `bps` от mid.
    pub fn depth_within_bps(&self, side: Side, bps: Decimal) -> Option<Qty> {
        let mid = self.mid()?.0;
        let band = mid * bps / Decimal::from(10_000);
        let total = match side {
            Side::Buy => self.bids.range(Px(mid - band)..).map(|(_, q)| q.0).sum(),
            Side::Sell => self.asks.range(..=Px(mid + band)).map(|(_, q)| q.0).sum(),
        };
        Some(Qty(total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::dec;
    use std::collections::BTreeMap as Model;

    fn lvl(px: Decimal, qty: Decimal) -> Level {
        Level::new(Px(px), Qty(qty))
    }

    fn sample() -> OrderBook {
        let mut b = OrderBook::new();
        b.replace(
            &[lvl(dec!(99), dec!(1)), lvl(dec!(98), dec!(2)), lvl(dec!(97), dec!(3))],
            &[lvl(dec!(101), dec!(1)), lvl(dec!(102), dec!(2)), lvl(dec!(103), dec!(3))],
        );
        b
    }

    #[test]
    fn bbo_and_mid() {
        let b = sample();
        assert_eq!(b.best_bid(), Some(lvl(dec!(99), dec!(1))));
        assert_eq!(b.best_ask(), Some(lvl(dec!(101), dec!(1))));
        assert_eq!(b.mid(), Some(Px(dec!(100))));
        assert!(!b.is_crossed());
    }

    #[test]
    fn zero_removes_level() {
        let mut b = sample();
        b.set_level(Side::Buy, lvl(dec!(99), dec!(0)));
        assert_eq!(b.best_bid(), Some(lvl(dec!(98), dec!(2))));
        // удаление отсутствующего уровня допустимо
        b.set_level(Side::Sell, lvl(dec!(150), dec!(0)));
        assert_eq!(b.depth(Side::Sell), 3);
    }

    #[test]
    fn walk_buys_through_asks() {
        let b = sample();
        let w = b.walk(Side::Buy, Qty(dec!(2))).unwrap();
        assert_eq!(w.filled, Qty(dec!(2)));
        assert_eq!(w.worst_px, Px(dec!(102)));
        assert_eq!(w.avg_px, Px(dec!(101.5)));
        // не хватает глубины: исполняется частично
        let w = b.walk(Side::Sell, Qty(dec!(10))).unwrap();
        assert_eq!(w.filled, Qty(dec!(6)));
        assert!(b.walk(Side::Buy, Qty::ZERO).is_none());
        assert!(OrderBook::new().walk(Side::Buy, Qty(dec!(1))).is_none());
    }

    #[test]
    fn top_and_depth_within_bps() {
        let b = sample();
        assert_eq!(b.top(Side::Buy, 2), vec![lvl(dec!(99), dec!(1)), lvl(dec!(98), dec!(2))]);
        assert_eq!(b.top(Side::Sell, 1), vec![lvl(dec!(101), dec!(1))]);
        // 200 bp от 100 = 98..102
        assert_eq!(b.depth_within_bps(Side::Buy, dec!(200)), Some(Qty(dec!(3))));
        assert_eq!(b.depth_within_bps(Side::Sell, dec!(200)), Some(Qty(dec!(3))));
    }

    #[test]
    fn staleness() {
        let mut b = sample();
        assert_eq!(b.staleness_ms(5_000_000), None);
        b.touch(1_000_000, Some(42));
        assert_eq!(b.staleness_ms(3_500_000), Some(2));
        assert_eq!(b.venue_ts_ms(), Some(42));
    }

    #[derive(Debug, Clone)]
    struct Op {
        side: Side,
        px: i64,
        qty: i64,
    }

    fn arb_op() -> impl Strategy<Value = Op> {
        (any::<bool>(), 1i64..60, 0i64..5).prop_map(|(buy, px, qty)| Op {
            side: if buy { Side::Buy } else { Side::Sell },
            px,
            qty,
        })
    }

    proptest! {
        #[test]
        fn book_matches_reference_model(ops in proptest::collection::vec(arb_op(), 0..200)) {
            let mut book = OrderBook::new();
            let mut bids: Model<i64, i64> = Model::new();
            let mut asks: Model<i64, i64> = Model::new();
            for op in &ops {
                book.set_level(op.side, lvl(Decimal::from(op.px), Decimal::from(op.qty)));
                let m = if op.side == Side::Buy { &mut bids } else { &mut asks };
                if op.qty == 0 { m.remove(&op.px); } else { m.insert(op.px, op.qty); }
            }
            let got_bids: Vec<(i64, i64)> = book.top(Side::Buy, usize::MAX).iter()
                .map(|l| (i64::try_from(l.px.0.mantissa()).unwrap(), i64::try_from(l.qty.0.mantissa()).unwrap()))
                .collect();
            let want_bids: Vec<(i64, i64)> = bids.iter().rev().map(|(p, q)| (*p, *q)).collect();
            prop_assert_eq!(got_bids, want_bids);
            let got_asks: Vec<(i64, i64)> = book.top(Side::Sell, usize::MAX).iter()
                .map(|l| (i64::try_from(l.px.0.mantissa()).unwrap(), i64::try_from(l.qty.0.mantissa()).unwrap()))
                .collect();
            let want_asks: Vec<(i64, i64)> = asks.iter().map(|(p, q)| (*p, *q)).collect();
            prop_assert_eq!(got_asks, want_asks);
            // нулевых уровней в стакане не бывает
            prop_assert!(book.top(Side::Buy, usize::MAX).iter().all(|l| l.qty.is_positive()));
        }

        #[test]
        fn walk_is_bounded_and_monotone(ops in proptest::collection::vec(arb_op(), 1..100), q1 in 1i64..20, q2 in 1i64..20) {
            let mut book = OrderBook::new();
            for op in &ops {
                book.set_level(op.side, lvl(Decimal::from(op.px), Decimal::from(op.qty)));
            }
            let (small, large) = (q1.min(q2), q1.max(q2));
            if let (Some(a), Some(b)) = (book.walk(Side::Buy, Qty(Decimal::from(small))), book.walk(Side::Buy, Qty(Decimal::from(large)))) {
                let best = book.best_ask().unwrap().px;
                // исполнено не больше запрошенного, средняя цена между лучшей и худшей
                prop_assert!(a.filled.0 <= Decimal::from(small));
                prop_assert!(a.avg_px >= best && a.avg_px <= a.worst_px);
                // для покупки средняя цена не убывает с ростом объёма
                prop_assert!(b.avg_px >= a.avg_px);
                prop_assert!(b.filled >= a.filled);
            }
        }
    }
}
