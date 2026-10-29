//! Потребитель нормализованных событий в режиме записи: свежесть данных по
//! площадкам (`md_staleness_ms`) и периодическая сводка в лог.

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

/// Читает события, пока живы отправители: при остановке адаптеры ещё дописывают
/// события отключения, и канал не должен закрываться раньше них.
pub async fn run(mut rx: mpsc::Receiver<MdEvent>, clock: Arc<dyn Clock>) {
    let mut stats: BTreeMap<String, VenueStats> = BTreeMap::new();
    let mut gauge_tick = tokio::time::interval(Duration::from_secs(1));
    let mut log_tick = tokio::time::interval(Duration::from_secs(60));
    log_tick.tick().await;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                let s = stats.entry(ev.venue.exchange.as_str().to_owned()).or_default();
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
            _ = gauge_tick.tick() => {
                let now = clock.mono_ns();
                for (venue, s) in &stats {
                    let age_ms = now.saturating_sub(s.last_mono_ns) / 1_000_000;
                    metrics::gauge!("md_staleness_ms", "venue" => venue.clone()).set(age_ms as f64);
                }
            }
            _ = log_tick.tick() => {
                for (venue, s) in &stats {
                    tracing::info!(venue = %venue, events = s.events, books = s.books, trades = s.trades, funding = s.funding, metadata = s.metadata, invalidated = s.invalidated, "market data summary (cumulative)");
                }
            }
        }
    }
}
