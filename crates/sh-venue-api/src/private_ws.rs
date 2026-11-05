//! Приватный поток аккаунта поверх WebSocket: подключение, подписки, разбор сообщений
//! адаптером, пинг, переподключение с задержкой. Сырые приватные сообщения не пишутся
//! в рекордер (данные аккаунта не нужны для повтора рынка).

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sh_core::Exchange;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::exec::ExecEvent;
use crate::ws::{Backoff, ProxyConfig, connect_ws};

/// Разбор и подписки приватного потока.
pub trait PrivateStream: Send {
    /// URL подключения (может меняться: listenKey, токен).
    fn url(&mut self) -> impl std::future::Future<Output = Result<String, String>> + Send;
    /// Сообщения подписки после подключения.
    fn subscribe(&mut self) -> impl std::future::Future<Output = Result<Vec<String>, String>> + Send;
    /// Разбор текстового сообщения.
    fn parse(&mut self, text: &str) -> Vec<ExecEvent>;
    /// Текст пинга приложения (если площадка требует).
    fn ping(&self) -> Option<String> {
        None
    }
}

/// Запускает поток до сигнала остановки; сообщает `StreamDown`/`StreamUp`.
pub async fn run_private<S: PrivateStream>(
    exchange: Exchange,
    mut s: S,
    proxy: Option<ProxyConfig>,
    tx: mpsc::Sender<ExecEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(30), 7);
    while !*shutdown.borrow() {
        let url = match s.url().await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(venue = %exchange, error = %e, "private stream url failed");
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            }
        };
        let mut ws = match connect_ws(&url, proxy.as_ref(), Duration::from_secs(10)).await {
            Ok(ws) => ws,
            Err(e) => {
                tracing::warn!(venue = %exchange, error = %e, "private stream connect failed");
                let _ = tx.send(ExecEvent::StreamDown(exchange)).await;
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            }
        };
        let subs = match s.subscribe().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(venue = %exchange, error = %e, "private stream subscribe failed");
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            }
        };
        let mut ok = true;
        for m in subs {
            if ws.send(Message::text(m)).await.is_err() {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }
        backoff.reset();
        let _ = tx.send(ExecEvent::StreamUp(exchange)).await;
        tracing::info!(venue = %exchange, "private stream connected");
        let mut ping = tokio::time::interval(Duration::from_secs(20));
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        let _ = ws.close(None).await;
                        return;
                    }
                }
                _ = ping.tick() => {
                    let msg = s.ping().map(Message::text).unwrap_or_else(|| Message::Ping(Vec::new().into()));
                    if ws.send(msg).await.is_err() {
                        break;
                    }
                }
                m = ws.next() => {
                    match m {
                        Some(Ok(Message::Text(t))) => {
                            for ev in s.parse(t.as_str()) {
                                // события ордеров и сделок не теряются: ждём место в канале
                                if tx.send(ev).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Some(Ok(Message::Ping(p))) => {
                            let _ = ws.send(Message::Pong(p)).await;
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(_)) => {}
                    }
                }
            }
        }
        tracing::warn!(venue = %exchange, "private stream disconnected");
        let _ = tx.send(ExecEvent::StreamDown(exchange)).await;
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

/// Ответ HTTP как строка с ошибкой исполнения: тайм-аут → `Timeout` (исход неизвестен),
/// ошибка до отправки → `Transport`.
pub fn map_reqwest_error(e: &reqwest::Error) -> crate::exec::ExecError {
    if e.is_timeout() {
        crate::exec::ExecError::Timeout
    } else if e.is_connect() || e.is_builder() {
        crate::exec::ExecError::Transport(if e.is_connect() { "connect".into() } else { "request build".into() })
    } else {
        // ответ мог не дойти после отправки запроса: исход неизвестен
        crate::exec::ExecError::Timeout
    }
}
