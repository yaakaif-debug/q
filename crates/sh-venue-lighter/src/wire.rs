//! Типы сообщений Lighter как их присылает площадка.
//! Источник: docs/venues/lighter.md (документация и живые ответы 2026-10-29).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::Deserialize;
use sh_core::serde_util::{lenient_decimal, lenient_opt};
use sh_core::{Px, Qty};

/// Уровень стакана: `size` — новый итог, `"0"` удаляет уровень.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WsLevel {
    /// Цена.
    pub price: Px,
    /// Размер.
    pub size: Qty,
}

/// Тело `order_book`.
#[derive(Debug, Clone, Deserialize)]
pub struct WsOrderBook {
    /// Asks.
    #[serde(default)]
    pub asks: Vec<WsLevel>,
    /// Bids.
    #[serde(default)]
    pub bids: Vec<WsLevel>,
    /// Номер последнего изменения (непрерывность: `begin_nonce` следующего == этот `nonce`).
    pub nonce: u64,
    /// Номер, с которого начинается сообщение (0 в снимке).
    #[serde(default)]
    pub begin_nonce: u64,
    /// Время последнего изменения, мкс.
    #[serde(default)]
    pub last_updated_at: Option<i64>,
}

/// Сделка.
#[derive(Debug, Clone, Deserialize)]
pub struct WsTrade {
    /// Идентификатор сделки.
    pub trade_id: u64,
    /// Тип: `trade`, `liquidation`, `deleverage`, `market-settlement`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Рынок.
    pub market_id: u32,
    /// Размер.
    pub size: Qty,
    /// Цена.
    pub price: Px,
    /// Мейкер был на стороне ask.
    pub is_maker_ask: bool,
    /// Время блока, мс.
    pub timestamp: i64,
}

/// Статистика рынка (`market_stats`). Ставки фандинга — в процентах за интервал.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketStats {
    /// Символ.
    pub symbol: String,
    /// Рынок.
    pub market_id: u32,
    /// Индекс.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub index_price: Option<Px>,
    /// Цена маркировки.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub mark_price: Option<Px>,
    /// Mid.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub mid_price: Option<Px>,
    /// Открытый интерес одной стороны в USD (живая проверка: `open_interest_usd = 2 * open_interest`).
    #[serde(default, deserialize_with = "lenient_decimal")]
    pub open_interest: Option<Decimal>,
    /// Оценка ближайшего платежа, %.
    #[serde(default, deserialize_with = "lenient_decimal")]
    pub current_funding_rate: Option<Decimal>,
    /// Последний расчётный платёж, %.
    #[serde(default, deserialize_with = "lenient_decimal")]
    pub funding_rate: Option<Decimal>,
    /// Время последнего расчёта, мс.
    #[serde(default)]
    pub funding_timestamp: Option<i64>,
}

/// Разбирает `market_stats`: словарь по `market_id` (`market_stats/all`) или один объект.
/// Элементы разбираются по отдельности: ошибка в одном рынке не теряет остальные.
/// Возвращает статистики и число пропущенных элементов.
pub fn parse_market_stats(raw: &serde_json::value::RawValue) -> (Vec<MarketStats>, usize) {
    if let Ok(one) = serde_json::from_str::<MarketStats>(raw.get()) {
        return (vec![one], 0);
    }
    let Ok(map) = serde_json::from_str::<BTreeMap<String, &serde_json::value::RawValue>>(raw.get()) else {
        return (Vec::new(), 1);
    };
    let mut out = Vec::with_capacity(map.len());
    let mut skipped = 0;
    for v in map.values() {
        match serde_json::from_str::<MarketStats>(v.get()) {
            Ok(s) => out.push(s),
            Err(_) => skipped += 1,
        }
    }
    (out, skipped)
}

