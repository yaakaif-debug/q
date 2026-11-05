//! Исполнение: идентификаторы ордеров, запросы, обновления, заполнения, позиции,
//! балансы и трейт `Execution`, общий для симулятора и приватных адаптеров.
//! Трейт объектно-безопасен (`Arc<dyn Execution>`), методы возвращают упакованные
//! future. Состояния ордера меняются только по подтверждениям площадки.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Collateral, Exchange, Px, Qty, Side, Usd, VenueKey};
use thiserror::Error;
use tokio::sync::{mpsc, watch};

use crate::meta::InstrumentMeta;

/// Ключ инструмента в исполнении: площадка и символ площадки.
pub type InstrumentId = (VenueKey, String);

/// Реестр метаданных инструментов, общий для движка и адаптеров исполнения.
pub type MetaRegistry = Arc<RwLock<HashMap<InstrumentId, InstrumentMeta>>>;

/// Упакованный future результата исполнения.
pub type ExecFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ExecError>> + Send + 'a>>;

/// Клиентский идентификатор ордера (уникален в сессии и между перезапусками).
/// Lighter требует `client_order_index ≤ 2^48 − 1`, поэтому значение 48-битное.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClientId(pub u64);

/// Максимум для Lighter `client_order_index`.
pub const CLIENT_ID_MAX: u64 = (1 << 48) - 1;

impl ClientId {
    /// Hyperliquid `cloid`: 128-битная hex-строка.
    pub fn hl_cloid(self) -> String {
        format!("0x{:032x}", self.0)
    }

    /// Разбор `cloid` Hyperliquid.
    pub fn from_hl_cloid(s: &str) -> Option<Self> {
        let v = u128::from_str_radix(s.strip_prefix("0x")?, 16).ok()?;
        u64::try_from(v).ok().map(Self)
    }

    /// Aster `newClientOrderId` (`^[.A-Z:/a-z0-9_-]{1,36}$`).
    pub fn aster_id(self) -> String {
        format!("sh-{}", self.0)
    }

    /// Разбор `clientOrderId` Aster.
    pub fn from_aster_id(s: &str) -> Option<Self> {
        s.strip_prefix("sh-")?.parse().ok().map(Self)
    }
}

/// Генератор клиентских идентификаторов: миллисекунды × 1000 + счётчик, по модулю 2^48.
/// Монотонен в пределах процесса и не повторяется после перезапуска (время растёт).
#[derive(Debug)]
pub struct ClientIdGen {
    last: std::sync::atomic::AtomicU64,
}

impl ClientIdGen {
    /// Генератор, начинающий не раньше `now_ms`.
    pub fn new(now_ms: i64) -> Self {
        let start = (u64::try_from(now_ms).unwrap_or(0) % (CLIENT_ID_MAX / 1000)) * 1000;
        Self { last: std::sync::atomic::AtomicU64::new(start) }
    }

    /// Следующий идентификатор.
    pub fn next(&self, now_ms: i64) -> ClientId {
        use std::sync::atomic::Ordering;
        let floor = (u64::try_from(now_ms).unwrap_or(0) % (CLIENT_ID_MAX / 1000)) * 1000;
        let mut cur = self.last.load(Ordering::SeqCst);
        loop {
            let next = (cur + 1).max(floor) & CLIENT_ID_MAX;
            match self.last.compare_exchange(cur, next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return ClientId(next),
                Err(v) => cur = v,
            }
        }
    }
}

/// Время жизни ордера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tif {
    /// Только мейкер (отклоняется, если пересекает стакан).
    PostOnly,
    /// Немедленно или отмена.
    Ioc,
    /// До отмены.
    Gtc,
}

/// Запрос на ордер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRequest {
    /// Клиентский идентификатор.
    pub cid: ClientId,
    /// Площадка.
    pub venue: VenueKey,
    /// Символ площадки.
    pub symbol: String,
    /// Сторона.
    pub side: Side,
    /// Количество (кратно лоту).
    pub qty: Qty,
    /// Предельная цена (по правилу цены площадки).
    pub px: Px,
    /// Время жизни.
    pub tif: Tif,
    /// Только сокращение позиции.
    pub reduce_only: bool,
}

/// Состояние ордера (SPEC.md, «Order handling rules»).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Создан, не отправлен.
    New,
    /// Отправлен, подтверждения нет.
    Sent,
    /// Принят площадкой.
    Acked,
    /// Частично исполнен.
    PartiallyFilled,
    /// Исполнен.
    Filled,
    /// Отменён.
    Cancelled,
    /// Отклонён.
    Rejected,
    /// Истёк.
    Expired,
    /// Неизвестно (тайм-аут): разрешается только запросом по клиентскому идентификатору.
    Unknown,
}

