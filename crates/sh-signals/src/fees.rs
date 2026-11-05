//! Комиссии по инструменту. Площадки публикуют их по-разному
//! (docs/venues/*.md): Hyperliquid — уровень аккаунта (из `userFees`) с множителем
//! HIP-3 из метаданных; Lighter — по типу аккаунта; Aster — только приватно
//! (`commissionRate`). Без ключей используются значения уровня аккаунта из конфига.

use rust_decimal::Decimal;
use rust_decimal::dec;
use serde::Deserialize;
use sh_core::Exchange;
use sh_venue_api::{FeeSource, InstrumentMeta};

use crate::params::pct;

/// Ставки комиссий (доли).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeeRates {
    /// Мейкер.
    pub maker: Decimal,
    /// Тейкер.
    pub taker: Decimal,
}

/// Ставки уровня аккаунта на площадке, % (`[fees.<venue>]`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VenueFees {
    /// Мейкер, %.
    pub maker_pct: Decimal,
    /// Тейкер, %.
    pub taker_pct: Decimal,
}

/// Секция `[fees]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct FeesConfig {
    /// Hyperliquid: базовый уровень ядра (Tier 0: 0.015% / 0.045%).
    pub hyperliquid: VenueFees,
    /// Lighter: тип аккаунта `standard` (0/0) или `premium` (ставки рынка из метаданных).
    pub lighter_account_tier: String,
    /// Lighter Premium при 0 LIT, если в метаданных ставки нет.
    pub lighter_premium: VenueFees,
    /// Aster: ставки аккаунта (SPEC.md: 0% / 0.04%; UNVERIFIED, только `commissionRate`).
    pub aster: VenueFees,
}

impl Default for FeesConfig {
    fn default() -> Self {
        Self {
            hyperliquid: VenueFees { maker_pct: dec!(0.015), taker_pct: dec!(0.045) },
            lighter_account_tier: "standard".into(),
            lighter_premium: VenueFees { maker_pct: dec!(0.004), taker_pct: dec!(0.028) },
            aster: VenueFees { maker_pct: dec!(0), taker_pct: dec!(0.04) },
        }
    }
}

impl FeesConfig {
    /// Проверка.
    pub fn validate(&self) -> Result<(), String> {
        match self.lighter_account_tier.as_str() {
            "standard" | "premium" => Ok(()),
            other => Err(format!("fees.lighter_account_tier must be standard or premium, got {other}")),
        }
    }

    /// Ставки для инструмента.
    pub fn resolve(&self, meta: &InstrumentMeta) -> FeeRates {
        match meta.venue.exchange {
            Exchange::Hyperliquid => {
                let base = FeeRates { maker: pct(self.hyperliquid.maker_pct), taker: pct(self.hyperliquid.taker_pct) };
                hip3_scaled(base, &meta.venue.dex, meta.fees.deployer_fee_scale, meta.fees.growth_mode)
            }
            Exchange::Lighter => {
                if self.lighter_account_tier == "standard" {
                    // Standard: 0 / 0 на обычных ордерах (docs/venues/lighter.md)
                    FeeRates::default()
                } else if meta.fees.source == FeeSource::Metadata && meta.fees.maker.is_some() {
                    FeeRates { maker: meta.fees.maker.unwrap_or_default(), taker: meta.fees.taker.unwrap_or_default() }
                } else {
                    FeeRates { maker: pct(self.lighter_premium.maker_pct), taker: pct(self.lighter_premium.taker_pct) }
                }
            }
            Exchange::Aster => FeeRates { maker: pct(self.aster.maker_pct), taker: pct(self.aster.taker_pct) },
        }
    }
}

/// Множитель комиссий HIP-3 по документации Hyperliquid «Fees»:
/// `feeScale + 1` при `feeScale < 1`, иначе `2 · feeScale`; growth mode — ×0.1.
/// Без `deployerFeeScale` считаем 1 (комиссия вдвое выше ядра). Ядро — без множителя.
pub fn hip3_scaled(base: FeeRates, dex: &str, fee_scale: Option<Decimal>, growth: Option<bool>) -> FeeRates {
    if dex.is_empty() {
        return base;
    }
    let s = fee_scale.unwrap_or(Decimal::ONE);
    let mut mult = if s < Decimal::ONE { s + Decimal::ONE } else { Decimal::TWO * s };
    if growth == Some(true) {
        mult *= dec!(0.1);
    }
    FeeRates { maker: base.maker * mult, taker: base.taker * mult }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hip3_scaling_matches_docs() {
        let base = FeeRates { maker: dec!(0.00015), taker: dec!(0.00045) };
        assert_eq!(hip3_scaled(base, "", Some(dec!(3)), Some(true)), base);
        assert_eq!(hip3_scaled(base, "xyz", None, None).taker, dec!(0.0009));
        assert_eq!(hip3_scaled(base, "xyz", Some(dec!(0.5)), None).taker, dec!(0.000675));
        // growth mode: 2 * 1.0 * 0.1 = 0.2 от базы
        assert_eq!(hip3_scaled(base, "xyz", Some(dec!(1.0)), Some(true)).taker, dec!(0.00009));
    }

    #[test]
    fn tier_validation() {
        let mut f = FeesConfig::default();
        assert!(f.validate().is_ok());
        f.lighter_account_tier = "vip".into();
        assert!(f.validate().is_err());
    }
}
