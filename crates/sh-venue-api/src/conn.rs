//! Общий цикл WebSocket-соединения для всех адаптеров:
//! подключение с экспоненциальной задержкой, запись сырых входящих и исходящих
//! сообщений, системные события `ConnUp`/`ConnDown`, пинги, тайм-аут тишины,
//! максимальный возраст соединения и отдельная задача записи с лимитом сообщений,
//! чтобы чтение никогда не блокировалось ожиданием лимита. При разрыве задача записи
//! прерывается вместе с очередью; новое соединение подписывается заново.

use std::future::Future;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sh_core::{Exchange, RawSource, RecvTs, SysEvent};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use crate::error::VenueError;
use crate::rest::WeightLimiter;
use crate::sink::Sinks;
use crate::ws::{Backoff, ProxyConfig, connect_ws};

/// Как проверять живость соединения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PingKind {
    /// Прикладной текстовый пинг (например, `{"method":"ping"}`).
    Text(String),
    /// Управляющий кадр WebSocket Ping.
    Frame,
}

/// Параметры соединения.
#[derive(Debug, Clone)]
pub struct ConnSpec {
    /// Семейство площадок.
    pub exchange: Exchange,
    /// Идентификатор соединения внутри сессии (`hl-ws-0`).
    pub conn_id: String,
    /// Как пинговать.
    pub ping: PingKind,
    /// Период пинга.
    pub ping_every: Duration,
    /// Переподключение, если входящих сообщений нет дольше этого.
    pub idle_timeout: Duration,
    /// Плановое переподключение по возрасту (Aster рвёт соединения через 24 ч).
    pub max_age: Option<Duration>,
    /// Тайм-аут подключения.
    pub connect_timeout: Duration,
    /// Начальная задержка переподключения.
    pub backoff_base: Duration,
    /// Максимальная задержка переподключения.
    pub backoff_max: Duration,
}

/// Исходящие действия, накопленные сессией за один шаг.
#[derive(Debug, Default)]
pub struct Outbox {
    msgs: Vec<String>,
    reconnect: Option<String>,
}

impl Outbox {
    /// Ставит текстовое сообщение в очередь отправки.
    pub fn send(&mut self, msg: String) {
        self.msgs.push(msg);
    }

    /// Просит переподключиться (например, при неустранимой ошибке протокола).
    pub fn reconnect(&mut self, reason: impl Into<String>) {
        self.reconnect = Some(reason.into());
    }
}

/// Протокольная часть соединения конкретной площадки.
pub trait Session: Send {
    /// Команды от управляющей задачи (изменение подписок, снимки стаканов).
    type Cmd: Send + 'static;

    /// URL для очередного подключения (может меняться между подключениями).
    fn url(&self) -> String;

    /// Вызывается после подключения: подписки и начальная инициализация.
    fn on_connected(&mut self, out: &mut Outbox) -> impl Future<Output = Result<(), VenueError>> + Send;

    /// Обработка входящего текстового сообщения (уже записанного в рекордер).
    fn on_text(
        &mut self,
        text: &str,
        recv: RecvTs,
        out: &mut Outbox,
    ) -> impl Future<Output = Result<(), VenueError>> + Send;

    /// Обработка команды.
    fn on_command(&mut self, cmd: Self::Cmd, out: &mut Outbox) -> impl Future<Output = Result<(), VenueError>> + Send;

    /// Соединение потеряно: сбросить стаканы и состояние подписок.
    fn on_disconnected(&mut self, reason: &str) -> impl Future<Output = Result<(), VenueError>> + Send;
}

/// Ёмкость очереди исходящих сообщений одного соединения.
const OUT_QUEUE: usize = 8192;

