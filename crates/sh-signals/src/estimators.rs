//! Оценки спреда фандинга: текущий прогноз, EMA расчётных ставок за 24 ч,
//! реализованное среднее за 7 дней и устойчивость знака спреда.

use rust_decimal::Decimal;
use sh_md::Settlement;

use crate::apr::{HOURS_PER_YEAR, funding_apr};

const MS_PER_HOUR: i64 = 3_600_000;

/// Три оценки спреда (доли годовых) и устойчивость знака.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpreadEstimates {
    /// По текущим прогнозам.
    pub now: Option<Decimal>,
    /// EMA расчётных ставок за 24 ч.
    pub ema_24h: Option<Decimal>,
    /// Реализованное среднее за 7 дней.
    pub realized_7d: Option<Decimal>,
    /// Знак спреда держался последние `k` расчётов.
    pub persistent: bool,
}

/// EMA годовых ставок по расчётам в окне `window_hours` до `now_ms`.
/// Коэффициент `2 / (n + 1)`, где `n` — число интервалов в окне по последнему расчёту.
pub fn ema_apr(settled: &[Settlement], now_ms: i64, window_hours: i64) -> Option<Decimal> {
    let from = now_ms - window_hours * MS_PER_HOUR;
    let window: Vec<&Settlement> = settled.iter().filter(|s| s.ts_ms > from && s.ts_ms <= now_ms).collect();
    let last = window.last()?;
    let n = Decimal::from(window_hours) / last.interval_hours;
    let alpha = Decimal::TWO / (n + Decimal::ONE);
    let mut ema: Option<Decimal> = None;
    for s in window {
        let apr = funding_apr(s.rate, s.interval_hours)?;
        ema = Some(match ema {
            None => apr,
            Some(prev) => prev + alpha * (apr - prev),
        });
    }
    ema
}

/// Реализованный фандинг за окно в годовых: сумма ставок × 8760 / покрытые часы.
/// Покрытие — от первого расчёта в окне (с его интервалом) до `now_ms`, не больше окна;
/// так новые листинги не занижаются, но и не завышаются.
pub fn realized_apr(settled: &[Settlement], now_ms: i64, window_hours: i64) -> Option<Decimal> {
    let from = now_ms - window_hours * MS_PER_HOUR;
    let window: Vec<&Settlement> = settled.iter().filter(|s| s.ts_ms > from && s.ts_ms <= now_ms).collect();
    let first = window.first()?;
    let start = (first.ts_ms - ms_of_hours(first.interval_hours)).max(from);
    let covered_ms = now_ms - start;
    if covered_ms <= 0 {
        return None;
    }
    let sum: Decimal = window.iter().map(|s| s.rate).sum();
    Some(sum * HOURS_PER_YEAR * Decimal::from(MS_PER_HOUR) / Decimal::from(covered_ms))
}

fn ms_of_hours(h: Decimal) -> i64 {
    (h * Decimal::from(MS_PER_HOUR)).trunc().try_into().unwrap_or(0)
}

/// Ряд спреда `APR_short - APR_long` в моменты расчётов ноги с более длинным интервалом;
/// для второй ноги берётся последний расчёт не позже этого момента.
pub fn spread_series(short: &[Settlement], long: &[Settlement]) -> Vec<(i64, Decimal)> {
    let interval = |v: &[Settlement]| v.last().map(|s| s.interval_hours).unwrap_or(Decimal::ZERO);
    let (anchor, other, anchor_is_short) =
        if interval(short) >= interval(long) { (short, long, true) } else { (long, short, false) };
    let mut out = Vec::with_capacity(anchor.len());
    for a in anchor {
        let Some(o) = other.iter().rev().find(|o| o.ts_ms <= a.ts_ms) else { continue };
        let (Some(apr_a), Some(apr_o)) = (funding_apr(a.rate, a.interval_hours), funding_apr(o.rate, o.interval_hours))
        else {
            continue;
        };
        let s = if anchor_is_short { apr_a - apr_o } else { apr_o - apr_a };
        out.push((a.ts_ms, s));
    }
    out
}

