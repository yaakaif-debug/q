//! Параметры стратегий, сканера и лимитов риска (секции `[strategy.*]`, `[scanner]`, `[risk]`).
//! Значения в процентах — как в SPEC.md (`min_gross_apr = 15.0` значит 15%).

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use rust_decimal::dec;
use serde::Deserialize;
use sh_core::Exchange;

/// Процент → доля.
pub fn pct(v: Decimal) -> Decimal {
    v / Decimal::ONE_HUNDRED
}

/// Параметры S1 `[strategy.s1_funding_carry]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct S1Params {
    /// Включена ли стратегия.
    pub enabled: bool,
    /// Минимальный валовой спред по всем трём оценкам, % годовых.
    pub min_gross_apr: Decimal,
    /// Минимальный чистый спред, % годовых.
    pub min_net_apr: Decimal,
    /// Порог выхода по EMA, % годовых.
    pub exit_apr: Decimal,
    /// Максимальная точка безубыточности, ч.
    pub max_breakeven_hours: Decimal,
    /// Сколько последних расчётов спред должен держать знак.
    pub k_settlements: usize,
    /// Сколько оценок подряд EMA ниже `exit_apr` до выхода.
    pub m_intervals: u32,
    /// Перевес другой пары после издержек переключения для замены, % годовых.
    pub switch_margin_apr: Decimal,
    /// Ожидаемое время удержания `H_exp`, ч.
    pub expected_hold_hours: Decimal,
    /// Плечо на ногу (дополнительно ограничено лимитами риска и площадкой).
    pub leverage: Decimal,
    /// Максимальный размер дочернего ордера, $.
    pub child_usd: Decimal,
    /// Тайм-аут хеджа, мс.
    pub hedge_timeout_ms: u64,
    /// Повторы хеджа с расширенным лимитом.
    pub hedge_retries: u32,
    /// Запас мейкер-цены, bp.
    pub edge_bps: Decimal,
    /// Перевыставление при сдвиге BBO хедж-площадки, bp.
    pub reprice_bps: Decimal,
    /// Предел цены IOC-хеджа от BBO, bp.
    pub max_slip_bps: Decimal,
    /// Допуск дельты между ногами, % от N.
    pub delta_tolerance_pct: Decimal,
    /// Порог отношения маржи для перебалансировки ноги (доля маржи поддержания к equity ноги).
    pub rebalance_margin_ratio: Decimal,
}

impl Default for S1Params {
    fn default() -> Self {
        Self {
            enabled: true,
            min_gross_apr: dec!(15),
            min_net_apr: dec!(12),
            exit_apr: dec!(3),
            max_breakeven_hours: dec!(72),
            k_settlements: 6,
            m_intervals: 3,
            switch_margin_apr: dec!(5),
            expected_hold_hours: dec!(168),
            leverage: dec!(3),
            child_usd: dec!(5000),
            hedge_timeout_ms: 1500,
            hedge_retries: 2,
            edge_bps: dec!(1),
            reprice_bps: dec!(2),
            max_slip_bps: dec!(15),
            delta_tolerance_pct: dec!(0.5),
            rebalance_margin_ratio: dec!(0.5),
        }
    }
}

/// Параметры S2 `[strategy.s2_cross_dex]`; незаданные поля берутся из S1.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct S2Params {
    /// Включена ли стратегия.
    pub enabled: bool,
    /// Минимальный чистый спред, % годовых.
    pub min_net_apr: Decimal,
    /// Нет новых входов ближе этого к смене сессии базового актива, мин.
    pub session_guard_min: i64,
    /// Переопределение минимального валового спреда, % годовых.
    pub min_gross_apr: Option<Decimal>,
    /// Переопределение порога выхода, % годовых.
    pub exit_apr: Option<Decimal>,
    /// Переопределение точки безубыточности, ч.
    pub max_breakeven_hours: Option<Decimal>,
    /// Переопределение тайм-аута хеджа, мс.
    pub hedge_timeout_ms: Option<u64>,
}

impl Default for S2Params {
    fn default() -> Self {
        Self {
            enabled: true,
            min_net_apr: dec!(10),
            session_guard_min: 60,
            min_gross_apr: None,
            exit_apr: None,
            max_breakeven_hours: None,
            hedge_timeout_ms: None,
        }
    }
}

/// Секция `[strategy]`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct StrategyParams {
    /// S1.
    pub s1_funding_carry: S1Params,
    /// S2.
    pub s2_cross_dex: S2Params,
}

