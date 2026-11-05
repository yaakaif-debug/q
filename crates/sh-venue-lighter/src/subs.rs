//! Подписки Lighter.

#![forbid(unsafe_code)]

use serde_json::json;

/// Подписка; порядок вариантов — приоритет.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LtSub {
    /// Статистика всех рынков: фандинг, цены, ОИ.
    MarketStatsAll,
    /// Стакан рынка.
    OrderBook(u32),
    /// Сделки рынка.
    Trade(u32),
    /// BBO рынка.
    Ticker(u32),
}

impl LtSub {
    /// Имя канала для подписки (через `/`).
    pub fn channel(&self) -> String {
        match self {
            LtSub::MarketStatsAll => "market_stats/all".into(),
            LtSub::OrderBook(m) => format!("order_book/{m}"),
            LtSub::Trade(m) => format!("trade/{m}"),
            LtSub::Ticker(m) => format!("ticker/{m}"),
        }
    }

    /// Сообщение подписки.
    pub fn subscribe_msg(&self) -> String {
        json!({"type": "subscribe", "channel": self.channel()}).to_string()
    }

    /// Сообщение отписки.
    pub fn unsubscribe_msg(&self) -> String {
        json!({"type": "unsubscribe", "channel": self.channel()}).to_string()
    }
}

/// Желаемые подписки по убыванию приоритета.
pub fn desired_subs(markets: &[u32], books: bool, trades: bool, ticker: bool) -> Vec<LtSub> {
    let mut out = vec![LtSub::MarketStatsAll];
    if books {
        out.extend(markets.iter().copied().map(LtSub::OrderBook));
    }
    if trades {
        out.extend(markets.iter().copied().map(LtSub::Trade));
    }
    if ticker {
        out.extend(markets.iter().copied().map(LtSub::Ticker));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_docs() {
        assert_eq!(LtSub::OrderBook(0).subscribe_msg(), r#"{"channel":"order_book/0","type":"subscribe"}"#);
        assert_eq!(LtSub::MarketStatsAll.unsubscribe_msg(), r#"{"channel":"market_stats/all","type":"unsubscribe"}"#);
        assert_eq!(desired_subs(&[1, 2], true, false, false).len(), 3);
    }
}
