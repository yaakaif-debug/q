//! REST-клиент с общим весовым лимитом на площадку и записью сырых ответов.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use sh_core::{Exchange, RawSource};

use crate::error::VenueError;
use crate::sink::Sinks;

/// Весовой лимитер «вес в минуту», общий для всех запросов площадки.
#[derive(Clone)]
pub struct WeightLimiter {
    inner: Arc<DefaultDirectRateLimiter>,
    /// Размер всплеска: больший вес за один запрос урезается до него.
    per_minute: NonZeroU32,
}

impl WeightLimiter {
    /// Лимит `per_minute` единиц веса в минуту (с всплеском того же размера).
    pub fn per_minute(per_minute: u32) -> Self {
        Self::with_burst(per_minute, per_minute)
    }

    /// Лимит `per_minute` в минуту с всплеском не больше `burst`
    /// (Lighter: не более 50 сообщений «в полёте»).
    pub fn with_burst(per_minute: u32, burst: u32) -> Self {
        let rate = NonZeroU32::new(per_minute.max(1)).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(burst.clamp(1, rate.get())).unwrap_or(NonZeroU32::MIN);
        Self { inner: Arc::new(RateLimiter::direct(Quota::per_minute(rate).allow_burst(burst))), per_minute: burst }
    }

    /// Ждёт, пока можно потратить `weight` единиц.
    pub async fn acquire(&self, weight: u32) {
        // вес больше всплеска governor отвергнет — урезаем до размера всплеска
        let w = NonZeroU32::new(weight.clamp(1, self.per_minute.get())).unwrap_or(NonZeroU32::MIN);
        if self.inner.until_n_ready(w).await.is_err() {
            tracing::warn!(weight, "rate limiter rejected weight above burst");
        }
    }
}

/// REST-клиент площадки.
#[derive(Clone)]
pub struct RestClient {
    http: reqwest::Client,
    base: String,
    exchange: Exchange,
    limiter: WeightLimiter,
    sinks: Sinks,
}

impl RestClient {
    /// Создаёт клиент. Прокси reqwest берёт из окружения.
    pub fn new(
        base: impl Into<String>,
        exchange: Exchange,
        limiter: WeightLimiter,
        sinks: Sinks,
        timeout: Duration,
    ) -> Result<Self, VenueError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("spread-hunter/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| VenueError::Http(e.to_string()))?;
        Ok(Self { http, base: base.into().trim_end_matches('/').to_owned(), exchange, limiter, sinks })
    }

    /// Базовый URL.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// GET `path` (с query), тело ответа записывается в рекордер.
    pub async fn get(&self, path: &str, weight: u32) -> Result<String, VenueError> {
        self.limiter.acquire(weight).await;
        let url = format!("{}{}", self.base, path);
        let req = self.http.get(&url);
        self.execute(req, format!("GET {path}")).await
    }

    /// POST JSON на `path`; метка запроса включает тело (в M0 тела публичные).
    pub async fn post_json(&self, path: &str, body: &serde_json::Value, weight: u32) -> Result<String, VenueError> {
        self.limiter.acquire(weight).await;
        let url = format!("{}{}", self.base, path);
        let req = self.http.post(&url).json(body);
        self.execute(req, format!("POST {path} {body}")).await
    }

    async fn execute(&self, req: reqwest::RequestBuilder, ep: String) -> Result<String, VenueError> {
        let resp = req.send().await.map_err(|e| VenueError::Http(format!("{ep}: {e}")))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| VenueError::Http(format!("{ep}: {e}")))?;
        let recv = self.sinks.now();
        if !status.is_success() {
            metrics::counter!("rest_errors_total", "venue" => self.exchange.as_str()).increment(1);
            let snippet: String = body.chars().take(200).collect();
            return Err(VenueError::Http(format!("{ep}: status {status}: {snippet}")));
        }
        self.sinks.raw(self.exchange, RawSource::Rest { ep }, "rest", recv, body.clone()).await?;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn burst_is_bounded() {
        // governor использует собственные часы, поэтому тест в реальном времени
        let l = WeightLimiter::with_burst(600, 5);
        let start = std::time::Instant::now();
        for _ in 0..5 {
            l.acquire(1).await;
        }
        assert!(start.elapsed() < Duration::from_millis(80));
        // шестой ждёт пополнения: 600/мин = 1 каждые 100 мс
        l.acquire(1).await;
        assert!(start.elapsed() >= Duration::from_millis(80));
    }
}