impl OrderStatus {
    /// Конечное состояние.
    pub fn is_terminal(self) -> bool {
        matches!(self, OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected | OrderStatus::Expired)
    }
}

/// Обновление ордера от площадки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderUpdate {
    /// Клиентский идентификатор.
    pub cid: ClientId,
    /// Площадка.
    pub venue: VenueKey,
    /// Символ.
    pub symbol: String,
    /// Состояние.
    pub status: OrderStatus,
    /// Исполнено всего.
    pub filled_qty: Qty,
    /// Средняя цена исполнения.
    pub avg_px: Option<Px>,
    /// Идентификатор ордера на площадке.
    pub venue_order_id: Option<String>,
    /// Причина отказа или отмены.
    pub reason: Option<String>,
    /// Время, мс.
    pub ts_ms: i64,
}

/// Заполнение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    /// Клиентский идентификатор, если ордер наш.
    pub cid: Option<ClientId>,
    /// Площадка.
    pub venue: VenueKey,
    /// Символ.
    pub symbol: String,
    /// Сторона.
    pub side: Side,
    /// Количество.
    pub qty: Qty,
    /// Цена.
    pub px: Px,
    /// Комиссия (плюс — уплачена, минус — ребейт).
    pub fee: Usd,
    /// Мейкер.
    pub maker: bool,
    /// Идентификатор сделки.
    pub trade_id: String,
    /// Время, мс.
    pub ts_ms: i64,
    /// Ликвидация или ADL (не наш ордер).
    pub liquidation: bool,
}

/// Позиция на площадке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenuePosition {
    /// Площадка.
    pub venue: VenueKey,
    /// Символ.
    pub symbol: String,
    /// Количество со знаком (плюс — лонг).
    pub qty: Decimal,
    /// Цена входа.
    pub entry_px: Option<Px>,
    /// Цена ликвидации.
    pub liq_px: Option<Px>,
    /// Маржа позиции.
    pub margin: Option<Usd>,
}

/// Баланс на площадке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueBalance {
    /// Площадка.
    pub venue: VenueKey,
    /// Актив.
    pub collateral: Collateral,
    /// Капитал (с нереализованным результатом).
    pub equity: Usd,
    /// Доступно.
    pub available: Usd,
    /// Маржа поддержания.
    pub maintenance_margin: Option<Usd>,
}

/// Платёж фандинга по позиции.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingPayment {
    /// Площадка.
    pub venue: VenueKey,
    /// Символ.
    pub symbol: String,
    /// Сумма (плюс — получено).
    pub amount: Usd,
    /// Ставка.
    pub rate: Option<Decimal>,
    /// Время, мс.
    pub ts_ms: i64,
}

/// Событие исполнения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecEvent {
    /// Обновление ордера.
    Order(OrderUpdate),
    /// Заполнение.
    Fill(Fill),
    /// Снимок позиций площадки.
    Positions {
        /// Семейство.
        exchange: Exchange,
        /// Позиции.
        positions: Vec<VenuePosition>,
    },
    /// Снимок балансов.
    Balances(Vec<VenueBalance>),
    /// Фандинг.
    Funding(FundingPayment),
    /// Приватный поток площадки потерян.
    StreamDown(Exchange),
    /// Приватный поток восстановлен.
    StreamUp(Exchange),
}

/// Ошибка исполнения. Тексты без секретов.
#[derive(Debug, Clone, Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecError {
    /// Площадка отклонила запрос.
    #[error("rejected: {0}")]
    Rejected(String),
    /// Ответа нет: результат неизвестен.
    #[error("timeout")]
    Timeout,
    /// Сетевая ошибка до отправки (запрос точно не дошёл).
    #[error("transport: {0}")]
    Transport(String),
    /// Превышен лимит.
    #[error("rate limited")]
    RateLimited,
    /// Неверный запрос (цена, размер, инструмент).
    #[error("invalid request: {0}")]
    Invalid(String),
    /// Операция не поддерживается площадкой.
    #[error("not supported: {0}")]
    NotSupported(String),
    /// Ошибка ключей или подписи.
    #[error("auth: {0}")]
    Auth(String),
    /// Торговля на площадке выключена конфигурацией.
    #[error("trading disabled for {0}")]
    TradingDisabled(Exchange),
}

impl ExecError {
    /// Исход неизвестен: ордер переводится в `Unknown` и уточняется запросом.
    pub fn outcome_unknown(&self) -> bool {
        matches!(self, ExecError::Timeout)
    }
}

