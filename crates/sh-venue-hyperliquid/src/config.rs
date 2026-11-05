//! Настройки адаптера Hyperliquid. Неизвестные ключи — ошибка.

use serde::Deserialize;

/// Настройки `[venues.hyperliquid]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct HyperliquidConfig {
    /// Включить адаптер.
    pub enabled: bool,
    /// Торговля; в M0 обязана быть `false`.
    pub trading_enabled: bool,
    /// REST URL.
    pub rest: String,
    /// WebSocket URL.
    pub ws: String,
    /// Выбор дексов: в M0 поддерживается только `auto` (ядро + все HIP-3 из `perpDexs`).
    pub dexes: String,
    /// Дексы, которые не записываем.
    pub dex_denylist: Vec<String>,
    /// Лимит подписок на IP (площадка: 1000).
    pub max_subscriptions: usize,
    /// Подписок на одно соединение.
    pub subs_per_connection: usize,
    /// Подписка `l2Book` на каждую монету.
    pub subscribe_books: bool,
    /// Подписка `trades` на каждую монету.
    pub subscribe_trades: bool,
    /// Подписка `bbo` на каждую монету.
    pub subscribe_bbo: bool,
    /// Бюджет веса REST в минуту (площадка: 1200 на IP).
    pub rest_weight_per_minute: u32,
    /// Бюджет исходящих сообщений WS в минуту (площадка: 2000 на IP).
    pub ws_messages_per_minute: u32,
    /// Всплеск исходящих сообщений WS (100 одновременных post на площадке).
    pub ws_message_burst: u32,
    /// Период снимка метаданных и контекстов, с.
    pub metadata_refresh_secs: u64,
    /// Период обнаружения дексов, с.
    pub dex_discovery_secs: u64,
    /// Период опроса `predictedFundings`, с.
    pub predicted_funding_secs: u64,
    /// Период опроса расчётного фандинга `fundingHistory`, с.
    pub settled_funding_secs: u64,
    /// Глубина первой загрузки истории фандинга, ч (168 — для 7-дневной оценки спреда).
    pub settled_funding_lookback_hours: u64,
    /// Опрашивать ли расчётный фандинг по каждой монете.
    pub settled_funding_poll: bool,
    /// Период пинга, с (сервер закрывает соединение после 60 с тишины).
    pub ping_secs: u64,
    /// Переподключение при тишине дольше, с.
    pub idle_timeout_secs: u64,
}

impl Default for HyperliquidConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trading_enabled: false,
            rest: "https://api.hyperliquid.xyz".into(),
            ws: "wss://api.hyperliquid.xyz/ws".into(),
            dexes: "auto".into(),
            dex_denylist: Vec::new(),
            max_subscriptions: 950,
            subs_per_connection: 200,
            subscribe_books: true,
            subscribe_trades: true,
            subscribe_bbo: false,
            rest_weight_per_minute: 800,
            ws_message_burst: 100,
            ws_messages_per_minute: 1500,
            metadata_refresh_secs: 300,
            dex_discovery_secs: 3600,
            predicted_funding_secs: 300,
            settled_funding_secs: 3600,
            settled_funding_lookback_hours: 168,
            settled_funding_poll: true,
            ping_secs: 20,
            idle_timeout_secs: 45,
        }
    }
}

impl HyperliquidConfig {
    /// Проверка настроек.
    pub fn validate(&self) -> Result<(), String> {
        if self.trading_enabled {
            return Err("venues.hyperliquid.trading_enabled must be false in M0".into());
        }
        if self.dexes != "auto" {
            return Err("venues.hyperliquid.dexes: only \"auto\" is supported (discovered from perpDexs)".into());
        }
        if self.max_subscriptions == 0 || self.max_subscriptions > 1000 {
            return Err("venues.hyperliquid.max_subscriptions must be in 1..=1000".into());
        }
        if self.subs_per_connection == 0 {
            return Err("venues.hyperliquid.subs_per_connection must be positive".into());
        }
        // площадка: не более 10 соединений на IP; оставляем одно про запас для latency-probe
        if self.max_subscriptions.div_ceil(self.subs_per_connection) > 9 {
            return Err(
                "venues.hyperliquid: max_subscriptions / subs_per_connection needs more than 9 connections".into()
            );
        }
        if self.idle_timeout_secs <= self.ping_secs {
            return Err("venues.hyperliquid.idle_timeout_secs must exceed ping_secs".into());
        }
        Ok(())
    }

    /// Число WS-соединений под план подписок.
    pub fn connections(&self) -> usize {
        self.max_subscriptions.div_ceil(self.subs_per_connection.max(1)).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let c = HyperliquidConfig::default();
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.connections(), 5);
    }

    #[test]
    fn rejects_trading_and_static_dex_lists() {
        let c = HyperliquidConfig { trading_enabled: true, ..HyperliquidConfig::default() };
        assert!(c.validate().is_err());
        let c = HyperliquidConfig { dexes: "xyz,flx".into(), ..HyperliquidConfig::default() };
        assert!(c.validate().is_err());
        let c = HyperliquidConfig { subs_per_connection: 50, ..HyperliquidConfig::default() };
        assert!(c.validate().is_err());
    }
}
