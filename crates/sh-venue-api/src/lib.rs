//! Общий API адаптеров площадок для M0: трейт `MarketDataVenue`,
//! нормализованные события рыночных данных и метаданных, а также общая
//! инфраструктура WebSocket/REST (переподключение, лимиты, запись сырых сообщений).
//!
//! Торговых трейтов и типов ордеров здесь нет: в M0 код исполнения запрещён.

pub mod conn;
pub mod eip712;
pub mod error;
pub mod events;
pub mod exec;
pub mod funding;
pub mod meta;
pub mod rest;
pub mod sink;
pub mod subs;
pub mod venue;
pub mod ws;

pub use conn::{ConnSpec, Outbox, PingKind, Session, run_connection};
pub use error::VenueError;
pub use events::{FundingUpdate, MdEvent, MdEventKind, TradeEvent};
pub use exec::{
    ClientId, ClientIdGen, CommandResult, ExecCommand, ExecError, ExecEvent, ExecFuture, ExecInput, Execution, Fill,
    FundingPayment, InstrumentId, MetaRegistry, OrderRequest, OrderStatus, OrderUpdate, Tif, VenueBalance,
    VenuePosition, meta_of,
};
pub use funding::{FundingIntervalTracker, infer_interval_ms};
pub use meta::{FeeInfo, FeeSource, InstrumentMeta, InstrumentStatus, PriceRule, Rounding};
pub use rest::{RestClient, WeightLimiter};
pub use sink::Sinks;
pub use subs::{PlanDiff, SubPlan};
pub use venue::{MarketDataVenue, Replay, query_param, rest_body};
pub use ws::{Backoff, ProxyConfig, WsStream, connect_ws};
