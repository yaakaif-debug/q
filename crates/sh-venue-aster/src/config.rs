//! Настройки адаптера Aster. Неизвестные ключи — ошибка.

use serde::Deserialize;

/// Настройки `[venues.aster]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AsterConfig {
    /// Включить адаптер.
    pub enabled: bool,
    /// Торговля; в M0 обязана быть `false`.
    pub trading_enabled: bool,
    /// REST URL.
    pub rest: String,
    /// WebSocket URL (без пути).
    pub ws: String,
    /// Потоков на соединение (площадка: 200).
    pub streams_per_connection: usize,
    /// Поток `@depth@100ms` (дельты стакана) на каждый символ.
    pub subscribe_depth: bool,
    /// Поток `@aggTrade` на каждый символ.
    pub subscribe_trades: bool,
    /// Общий поток `!bookTicker` (BBO всех символов в реальном времени).
    /// По умолчанию выключен: около половины объёма записи Aster, а BBO с шагом
    /// 100 мс уже есть в локальном стакане из `@depth@100ms`.
    pub subscribe_book_ticker: bool,
    /// Глубина REST-снимка для синхронизации стакана (5, 10, 20, 50, 100, 500, 1000).
    pub snapshot_depth: u32,
    /// Одновременных запросов снимков.
    pub snapshot_concurrency: usize,
    /// Бюджет веса REST в минуту (площадка: 2400 на IP).
    pub rest_weight_per_minute: u32,
    /// Период снимка метаданных (`exchangeInfo`, `fundingInfo`, `premiumIndex`), с.
    pub metadata_refresh_secs: u64,
    /// Период опроса открытого интереса по каждому символу, с; 0 — не опрашивать.
    pub open_interest_secs: u64,
    /// Период опроса расчётного фандинга по каждому символу, с; 0 — не опрашивать.
    pub settled_funding_secs: u64,
    /// Плановое переподключение до 24-часового лимита площадки, с.
    pub max_connection_age_secs: u64,
    /// Период пинга (кадр Ping), с.
    pub ping_secs: u64,
    /// Переподключение при тишине дольше, с.
    pub idle_timeout_secs: u64,
}

impl Default for AsterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trading_enabled: false,
            rest: "https://fapi.asterdex.com".into(),
            ws: "wss://fstream.asterdex.com".into(),
            streams_per_connection: 180,
            subscribe_depth: true,
            subscribe_trades: true,
            subscribe_book_ticker: false,
            snapshot_depth: 50,
            snapshot_concurrency: 4,
            rest_weight_per_minute: 1500,
            metadata_refresh_secs: 300,
            open_interest_secs: 300,
            settled_funding_secs: 3600,
            max_connection_age_secs: 23 * 3600 + 30 * 60,
            ping_secs: 120,
            idle_timeout_secs: 60,
        }
    }
}

impl AsterConfig {
    /// Проверка настроек.
    pub fn validate(&self) -> Result<(), String> {
        if self.trading_enabled {
            return Err("venues.aster.trading_enabled must be false in M0".into());
        }
        if self.streams_per_connection < 2 || self.streams_per_connection > 200 {
            return Err("venues.aster.streams_per_connection must be in 2..=200".into());
        }
        if ![5, 10, 20, 50, 100, 500, 1000].contains(&self.snapshot_depth) {
            return Err("venues.aster.snapshot_depth must be one of 5, 10, 20, 50, 100, 500, 1000".into());
        }
        if self.rest_weight_per_minute == 0 || self.rest_weight_per_minute > 2400 {
            return Err("venues.aster.rest_weight_per_minute must be in 1..=2400".into());
        }
        if self.max_connection_age_secs == 0 || self.max_connection_age_secs >= 24 * 3600 {
            return Err("venues.aster.max_connection_age_secs must be below 24 h".into());
        }
        Ok(())
    }

    /// Вес REST-снимка стакана по документации.
    pub fn snapshot_weight(&self) -> u32 {
        match self.snapshot_depth {
            0..=50 => 2,
            51..=100 => 5,
            101..=500 => 10,
            _ => 20,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_valid() {
        let c = AsterConfig::default();
        assert_eq!(c.validate(), Ok(()));
        // поток глубины включён, общий BBO-поток выключен ради объёма записи
        assert!(c.subscribe_depth && !c.subscribe_book_ticker);
        assert_eq!(c.snapshot_weight(), 2);
        assert!(AsterConfig { snapshot_depth: 30, ..c.clone() }.validate().is_err());
        assert!(AsterConfig { max_connection_age_secs: 86_400, ..c.clone() }.validate().is_err());
        assert!(AsterConfig { trading_enabled: true, ..c }.validate().is_err());
    }
}
