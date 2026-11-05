//! Рабочая часть адаптера Lighter: соединения с контролем `nonce` стаканов
//! (разрыв → отписка и повторная подписка ради нового снимка), опрос метаданных
//! и вывод интервала фандинга.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rust_decimal::Decimal;
use serde_json::json;
use sh_core::{Exchange, RecvTs, SysEvent};
use sh_md::{ApplyOutcome, SequencedBook};
use sh_venue_api::conn::{ConnSpec, Outbox, PingKind, Session, run_connection};
use sh_venue_api::{
    FundingIntervalTracker, InstrumentMeta, MarketDataVenue, MdEvent, MdEventKind, PlanDiff, ProxyConfig, RestClient,
    Sinks, SubPlan, VenueError, WeightLimiter,
};
use tokio::sync::{mpsc, watch};

use crate::config::LighterConfig;
use crate::normalize::{self, LtMsg};
use crate::subs::{LtSub, desired_subs};
use crate::wire::{Fundings, OrderBookDetails, PerpDetail};

const EX: Exchange = Exchange::Lighter;
/// Буфер дельт до снимка: снимок приходит первым после подписки, буфер почти не нужен.
const MAX_PENDING: usize = 256;

#[derive(Default)]
struct Shared {
    markets: RwLock<Vec<PerpDetail>>,
    trackers: Mutex<HashMap<u32, FundingIntervalTracker>>,
}

impl Shared {
    fn symbol(&self, market: u32) -> String {
        let m = self.markets.read().unwrap_or_else(|e| e.into_inner());
        m.iter().find(|d| d.market_id == market).map(|d| d.symbol.clone()).unwrap_or_else(|| format!("market{market}"))
    }

    fn active_markets(&self) -> Vec<u32> {
        let m = self.markets.read().unwrap_or_else(|e| e.into_inner());
        m.iter().filter(|d| d.status == "active").map(|d| d.market_id).collect()
    }

    fn interval(&self, market: u32) -> Option<Decimal> {
        self.trackers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&market)
            .and_then(FundingIntervalTracker::interval_hours)
    }

    fn instruments(&self) -> Vec<InstrumentMeta> {
        let m = self.markets.read().unwrap_or_else(|e| e.into_inner()).clone();
        m.iter().map(|d| normalize::instrument(d, self.interval(d.market_id))).collect()
    }
}

/// Адаптер Lighter.
pub struct LighterVenue {
    cfg: LighterConfig,
    proxy: Option<ProxyConfig>,
}

impl LighterVenue {
    /// Создаёт адаптер; конфигурация должна быть проверена `validate()`.
    pub fn new(cfg: LighterConfig, proxy: Option<ProxyConfig>) -> Self {
        Self { cfg, proxy }
    }

    fn rest(&self, sinks: Sinks) -> Result<RestClient, VenueError> {
        RestClient::new(
            &self.cfg.rest,
            EX,
            WeightLimiter::per_minute(self.cfg.rest_requests_per_minute),
            sinks,
            Duration::from_secs(20),
        )
    }

    fn desired(&self, shared: &Shared) -> Vec<LtSub> {
        desired_subs(
            &shared.active_markets(),
            self.cfg.subscribe_books,
            self.cfg.subscribe_trades,
            self.cfg.subscribe_ticker,
        )
    }

    fn conn_spec(&self, i: usize) -> ConnSpec {
        ConnSpec {
            exchange: EX,
            conn_id: format!("lt-ws-{i}"),
            ping: PingKind::Text(json!({"type": "ping"}).to_string()),
            ping_every: Duration::from_secs(self.cfg.ping_secs),
            idle_timeout: Duration::from_secs(self.cfg.idle_timeout_secs),
            max_age: None,
            connect_timeout: Duration::from_secs(15),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        }
    }
}

async fn load_markets(rest: &RestClient) -> Result<Vec<PerpDetail>, VenueError> {
    let body = rest.get("/api/v1/orderBookDetails?filter=perp", 1).await?;
    let d: OrderBookDetails = serde_json::from_str(&body)?;
    Ok(d.order_book_details)
}

