//! Наблюдаемость: логирование `tracing` (JSON, ежедневная ротация файлов),
//! экспорт метрик Prometheus. Telegram-оповещения появятся вместе с риском (M2).

pub mod logging;
pub mod metrics_setup;

pub use logging::{LogConfig, init_logging};
pub use metrics_setup::{MetricsConfig, init_metrics};
