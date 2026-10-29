//! Подписки Hyperliquid и их приоритеты.

use serde_json::json;

/// Одна подписка WebSocket. Порядок вариантов — приоритет при нехватке лимита.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HlSub {
    /// Контексты всех дексов: фандинг, цены, ОИ одной подпиской.
    AllDexsAssetCtxs,
    /// Стакан монеты (20 уровней).
    L2Book(String),
    /// Сделки монеты.
    Trades(String),
    /// BBO монеты.
    Bbo(String),
}

impl HlSub {
    /// Объект `subscription`.
    pub fn subscription(&self) -> serde_json::Value {
        match self {
            HlSub::AllDexsAssetCtxs => json!({"type": "allDexsAssetCtxs"}),
            HlSub::L2Book(c) => json!({"type": "l2Book", "coin": c}),
            HlSub::Trades(c) => json!({"type": "trades", "coin": c}),
            HlSub::Bbo(c) => json!({"type": "bbo", "coin": c}),
        }
    }

    /// Сообщение подписки.
    pub fn subscribe_msg(&self) -> String {
        json!({"method": "subscribe", "subscription": self.subscription()}).to_string()
    }

    /// Сообщение отписки.
    pub fn unsubscribe_msg(&self) -> String {
        json!({"method": "unsubscribe", "subscription": self.subscription()}).to_string()
    }

    /// Монета подписки, если есть.
    pub fn coin(&self) -> Option<&str> {
        match self {
            HlSub::AllDexsAssetCtxs => None,
            HlSub::L2Book(c) | HlSub::Trades(c) | HlSub::Bbo(c) => Some(c),
        }
    }

    /// Краткое описание для логов.
    pub fn label(&self) -> String {
        match self {
            HlSub::AllDexsAssetCtxs => "allDexsAssetCtxs".into(),
            HlSub::L2Book(c) => format!("l2Book:{c}"),
            HlSub::Trades(c) => format!("trades:{c}"),
            HlSub::Bbo(c) => format!("bbo:{c}"),
        }
    }
}

/// Желаемый список подписок по убыванию приоритета: контексты, стаканы, сделки, BBO.
pub fn desired_subs(coins: &[String], books: bool, trades: bool, bbo: bool) -> Vec<HlSub> {
    let mut out = vec![HlSub::AllDexsAssetCtxs];
    if books {
        out.extend(coins.iter().cloned().map(HlSub::L2Book));
    }
    if trades {
        out.extend(coins.iter().cloned().map(HlSub::Trades));
    }
    if bbo {
        out.extend(coins.iter().cloned().map(HlSub::Bbo));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_docs() {
        assert_eq!(
            HlSub::Trades("SOL".into()).subscribe_msg(),
            r#"{"method":"subscribe","subscription":{"coin":"SOL","type":"trades"}}"#
        );
        assert_eq!(
            HlSub::AllDexsAssetCtxs.unsubscribe_msg(),
            r#"{"method":"unsubscribe","subscription":{"type":"allDexsAssetCtxs"}}"#
        );
    }

    #[test]
    fn priority_order() {
        let coins = vec!["BTC".to_owned(), "xyz:NVDA".to_owned()];
        let d = desired_subs(&coins, true, true, false);
        assert_eq!(d[0], HlSub::AllDexsAssetCtxs);
        assert_eq!(d[1], HlSub::L2Book("BTC".into()));
        assert_eq!(d[3], HlSub::Trades("BTC".into()));
        assert_eq!(d.len(), 5);
    }
}