/// Число соединений: желаемые подписки с запасом 25% на новые рынки.
fn conn_count(desired: usize, per_conn: usize, max_total: usize) -> usize {
    (desired * 5 / 4).min(max_total).div_ceil(per_conn.max(1)).max(1)
}

impl MarketDataVenue for LighterVenue {
    fn exchange(&self) -> Exchange {
        EX
    }

    async fn fetch_metadata(&self, sinks: &Sinks) -> Result<Vec<InstrumentMeta>, VenueError> {
        let rest = self.rest(sinks.clone())?;
        Ok(load_markets(&rest).await?.iter().map(|d| normalize::instrument(d, None)).collect())
    }

    async fn run(&self, sinks: Sinks, mut shutdown: watch::Receiver<bool>) -> Result<(), VenueError> {
        let rest = self.rest(sinks.clone())?;
        let shared = Arc::new(Shared::default());
        let markets = loop {
            match load_markets(&rest).await {
                Ok(m) => break m,
                Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
                Err(e) => {
                    tracing::warn!(venue = "lighter", error = %e, "initial metadata load failed, retrying");
                    sinks
                        .sys(EX, "rest", &SysEvent::RestError { ep: "orderBookDetails".into(), error: e.to_string() })
                        .await?;
                    if sleep_or_shutdown(Duration::from_secs(15), &mut shutdown).await {
                        return Ok(());
                    }
                }
            }
        };
        *shared.markets.write().unwrap_or_else(|e| e.into_inner()) = markets;
        let mut tracker = MetaTracker::default();
        tracker.emit(&shared.instruments(), &sinks).await?;

        let desired = self.desired(&shared);
        let n = conn_count(desired.len(), self.cfg.subs_per_connection, self.cfg.max_subscriptions);
        let mut plan = SubPlan::new(n, self.cfg.subs_per_connection, self.cfg.max_subscriptions);
        let first = plan.apply(&desired);
        report_uncovered(&first, &sinks).await?;
        tracing::info!(
            venue = "lighter",
            markets = shared.active_markets().len(),
            connections = n,
            subscriptions = plan.total(),
            "starting streams"
        );

        let ws_limiter = WeightLimiter::with_burst(self.cfg.ws_messages_per_minute, self.cfg.ws_message_burst);
        let mut cmd_txs = Vec::with_capacity(n);
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..n {
            let (tx, rx) = mpsc::channel(256);
            let session = LtSession {
                url: self.cfg.ws.clone(),
                conn_id: format!("lt-ws-{i}"),
                subs: plan.conn_subs(i).cloned().unwrap_or_default(),
                books: HashMap::new(),
                shared: shared.clone(),
                sinks: sinks.clone(),
            };
            tasks.spawn(run_connection(
                self.conn_spec(i),
                session,
                rx,
                sinks.clone(),
                Some(ws_limiter.clone()),
                self.proxy.clone(),
                shutdown.clone(),
            ));
            cmd_txs.push(tx);
        }

        let (replan_tx, mut replan_rx) = mpsc::channel::<()>(4);
        tasks.spawn(metadata_loop(
            self.cfg.clone(),
            rest.clone(),
            shared.clone(),
            sinks.clone(),
            tracker,
            replan_tx,
            shutdown.clone(),
        ));
        if self.cfg.funding_backfill {
            tasks.spawn(funding_backfill(rest.clone(), shared.clone(), sinks.clone(), self.cfg.funding_history_hours, shutdown.clone()));
        }

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                Some(()) = replan_rx.recv() => {
                    let diff = plan.apply(&self.desired(&shared));
                    for (i, subs) in &diff.removes {
                        let _ = cmd_txs[*i].send(LtCmd::Unsubscribe(subs.clone())).await;
                    }
                    for (i, subs) in &diff.adds {
                        let _ = cmd_txs[*i].send(LtCmd::Subscribe(subs.clone())).await;
                    }
                    report_uncovered(&diff, &sinks).await?;
                }
                Some(res) = tasks.join_next() => {
                    if let Ok(Err(VenueError::SinkClosed)) = res {
                        return Err(VenueError::SinkClosed);
                    }
                }
            }
        }
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}

