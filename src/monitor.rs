//! Учёт потока нормализованных событий: свежесть данных по площадкам
//! (`md_staleness_ms`) и периодическая сводка в лог. Используется в режиме записи
//! (как единственный потребитель событий) и внутри цикла сканера.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use sh_core::Clock;
use sh_venue_api::{MdEvent, MdEventKind};
use tokio::sync::mpsc;

#[derive(Default)]
struct VenueStats {
    last_mono_ns: u64,
    events: u64,
    books: u64,
    trades: u64,
    funding: u64,
    metadata: u64,
    invalidated: u64,
}

/// Счётчики событий по площадкам.
#[derive(Default)]
pub struct MdStats {
    venues: BTreeMap<String, VenueStats>,
}

impl MdStats {
    /// Учитывает событие.
    pub fn on_event(&mut self, ev: &MdEvent) {
        let s = self.venues.entry(ev.venue.exchange.as_str().to_owned()).or_default();
        s.last_mono_ns = s.last_mono_ns.max(ev.recv.mono_ns);
        s.events += 1;
        match ev.kind {
            MdEventKind::BookSnapshot { .. } | MdEventKind::BookDelta { .. } | MdEventKind::Bbo { .. } => s.books += 1,
            MdEventKind::Trades(_) => s.trades += 1,
            MdEventKind::Funding(_) => s.funding += 1,
            MdEventKind::Metadata(_) => s.metadata += 1,
            MdEventKind::BookInvalidated => s.invalidated += 1,
            MdEventKind::Mark { .. } | MdEventKind::OpenInterest { .. } => {}
        }
    }

    /// Обновляет метрику свежести; возвращает возраст по площадкам, мс.
    pub fn staleness(&self, now_mono_ns: u64) -> BTreeMap<String, u64> {
        self.venues
            .iter()
            .map(|(v, s)| {
                let age = now_mono_ns.saturating_sub(s.last_mono_ns) / 1_000_000;
                metrics::gauge!("md_staleness_ms", "venue" => v.clone()).set(age as f64);
                (v.clone(), age)
            })
            .collect()
    }

    /// Сводка в лог.
    pub fn log_summary(&self) {
        for (venue, s) in &self.venues {
            tracing::info!(venue = %venue, events = s.events, books = s.books, trades = s.trades, funding = s.funding, metadata = s.metadata, invalidated = s.invalidated, "market data summary (cumulative)");
        }
    }
}

/// Читает события, пока живы отправители: при остановке адаптеры ещё дописывают
/// события отключения, и канал не должен закрываться раньше них.
pub async fn run(mut rx: mpsc::Receiver<MdEvent>, clock: Arc<dyn Clock>) {
    let mut stats = MdStats::default();
    let mut gauge_tick = tokio::time::interval(Duration::from_secs(1));
    let mut log_tick = tokio::time::interval(Duration::from_secs(60));
    log_tick.tick().await;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                stats.on_event(&ev);
            }
            _ = gauge_tick.tick() => {
                stats.staleness(clock.mono_ns());
            }
            _ = log_tick.tick() => stats.log_summary(),
        }
    }
}
