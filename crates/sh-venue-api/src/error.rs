//! Ошибки адаптеров площадок.

use thiserror::Error;

/// Ошибка адаптера. Тексты не содержат секретов (в M0 их и нет).
#[derive(Debug, Error)]
pub enum VenueError {
    /// Ошибка HTTP/REST.
    #[error("http error: {0}")]
    Http(String),
    /// Ошибка WebSocket.
    #[error("websocket error: {0}")]
    Ws(String),
    /// Ошибка разбора сообщения площадки.
    #[error("parse error: {0}")]
    Parse(String),
    /// Ошибка ввода-вывода.
    #[error("io error: {0}")]
    Io(String),
    /// Нарушение конфигурации или лимитов.
    #[error("config error: {0}")]
    Config(String),
    /// Получатель событий закрыт (процесс завершается).
    #[error("sink closed")]
    SinkClosed,
}

impl From<serde_json::Error> for VenueError {
    fn from(e: serde_json::Error) -> Self {
        VenueError::Parse(e.to_string())
    }
}

impl From<std::io::Error> for VenueError {
    fn from(e: std::io::Error) -> Self {
        VenueError::Io(e.to_string())
    }
}