async fn report_uncovered(diff: &PlanDiff<LtSub>, sinks: &Sinks) -> Result<(), VenueError> {
    metrics::gauge!("md_uncovered_streams", "venue" => "lighter").set(diff.uncovered.len() as f64);
    if !diff.uncovered.is_empty() {
        let streams: Vec<String> = diff.uncovered.iter().map(LtSub::channel).collect();
        tracing::warn!(venue = "lighter", count = streams.len(), "streams not subscribed: subscription limit");
        sinks.sys(EX, "plan", &SysEvent::Uncovered { streams }).await?;
    }
    Ok(())
}

/// Команды соединению.
#[derive(Debug)]
pub enum LtCmd {
    /// Подписаться.
    Subscribe(Vec<LtSub>),
    /// Отписаться.
    Unsubscribe(Vec<LtSub>),
}

struct LtSession {
    url: String,
    conn_id: String,
    subs: BTreeSet<LtSub>,
    books: HashMap<u32, SequencedBook>,
    shared: Arc<Shared>,
    sinks: Sinks,
}

impl LtSession {
    fn books_desynced_gauge(&self) {
        let n = self.books.values().filter(|b| !b.is_synced()).count();
        metrics::gauge!("md_books_desynced", "venue" => "lighter", "conn" => self.conn_id.clone()).set(n as f64);
    }

    async fn on_book(
        &mut self,
        market: u32,
        snapshot: bool,
        ts_ms: Option<i64>,
        ob: crate::wire::WsOrderBook,
        recv: RecvTs,
        out: &mut Outbox,
    ) -> Result<(), VenueError> {
        let symbol = self.shared.symbol(market);
        let book = self.books.entry(market).or_insert_with(|| SequencedBook::new(MAX_PENDING));
        if snapshot {
            let bids = normalize::book_delta(&ob).bids;
            let asks = normalize::book_delta(&ob).asks;
            book.apply_snapshot(ob.nonce, &bids, &asks);
            book.book_mut().touch(recv.mono_ns, ts_ms);
            self.sinks
                .sys(EX, &self.conn_id, &SysEvent::BookSynced { symbol: symbol.clone(), seq: Some(ob.nonce) })
                .await?;
            self.sinks.md(normalize::book_event(&symbol, true, ts_ms, &ob, recv)).await?;
            self.books_desynced_gauge();
            return Ok(());
        }
        match book.apply_delta(normalize::book_delta(&ob)) {
            ApplyOutcome::Applied => {
                book.book_mut().touch(recv.mono_ns, ts_ms);
                self.sinks.md(normalize::book_event(&symbol, false, ts_ms, &ob, recv)).await?;
            }
            ApplyOutcome::Buffered | ApplyOutcome::Stale => {}
            ApplyOutcome::Gap { expected, got } => {
                metrics::counter!("md_book_gaps_total", "venue" => "lighter").increment(1);
                tracing::info!(venue = "lighter", %symbol, expected, got, "order book gap, resubscribing");
                self.sinks.sys(EX, &self.conn_id, &SysEvent::BookGap { symbol: symbol.clone(), expected, got }).await?;
                self.sinks.md(invalidated(&symbol, recv)).await?;
                // документация: при разрыве переподписаться, чтобы получить новый снимок
                let sub = LtSub::OrderBook(market);
                out.send(sub.unsubscribe_msg());
                out.send(sub.subscribe_msg());
                self.books_desynced_gauge();
            }
        }
        Ok(())
    }
}

fn invalidated(symbol: &str, recv: RecvTs) -> MdEvent {
    MdEvent {
        venue: normalize::venue(),
        symbol: symbol.to_owned(),
        recv,
        venue_ts_ms: None,
        kind: MdEventKind::BookInvalidated,
    }
}