/// Запускает соединение до сигнала остановки. Возвращает ошибку только если
/// закрыт канал рекордера (процесс завершается).
pub async fn run_connection<S: Session>(
    spec: ConnSpec,
    mut session: S,
    mut cmd_rx: mpsc::Receiver<S::Cmd>,
    sinks: Sinks,
    limiter: Option<WeightLimiter>,
    proxy: Option<ProxyConfig>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let seed = u64::try_from(sinks.now().wall_ns).unwrap_or(1) ^ spec.conn_id.len() as u64;
    let mut backoff = Backoff::new(spec.backoff_base, spec.backoff_max, seed);
    let venue = spec.exchange.as_str();
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let url = session.url();
        let ws = match connect_ws(&url, proxy.as_ref(), spec.connect_timeout).await {
            Ok(ws) => ws,
            Err(e) => {
                metrics::counter!("ws_connect_failures_total", "venue" => venue).increment(1);
                tracing::warn!(venue, conn = %spec.conn_id, error = %e, "websocket connect failed");
                if sleep_or_shutdown(backoff.next_delay(), &mut shutdown).await {
                    return Ok(());
                }
                continue;
            }
        };
        let started = Instant::now();
        tracing::info!(venue, conn = %spec.conn_id, url = %redact_query(&url), "websocket connected");
        sinks.sys(spec.exchange, &spec.conn_id, &SysEvent::ConnUp { url: redact_query(&url) }).await?;

        let (write, mut read) = ws.split();
        let (out_tx, out_rx) = mpsc::channel::<Message>(OUT_QUEUE);
        let writer = tokio::spawn(writer_task(
            write,
            out_rx,
            limiter.clone(),
            sinks.clone(),
            spec.exchange,
            spec.conn_id.clone(),
        ));

        let mut outbox = Outbox::default();
        let mut reason = match session.on_connected(&mut outbox).await {
            Ok(()) => flush(&mut outbox, &out_tx),
            Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
            Err(e) => Some(format!("session init failed: {e}")),
        };

        let mut last_rx = Instant::now();
        let mut last_ping = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while reason.is_none() {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    reason = Some("shutdown".into());
                }
                msg = read.next() => {
                    last_rx = Instant::now();
                    match msg {
                        Some(Ok(Message::Text(t))) => {
                            let recv = sinks.now();
                            metrics::counter!("ws_messages_total", "venue" => venue).increment(1);
                            sinks.raw(spec.exchange, RawSource::Ws, &spec.conn_id, recv, t.as_str().to_owned()).await?;
                            match session.on_text(t.as_str(), recv, &mut outbox).await {
                                Ok(()) => reason = flush(&mut outbox, &out_tx),
                                Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
                                Err(e) => {
                                    metrics::counter!("md_parse_errors_total", "venue" => venue).increment(1);
                                    tracing::debug!(venue, conn = %spec.conn_id, error = %e, "message handling failed");
                                }
                            }
                        }
                        Some(Ok(Message::Binary(b))) => {
                            // площадки M0 шлют только текст; бинарное сохраняем как есть в UTF-8-представлении
                            let recv = sinks.now();
                            sinks.raw(spec.exchange, RawSource::Ws, &spec.conn_id, recv, String::from_utf8_lossy(&b).into_owned()).await?;
                        }
                        Some(Ok(Message::Ping(p))) => {
                            // ответный Pong явно: незапрошенные Pong допустимы по RFC 6455
                            if out_tx.try_send(Message::Pong(p)).is_err() {
                                reason = Some("outgoing queue full".into());
                            }
                        }
                        Some(Ok(Message::Close(frame))) => {
                            reason = Some(format!("closed by server: {frame:?}"));
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => reason = Some(format!("read error: {e}")),
                        None => reason = Some("stream ended".into()),
                    }
                }
                cmd = cmd_rx.recv() => {
                    if let Some(cmd) = cmd {
                        match session.on_command(cmd, &mut outbox).await {
                            Ok(()) => reason = flush(&mut outbox, &out_tx),
                            Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
                            Err(e) => tracing::warn!(venue, conn = %spec.conn_id, error = %e, "command failed"),
                        }
                    }
                }
                _ = tick.tick() => {
                    if writer.is_finished() {
                        reason = Some("writer stopped".into());
                    } else if last_rx.elapsed() > spec.idle_timeout {
                        reason = Some(format!("idle for {:?}", spec.idle_timeout));
                    } else if spec.max_age.is_some_and(|age| started.elapsed() >= age) {
                        reason = Some("max connection age reached".into());
                    } else if last_ping.elapsed() >= spec.ping_every {
                        last_ping = Instant::now();
                        let ping = match &spec.ping {
                            PingKind::Text(t) => Message::text(t.clone()),
                            PingKind::Frame => Message::Ping(Vec::new().into()),
                        };
                        if out_tx.try_send(ping).is_err() {
                            reason = Some("outgoing queue full".into());
                        }
                    }
                }
            }
        }
        let reason = reason.unwrap_or_default();
        // очередь исходящих не дописываем: старые подписки на брошенном сокете
        // только тратили бы общий лимит сообщений площадки
        drop(out_tx);
        writer.abort();
        let _ = writer.await;
        session.on_disconnected(&reason).await?;
        sinks.sys(spec.exchange, &spec.conn_id, &SysEvent::ConnDown { reason: reason.clone() }).await?;
        if reason == "shutdown" {
            return Ok(());
        }
        metrics::counter!("ws_reconnects_total", "venue" => venue).increment(1);
        tracing::warn!(venue, conn = %spec.conn_id, %reason, "websocket disconnected");
        if started.elapsed() > Duration::from_secs(60) {
            backoff.reset();
        }
        if sleep_or_shutdown(backoff.next_delay(), &mut shutdown).await {
            return Ok(());
        }
    }
}

