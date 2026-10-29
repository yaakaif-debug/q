//! Сырые записи рекордера и системные события о разрывах.
//! Сообщение площадки хранится строкой ровно в том виде, как пришло.

use serde::{Deserialize, Serialize};

use crate::clock::RecvTs;
use crate::venue::Exchange;

/// Происхождение сырой записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "src", rename_all = "lowercase")]
pub enum RawSource {
    /// Входящее сообщение WebSocket.
    Ws,
    /// Ответ REST; `ep` — метка запроса (метод, путь, параметры).
    Rest {
        /// Метка запроса, например `POST /info meta dex=xyz`.
        ep: String,
    },
    /// Системное событие рекордера (подключение, разрыв, пересинхронизация).
    Sys,
    /// Исходящее сообщение клиента (подписки), нужно для точного повтора.
    Out,
}

/// Одна сырая запись: сообщение площадки или системное событие.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawRecord {
    /// Семейство площадок; определяет файл записи.
    pub exchange: Exchange,
    /// Происхождение записи.
    #[serde(flatten)]
    pub source: RawSource,
    /// Идентификатор соединения или опросчика внутри сессии.
    pub conn: String,
    /// Локальные метки времени получения.
    pub recv: RecvTs,
    /// Тело сообщения без изменений (для `Sys` — сериализованный `SysEvent`).
    pub payload: String,
}

/// Системные события, по которым считается время разрывов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SysEvent {
    /// Соединение установлено.
    ConnUp {
        /// URL без секретов.
        url: String,
    },
    /// Соединение потеряно или закрыто; все стаканы соединения считаются рассинхронизированными.
    ConnDown {
        /// Причина на английском.
        reason: String,
    },
    /// Обнаружен разрыв последовательности в стакане.
    BookGap {
        /// Символ площадки.
        symbol: String,
        /// Ожидаемый номер.
        expected: u64,
        /// Полученный номер.
        got: u64,
    },
    /// Стакан синхронизирован по снимку.
    BookSynced {
        /// Символ площадки.
        symbol: String,
        /// Номер последовательности снимка, если есть.
        seq: Option<u64>,
    },
    /// Поток не подписан из-за лимитов площадки.
    Uncovered {
        /// Описание потоков.
        streams: Vec<String>,
    },
    /// Ошибка опроса REST.
    RestError {
        /// Метка запроса.
        ep: String,
        /// Текст ошибки.
        error: String,
    },
}

impl SysEvent {
    /// Сериализует событие в JSON. Сериализация этих типов не может завершиться ошибкой.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| String::from("{\"event\":\"unserializable\"}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_record_roundtrip() {
        let rec = RawRecord {
            exchange: Exchange::Lighter,
            source: RawSource::Rest { ep: "GET /api/v1/orderBookDetails".into() },
            conn: "rest".into(),
            recv: RecvTs { wall_ns: 1, mono_ns: 2 },
            payload: "{\"a\": 1}".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"src\":\"rest\""));
        let back: RawRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn sys_event_json() {
        let ev = SysEvent::BookGap { symbol: "ETH".into(), expected: 5, got: 7 };
        assert_eq!(ev.to_json(), r#"{"event":"book_gap","symbol":"ETH","expected":5,"got":7}"#);
    }
}
