//! Нормализация ставки фандинга к годовой.

use rust_decimal::Decimal;

/// Часов в году (високосные годы намеренно не учитываем).
pub const HOURS_PER_YEAR: Decimal = Decimal::from_parts(8760, 0, 0, false, 0);

/// Нормализует ставку фандинга за интервал к годовой (APR, доля).
/// Интервал берём из метаданных площадки, а не хардкодим; `None` при неположительном интервале.
pub fn funding_apr(rate_per_interval: Decimal, interval_hours: Decimal) -> Option<Decimal> {
    if interval_hours <= Decimal::ZERO {
        return None;
    }
    Some(rate_per_interval * HOURS_PER_YEAR / interval_hours)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn hourly_and_eight_hourly() {
        // 0.00125% в час = 10.95% годовых
        assert_eq!(funding_apr(dec!(0.0000125), dec!(1)), Some(dec!(0.1095)));
        // 0.01% за 8 ч = 10.95% годовых
        assert_eq!(funding_apr(dec!(0.0001), dec!(8)), Some(dec!(0.1095)));
        assert_eq!(funding_apr(dec!(0.0001), Decimal::ZERO), None);
    }
}
