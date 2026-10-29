//! Идентификаторы площадок, обеспечения и базовых активов.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::num::{Px, Qty};

/// Семейство площадок (биржа). HIP-3 дексы — это `Hyperliquid` с непустым `dex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Exchange {
    /// Hyperliquid: ядро и все HIP-3 дексы.
    Hyperliquid,
    /// Lighter.
    Lighter,
    /// Aster (фьючерсы v3).
    Aster,
}

impl Exchange {
    /// Все поддерживаемые семейства.
    pub const ALL: [Exchange; 3] = [Exchange::Hyperliquid, Exchange::Lighter, Exchange::Aster];

    /// Каноническое имя в нижнем регистре (метки метрик, имена файлов).
    pub const fn as_str(self) -> &'static str {
        match self {
            Exchange::Hyperliquid => "hyperliquid",
            Exchange::Lighter => "lighter",
            Exchange::Aster => "aster",
        }
    }
}

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Exchange {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hyperliquid" => Ok(Exchange::Hyperliquid),
            "lighter" => Ok(Exchange::Lighter),
            "aster" => Ok(Exchange::Aster),
            other => Err(CoreError::UnknownExchange(other.to_owned())),
        }
    }
}

/// Ключ площадки `(exchange, dex)`. Для ядра Hyperliquid и прочих бирж `dex` пустой.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VenueKey {
    /// Семейство площадок.
    pub exchange: Exchange,
    /// Имя HIP-3 декса; пустая строка — ядро или биржа без дексов.
    pub dex: String,
}

impl VenueKey {
    /// Ключ без декса.
    pub fn new(exchange: Exchange) -> Self {
        Self { exchange, dex: String::new() }
    }

    /// Ключ HIP-3 декса (или ядра при пустом имени).
    pub fn hyperliquid(dex: impl Into<String>) -> Self {
        Self { exchange: Exchange::Hyperliquid, dex: dex.into() }
    }

    /// Истина для ядра Hyperliquid и бирж без дексов.
    pub fn is_core(&self) -> bool {
        self.dex.is_empty()
    }
}

impl fmt::Display for VenueKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.dex.is_empty() {
            f.write_str(self.exchange.as_str())
        } else {
            write!(f, "{}:{}", self.exchange.as_str(), self.dex)
        }
    }
}

/// Актив обеспечения позиции.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Collateral {
    /// USDC.
    Usdc,
    /// USDT.
    Usdt,
    /// USDH (Hyperliquid).
    Usdh,
    /// USDe (Ethena).
    Usde,
    /// Любой другой актив, имя как у площадки.
    Other(String),
}

impl Collateral {
    /// Распознаёт актив по тикеру площадки (без учёта регистра).
    pub fn from_symbol(symbol: &str) -> Self {
        match symbol.to_ascii_uppercase().as_str() {
            "USDC" => Collateral::Usdc,
            "USDT" => Collateral::Usdt,
            "USDH" => Collateral::Usdh,
            "USDE" => Collateral::Usde,
            _ => Collateral::Other(symbol.to_owned()),
        }
    }
}

/// Канонический идентификатор базового актива (сопоставление — задача M1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnderlyingId(pub String);

/// Сторона сделки или агрессора.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Покупка (bid).
    Buy,
    /// Продажа (ask).
    Sell,
}

/// Уровень стакана: цена и суммарное количество.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Level {
    /// Цена уровня.
    pub px: Px,
    /// Суммарное количество на уровне; ноль означает удаление уровня.
    pub qty: Qty,
}

impl Level {
    /// Создаёт уровень.
    pub fn new(px: Px, qty: Qty) -> Self {
        Self { px, qty }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_key_display() {
        assert_eq!(VenueKey::hyperliquid("").to_string(), "hyperliquid");
        assert_eq!(VenueKey::hyperliquid("xyz").to_string(), "hyperliquid:xyz");
        assert_eq!(VenueKey::new(Exchange::Aster).to_string(), "aster");
    }

    #[test]
    fn exchange_roundtrip() {
        for ex in Exchange::ALL {
            assert_eq!(ex.as_str().parse::<Exchange>().ok(), Some(ex));
        }
        assert!("binance".parse::<Exchange>().is_err());
    }

    #[test]
    fn collateral_from_symbol() {
        assert_eq!(Collateral::from_symbol("usdc"), Collateral::Usdc);
        assert_eq!(Collateral::from_symbol("USDE"), Collateral::Usde);
        assert_eq!(Collateral::from_symbol("USD1"), Collateral::Other("USD1".into()));
    }
}
