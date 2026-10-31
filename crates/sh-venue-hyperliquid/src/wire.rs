//! Типы сообщений Hyperliquid в том виде, как их присылает площадка.
//! Источник: docs/venues/hyperliquid.md (документация и живые ответы 2026-10-29).

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::value::RawValue;
use sh_core::serde_util::{lenient_decimal, lenient_opt};
use sh_core::{Px, Qty};

/// Конверт WebSocket: `{"channel": ..., "data": ...}`.
#[derive(Debug, Deserialize)]
pub struct WsEnvelope<'a> {
    /// Имя канала (`l2Book`, `trades`, `pong`, ...).
    pub channel: &'a str,
    /// Данные канала без разбора.
    #[serde(borrow, default)]
    pub data: Option<&'a RawValue>,
}

/// Уровень стакана.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WsLevel {
    /// Цена.
    pub px: Px,
    /// Размер.
    pub sz: Qty,
    /// Число ордеров.
    pub n: u64,
}

/// Снимок стакана `l2Book` (каждое сообщение — полный верх стакана).
#[derive(Debug, Clone, Deserialize)]
pub struct WsBook {
    /// Монета (`BTC`, `xyz:NVDA`).
    pub coin: String,
    /// `[bids, asks]`.
    pub levels: Vec<Vec<WsLevel>>,
    /// Время площадки, мс.
    pub time: i64,
}

/// Сделка.
#[derive(Debug, Clone, Deserialize)]
pub struct WsTrade {
    /// Монета.
    pub coin: String,
    /// Сторона: `B` или `A`.
    pub side: String,
    /// Цена.
    pub px: Px,
    /// Размер.
    pub sz: Qty,
    /// Время, мс.
    pub time: i64,
    /// Идентификатор сделки (уникален в паре с временем и монетой).
    pub tid: u64,
}

/// Лучшие цены.
#[derive(Debug, Clone, Deserialize)]
pub struct WsBbo {
    /// Монета.
    pub coin: String,
    /// Время, мс.
    pub time: i64,
    /// `[bid, ask]`, любой может быть `null`.
    pub bbo: Vec<Option<WsLevel>>,
}

/// Контекст perp-актива (`metaAndAssetCtxs`, `allDexsAssetCtxs`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PerpAssetCtx {
    /// Текущая (прогнозная) ставка фандинга за интервал.
    #[serde(default, deserialize_with = "lenient_decimal")]
    pub funding: Option<Decimal>,
    /// Открытый интерес в базовом активе.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub open_interest: Option<Qty>,
    /// Цена оракула.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub oracle_px: Option<Px>,
    /// Цена маркировки.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub mark_px: Option<Px>,
    /// Mid стакана.
    #[serde(default, deserialize_with = "lenient_opt")]
    pub mid_px: Option<Px>,
    /// Премия.
    #[serde(default, deserialize_with = "lenient_decimal")]
    pub premium: Option<Decimal>,
}

/// Данные канала `allDexsAssetCtxs`: `{"ctxs": [[dex, [ctx, ...]], ...]}`;
/// контексты идут в порядке `universe` соответствующего декса.
#[derive(Debug, Clone, Deserialize)]
pub struct AllDexsAssetCtxs {
    /// Пары (имя декса, контексты).
    pub ctxs: Vec<(String, Vec<PerpAssetCtx>)>,
}

/// Описание HIP-3 декса из `perpDexs` (элемент 0 — `null`, ядро).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpDexInfo {
    /// Короткое имя декса (`xyz`).
    pub name: String,
    /// Полное имя.
    #[serde(default)]
    pub full_name: Option<String>,
    /// Адрес деплоера.
    #[serde(default)]
    pub deployer: Option<String>,
}

/// Метаданные актива из `meta`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetMeta {
    /// Имя монеты.
    pub name: String,
    /// Десятичные знаки размера.
    pub sz_decimals: u32,
    /// Максимальное плечо.
    pub max_leverage: Decimal,
    /// Только изолированная маржа (устаревшее поле).
    #[serde(default)]
    pub only_isolated: Option<bool>,
    /// Режим маржи (`strictIsolated`, `noCross`).
    #[serde(default)]
    pub margin_mode: Option<String>,
    /// Признак делистинга.
    #[serde(default)]
    pub is_delisted: Option<bool>,
    /// HIP-3: growth mode (`enabled`).
    #[serde(default)]
    pub growth_mode: Option<String>,
    /// HIP-3: доля деплоера в комиссиях.
    #[serde(default)]
    pub deployer_fee_scale: Option<Decimal>,
}

/// Ответ `meta`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    /// Активы в порядке индексов.
    pub universe: Vec<AssetMeta>,
    /// Индекс спот-токена обеспечения.
    #[serde(default)]
    pub collateral_token: Option<u32>,
}

/// Ответ `metaAndAssetCtxs`: `[meta, ctxs]`.
#[derive(Debug, Clone, Deserialize)]
pub struct MetaAndAssetCtxs(pub Meta, pub Vec<PerpAssetCtx>);

/// Токен из `spotMeta`.
#[derive(Debug, Clone, Deserialize)]
pub struct SpotToken {
    /// Имя токена.
    pub name: String,
    /// Индекс токена.
    pub index: u32,
}

/// Ответ `spotMeta` (нужны только токены).
#[derive(Debug, Clone, Deserialize)]
pub struct SpotMeta {
    /// Токены.
    pub tokens: Vec<SpotToken>,
}

/// Элемент `fundingHistory`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingHistoryItem {
    /// Монета.
    pub coin: String,
    /// Расчётная ставка за интервал.
    pub funding_rate: Decimal,
    /// Время расчёта, мс.
    pub time: i64,
}

/// Прогноз по одной площадке из `predictedFundings`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PredictedFunding {
    /// Ставка за интервал.
    pub funding_rate: Decimal,
    /// Время следующего расчёта, мс.
    pub next_funding_time: i64,
    /// Интервал, ч (есть в живых ответах, нет в примере документации).
    #[serde(default)]
    pub funding_interval_hours: Option<Decimal>,
}

/// Ответ `predictedFundings`: `[[coin, [[venue, {...}|null], ...]], ...]`.
pub type PredictedFundings = Vec<(String, Vec<(String, Option<PredictedFunding>)>)>;
