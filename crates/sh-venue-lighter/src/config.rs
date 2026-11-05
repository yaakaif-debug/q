//! Настройки адаптера Lighter. Неизвестные ключи — ошибка.

use serde::Deserialize;

/// Настройки `[venues.lighter]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct LighterConfig {
    /// Включить адаптер.
    pub enabled: bool,
    /// Торговля; в M0 обязана быть `false`.
    pub trading_enabled: bool,
    /// REST URL.
    pub rest: String,
    /// WebSocket URL; `readonly=true` — режим только чтения (достаточно для рыночных данных).
    pub ws: String,
    /// Подписок на соединение (площадка: 500).
    pub subs_per_connection: usize,
    /// Подписок всего (площадка: 5000 на IP).
    pub max_subscriptions: usize,
    /// Подписка `order_book` на каждый рынок.
    pub subscribe_books: bool,
    /// Подписка `trade` на каждый рынок.
    pub subscribe_trades: bool,
    /// Подписка `ticker` (BBO) на каждый рынок.
    pub subscribe_ticker: bool,
    /// Бюджет REST-запросов в минуту (площадка, Standard: 60 на IP, включая открытие WS).
    pub rest_requests_per_minute: u32,
    /// Бюджет исходящих сообщений WS в минуту (площадка: 200 на IP).
    pub ws_messages_per_minute: u32,
    /// Всплеск исходящих сообщений WS (не более 50 сообщений «в полёте» на площадке).
    pub ws_message_burst: u32,
    /// Период снимка метаданных, с.
    pub metadata_refresh_secs: u64,
    /// При старте загрузить историю фандинга по каждому рынку (интервал и 7-дневная оценка).
    pub funding_backfill: bool,
    /// Глубина истории фандинга при старте, ч.
    pub funding_history_hours: i64,
    /// Период пинга, с (сервер ждёт кадр хотя бы раз в 2 минуты).
    pub ping_secs: u64,
    /// Переподключение при тишине дольше, с.
    pub idle_timeout_secs: u64,
}

impl Default for LighterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trading_enabled: false,
            rest: "https://mainnet.zklighter.elliot.ai".into(),
            ws: "wss://mainnet.zklighter.elliot.ai/stream?readonly=true".into(),
            subs_per_connection: 200,
            max_subscriptions: 2000,
            subscribe_books: true,
            subscribe_trades: true,
            subscribe_ticker: false,
            rest_requests_per_minute: 40,
            ws_message_burst: 10,
            ws_messages_per_minute: 150,
            metadata_refresh_secs: 300,
            funding_backfill: true,
            funding_history_hours: 168,
            ping_secs: 30,
            idle_timeout_secs: 60,
        }
    }
}

impl LighterConfig {
    /// Проверка настроек.
    pub fn validate(&self) -> Result<(), String> {
        if self.trading_enabled {
            return Err("venues.lighter.trading_enabled must be false in M0".into());
        }
        if self.subs_per_connection == 0 || self.subs_per_connection > 500 {
            return Err("venues.lighter.subs_per_connection must be in 1..=500".into());
        }
        if self.max_subscriptions == 0 || self.max_subscriptions > 5000 {
            return Err("venues.lighter.max_subscriptions must be in 1..=5000".into());
        }
        if self.ws_messages_per_minute == 0 || self.ws_messages_per_minute > 200 {
            return Err("venues.lighter.ws_messages_per_minute must be in 1..=200".into());
        }
        if self.ping_secs >= 120 || self.idle_timeout_secs <= self.ping_secs {
            return Err("venues.lighter: need ping_secs < 120 and idle_timeout_secs > ping_secs".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_valid_and_limits_enforced() {
        assert_eq!(LighterConfig::default().validate(), Ok(()));
        assert!(LighterConfig { trading_enabled: true, ..LighterConfig::default() }.validate().is_err());
        assert!(LighterConfig { ws_messages_per_minute: 500, ..LighterConfig::default() }.validate().is_err());
        assert!(LighterConfig { subs_per_connection: 501, ..LighterConfig::default() }.validate().is_err());
    }
}