/// Передаёт накопленные сообщения писателю; возвращает причину переподключения, если нужна.
fn flush(outbox: &mut Outbox, out_tx: &mpsc::Sender<Message>) -> Option<String> {
    for msg in outbox.msgs.drain(..) {
        if out_tx.try_send(Message::text(msg)).is_err() {
            return Some("outgoing queue full".into());
        }
    }
    outbox.reconnect.take()
}

/// Задача записи: лимит на текстовые сообщения, запись исходящих в рекордер.
async fn writer_task<W>(
    mut write: W,
    mut rx: mpsc::Receiver<Message>,
    limiter: Option<WeightLimiter>,
    sinks: Sinks,
    exchange: Exchange,
    conn: String,
) where
    W: futures_util::Sink<Message> + Unpin,
    W::Error: std::fmt::Display,
{
    while let Some(msg) = rx.recv().await {
        if let Message::Text(t) = &msg {
            if let Some(l) = &limiter {
                l.acquire(1).await;
            }
            if sinks.raw(exchange, RawSource::Out, &conn, sinks.now(), t.as_str().to_owned()).await.is_err() {
                return;
            }
        }
        if let Err(e) = write.send(msg).await {
            tracing::debug!(venue = exchange.as_str(), %conn, error = %e, "websocket write failed");
            return;
        }
    }
    let _ = write.send(Message::Close(None)).await;
}

/// Спит `d` или до сигнала остановки; истина — если пришла остановка.
async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => *shutdown.borrow(),
        _ = shutdown.changed() => true,
    }
}

/// Убирает query из URL для логов (в M0 секретов нет, но длинные списки потоков не нужны).
pub fn redact_query(url: &str) -> String {
    match url.split_once('?') {
        Some((base, q)) if q.len() > 64 => format!("{base}?<{} bytes>", q.len()),
        _ => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbox_flush_and_reconnect() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut o = Outbox::default();
        o.send("a".into());
        o.send("b".into());
        assert_eq!(flush(&mut o, &tx), None);
        assert_eq!(rx.try_recv().ok(), Some(Message::text("a")));
        o.reconnect("x");
        assert_eq!(flush(&mut o, &tx), Some("x".into()));
        // переполнение очереди — повод переподключиться
        for i in 0..10 {
            o.send(i.to_string());
        }
        assert_eq!(flush(&mut o, &tx), Some("outgoing queue full".into()));
    }

    #[test]
    fn redacts_long_query() {
        assert_eq!(redact_query("wss://a/ws"), "wss://a/ws");
        assert_eq!(redact_query("wss://a/s?readonly=true"), "wss://a/s?readonly=true");
        let long = format!("wss://a/stream?streams={}", "x".repeat(100));
        assert_eq!(redact_query(&long), "wss://a/stream?<108 bytes>");
    }
}
