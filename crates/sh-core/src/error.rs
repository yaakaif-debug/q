//! Общие ошибки базовых типов.

use thiserror::Error;

/// Ошибки разбора и валидации базовых типов.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// Строка не является корректным десятичным числом.
    #[error("invalid decimal '{0}'")]
    InvalidDecimal(String),
    /// Неизвестное имя биржи.
    #[error("unknown exchange '{0}'")]
    UnknownExchange(String),
    /// Шаг (тик или лот) должен быть строго положительным.
    #[error("step must be positive, got {0}")]
    NonPositiveStep(String),
}
