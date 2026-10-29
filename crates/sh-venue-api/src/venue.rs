//! Трейт площадки рыночных данных.

use std::future::Future;

use sh_core::Exchange;
use tokio::sync::watch;

use crate::error::VenueError;
use crate::meta::InstrumentMeta;
use crate::sink::Sinks;

/// Адаптер публичных рыночных данных одного семейства площадок.
/// Для Hyperliquid один адаптер обслуживает ядро и все HIP-3 дексы
/// (отдельные `VenueKey`, общие соединения и лимиты).
pub trait MarketDataVenue: Send + Sync + 'static {
    /// Семейство площадок.
    fn exchange(&self) -> Exchange;

    /// Загружает метаданные всех инструментов (для HL — всех дексов).
    fn fetch_metadata(&self, sinks: &Sinks) -> impl Future<Output = Result<Vec<InstrumentMeta>, VenueError>> + Send;

    /// Запускает потоки рыночных данных до сигнала остановки.
    /// Переподключения и пересинхронизации — внутри; ошибка — только фатальная.
    fn run(&self, sinks: Sinks, shutdown: watch::Receiver<bool>)
    -> impl Future<Output = Result<(), VenueError>> + Send;
}
