//! Общая часть бинарников: конфигурация, движок сканера, запуск режимов,
//! источник повтора записей и производные Parquet.

pub mod backtest;
pub mod config;
pub mod ctl;
pub mod derive;
pub mod engine;
pub mod monitor;
pub mod replay_source;
pub mod runner;
pub mod scan;
pub mod selftest;
pub mod simdrive;
pub mod trade;
