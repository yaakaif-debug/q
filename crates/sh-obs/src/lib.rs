//! Наблюдаемость: логирование `tracing` (JSON, ежедневная ротация файлов),
//! экспорт метрик Prometheus, оповещения и команды Telegram.

pub mod alerts;
pub mod logging;
pub mod metrics_setup;

pub use alerts::{Alerts, Level, Telegram, Throttle};
pub use logging::{LogConfig, init_logging};
pub use metrics_setup::{MetricsConfig, init_metrics};
