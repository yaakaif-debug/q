//! Нормализованные метаданные инструментов. Тики, лоты, интервалы фандинга
//! и комиссии берутся только из ответов площадок.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Collateral, Px, Qty, Usd, VenueKey};

/// Правило допустимых цен.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PriceRule {
    /// Фиксированный тик.
    Tick {
        /// Шаг цены.
        tick: Px,
    },
    /// Hyperliquid: не более `max_sig_figs` значащих цифр и `max_decimals` знаков
    /// после запятой; целые цены допустимы всегда.
    SigFigs {
        /// Максимум значащих цифр.
        max_sig_figs: u32,
        /// Максимум десятичных знаков.
        max_decimals: u32,
    },
}

impl PriceRule {
    /// Проверяет цену на соответствие правилу.
    pub fn is_valid(&self, px: Px) -> bool {
        let v = px.0.normalize();
        if v <= Decimal::ZERO {
            return false;
        }
        match self {
            PriceRule::Tick { tick } => tick.0 > Decimal::ZERO && (v / tick.0).fract().is_zero(),
            PriceRule::SigFigs { max_sig_figs, max_decimals } => {
                if v.fract().is_zero() {
                    return true;
                }
                if v.scale() > *max_decimals {
                    return false;
                }
                significant_digits(v) <= *max_sig_figs
            }
        }
    }
}

/// Направление округления цены.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// Вниз (покупка не дороже).
    Down,
    /// Вверх (продажа не дешевле).
    Up,
}

fn round_dp(v: Decimal, dp: u32, mode: Rounding) -> Decimal {
    let strategy = match mode {
        Rounding::Down => rust_decimal::RoundingStrategy::ToNegativeInfinity,
        Rounding::Up => rust_decimal::RoundingStrategy::ToPositiveInfinity,
    };
    v.round_dp_with_strategy(dp, strategy).normalize()
}

impl PriceRule {
    /// Округляет цену до допустимой в заданную сторону; `None` для неположительных.
    pub fn round(&self, px: Px, mode: Rounding) -> Option<Px> {
        let v = px.0;
        if v <= Decimal::ZERO {
            return None;
        }
        let out = match self {
            PriceRule::Tick { tick } => {
                if tick.0 <= Decimal::ZERO {
                    return None;
                }
                let n = v / tick.0;
                let n = match mode {
                    Rounding::Down => n.floor(),
                    Rounding::Up => n.ceil(),
                };
                (n * tick.0).normalize()
            }
            PriceRule::SigFigs { max_sig_figs, max_decimals } => {
                let int_digits = u32::try_from(v.trunc().abs().to_string().trim_start_matches('0').len()).unwrap_or(0);
                let dp = if int_digits >= *max_sig_figs {
                    0
                } else if int_digits > 0 {
                    (*max_sig_figs - int_digits).min(*max_decimals)
                } else {
                    // для v < 1 значащие цифры отсчитываются от первой ненулевой
                    let mut lead = 0u32;
                    let mut x = v;
                    while x < Decimal::ONE / Decimal::TEN && lead < 28 {
                        x *= Decimal::TEN;
                        lead += 1;
                    }
                    (lead + *max_sig_figs).min(*max_decimals)
                };
                round_dp(v, dp, mode)
            }
        };
        (out > Decimal::ZERO && self.is_valid(Px(out))).then_some(Px(out))
    }
}

/// Число значащих цифр нормализованного десятичного числа.
fn significant_digits(v: Decimal) -> u32 {
    let digits = v.mantissa().unsigned_abs().to_string();
    // ведущих нулей в мантиссе нет, хвостовые убраны normalize()
    u32::try_from(digits.trim_start_matches('0').len()).unwrap_or(u32::MAX)
}

/// Откуда известны комиссии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeSource {
    /// Публичные метаданные площадки (базовая ставка рынка).
    Metadata,
    /// Нужен приватный запрос по аккаунту (уровень комиссий); в M0 недоступно.
    AccountRequired,
}

