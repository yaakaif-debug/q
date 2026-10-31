//! Типы сообщений Aster (futures v3) как их присылает площадка.
//! Источник: docs/venues/aster.md (документация и живые ответы 2026-10-29).

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::value::RawValue;
use sh_core::serde_util::{lenient_decimal, lenient_opt};
use sh_core::{Px, Qty};

/// Конверт комбинированного потока `{"stream": ..., "data": ...}`.
#[derive(Debug, Deserialize)]
pub struct Combined<'a> {
    /// Имя потока (`btcusdt@depth@100ms`).
    pub stream: &'a str,
    /// Данные.
    #[serde(borrow)]
    pub data: &'a RawValue,
}

/// Уровень `[price, qty]`.
pub type PxQty = (Px, Qty);

/// Дельта стакана `depthUpdate`.
#[derive(Debug, Clone, Deserialize)]
pub struct DepthUpdate {
    /// Время события, мс.
    #[serde(rename = "E")]
    pub event_ms: i64,
    /// Время транзакции, мс.
    #[serde(rename = "T", default)]
    pub tx_ms: Option<i64>,
    /// Символ.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Первый id обновления.
    #[serde(rename = "U")]
    pub first_id: u64,
    /// Последний id обновления.
    #[serde(rename = "u")]
    pub last_id: u64,
    /// Последний id предыдущего события.
    #[serde(rename = "pu")]
    pub prev_id: u64,
    /// Bids.
    #[serde(rename = "b")]
    pub bids: Vec<PxQty>,
    /// Asks.
    #[serde(rename = "a")]
    pub asks: Vec<PxQty>,
}

/// Агрегированная сделка.
#[derive(Debug, Clone, Deserialize)]
pub struct AggTrade {
    /// Символ.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Id агрегированной сделки.
    #[serde(rename = "a")]
    pub id: u64,
    /// Цена.
    #[serde(rename = "p")]
    pub px: Px,
    /// Количество.
    #[serde(rename = "q")]
    pub qty: Qty,
    /// Время сделки, мс.
    #[serde(rename = "T")]
    pub trade_ms: i64,
    /// Покупатель — мейкер (значит агрессор продавал).
    #[serde(rename = "m")]
    pub buyer_is_maker: bool,
}

/// Цена маркировки и фандинг.
#[derive(Debug, Clone, Deserialize)]
pub struct MarkPriceUpdate {
    /// Время события, мс.
    #[serde(rename = "E")]
    pub event_ms: i64,
    /// Символ.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Цена маркировки.
    #[serde(rename = "p")]
    pub mark: Px,
    /// Индекс.
    #[serde(rename = "i", default, deserialize_with = "lenient_opt")]
    pub index: Option<Px>,
    /// Ставка фандинга (доля за интервал).
    #[serde(rename = "r", default, deserialize_with = "lenient_decimal")]
    pub funding_rate: Option<Decimal>,
    /// Время следующего расчёта, мс.
    #[serde(rename = "T", default)]
    pub next_funding_ms: Option<i64>,
}

/// Лучшие цены.
#[derive(Debug, Clone, Deserialize)]
pub struct BookTicker {
    /// Время события, мс.
    #[serde(rename = "E")]
    pub event_ms: i64,
    /// Символ.
    #[serde(rename = "s")]
    pub symbol: String,
    /// Bid.
    #[serde(rename = "b")]
    pub bid: Px,
    /// Количество bid.
    #[serde(rename = "B")]
    pub bid_qty: Qty,
    /// Ask.
    #[serde(rename = "a")]
    pub ask: Px,
    /// Количество ask.
    #[serde(rename = "A")]
    pub ask_qty: Qty,
}

/// REST-снимок стакана.
#[derive(Debug, Clone, Deserialize)]
pub struct DepthSnapshot {
    /// Id последнего обновления.
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    /// Время транзакции, мс.
    #[serde(rename = "T", default)]
    pub tx_ms: Option<i64>,
    /// Bids.
    pub bids: Vec<PxQty>,
    /// Asks.
    pub asks: Vec<PxQty>,
}

/// Фильтр символа (интересующие поля).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "filterType")]
pub enum Filter {
    /// Цена.
    #[serde(rename = "PRICE_FILTER")]
    Price {
        /// Тик.
        #[serde(rename = "tickSize")]
        tick_size: Px,
    },
    /// Лот.
    #[serde(rename = "LOT_SIZE")]
    Lot {
        /// Шаг.
        #[serde(rename = "stepSize")]
        step_size: Qty,
        /// Минимум.
        #[serde(rename = "minQty")]
        min_qty: Qty,
    },
    /// Минимальный номинал.
    #[serde(rename = "MIN_NOTIONAL")]
    MinNotional {
        /// Номинал.
        notional: Decimal,
    },
    /// Прочие фильтры.
    #[serde(other)]
    Other,
}

/// Символ из `exchangeInfo`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInfo {
    /// Символ.
    pub symbol: String,
    /// Статус (`TRADING`).
    pub status: String,
    /// Тип контракта (`PERPETUAL`).
    #[serde(default)]
    pub contract_type: String,
    /// Базовый актив.
    pub base_asset: String,
    /// Котируемый актив.
    pub quote_asset: String,
    /// Актив маржи.
    pub margin_asset: String,
    /// Время листинга, мс.
    #[serde(default)]
    pub onboard_date: Option<i64>,
    /// Фильтры.
    #[serde(default)]
    pub filters: Vec<Filter>,
}

/// Ответ `exchangeInfo` (нужные поля).
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeInfo {
    /// Символы.
    pub symbols: Vec<SymbolInfo>,
}

/// Элемент `fundingInfo`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingInfo {
    /// Символ.
    pub symbol: String,
    /// Интервал фандинга, ч.
    pub funding_interval_hours: Decimal,
}

/// Элемент `premiumIndex`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PremiumIndex {
    /// Символ.
    pub symbol: String,
    /// Цена маркировки.
    pub mark_price: Px,
    /// Индекс.
    pub index_price: Px,
    /// Ставка (по живой проверке — текущий прогноз, не последний расчёт).
    pub last_funding_rate: Decimal,
    /// Следующий расчёт, мс.
    pub next_funding_time: i64,
}

/// Элемент `fundingRate`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingRateItem {
    /// Символ.
    pub symbol: String,
    /// Расчётная ставка.
    pub funding_rate: Decimal,
    /// Время расчёта, мс.
    pub funding_time: i64,
}

/// Ответ `openInterest`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenInterest {
    /// Символ.
    pub symbol: String,
    /// ОИ в базовом активе.
    pub open_interest: Qty,
    /// Время, мс.
    pub time: i64,
}
