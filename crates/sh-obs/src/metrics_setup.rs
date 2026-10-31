//! Экспорт метрик Prometheus. Слушаем только loopback: доступ — через WireGuard.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;
use serde::Deserialize;

/// Настройки `[metrics]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    /// Включить экспорт.
    pub enabled: bool,
    /// Адрес HTTP-листенера.
    pub listen: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: true, listen: SocketAddr::from(([127, 0, 0, 1], 9464)) }
    }
}

/// Запускает экспортёр и описывает метрики M0. Требует рантайм tokio.
pub fn init_metrics(cfg: &MetricsConfig) -> Result<(), String> {
    if !cfg.enabled {
        return Ok(());
    }
    if !cfg.listen.ip().is_loopback() {
        return Err(format!("metrics.listen must be a loopback address, got {}", cfg.listen));
    }
    PrometheusBuilder::new().with_http_listener(cfg.listen).install().map_err(|e| e.to_string())?;
    describe();
    Ok(())
}

fn describe() {
    use metrics::{describe_counter, describe_gauge};
    describe_gauge!("md_staleness_ms", "Age of the newest market-data event per venue");
    describe_counter!("ws_reconnects_total", "WebSocket reconnects per venue");
    describe_counter!("ws_connect_failures_total", "Failed WebSocket connect attempts per venue");
    describe_counter!("ws_messages_total", "Inbound WebSocket text messages per venue");
    describe_counter!("md_book_gaps_total", "Order-book sequence gaps detected per venue");
    describe_gauge!("md_books_desynced", "Books currently awaiting a snapshot per venue");
    describe_gauge!("md_uncovered_streams", "Streams not subscribed because of venue limits");
    describe_counter!("md_events_dropped_total", "Normalised book events dropped on back-pressure");
    describe_counter!("md_parse_errors_total", "Messages that failed to parse per venue");
    describe_counter!("rest_errors_total", "Non-2xx REST responses per venue");
    describe_counter!("recorder_raw_bytes_total", "Uncompressed bytes written by the recorder");
    describe_counter!("recorder_compressed_bytes_total", "Compressed bytes written by the recorder");
    describe_counter!("recorder_write_errors_total", "Recorder write errors");
    describe_gauge!("recorder_queue_depth", "Records waiting in the recorder queue");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback() {
        let cfg = MetricsConfig { enabled: true, listen: "0.0.0.0:9464".parse().unwrap() };
        assert!(init_metrics(&cfg).is_err());
        assert!(init_metrics(&MetricsConfig { enabled: false, ..cfg }).is_ok());
    }
}