impl Session for LtSession {
    type Cmd = LtCmd;

    fn url(&self) -> String {
        self.url.clone()
    }

    async fn on_connected(&mut self, out: &mut Outbox) -> Result<(), VenueError> {
        self.books.clear();
        for s in &self.subs {
            out.send(s.subscribe_msg());
        }
        Ok(())
    }

    async fn on_text(&mut self, text: &str, recv: RecvTs, out: &mut Outbox) -> Result<(), VenueError> {
        match normalize::parse_ws(text)? {
            LtMsg::OrderBook { market, snapshot, ts_ms, book } => {
                self.on_book(market, snapshot, ts_ms, book, recv, out).await?;
            }
            LtMsg::Trades { market, trades, .. } => {
                let symbol = self.shared.symbol(market);
                if let Some(ev) = normalize::trades_event(&symbol, &trades, recv) {
                    self.sinks.md(ev).await?;
                }
            }
            LtMsg::MarketStats(stats) => {
                let mut events = Vec::with_capacity(stats.len() * 3);
                {
                    let mut tr = self.shared.trackers.lock().unwrap_or_else(|e| e.into_inner());
                    for s in &stats {
                        let t = tr.entry(s.market_id).or_default();
                        if let Some(ts) = s.funding_timestamp.filter(|t| *t > 0) {
                            t.observe(ts);
                        }
                        events.extend(normalize::stats_events(s, t.interval_hours(), recv));
                    }
                }
                for ev in events {
                    self.sinks.md(ev).await?;
                }
            }
            LtMsg::Ticker { market, ts_ms, ticker } => {
                let symbol = self.shared.symbol(market);
                self.sinks.md(normalize::ticker_event(&symbol, ts_ms, &ticker, recv)).await?;
            }
            LtMsg::Error(e) => {
                tracing::warn!(venue = "lighter", conn = %self.conn_id, error = %e, "venue error message")
            }
            LtMsg::Connected | LtMsg::Pong | LtMsg::Other(_) => {}
        }
        Ok(())
    }