/// Правила входа и выхода для конкретного вида пары (S2 наследует S1).
#[derive(Debug, Clone, PartialEq)]
pub struct PairRules {
    /// Минимальный валовой спред (доля).
    pub min_gross: Decimal,
    /// Минимальный чистый спред (доля).
    pub min_net: Decimal,
    /// Порог выхода (доля).
    pub exit: Decimal,
    /// Максимальная точка безубыточности, ч.
    pub max_breakeven_hours: Decimal,
    /// Устойчивость знака, расчётов.
    pub k_settlements: usize,
    /// Оценок ниже порога до выхода.
    pub m_intervals: u32,
    /// Перевес для замены пары (доля).
    pub switch_margin: Decimal,
    /// Ожидаемое удержание, ч.
    pub hold_hours: Decimal,
    /// Плечо на ногу.
    pub leverage: Decimal,
    /// Охрана сессий, мин (только S2).
    pub session_guard_min: Option<i64>,
    /// Тайм-аут хеджа, мс.
    pub hedge_timeout_ms: u64,
}

impl StrategyParams {
    /// Правила S1.
    pub fn s1_rules(&self) -> PairRules {
        let s = &self.s1_funding_carry;
        PairRules {
            min_gross: pct(s.min_gross_apr),
            min_net: pct(s.min_net_apr),
            exit: pct(s.exit_apr),
            max_breakeven_hours: s.max_breakeven_hours,
            k_settlements: s.k_settlements,
            m_intervals: s.m_intervals,
            switch_margin: pct(s.switch_margin_apr),
            hold_hours: s.expected_hold_hours,
            leverage: s.leverage,
            session_guard_min: None,
            hedge_timeout_ms: s.hedge_timeout_ms,
        }
    }

    /// Правила S2: S1 с переопределениями.
    pub fn s2_rules(&self) -> PairRules {
        let s2 = &self.s2_cross_dex;
        let mut r = self.s1_rules();
        r.min_net = pct(s2.min_net_apr);
        if let Some(v) = s2.min_gross_apr {
            r.min_gross = pct(v);
        }
        if let Some(v) = s2.exit_apr {
            r.exit = pct(v);
        }
        if let Some(v) = s2.max_breakeven_hours {
            r.max_breakeven_hours = v;
        }
        if let Some(v) = s2.hedge_timeout_ms {
            r.hedge_timeout_ms = v;
        }
        r.session_guard_min = Some(s2.session_guard_min);
        r
    }
}

/// Секция `[scanner]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ScannerParams {
    /// Период оценки пар, с.
    pub interval_secs: u64,
    /// Номиналы на ногу для таблицы издержек, $.
    pub notional_tiers_usd: Vec<Decimal>,
    /// Целевой ROC для печати нужного чистого спреда, %.
    pub target_roc_pct: Decimal,
    /// Каталог отчётов.
    pub report_dir: String,
    /// Отправлять дневную сводку в Telegram.
    pub telegram_summary: bool,
    /// Капитал теневого портфеля, $.
    pub shadow_capital_usd: Decimal,
    /// Вести ли теневой портфель.
    pub shadow_enabled: bool,
    /// Файл предложений автосопоставления символов (для одобрения оператором).
    pub suggestions_path: String,
}

impl Default for ScannerParams {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            notional_tiers_usd: vec![dec!(10000), dec!(50000), dec!(100000)],
            target_roc_pct: dec!(15),
            report_dir: "reports".into(),
            telegram_summary: true,
            shadow_capital_usd: dec!(100000),
            shadow_enabled: true,
            suggestions_path: "config/symbols.suggested.toml".into(),
        }
    }
}

