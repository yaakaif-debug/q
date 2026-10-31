//! Подключение WebSocket (с поддержкой HTTP CONNECT-прокси) и экспоненциальная задержка
//! переподключения.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::VenueError;

/// Поток WebSocket, общий для всех адаптеров.
pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Настройки HTTP-прокси для WebSocket (reqwest читает прокси сам).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// Хост прокси.
    pub host: String,
    /// Порт прокси.
    pub port: u16,
    /// Хосты-исключения (суффиксы), как в `NO_PROXY`.
    pub no_proxy: Vec<String>,
}

impl ProxyConfig {
    /// Разбирает `http://host:port` (учётные данные не поддерживаются).
    pub fn parse(url: &str, no_proxy: &str) -> Result<Self, VenueError> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| VenueError::Config(format!("only http:// proxies are supported: {url}")))?;
        let authority = rest.trim_end_matches('/');
        if authority.contains('@') {
            return Err(VenueError::Config("proxy credentials are not supported".into()));
        }
        let (host, port) =
            authority.rsplit_once(':').ok_or_else(|| VenueError::Config(format!("proxy url has no port: {url}")))?;
        let port = port.parse::<u16>().map_err(|_| VenueError::Config(format!("bad proxy port in {url}")))?;
        let no_proxy = no_proxy
            .split(',')
            .map(|s| s.trim().trim_start_matches('*').trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        Ok(Self { host: host.to_owned(), port, no_proxy })
    }

    /// Прокси из окружения (`HTTPS_PROXY`/`https_proxy`, `NO_PROXY`/`no_proxy`).
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")).ok()?;
        let no_proxy = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")).unwrap_or_default();
        match Self::parse(&url, &no_proxy) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(error = %e, "ignoring unsupported HTTPS_PROXY");
                None
            }
        }
    }

    /// Нужно ли идти к `host` через прокси.
    pub fn applies_to(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        !self.no_proxy.iter().any(|np| host == *np || host.ends_with(&format!(".{np}")))
    }
}

/// Хост и порт из URL `wss://host[:port]/path`.
pub fn host_port(url: &str) -> Result<(String, u16), VenueError> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| VenueError::Config(format!("bad url {url}")))?;
    let default_port = match scheme {
        "wss" | "https" => 443,
        "ws" | "http" => 80,
        other => return Err(VenueError::Config(format!("unsupported scheme {other}"))),
    };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port = p.parse().map_err(|_| VenueError::Config(format!("bad port in {url}")))?;
            Ok((h.to_owned(), port))
        }
        None => Ok((authority.to_owned(), default_port)),
    }
}

/// Открывает туннель через HTTP CONNECT.
async fn connect_via_proxy(proxy: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream, VenueError> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port)).await?;
    let req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    // читаем ответ прокси побайтно до конца заголовков, чтобы не съесть TLS-данные
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err(VenueError::Ws("proxy response header too long".into()));
        }
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(VenueError::Ws("proxy closed connection during CONNECT".into()));
        }
        head.push(byte[0]);
    }
    let status_line = String::from_utf8_lossy(&head);
    let status_line = status_line.lines().next().unwrap_or_default().to_owned();
    if status_line.split_whitespace().nth(1) != Some("200") {
        return Err(VenueError::Ws(format!("proxy CONNECT failed: {status_line}")));
    }
    Ok(stream)
}

/// Подключается к WebSocket; через прокси, если он задан и применим к хосту.
pub async fn connect_ws(url: &str, proxy: Option<&ProxyConfig>, timeout: Duration) -> Result<WsStream, VenueError> {
    let fut = async {
        let (host, port) = host_port(url)?;
        let tcp = match proxy.filter(|p| p.applies_to(&host)) {
            Some(p) => connect_via_proxy(p, &host, port).await?,
            None => TcpStream::connect((host.as_str(), port)).await?,
        };
        tcp.set_nodelay(true)?;
        let (ws, _resp) = tokio_tungstenite::client_async_tls_with_config(url, tcp, None, None)
            .await
            .map_err(|e| VenueError::Ws(e.to_string()))?;
        Ok(ws)
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| VenueError::Ws(format!("connect timeout after {timeout:?}")))?
}

/// Экспоненциальная задержка переподключения с детерминированным «дрожанием».
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempt: u32,
    seed: u64,
}

impl Backoff {
    /// Создаёт задержку: `base * 2^n`, не более `max`, дрожание ±20%.
    pub fn new(base: Duration, max: Duration, seed: u64) -> Self {
        Self { base, max, attempt: 0, seed: seed | 1 }
    }

    /// Следующая задержка; увеличивает счётчик попыток.
    pub fn next_delay(&mut self) -> Duration {
        let exp = self.attempt.min(20);
        self.attempt = self.attempt.saturating_add(1);
        let raw = self.base.saturating_mul(1u32 << exp).min(self.max);
        // xorshift: дрожание без внешних зависимостей, чтобы соединения не переподключались хором
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let jitter_permille = 800 + (self.seed % 401);
        raw.mul_f64(jitter_permille as f64 / 1000.0).min(self.max)
    }

    /// Сбрасывает счётчик после устойчивого соединения.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Номер текущей попытки.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_and_no_proxy() {
        let p = ProxyConfig::parse("http://127.0.0.1:41341", "localhost,.svc.cluster.local,index.crates.io").unwrap();
        assert_eq!((p.host.as_str(), p.port), ("127.0.0.1", 41341));
        assert!(p.applies_to("api.hyperliquid.xyz"));
        assert!(!p.applies_to("index.crates.io"));
        assert!(!p.applies_to("a.svc.cluster.local"));
        assert!(ProxyConfig::parse("socks5://x:1", "").is_err());
        assert!(ProxyConfig::parse("http://u:p@x:1", "").is_err());
    }

    #[test]
    fn host_port_defaults() {
        assert_eq!(host_port("wss://api.hyperliquid.xyz/ws").unwrap(), ("api.hyperliquid.xyz".into(), 443));
        assert_eq!(host_port("ws://localhost:9000/x?y").unwrap(), ("localhost".into(), 9000));
        assert!(host_port("ftp://x").is_err());
    }

    #[test]
    fn backoff_grows_and_caps() {
        let mut b = Backoff::new(Duration::from_millis(500), Duration::from_secs(30), 42);
        let d: Vec<Duration> = (0..12).map(|_| b.next_delay()).collect();
        assert!(d[0] >= Duration::from_millis(400) && d[0] <= Duration::from_millis(600));
        assert!(d[3] > d[0]);
        assert!(d.iter().all(|x| *x <= Duration::from_secs(30)));
        assert!(d[11] >= Duration::from_secs(24));
        b.reset();
        assert_eq!(b.attempt(), 0);
    }
}