/// BBO `ticker`.
#[derive(Debug, Clone, Deserialize)]
pub struct WsTicker {
    /// Символ.
    #[serde(default)]
    pub s: String,
    /// Лучший ask.
    pub a: Option<WsLevel>,
    /// Лучший bid.
    pub b: Option<WsLevel>,
}

/// Ошибка площадки.
#[derive(Debug, Clone, Deserialize)]
pub struct WsErrorBody {
    /// Код.
    #[serde(default)]
    pub code: i64,
    /// Текст.
    #[serde(default)]
    pub message: String,
}

/// Конверт сообщения WebSocket (поля зависят от `type`).
#[derive(Debug, Clone, Deserialize)]
pub struct WsEnvelope<'a> {
    /// `connected`, `subscribed/<ch>`, `update/<ch>`, `pong`, ...
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Канал с двоеточием (`order_book:0`).
    #[serde(default)]
    pub channel: Option<String>,
    /// Время кадра на сервере API, мс.
    #[serde(default)]
    pub timestamp: Option<i64>,
    /// Стакан.
    #[serde(default)]
    pub order_book: Option<WsOrderBook>,
    /// Сделки.
    #[serde(default)]
    pub trades: Option<Vec<WsTrade>>,
    /// Ликвидационные сделки.
    #[serde(default)]
    pub liquidation_trades: Option<Vec<WsTrade>>,
    /// Статистика (разбирается отдельно, см. `parse_market_stats`).
    #[serde(default, borrow)]
    pub market_stats: Option<&'a serde_json::value::RawValue>,
    /// BBO.
    #[serde(default)]
    pub ticker: Option<WsTicker>,
    /// Ошибка.
    #[serde(default)]
    pub error: Option<WsErrorBody>,
}

/// Конфигурация рынка из `orderBookDetails`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MarketConfig {
    /// Режим маржи рынка.
    #[serde(default)]
    pub market_margin_mode: Option<i64>,
    /// Только сокращение позиций.
    #[serde(default)]
    pub force_reduce_only: Option<bool>,
    /// Часы торговли (пусто — круглосуточно).
    #[serde(default)]
    pub trading_hours: Option<String>,
    /// Скрытый рынок.
    #[serde(default)]
    pub hidden: Option<bool>,
}

/// Perp-рынок из `orderBookDetails`. Комиссии — в процентах.
#[derive(Debug, Clone, Deserialize)]
pub struct PerpDetail {
    /// Символ.
    pub symbol: String,
    /// Рынок.
    pub market_id: u32,
    /// `active` / `inactive`.
    pub status: String,
    /// Комиссия тейкера, %.
    pub taker_fee: Decimal,
    /// Комиссия мейкера, %.
    pub maker_fee: Decimal,
    /// Минимальный размер ордера.
    pub min_base_amount: Qty,
    /// Минимальный номинал ордера.
    pub min_quote_amount: Decimal,
    /// Десятичные знаки размера.
    pub size_decimals: u32,
    /// Десятичные знаки цены.
    pub price_decimals: u32,
    /// Минимальная начальная маржа (доля в 1/10000).
    #[serde(default)]
    pub min_initial_margin_fraction: Option<u32>,
    /// Время создания рынка, мс (строкой).
    #[serde(default)]
    pub created_at: Option<String>,
    /// Конфигурация рынка.
    #[serde(default)]
    pub market_config: Option<MarketConfig>,
}

/// Ответ `orderBookDetails`.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderBookDetails {
    /// Perp-рынки.
    #[serde(default)]
    pub order_book_details: Vec<PerpDetail>,
}

/// Элемент `fundings`.
#[derive(Debug, Clone, Deserialize)]
pub struct FundingItem {
    /// Время расчёта, **секунды**.
    pub timestamp: i64,
    /// Ставка, %.
    pub rate: Decimal,
}

/// Ответ `fundings`.
#[derive(Debug, Clone, Deserialize)]
pub struct Fundings {
    /// Расчёты.
    #[serde(default)]
    pub fundings: Vec<FundingItem>,
}