/// Лимиты риска `[risk]` (SPEC.md, «Risk management»).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RiskLimits {
    /// Доля капитала на платформу, %: `hyperliquid` (ядро + все HIP-3), `lighter`, `aster`.
    pub max_platform_share_pct: BTreeMap<String, Decimal>,
    /// Доля капитала в S2, %.
    pub max_s2_share_pct: Decimal,
    /// Доля капитала на пару, %.
    pub max_pair_share_pct: Decimal,
    /// Максимальное плечо ноги.
    pub max_leverage_leg: Decimal,
    /// Максимальное плечо ноги Lighter.
    pub max_leverage_lighter: Decimal,
    /// Минимальное расстояние до ликвидации, % от маркировки.
    pub min_liq_distance_pct: Decimal,
    /// Множитель среднего дневного диапазона за 30 дней для расстояния до ликвидации.
    pub liq_distance_adr_mult: Decimal,
    /// Максимум незахеджированного номинала, $.
    pub max_unhedged_usd: Decimal,
    /// Максимум чистой дельты книги, $.
    pub max_net_delta_usd: Decimal,
    /// Максимум средств в переводах, % капитала.
    pub max_in_transit_pct: Decimal,
    /// Дневной лимит убытка, % капитала.
    pub daily_loss_limit_pct: Decimal,
    /// Максимальная просадка от пика, %.
    pub max_drawdown_pct: Decimal,
    /// Устаревание рыночных данных, мс.
    pub md_stale_ms: u64,
    /// Период сверки, с.
    pub reconcile_every_s: u64,
    /// Минимальный ОИ на площадке, $.
    pub min_oi_usd: Decimal,
    /// Минимальный возраст листинга, ч.
    pub min_listing_age_h: i64,
    /// Максимальный отрыв стейблкоина от USDC, %.
    pub max_stable_depeg_pct: Decimal,
    /// Максимальное расхождение маркировок ног, % (иначе разные базовые активы).
    pub max_mark_divergence_pct: Decimal,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_platform_share_pct: BTreeMap::from([
                ("hyperliquid".to_owned(), dec!(60)),
                ("lighter".to_owned(), dec!(40)),
                ("aster".to_owned(), dec!(40)),
            ]),
            max_s2_share_pct: dec!(50),
            max_pair_share_pct: dec!(25),
            max_leverage_leg: dec!(3),
            max_leverage_lighter: dec!(2),
            min_liq_distance_pct: dec!(25),
            liq_distance_adr_mult: dec!(3),
            max_unhedged_usd: dec!(2000),
            max_net_delta_usd: dec!(1000),
            max_in_transit_pct: dec!(25),
            daily_loss_limit_pct: dec!(1),
            max_drawdown_pct: dec!(3),
            md_stale_ms: 2000,
            reconcile_every_s: 10,
            min_oi_usd: dec!(5000000),
            min_listing_age_h: 72,
            max_stable_depeg_pct: dec!(0.3),
            max_mark_divergence_pct: dec!(2),
        }
    }
}

impl RiskLimits {
    /// Предельное плечо ноги на площадке.
    pub fn leverage_cap(&self, exchange: Exchange) -> Decimal {
        match exchange {
            Exchange::Lighter => self.max_leverage_lighter.min(self.max_leverage_leg),
            _ => self.max_leverage_leg,
        }
    }

    /// Доля капитала на платформу (доля); неизвестная платформа — 0.
    pub fn platform_share(&self, exchange: Exchange) -> Decimal {
        self.max_platform_share_pct.get(exchange.as_str()).map(|v| pct(*v)).unwrap_or(Decimal::ZERO)
    }

    /// Проверка значений.
    pub fn validate(&self) -> Result<(), String> {
        for ex in Exchange::ALL {
            if !self.max_platform_share_pct.contains_key(ex.as_str()) {
                return Err(format!("risk.max_platform_share_pct is missing '{ex}'"));
            }
        }
        if self.max_leverage_leg <= Decimal::ZERO || self.max_leverage_lighter <= Decimal::ZERO {
            return Err("risk: leverage caps must be positive".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s2_inherits_s1() {
        let mut p = StrategyParams::default();
        p.s2_cross_dex.exit_apr = Some(dec!(2));
        let r = p.s2_rules();
        assert_eq!(r.min_net, dec!(0.10));
        assert_eq!(r.min_gross, dec!(0.15));
        assert_eq!(r.exit, dec!(0.02));
        assert_eq!(r.session_guard_min, Some(60));
        assert_eq!(p.s1_rules().session_guard_min, None);
    }

    #[test]
    fn risk_defaults_match_spec() {
        let r = RiskLimits::default();
        assert_eq!(r.validate(), Ok(()));
        assert_eq!(r.leverage_cap(Exchange::Lighter), dec!(2));
        assert_eq!(r.leverage_cap(Exchange::Aster), dec!(3));
        assert_eq!(r.platform_share(Exchange::Hyperliquid), dec!(0.6));
        let mut bad = r.clone();
        bad.max_platform_share_pct.remove("aster");
        assert!(bad.validate().is_err());
    }
}