/// Истина, если последние `k` значений ряда строго положительны.
pub fn sign_persistent(series: &[(i64, Decimal)], k: usize) -> bool {
    k > 0 && series.len() >= k && series[series.len() - k..].iter().all(|(_, s)| *s > Decimal::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::dec;

    fn hourly(start_ms: i64, rates: &[Decimal]) -> Vec<Settlement> {
        rates
            .iter()
            .enumerate()
            .map(|(i, r)| Settlement { ts_ms: start_ms + i as i64 * MS_PER_HOUR, rate: *r, interval_hours: dec!(1) })
            .collect()
    }

    #[test]
    fn constant_rate_estimators_agree() {
        let s = hourly(MS_PER_HOUR, &[dec!(0.0001); 200]);
        let now = s.last().unwrap().ts_ms;
        let apr = dec!(0.876);
        assert_eq!(ema_apr(&s, now, 24), Some(apr));
        assert_eq!(realized_apr(&s, now, 168), Some(apr));
    }

    #[test]
    fn realized_uses_covered_hours_for_short_history() {
        // 10 часовых расчётов по 0.0001 → 0.001 за 10 ч → 0.876 годовых, а не 0.876 * 10/168
        let s = hourly(MS_PER_HOUR, &[dec!(0.0001); 10]);
        let now = s.last().unwrap().ts_ms;
        assert_eq!(realized_apr(&s, now, 168), Some(dec!(0.876)));
        assert_eq!(realized_apr(&[], now, 168), None);
    }

    #[test]
    fn ema_weights_recent_more() {
        let mut rates = vec![dec!(0); 23];
        rates.push(dec!(0.001));
        let s = hourly(MS_PER_HOUR, &rates);
        let now = s.last().unwrap().ts_ms;
        let ema = ema_apr(&s, now, 24).unwrap();
        // alpha = 2/25 = 0.08 → 0.08 * 8.76
        assert_eq!(ema, dec!(0.7008));
    }

    #[test]
    fn spread_series_aligns_mixed_intervals() {
        let h = MS_PER_HOUR;
        // короткая нога — Aster 8 ч, длинная — HL 1 ч
        let short = vec![
            Settlement { ts_ms: 8 * h, rate: dec!(0.0008), interval_hours: dec!(8) },
            Settlement { ts_ms: 16 * h, rate: dec!(0.0008), interval_hours: dec!(8) },
        ];
        let long = hourly(h, &[dec!(0.00001); 16]);
        let series = spread_series(&short, &long);
        assert_eq!(series.len(), 2);
        // 0.0008*1095 - 0.00001*8760 = 0.876 - 0.0876
        assert_eq!(series[0].1, dec!(0.7884));
        assert!(sign_persistent(&series, 2));
        assert!(!sign_persistent(&series, 3));
        // ноги поменяны местами — знак меняется
        let flipped = spread_series(&long, &short);
        assert_eq!(flipped[0].1, dec!(-0.7884));
    }

    proptest! {
        #[test]
        fn realized_is_bounded_by_min_max_apr(rates in proptest::collection::vec(-100i64..100, 1..300)) {
            let rates: Vec<Decimal> = rates.into_iter().map(|r| Decimal::new(r, 6)).collect();
            let s = hourly(MS_PER_HOUR, &rates);
            let now = s.last().unwrap().ts_ms;
            let r = realized_apr(&s, now, 168).unwrap();
            let in_window: Vec<Decimal> = s.iter().filter(|x| x.ts_ms > now - 168 * MS_PER_HOUR).map(|x| x.rate * dec!(8760)).collect();
            let lo = in_window.iter().copied().min().unwrap();
            let hi = in_window.iter().copied().max().unwrap();
            prop_assert!(r >= lo - dec!(0.0000001) && r <= hi + dec!(0.0000001));
            let e = ema_apr(&s, now, 24).unwrap();
            let in24: Vec<Decimal> = s.iter().filter(|x| x.ts_ms > now - 24 * MS_PER_HOUR).map(|x| x.rate * dec!(8760)).collect();
            prop_assert!(e >= *in24.iter().min().unwrap() - dec!(0.0000001) && e <= *in24.iter().max().unwrap() + dec!(0.0000001));
        }
    }
}
