//! Нормализованные события рыночных данных.
//! Ставки фандинга — доли за интервал площадки (не проценты и не APR).

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Level, Px, Qty, RecvTs, Side, Usd, VenueKey};

use crate::meta::InstrumentMeta;

/// Событие рыночных данных одной площадки и инструмента.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MdEvent {
    /// Площадка (с дексом для HIP-3).
    pub venue: VenueKey,
    /// Символ площадки в её собственной записи (`ETH`, `xyz:NVDA`, `BTCUSDT`).
    pub symbol: String,
    /// Локальные метки получения.
    pub recv: RecvTs,
    /// Время площадки, мс, если она его сообщает.
    pub venue_ts_ms: Option<i64>,
    /// Содержимое события.
    pub kind: MdEventKind,
}

/// Виды событий рыночных данных.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MdEventKind {
    /// Полный снимок верхних уровней стакана.
    BookSnapshot {
        /// Bids по убыванию цены.
        bids: Vec<Level>,
        /// Asks по возрастанию цены.
        asks: Vec<Level>,
        /// Номер последовательности снимка, если есть.
        seq: Option<u64>,
    },
    /// Дельта стакана (абсолютные количества).
    BookDelta {
        /// Обновления bids.
        bids: Vec<Level>,
        /// Обновления asks.
        asks: Vec<Level>,
        /// Первый номер.
        first_seq: u64,
        /// Последний номер.
        last_seq: u64,
        /// Предыдущий номер, если площадка его сообщает.
        prev_seq: Option<u64>,
    },
    /// Лучшие цены.
    Bbo {
        /// Лучший bid.
        bid: Option<Level>,
        /// Лучший ask.
        ask: Option<Level>,
    },
    /// Сделки.
    Trades(Vec<TradeEvent>),
    /// Фандинг (прогноз и/или расчётный).
    Funding(FundingUpdate),
    /// Цены маркировки, индекса и оракула.
    Mark {
        /// Цена маркировки.
        mark: Option<Px>,
        /// Индекс.
        index: Option<Px>,
        /// Оракул.
        oracle: Option<Px>,
        /// Mid площадки.
        mid: Option<Px>,
    },
    /// Открытый интерес одной стороны: в базовом активе и/или в долларах.
    OpenInterest {
        /// Объём в базовом активе.
        base: Option<Qty>,
        /// Номинал в долларах (котируемом стейблкоине).
        usd: Option<Usd>,
    },
    /// Метаданные инструмента изменились (или первый снимок).
    Metadata(Box<InstrumentMeta>),
    /// Стакан потерял синхронизацию; данные недействительны до снимка.
    BookInvalidated,
}

/// Одна сделка.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeEvent {
    /// Цена.
    pub px: Px,
    /// Количество.
    pub qty: Qty,
    /// Сторона агрессора, если известна.
    pub taker_side: Option<Side>,
    /// Идентификатор сделки площадки.
    pub trade_id: Option<String>,
    /// Время сделки, мс.
    pub ts_ms: Option<i64>,
}

/// Обновление фандинга.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FundingUpdate {
    /// Прогноз ставки на текущий интервал (доля за интервал).
    pub predicted_rate: Option<Decimal>,
    /// Последняя расчётная ставка (доля за интервал).
    pub settled_rate: Option<Decimal>,
    /// Время последнего расчёта, мс.
    pub settled_at_ms: Option<i64>,
    /// Время следующего расчёта, мс.
    pub next_funding_ms: Option<i64>,
    /// Интервал в часах из метаданных или выведенный по истории.
    pub interval_hours: Option<Decimal>,
}