/// Сведения о комиссиях инструмента.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeInfo {
    /// Комиссия мейкера (доля), если известна из метаданных.
    pub maker: Option<Decimal>,
    /// Комиссия тейкера (доля), если известна из метаданных.
    pub taker: Option<Decimal>,
    /// Источник.
    pub source: FeeSource,
    /// HIP-3: `deployerFeeScale`.
    pub deployer_fee_scale: Option<Decimal>,
    /// HIP-3: включён ли growth mode.
    pub growth_mode: Option<bool>,
}

/// Статус инструмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstrumentStatus {
    /// Торгуется.
    Active,
    /// Не торгуется (неактивен, приостановлен).
    Inactive,
    /// Делистинг.
    Delisted,
}

/// Метаданные инструмента.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentMeta {
    /// Площадка.
    pub venue: VenueKey,
    /// Символ площадки.
    pub symbol: String,
    /// Числовой идентификатор площадки (asset id HL, market_id Lighter).
    pub venue_id: Option<u64>,
    /// Базовый актив в записи площадки.
    pub base: String,
    /// Актив обеспечения.
    pub collateral: Collateral,
    /// Правило цен.
    pub price_rule: PriceRule,
    /// Шаг количества.
    pub lot_size: Qty,
    /// Минимальное количество ордера.
    pub min_qty: Option<Qty>,
    /// Минимальный номинал ордера.
    pub min_notional: Option<Usd>,
    /// Максимальное плечо.
    pub max_leverage: Option<Decimal>,
    /// Режим маржи как у площадки (`cross`, `strictIsolated`, ...).
    pub margin_mode: Option<String>,
    /// Интервал фандинга в часах; `None` — ещё не выведен.
    pub funding_interval_hours: Option<Decimal>,
    /// Комиссии.
    pub fees: FeeInfo,
    /// Статус.
    pub status: InstrumentStatus,
    /// Время листинга, мс.
    pub listed_at_ms: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn hl_price_rule_examples_from_docs() {
        // примеры из документации Hyperliquid «Tick and lot size» (perps, MAX_DECIMALS = 6)
        let r = PriceRule::SigFigs { max_sig_figs: 5, max_decimals: 6 };
        assert!(r.is_valid(Px(dec!(1234.5))));
        assert!(!r.is_valid(Px(dec!(1234.56))));
        assert!(r.is_valid(Px(dec!(0.001234))));
        assert!(!r.is_valid(Px(dec!(0.0012345))));
        assert!(r.is_valid(Px(dec!(123456))));
        // szDecimals = 1 → не более 5 знаков после запятой
        let r1 = PriceRule::SigFigs { max_sig_figs: 5, max_decimals: 5 };
        assert!(r1.is_valid(Px(dec!(0.01234))));
        assert!(!r1.is_valid(Px(dec!(0.012345))));
    }

    #[test]
    fn rounding_to_rules() {
        let hl = PriceRule::SigFigs { max_sig_figs: 5, max_decimals: 6 };
        assert_eq!(hl.round(Px(dec!(1234.56)), Rounding::Down), Some(Px(dec!(1234.5))));
        assert_eq!(hl.round(Px(dec!(1234.56)), Rounding::Up), Some(Px(dec!(1234.6))));
        assert_eq!(hl.round(Px(dec!(123456.7)), Rounding::Down), Some(Px(dec!(123456))));
        assert_eq!(hl.round(Px(dec!(0.00123456)), Rounding::Down), Some(Px(dec!(0.001234))));
        let hl4 = PriceRule::SigFigs { max_sig_figs: 5, max_decimals: 2 };
        assert_eq!(hl4.round(Px(dec!(1.23456)), Rounding::Up), Some(Px(dec!(1.24))));
        let t = PriceRule::Tick { tick: Px(dec!(0.05)) };
        assert_eq!(t.round(Px(dec!(1.17)), Rounding::Down), Some(Px(dec!(1.15))));
        assert_eq!(t.round(Px(dec!(1.17)), Rounding::Up), Some(Px(dec!(1.2))));
        assert_eq!(t.round(Px(dec!(0)), Rounding::Up), None);
    }

    #[test]
    fn tick_rule() {
        let r = PriceRule::Tick { tick: Px(dec!(0.05)) };
        assert!(r.is_valid(Px(dec!(1.15))));
        assert!(!r.is_valid(Px(dec!(1.17))));
        assert!(!r.is_valid(Px(dec!(0))));
    }
}