/// Исполнение на площадке (симулятор или приватный адаптер).
pub trait Execution: Send + Sync {
    /// Семейство площадок.
    fn exchange(&self) -> Exchange;

    /// Отправляет ордер; ответ — первое подтверждённое состояние.
    fn place(&self, req: OrderRequest) -> ExecFuture<'_, OrderUpdate>;

    /// Отменяет ордер по клиентскому идентификатору.
    fn cancel(&self, venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, ()>;

    /// Отменяет все ордера на площадке (все дексы); возвращает число отменённых, если известно.
    fn cancel_all(&self) -> ExecFuture<'_, u32>;

    /// Состояние ордера по клиентскому идентификатору (`None` — площадка ордер не знает).
    fn query_order(&self, venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, Option<OrderUpdate>>;

    /// Позиции.
    fn positions(&self) -> ExecFuture<'_, Vec<VenuePosition>>;

    /// Балансы.
    fn balances(&self) -> ExecFuture<'_, Vec<VenueBalance>>;

    /// Платежи фандинга с момента `since_ms`.
    fn funding_payments(&self, since_ms: i64) -> ExecFuture<'_, Vec<FundingPayment>>;

    /// Перевод обеспечения между дексами одной платформы (Hyperliquid HIP-3).
    fn transfer_between_dexes(&self, _from_dex: String, _to_dex: String, _amount: Usd) -> ExecFuture<'_, ()> {
        Box::pin(async { Err(ExecError::NotSupported("transfer between dexes".into())) })
    }

    /// Приватный поток событий до сигнала остановки.
    fn run_stream(&self, tx: mpsc::Sender<ExecEvent>, shutdown: watch::Receiver<bool>) -> ExecFuture<'_, ()>;
}

/// Команда движка исполнения. Движок синхронный: команды исполняет оболочка
/// (симулятор напрямую, live — через `Execution` в отдельных задачах).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecCommand {
    /// Отправить ордер.
    Place(OrderRequest),
    /// Отменить ордер.
    Cancel {
        /// Площадка.
        venue: VenueKey,
        /// Символ.
        symbol: String,
        /// Клиентский идентификатор.
        cid: ClientId,
    },
    /// Отменить все ордера на семействе площадок.
    CancelAll(Exchange),
    /// Запросить состояние ордера.
    Query {
        /// Площадка.
        venue: VenueKey,
        /// Символ.
        symbol: String,
        /// Клиентский идентификатор.
        cid: ClientId,
    },
}

/// Результат команды.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandResult {
    /// Ответ на отправку.
    Placed(ClientId, Result<OrderUpdate, ExecError>),
    /// Ответ на отмену.
    Cancelled(ClientId, Result<(), ExecError>),
    /// Ответ на отмену всех.
    CancelledAll(Exchange, Result<u32, ExecError>),
    /// Ответ на запрос.
    Queried(ClientId, Result<Option<OrderUpdate>, ExecError>),
}

/// Вход движка от исполнения: событие потока или результат команды.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecInput {
    /// Событие приватного потока.
    Event(ExecEvent),
    /// Результат команды.
    Result(CommandResult),
}

/// Поиск метаданных в реестре.
pub fn meta_of(reg: &MetaRegistry, venue: &VenueKey, symbol: &str) -> Option<InstrumentMeta> {
    reg.read().ok()?.get(&(venue.clone(), symbol.to_owned())).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_formats_roundtrip() {
        let c = ClientId(0x1234_5678_9abc);
        assert_eq!(c.hl_cloid(), "0x0000000000000000000012345678 9abc".replace(' ', ""));
        assert_eq!(ClientId::from_hl_cloid(&c.hl_cloid()), Some(c));
        assert_eq!(ClientId::from_aster_id(&c.aster_id()), Some(c));
        assert!(c.aster_id().len() <= 36);
        assert_eq!(ClientId::from_aster_id("web-1"), None);
    }

    #[test]
    fn id_gen_monotonic_and_48_bit() {
        let g = ClientIdGen::new(1_793_000_000_000);
        let a = g.next(1_793_000_000_000);
        let b = g.next(1_793_000_000_000);
        assert!(b > a);
        assert!(b.0 <= CLIENT_ID_MAX);
        // время ушло вперёд — идентификаторы перепрыгивают на новое основание
        let c = g.next(1_793_000_005_000);
        assert!(c.0 >= b.0 + 4000);
    }

    #[test]
    fn terminal_states() {
        assert!(OrderStatus::Filled.is_terminal());
        assert!(!OrderStatus::Unknown.is_terminal());
        assert!(ExecError::Timeout.outcome_unknown());
        assert!(!ExecError::Transport("x".into()).outcome_unknown());
    }
}
