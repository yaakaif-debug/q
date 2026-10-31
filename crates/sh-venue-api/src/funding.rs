//! Вывод интервала фандинга по моментам расчётов, когда площадка
//! не публикует интервал в метаданных (Hyperliquid, Lighter).

use std::collections::BTreeMap;

use rust_decimal::Decimal;

/// Округление разностей до минуты: отметки расчётов у площадок «плавают» на миллисекунды.
const ROUND_MS: i64 = 60_000;

/// Выводит интервал (мс) по отметкам расчётов: самая частая положительная разность
/// соседних отметок после округления до минуты; при равенстве — меньшая.
/// Нужны как минимум две разные отметки.
pub fn infer_interval_ms(settlements_ms: &[i64]) -> Option<i64> {
    let mut ts: Vec<i64> = settlements_ms.to_vec();
    ts.sort_unstable();
    ts.dedup();
    let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
    for w in ts.windows(2) {
        let diff = (w[1] - w[0] + ROUND_MS / 2) / ROUND_MS * ROUND_MS;
        if diff > 0 {
            *counts.entry(diff).or_default() += 1;
        }
    }
    // BTreeMap обходится по возрастанию, поэтому при равных частотах выигрывает меньший интервал
    let mut best: Option<(i64, usize)> = None;
    for (diff, n) in counts {
        if best.is_none_or(|(_, bn)| n > bn) {
            best = Some((diff, n));
        }
    }
    best.map(|(d, _)| d)
}

/// Переводит интервал из миллисекунд в часы.
pub fn interval_hours(interval_ms: i64) -> Decimal {
    Decimal::from(interval_ms) / Decimal::from(3_600_000)
}

/// Накопитель отметок расчётов по одному инструменту.
#[derive(Debug, Clone, Default)]
pub struct FundingIntervalTracker {
    settlements: Vec<i64>,
}

impl FundingIntervalTracker {
    /// Сколько последних отметок хранить.
    const KEEP: usize = 16;

    /// Добавляет отметку расчёта (мс); повторы игнорируются.
    pub fn observe(&mut self, settled_at_ms: i64) {
        if self.settlements.contains(&settled_at_ms) {
            return;
        }
        self.settlements.push(settled_at_ms);
        self.settlements.sort_unstable();
        if self.settlements.len() > Self::KEEP {
            let excess = self.settlements.len() - Self::KEEP;
            self.settlements.drain(..excess);
        }
    }

    /// Последняя отметка расчёта.
    pub fn last_settlement_ms(&self) -> Option<i64> {
        self.settlements.last().copied()
    }

    /// Выведенный интервал в часах.
    pub fn interval_hours(&self) -> Option<Decimal> {
        infer_interval_ms(&self.settlements).map(interval_hours)
    }

    /// Ожидаемое время следующего расчёта: последняя отметка + интервал.
    pub fn next_settlement_ms(&self) -> Option<i64> {
        Some(self.last_settlement_ms()? + infer_interval_ms(&self.settlements)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::dec;

    #[test]
    fn hourly_with_jitter() {
        // реальные отметки Hyperliquid содержат миллисекундный «шум»
        let ts = [1_683_849_600_076, 1_683_853_200_003, 1_683_856_800_120, 1_683_860_400_001];
        assert_eq!(infer_interval_ms(&ts), Some(3_600_000));
        let mut t = FundingIntervalTracker::default();
        for x in ts {
            t.observe(x);
        }
        assert_eq!(t.interval_hours(), Some(dec!(1)));
        assert_eq!(t.next_settlement_ms(), Some(1_683_860_400_001 + 3_600_000));
    }

    #[test]
    fn missing_settlement_does_not_change_interval() {
        // пропуск одного расчёта даёт разность 16 ч, но мода остаётся 8 ч
        let h = 3_600_000;
        let ts = [0, 8 * h, 16 * h, 32 * h, 40 * h];
        assert_eq!(infer_interval_ms(&ts), Some(8 * h));
    }

    #[test]
    fn needs_two_points() {
        assert_eq!(infer_interval_ms(&[]), None);
        assert_eq!(infer_interval_ms(&[5, 5]), None);
    }

    proptest! {
        #[test]
        fn recovers_regular_interval(start in 0i64..2_000_000_000_000, hours in 1i64..=8, n in 2usize..20, jitter in proptest::collection::vec(0i64..500, 20)) {
            let interval = hours * 3_600_000;
            let ts: Vec<i64> = (0..n).map(|i| start + i64::try_from(i).unwrap() * interval + jitter[i]).collect();
            prop_assert_eq!(infer_interval_ms(&ts), Some(interval));
        }
    }
}