    async fn on_command(&mut self, cmd: LtCmd, out: &mut Outbox) -> Result<(), VenueError> {
        match cmd {
            LtCmd::Subscribe(subs) => {
                for s in subs {
                    if !self.subs.contains(&s) {
                        out.send(s.subscribe_msg());
                        self.subs.insert(s);
                    }
                }
            }
            LtCmd::Unsubscribe(subs) => {
                for s in subs {
                    if self.subs.remove(&s) {
                        out.send(s.unsubscribe_msg());
                        if let LtSub::OrderBook(m) = s {
                            self.books.remove(&m);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn on_disconnected(&mut self, _reason: &str) -> Result<(), VenueError> {
        let now = self.sinks.now();
        let synced: Vec<u32> = self.books.iter().filter(|(_, b)| b.is_synced()).map(|(m, _)| *m).collect();
        self.books.clear();
        for m in synced {
            self.sinks.md(invalidated(&self.shared.symbol(m), now)).await?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct MetaTracker {
    last: HashMap<String, InstrumentMeta>,
}

impl MetaTracker {
    async fn emit(&mut self, instruments: &[InstrumentMeta], sinks: &Sinks) -> Result<usize, VenueError> {
        let mut changed = 0;
        for m in instruments {
            if self.last.get(&m.symbol) != Some(m) {
                self.last.insert(m.symbol.clone(), m.clone());
                changed += 1;
                let ev = MdEvent {
                    venue: m.venue.clone(),
                    symbol: m.symbol.clone(),
                    recv: sinks.now(),
                    venue_ts_ms: None,
                    kind: MdEventKind::Metadata(Box::new(m.clone())),
                };
                sinks.md(ev).await?;
            }
        }
        Ok(changed)
    }
}

async fn metadata_loop(
    cfg: LighterConfig,
    rest: RestClient,
    shared: Arc<Shared>,
    sinks: Sinks,
    mut tracker: MetaTracker,
    replan: mpsc::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let every = Duration::from_secs(cfg.metadata_refresh_secs.max(10));
    loop {
        if sleep_or_shutdown(every, &mut shutdown).await {
            return Ok(());
        }
        match load_markets(&rest).await {
            Ok(markets) => {
                let before = shared.active_markets();
                *shared.markets.write().unwrap_or_else(|e| e.into_inner()) = markets;
                let changed = tracker.emit(&shared.instruments(), &sinks).await?;
                if changed > 0 {
                    tracing::info!(venue = "lighter", changed, "instrument metadata changed");
                }
                if shared.active_markets() != before {
                    let _ = replan.try_send(());
                }
            }
            Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
            Err(e) => {
                tracing::warn!(venue = "lighter", error = %e, "metadata refresh failed");
                sinks
                    .sys(EX, "rest", &SysEvent::RestError { ep: "orderBookDetails".into(), error: e.to_string() })
                    .await?;
            }
        }
    }
}

/// История фандинга по каждому активному рынку: интервал известен сразу,
/// не дожидаясь двух расчётов в потоке `market_stats`.
async fn funding_backfill(
    rest: RestClient,
    shared: Arc<Shared>,
    sinks: Sinks,
    history_hours: i64,
    shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let markets: Vec<PerpDetail> = shared.markets.read().unwrap_or_else(|e| e.into_inner()).clone();
    let now_s = sinks.clock().wall_ms() / 1000;
    for d in markets.iter().filter(|d| d.status == "active") {
        if *shutdown.borrow() {
            return Ok(());
        }
        let path = format!(
            "/api/v1/fundings?market_id={}&resolution=1h&start_timestamp={}&end_timestamp={}&count_back={}",
            d.market_id,
            now_s - history_hours.max(8) * 3600,
            now_s,
            history_hours.clamp(8, 750)
        );
        match rest.get(&path, 1).await {
            Ok(body) => match serde_json::from_str::<Fundings>(&body) {
                Ok(f) => {
                    let interval = {
                        let mut tr = shared.trackers.lock().unwrap_or_else(|e| e.into_inner());
                        let t = tr.entry(d.market_id).or_default();
                        for it in &f.fundings {
                            // в `fundings` время в секундах, в `market_stats` — в миллисекундах
                            t.observe(it.timestamp * 1000);
                        }
                        t.interval_hours()
                    };
                    let recv = sinks.now();
                    for it in &f.fundings {
                        let ev = MdEvent {
                            venue: normalize::venue(),
                            symbol: d.symbol.clone(),
                            recv,
                            venue_ts_ms: Some(it.timestamp * 1000),
                            kind: MdEventKind::Funding(sh_venue_api::FundingUpdate {
                                settled_rate: Some(normalize::pct(it.rate)),
                                settled_at_ms: Some(it.timestamp * 1000),
                                interval_hours: interval,
                                ..sh_venue_api::FundingUpdate::default()
                            }),
                        };
                        sinks.md(ev).await?;
                    }
                }
                Err(e) => tracing::debug!(venue = "lighter", market = d.market_id, error = %e, "fundings parse failed"),
            },
            Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
            Err(e) => tracing::debug!(venue = "lighter", market = d.market_id, error = %e, "fundings request failed"),
        }
    }
    let with_interval = markets.iter().filter(|d| shared.interval(d.market_id).is_some()).count();
    tracing::info!(venue = "lighter", markets = markets.len(), with_interval, "funding interval backfill done");
    Ok(())
}

async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => *shutdown.borrow(),
        _ = shutdown.changed() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_count_has_headroom() {
        // 213 рынков: 1 + 2*213 = 427 подписок → 534 с запасом → 3 соединения по 200
        assert_eq!(conn_count(427, 200, 2000), 3);
        assert_eq!(conn_count(0, 200, 2000), 1);
        assert_eq!(conn_count(10_000, 500, 5000), 10);
    }
}
