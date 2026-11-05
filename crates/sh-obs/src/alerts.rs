//! Оповещения в Telegram: три уровня, подавление повторов по ключу и ограничение
//! частоты. Отправка в отдельной задаче, чтобы не тормозить вызывающий цикл.
//! Токен бота никогда не попадает в логи: ошибки reqwest содержат URL с токеном,
//! поэтому наружу отдаётся только статус или вид ошибки.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use secrecy::{ExposeSecret, SecretString};
use tokio::sync::mpsc;

/// Уровень оповещения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Информация (дневной отчёт).
    Info,
    /// Предупреждение.
    Warn,
    /// Критично.
    Crit,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Crit => "CRIT",
        }
    }
}

/// Клиент Telegram Bot API.
#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    token: std::sync::Arc<SecretString>,
    chat_id: String,
    base: String,
}

impl Telegram {
    /// Создаёт клиент.
    pub fn new(token: SecretString, chat_id: String) -> Result<Self, String> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().map_err(|e| e.to_string())?;
        Ok(Self { http, token: std::sync::Arc::new(token), chat_id, base: "https://api.telegram.org".into() })
    }

    /// Отправляет сообщение.
    pub async fn send(&self, text: &str) -> Result<(), String> {
        let url = format!("{}/bot{}/sendMessage", self.base, self.token.expose_secret());
        let body = serde_json::json!({ "chat_id": self.chat_id, "text": text, "disable_web_page_preview": true });
        match self.http.post(&url).json(&body).send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(format!("telegram sendMessage status {}", r.status())),
            Err(e) => Err(format!(
                "telegram request failed: {}",
                if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect"
                } else {
                    "other"
                }
            )),
        }
    }

    /// Получает обновления (команды) long-poll'ом; возвращает `(update_id, user_id, text)`.
    pub async fn get_updates(&self, offset: i64, timeout_s: u64) -> Result<Vec<(i64, i64, String)>, String> {
        let url = format!("{}/bot{}/getUpdates", self.base, self.token.expose_secret());
        let body = serde_json::json!({ "offset": offset, "timeout": timeout_s, "allowed_updates": ["message"] });
        let resp = self.http.post(&url).timeout(Duration::from_secs(timeout_s + 10)).json(&body).send().await.map_err(
            |e| format!("telegram getUpdates failed: {}", if e.is_timeout() { "timeout" } else { "request" }),
        )?;
        let v: serde_json::Value = resp.json().await.map_err(|_| "telegram getUpdates: bad json".to_owned())?;
        Ok(v["result"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|u| {
                        Some((
                            u["update_id"].as_i64()?,
                            u["message"]["from"]["id"].as_i64()?,
                            u["message"]["text"].as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Ограничитель: подавление одинаковых ключей и не больше `per_minute` сообщений в минуту.
#[derive(Debug)]
pub struct Throttle {
    dedupe: Duration,
    per_minute: usize,
    last_by_key: HashMap<String, Instant>,
    window: Vec<Instant>,
}

impl Throttle {
    /// Создаёт ограничитель.
    pub fn new(dedupe: Duration, per_minute: usize) -> Self {
        Self { dedupe, per_minute: per_minute.max(1), last_by_key: HashMap::new(), window: Vec::new() }
    }

    /// Можно ли отправить сообщение с ключом сейчас. CRIT не ограничивается частотой, но подавляется по ключу.
    pub fn allow(&mut self, key: &str, level: Level, now: Instant) -> bool {
        if self.last_by_key.get(key).is_some_and(|t| now.duration_since(*t) < self.dedupe) {
            return false;
        }
        self.window.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if level != Level::Crit && self.window.len() >= self.per_minute {
            return false;
        }
        self.window.push(now);
        self.last_by_key.insert(key.to_owned(), now);
        true
    }
}

/// Отправитель оповещений; без Telegram — только лог.
#[derive(Clone)]
pub struct Alerts {
    tx: Option<mpsc::Sender<(Level, String, String)>>,
}

impl Alerts {
    /// Только лог.
    pub fn log_only() -> Self {
        Self { tx: None }
    }

    /// Запускает задачу отправки. Требует рантайм tokio.
    pub fn spawn(tg: Telegram, dedupe: Duration, per_minute: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<(Level, String, String)>(256);
        tokio::spawn(async move {
            let mut throttle = Throttle::new(dedupe, per_minute);
            while let Some((level, key, text)) = rx.recv().await {
                if !throttle.allow(&key, level, Instant::now()) {
                    continue;
                }
                if let Err(e) = tg.send(&format!("[{}] {text}", level.tag())).await {
                    tracing::warn!(error = %e, "telegram alert failed");
                }
            }
        });
        Self { tx: Some(tx) }
    }

    /// Оповещение; `key` — для подавления повторов (например, `venue_degraded:lighter`).
    pub fn send(&self, level: Level, key: &str, text: &str) {
        match level {
            Level::Crit => tracing::error!(alert = key, "{text}"),
            Level::Warn => tracing::warn!(alert = key, "{text}"),
            Level::Info => tracing::info!(alert = key, "{text}"),
        }
        if let Some(tx) = &self.tx
            && tx.try_send((level, key.to_owned(), text.to_owned())).is_err()
        {
            tracing::warn!("alert queue full, alert dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_dedupes_and_limits() {
        let mut t = Throttle::new(Duration::from_secs(300), 2);
        let now = Instant::now();
        assert!(t.allow("a", Level::Warn, now));
        assert!(!t.allow("a", Level::Warn, now + Duration::from_secs(10)));
        assert!(t.allow("b", Level::Warn, now));
        // третий за минуту — отклонён, но CRIT проходит
        assert!(!t.allow("c", Level::Warn, now));
        assert!(t.allow("d", Level::Crit, now));
        // после окна повтор того же ключа разрешён
        assert!(t.allow("a", Level::Warn, now + Duration::from_secs(301)));
    }

    #[tokio::test]
    async fn token_never_in_errors() {
        let mut tg = Telegram::new(SecretString::from("999:TOPSECRET"), "1".into()).unwrap();
        // недоступный адрес: ошибка не должна содержать токен
        tg.base = "http://127.0.0.1:9".into();
        let err = tg.send("x").await.unwrap_err();
        assert!(!err.contains("TOPSECRET"), "{err}");
        let err = tg.get_updates(0, 0).await.unwrap_err();
        assert!(!err.contains("TOPSECRET"), "{err}");
    }
}
