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

/// Повтор сырых записей: превращает записанные сообщения обратно в те же
/// нормализованные события, что выдавал живой адаптер (для сканера по записям,
/// Parquet-производных и бэктеста).
pub trait Replay: Send {
    /// Семейство площадок.
    fn exchange(&self) -> Exchange;

    /// Обрабатывает одну запись; события добавляются в `out`.
    fn feed(&mut self, rec: &sh_core::RawRecord, out: &mut Vec<crate::events::MdEvent>);
}

/// Тело POST-запроса из метки REST-записи `POST /info {...}`.
pub fn rest_body(ep: &str) -> Option<serde_json::Value> {
    let rest = ep.strip_prefix("POST ")?;
    let (_, body) = rest.split_once(' ')?;
    serde_json::from_str(body).ok()
}

/// Значение параметра query из метки `GET /path?a=1&b=2`.
pub fn query_param<'a>(ep: &'a str, name: &str) -> Option<&'a str> {
    let (_, q) = ep.split_once('?')?;
    q.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rest_labels() {
        let b = rest_body(r#"POST /info {"dex":"xyz","type":"metaAndAssetCtxs"}"#).unwrap();
        assert_eq!(b["dex"], "xyz");
        assert!(rest_body("GET /x").is_none());
        assert_eq!(query_param("GET /fapi/v3/depth?symbol=BTCUSDT&limit=50", "symbol"), Some("BTCUSDT"));
        assert_eq!(query_param("GET /fapi/v3/depth?symbol=BTCUSDT", "limit"), None);
    }
}
