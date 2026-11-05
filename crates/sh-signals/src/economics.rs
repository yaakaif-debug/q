//! Экономика позиции: `s_net`, точка безубыточности, капитал и ROC.
//!
//! `ROC = s_net · N / C`, `C = N / L_a + N / L_b + R`, где резерв `R` — доля `r` от `C`,
//! то есть `C = (N / L_a + N / L_b) / (1 − r)`. При `L = 3` и `r = 15%` `N / C ≈ 1.27`.

use rust_decimal::Decimal;
use sh_core::Usd;

use crate::apr::HOURS_PER_YEAR;

/// `s_net = s_ema − cost · 8760 / (N · H_exp)` (доли годовых).
pub fn s_net(s_ema: Decimal, cost: Usd, notional: Usd, hold_hours: Decimal) -> Option<Decimal> {
    if notional.0 <= Decimal::ZERO || hold_hours <= Decimal::ZERO {
        return None;
    }
    Some(s_ema - cost.0 * HOURS_PER_YEAR / (notional.0 * hold_hours))
}

/// Часы, за которые спред `s` (годовой) окупает издержки: `cost / (N · s / 8760)`.
pub fn breakeven_hours(cost: Usd, s: Decimal, notional: Usd) -> Option<Decimal> {
    if s <= Decimal::ZERO || notional.0 <= Decimal::ZERO {
        return None;
    }
    Some(cost.0 * HOURS_PER_YEAR / (notional.0 * s))
}

/// Капитал под пару: маржа обеих ног и резерв `reserve_frac` от итогового капитала.
pub fn capital_for(notional: Usd, lev_a: Decimal, lev_b: Decimal, reserve_frac: Decimal) -> Option<Usd> {
    if lev_a <= Decimal::ZERO || lev_b <= Decimal::ZERO || reserve_frac < Decimal::ZERO || reserve_frac >= Decimal::ONE {
        return None;
    }
    let margin = notional.0 / lev_a + notional.0 / lev_b;
    Some(Usd(margin / (Decimal::ONE - reserve_frac)))
}

/// Доходность на капитал: `s_net · N / C`.
pub fn roc(s_net: Decimal, notional: Usd, capital: Usd) -> Option<Decimal> {
    if capital.0 <= Decimal::ZERO {
        return None;
    }
    Some(s_net * notional.0 / capital.0)
}

/// Чистый спред, нужный для целевого ROC: `target · C / N` (печатается рядом с каждой парой).
pub fn required_net_spread(target_roc: Decimal, notional: Usd, capital: Usd) -> Option<Decimal> {
    if notional.0 <= Decimal::ZERO {
        return None;
    }
    Some(target_roc * capital.0 / notional.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::dec;

    #[test]
    fn spec_example_l3_reserve15() {
        let n = Usd(dec!(100000));
        let c = capital_for(n, dec!(3), dec!(3), dec!(0.15)).unwrap();
        let ratio = n.0 / c.0;
        assert!((ratio - dec!(1.275)).abs() < dec!(0.0001));
        // 15% ROC требует ~11.76% чистого спреда (в спецификации «около 12%»)
        let need = required_net_spread(dec!(0.15), n, c).unwrap();
        assert!((need - dec!(0.1176)).abs() < dec!(0.0001));
        assert!((roc(need, n, c).unwrap() - dec!(0.15)).abs() < dec!(0.0000001));
    }

    #[test]
    fn s_net_and_breakeven() {
        // издержки $100 на $10k, держим 168 ч: снижение 100*8760/(10000*168) = 0.5214...
        let s = s_net(dec!(0.8), Usd(dec!(100)), Usd(dec!(10000)), dec!(168)).unwrap();
        assert!((s - dec!(0.27857)).abs() < dec!(0.00001));
        // спред 87.6% годовых на $10k даёт $1 в час → $100 окупаются за 100 ч
        assert_eq!(breakeven_hours(Usd(dec!(100)), dec!(0.876), Usd(dec!(10000))), Some(dec!(100)));
        assert_eq!(breakeven_hours(Usd(dec!(100)), dec!(-0.1), Usd(dec!(10000))), None);
        assert!(s_net(dec!(0.1), Usd(dec!(1)), Usd::ZERO, dec!(1)).is_none());
        assert!(capital_for(Usd(dec!(1)), dec!(3), dec!(3), dec!(1)).is_none());
    }

    proptest! {
        #[test]
        fn breakeven_matches_s_net_zero(cost in 1i64..10_000, s_bp in 1i64..20_000, n in 1_000i64..1_000_000) {
            let cost = Usd(Decimal::from(cost));
            let s = Decimal::new(s_bp, 4);
            let n = Usd(Decimal::from(n));
            // держим ровно H_be часов — чистый спред равен нулю
            let h = breakeven_hours(cost, s, n).unwrap();
            let net = s_net(s, cost, n, h).unwrap();
            prop_assert!(net.abs() < dec!(0.000001));
        }

        #[test]
        fn roc_monotone_in_leverage(l in 1i64..20, extra in 1i64..20, s_bp in 1i64..5000) {
            let n = Usd(dec!(10000));
            let s = Decimal::new(s_bp, 4);
            let c1 = capital_for(n, Decimal::from(l), Decimal::from(l), dec!(0.15)).unwrap();
            let c2 = capital_for(n, Decimal::from(l + extra), Decimal::from(l + extra), dec!(0.15)).unwrap();
            prop_assert!(roc(s, n, c2).unwrap() > roc(s, n, c1).unwrap());
        }
    }
}
