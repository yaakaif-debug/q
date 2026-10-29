//! Рабочая часть адаптера Aster: комбинированные потоки по URL (без SUBSCRIBE),
//! локальные стаканы по дельтам с синхронизацией через REST-снимок по процедуре
//! документации, плановое переподключение до 24 ч, опрос метаданных, ОИ и фандинга.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rust_decimal::Decimal;
use sh_core::{Exchange, RecvTs, SysEvent};
use sh_md::{ApplyOutcome, SequencedBook};
use sh_venue_api::conn::{ConnSpec, Outbox, PingKind, Session, run_connection};
use sh_venue_api::{
    FundingUpdate, InstrumentMeta, MarketDataVenue, MdEvent, MdEventKind, ProxyConfig, RestClient, Sinks, VenueError,
    WeightLimiter,
};
use tokio::sync::{Semaphore, mpsc, watch};

use crate::config::AsterConfig;
use crate::normalize::{self, AsMsg};
use crate::wire::{DepthSnapshot, ExchangeInfo, FundingInfo, FundingRateItem, OpenInterest, SymbolInfo};

const EX: Exchange = Exchange::Aster;
/// Буфер дельт до снимка: снимок приходит через REST, дельты идут каждые 100 мс.
const MAX_PENDING: usize = 2048;
/// Потоков на символ: дельты стакана и сделки.
const STREAMS_PER_SYMBOL: usize = 2;

#[derive(Default)]
struct Shared {
    symbols: RwLock<Vec<SymbolInfo>>,
    intervals: RwLock<HashMap<String, Decimal>>,
}

impl Shared {
    fn recordable(&self) -> Vec<String> {
        let s = self.symbols.read().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<String> = s.iter().filter(|s| normalize::is_recordable(s)).map(|s| s.symbol.clone()).collect();
        v.sort();
        v
    }

    fn interval(&self, symbol: &str) -> Option<Decimal> {
        self.intervals.read().unwrap_or_else(|e| e.into_inner()).get(symbol).copied()
    }

    fn instruments(&self) -> Vec<InstrumentMeta> {
        let s = self.symbols.read().unwrap_or_else(|e| e.into_inner()).clone();
        s.iter().filter_map(|s| normalize::instrument(s, self.interval(&s.symbol))).collect()
    }
}

/// Адаптер Aster.
pub struct AsterVenue {
    cfg: AsterConfig,
    proxy: Option<ProxyConfig>,
}

/// Маршруты команд по номеру соединения (соединения добавляются при новых листингах).
type Routes = Arc<RwLock<Vec<mpsc::Sender<AsCmd>>>>;

impl AsterVenue {
    /// Создаёт адаптер; конфигурация должна быть проверена `validate()`.
    pub fn new(cfg: AsterConfig, proxy: Option<ProxyConfig>) -> Self {
        Self { cfg, proxy }
    }

    fn rest(&self, sinks: Sinks) -> Result<RestClient, VenueError> {
        RestClient::new(
            &self.cfg.rest,
            EX,
            WeightLimiter::per_minute(self.cfg.rest_weight_per_minute),
            sinks,
            Duration::from_secs(20),
        )
    }

    fn conn_spec(&self, i: usize) -> ConnSpec {
        // разносим плановые переподключения на 5 минут, чтобы не терять все стаканы разом
        let age = self.cfg.max_connection_age_secs.saturating_sub(300 * i as u64).max(3600);
        ConnSpec {
            exchange: EX,
            conn_id: format!("as-ws-{i}"),
            ping: PingKind::Frame,
            ping_every: Duration::from_secs(self.cfg.ping_secs),
            idle_timeout: Duration::from_secs(self.cfg.idle_timeout_secs),
            max_age: Some(Duration::from_secs(age)),
            connect_timeout: Duration::from_secs(15),
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
        }
    }

    fn symbol_streams(&self, symbols: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(symbols.len() * STREAMS_PER_SYMBOL);
        for s in symbols {
            let l = s.to_ascii_lowercase();
            if self.cfg.subscribe_depth {
                out.push(format!("{l}@depth@100ms"));
            }
            if self.cfg.subscribe_trades {
                out.push(format!("{l}@aggTrade"));
            }
        }
        out
    }

    fn symbols_per_conn(&self) -> usize {
        let per_symbol = usize::from(self.cfg.subscribe_depth) + usize::from(self.cfg.subscribe_trades);
        (self.cfg.streams_per_connection / per_symbol.max(1)).max(1)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_conn(
        &self,
        tasks: &mut tokio::task::JoinSet<Result<(), VenueError>>,
        routes: &Routes,
        streams: Vec<String>,
        shared: &Arc<Shared>,
        snap_tx: &mpsc::Sender<SnapReq>,
        sinks: &Sinks,
        shutdown: &watch::Receiver<bool>,
    ) -> usize {
        let (tx, rx) = mpsc::channel(1024);
        let idx = {
            let mut r = routes.write().unwrap_or_else(|e| e.into_inner());
            r.push(tx);
            r.len() - 1
        };
        let session = AsSession {
            idx,
            conn_id: format!("as-ws-{idx}"),
            base: self.cfg.ws.trim_end_matches('/').to_owned(),
            streams,
            books: HashMap::new(),
            requested: HashSet::new(),
            epoch: 0,
            snap_tx: snap_tx.clone(),
            shared: shared.clone(),
            sinks: sinks.clone(),
        };
        tasks.spawn(run_connection(
            self.conn_spec(idx),
            session,
            rx,
            sinks.clone(),
            None,
            self.proxy.clone(),
            shutdown.clone(),
        ));
        idx
    }
}

async fn load_symbols(rest: &RestClient) -> Result<(Vec<SymbolInfo>, HashMap<String, Decimal>), VenueError> {
    let info: ExchangeInfo = serde_json::from_str(&rest.get("/fapi/v3/exchangeInfo", 1).await?)?;
    let funding: Vec<FundingInfo> = serde_json::from_str(&rest.get("/fapi/v3/fundingInfo", 1).await?)?;
    let intervals = funding.into_iter().map(|f| (f.symbol, f.funding_interval_hours)).collect();
    Ok((info.symbols, intervals))
}

impl MarketDataVenue for AsterVenue {
    fn exchange(&self) -> Exchange {
        EX
    }

    async fn fetch_metadata(&self, sinks: &Sinks) -> Result<Vec<InstrumentMeta>, VenueError> {
        let rest = self.rest(sinks.clone())?;
        let (symbols, intervals) = load_symbols(&rest).await?;
        Ok(symbols.iter().filter_map(|s| normalize::instrument(s, intervals.get(&s.symbol).copied())).collect())
    }

    async fn run(&self, sinks: Sinks, mut shutdown: watch::Receiver<bool>) -> Result<(), VenueError> {
        let rest = self.rest(sinks.clone())?;
        let shared = Arc::new(Shared::default());
        let (symbols, intervals) = loop {
            match load_symbols(&rest).await {
                Ok(x) => break x,
                Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
                Err(e) => {
                    tracing::warn!(venue = "aster", error = %e, "initial metadata load failed, retrying");
                    sinks
                        .sys(EX, "rest", &SysEvent::RestError { ep: "exchangeInfo".into(), error: e.to_string() })
                        .await?;
                    if sleep_or_shutdown(Duration::from_secs(10), &mut shutdown).await {
                        return Ok(());
                    }
                }
            }
        };
        *shared.symbols.write().unwrap_or_else(|e| e.into_inner()) = symbols;
        *shared.intervals.write().unwrap_or_else(|e| e.into_inner()) = intervals;
        let mut tracker = MetaTracker::default();
        tracker.emit(&shared.instruments(), &sinks).await?;

        let routes: Routes = Arc::new(RwLock::new(Vec::new()));
        let (snap_tx, snap_rx) = mpsc::channel::<SnapReq>(16_384);
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(snapshot_worker(
            self.cfg.clone(),
            rest.clone(),
            snap_rx,
            routes.clone(),
            sinks.clone(),
            shutdown.clone(),
        ));

        // соединение 0 — общие потоки по всем символам
        let mut global = vec!["!markPrice@arr@1s".to_owned()];
        if self.cfg.subscribe_book_ticker {
            global.push("!bookTicker".to_owned());
        }
        self.spawn_conn(&mut tasks, &routes, global, &shared, &snap_tx, &sinks, &shutdown);
        let mut owner: BTreeMap<String, usize> = BTreeMap::new();
        let recordable = shared.recordable();
        for chunk in recordable.chunks(self.symbols_per_conn()) {
            let idx =
                self.spawn_conn(&mut tasks, &routes, self.symbol_streams(chunk), &shared, &snap_tx, &sinks, &shutdown);
            owner.extend(chunk.iter().map(|s| (s.clone(), idx)));
        }
        tracing::info!(
            venue = "aster",
            symbols = recordable.len(),
            connections = routes.read().map(|r| r.len()).unwrap_or(0),
            "starting streams"
        );

        let (changes_tx, mut changes_rx) = mpsc::channel::<Vec<String>>(4);
        tasks.spawn(metadata_loop(
            self.cfg.clone(),
            rest.clone(),
            shared.clone(),
            sinks.clone(),
            tracker,
            changes_tx,
            shutdown.clone(),
        ));
        if self.cfg.open_interest_secs > 0 {
            tasks.spawn(open_interest_loop(
                self.cfg.clone(),
                rest.clone(),
                shared.clone(),
                sinks.clone(),
                shutdown.clone(),
            ));
        }
        if self.cfg.settled_funding_secs > 0 {
            tasks.spawn(settled_loop(self.cfg.clone(), rest.clone(), shared.clone(), sinks.clone(), shutdown.clone()));
        }

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                Some(current) = changes_rx.recv() => {
                    let current: HashSet<String> = current.into_iter().collect();
                    let added: Vec<String> = current.iter().filter(|s| !owner.contains_key(*s)).cloned().collect();
                    let mut added = added;
                    added.sort();
                    for chunk in added.chunks(self.symbols_per_conn()) {
                        let idx = self.spawn_conn(&mut tasks, &routes, self.symbol_streams(chunk), &shared, &snap_tx, &sinks, &shutdown);
                        owner.extend(chunk.iter().map(|s| (s.clone(), idx)));
                        tracing::info!(venue = "aster", symbols = ?chunk, conn = idx, "new symbols subscribed");
                    }
                    let removed: Vec<(String, usize)> = owner.iter().filter(|(s, _)| !current.contains(*s)).map(|(s, i)| (s.clone(), *i)).collect();
                    for (s, idx) in removed {
                        owner.remove(&s);
                        let tx = routes.read().ok().and_then(|r| r.get(idx).cloned());
                        if let Some(tx) = tx {
                            let _ = tx.send(AsCmd::RemoveSymbol(s)).await;
                        }
                    }
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

/// Запрос снимка стакана.
#[derive(Debug, Clone)]
struct SnapReq {
    conn: usize,
    epoch: u64,
    symbol: String,
}

/// Команды соединению.
#[derive(Debug)]
pub enum AsCmd {
    /// Снимок стакана получен.
    Snapshot {
        /// Эпоха соединения, для которой запрашивался снимок.
        epoch: u64,
        /// Символ.
        symbol: String,
        /// Снимок.
        snap: Box<DepthSnapshot>,
        /// Время получения.
        recv: RecvTs,
    },
    /// Снимок получить не удалось; следующий запрос — по следующей дельте.
    SnapshotFailed {
        /// Эпоха.
        epoch: u64,
        /// Символ.
        symbol: String,
    },
    /// Символ снят с торгов: убрать его потоки при следующем подключении.
    RemoveSymbol(String),
}

struct AsSession {
    idx: usize,
    conn_id: String,
    base: String,
    streams: Vec<String>,
    books: HashMap<String, SequencedBook>,
    requested: HashSet<String>,
    epoch: u64,
    snap_tx: mpsc::Sender<SnapReq>,
    shared: Arc<Shared>,
    sinks: Sinks,
}

impl AsSession {
    fn request_snapshot(&mut self, symbol: &str) {
        if self.requested.insert(symbol.to_owned()) {
            let req = SnapReq { conn: self.idx, epoch: self.epoch, symbol: symbol.to_owned() };
            if self.snap_tx.try_send(req).is_err() {
                // очередь полна: повторим по следующей дельте
                self.requested.remove(symbol);
            }
        }
    }

    fn desynced_gauge(&self) {
        let n = self.books.values().filter(|b| !b.is_synced()).count();
        metrics::gauge!("md_books_desynced", "venue" => "aster", "conn" => self.conn_id.clone()).set(n as f64);
    }
}

impl Session for AsSession {
    type Cmd = AsCmd;

    fn url(&self) -> String {
        format!("{}/stream?streams={}", self.base, self.streams.join("/"))
    }

    async fn on_connected(&mut self, _out: &mut Outbox) -> Result<(), VenueError> {
        // новая эпоха: снимки, запрошенные прошлым соединением, игнорируются
        self.epoch += 1;
        self.books.clear();
        self.requested.clear();
        Ok(())
    }

    async fn on_text(&mut self, text: &str, recv: RecvTs, _out: &mut Outbox) -> Result<(), VenueError> {
        match normalize::parse_ws(text)? {
            AsMsg::Depth(d) => {
                let delta = normalize::book_delta(&d);
                let book = self.books.entry(d.symbol.clone()).or_insert_with(|| SequencedBook::new(MAX_PENDING));
                match book.apply_delta(delta.clone()) {
                    ApplyOutcome::Applied => {
                        book.book_mut().touch(recv.mono_ns, d.tx_ms);
                        self.sinks.md(normalize::delta_event(&d, recv)).await?;
                    }
                    ApplyOutcome::Buffered => self.request_snapshot(&d.symbol),
                    ApplyOutcome::Stale => {}
                    ApplyOutcome::Gap { expected, got } => {
                        // текущую дельту кладём в буфер нового цикла синхронизации
                        book.apply_delta(delta);
                        metrics::counter!("md_book_gaps_total", "venue" => "aster").increment(1);
                        tracing::info!(venue = "aster", symbol = %d.symbol, expected, got, "order book gap, resyncing from snapshot");
                        self.sinks
                            .sys(EX, &self.conn_id, &SysEvent::BookGap { symbol: d.symbol.clone(), expected, got })
                            .await?;
                        let ev = MdEvent {
                            venue: normalize::venue(),
                            symbol: d.symbol.clone(),
                            recv,
                            venue_ts_ms: None,
                            kind: MdEventKind::BookInvalidated,
                        };
                        self.sinks.md(ev).await?;
                        self.request_snapshot(&d.symbol);
                        self.desynced_gauge();
                    }
                }
            }
            AsMsg::AggTrade(t) => {
                self.sinks.md(normalize::trade_event(&t, recv)).await?;
            }
            AsMsg::MarkPrices(v) => {
                for m in &v {
                    for ev in normalize::mark_events(m, self.shared.interval(&m.symbol), recv) {
                        self.sinks.md(ev).await?;
                    }
                }
            }
            AsMsg::BookTicker(b) => {
                self.sinks.md(normalize::bbo_event(&b, recv)).await?;
            }
            AsMsg::Other(_) => {}
        }
        Ok(())
    }

    async fn on_command(&mut self, cmd: AsCmd, _out: &mut Outbox) -> Result<(), VenueError> {
        match cmd {
            AsCmd::Snapshot { epoch, symbol, snap, recv } => {
                if epoch != self.epoch {
                    return Ok(());
                }
                self.requested.remove(&symbol);
                let (bids, asks) = normalize::snapshot_levels(&snap);
                let book = self.books.entry(symbol.clone()).or_insert_with(|| SequencedBook::new(MAX_PENDING));
                match book.apply_snapshot(snap.last_update_id, &bids, &asks) {
                    ApplyOutcome::Gap { .. } => {
                        // снимок не стыкуется с буфером (старше первой дельты): берём новый
                        metrics::counter!("md_snapshot_retries_total", "venue" => "aster").increment(1);
                        self.request_snapshot(&symbol);
                    }
                    _ => {
                        book.book_mut().touch(recv.mono_ns, snap.tx_ms);
                        let ev = SysEvent::BookSynced { symbol: symbol.clone(), seq: Some(snap.last_update_id) };
                        self.sinks.sys(EX, &self.conn_id, &ev).await?;
                        self.sinks.md(normalize::snapshot_event(&symbol, &snap, recv)).await?;
                    }
                }
                self.desynced_gauge();
            }
            AsCmd::SnapshotFailed { epoch, symbol } => {
                if epoch == self.epoch {
                    self.requested.remove(&symbol);
                }
            }
            AsCmd::RemoveSymbol(symbol) => {
                let l = symbol.to_ascii_lowercase();
                self.streams.retain(|s| !s.starts_with(&format!("{l}@")));
                self.books.remove(&symbol);
            }
        }
        Ok(())
    }

    async fn on_disconnected(&mut self, _reason: &str) -> Result<(), VenueError> {
        let now = self.sinks.now();
        let synced: Vec<String> = self.books.iter().filter(|(_, b)| b.is_synced()).map(|(s, _)| s.clone()).collect();
        self.books.clear();
        self.requested.clear();
        for symbol in synced {
            let ev = MdEvent {
                venue: normalize::venue(),
                symbol,
                recv: now,
                venue_ts_ms: None,
                kind: MdEventKind::BookInvalidated,
            };
            self.sinks.md(ev).await?;
        }
        Ok(())
    }
}

/// Загружает REST-снимки стаканов с общим весовым лимитом и ограничением параллелизма.
async fn snapshot_worker(
    cfg: AsterConfig,
    rest: RestClient,
    mut rx: mpsc::Receiver<SnapReq>,
    routes: Routes,
    sinks: Sinks,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let sem = Arc::new(Semaphore::new(cfg.snapshot_concurrency.max(1)));
    // запросы отслеживаются, чтобы при остановке не оставлять фоновых повторов
    let mut inflight = tokio::task::JoinSet::new();
    loop {
        let req = tokio::select! {
            _ = shutdown.changed() => {
                inflight.abort_all();
                return Ok(());
            }
            Some(_) = inflight.join_next() => continue,
            r = rx.recv() => match r { Some(r) => r, None => return Ok(()) },
        };
        let Ok(permit) = sem.clone().acquire_owned().await else { return Ok(()) };
        let (rest, routes, sinks, cfg) = (rest.clone(), routes.clone(), sinks.clone(), cfg.clone());
        inflight.spawn(async move {
            let _permit = permit;
            let path = format!("/fapi/v3/depth?symbol={}&limit={}", req.symbol, cfg.snapshot_depth);
            let mut delay = Duration::from_secs(1);
            let route = routes.read().ok().and_then(|r| r.get(req.conn).cloned());
            let Some(route) = route else { return };
            for attempt in 0..5 {
                match rest.get(&path, cfg.snapshot_weight()).await {
                    Ok(body) => match serde_json::from_str::<DepthSnapshot>(&body) {
                        Ok(snap) => {
                            let cmd = AsCmd::Snapshot {
                                epoch: req.epoch,
                                symbol: req.symbol.clone(),
                                snap: Box::new(snap),
                                recv: sinks.now(),
                            };
                            let _ = route.send(cmd).await;
                            return;
                        }
                        Err(e) => {
                            tracing::debug!(venue = "aster", symbol = %req.symbol, error = %e, "depth snapshot parse failed")
                        }
                    },
                    Err(VenueError::SinkClosed) => return,
                    Err(e) => {
                        tracing::debug!(venue = "aster", symbol = %req.symbol, attempt, error = %e, "depth snapshot failed")
                    }
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
            let _ = route.send(AsCmd::SnapshotFailed { epoch: req.epoch, symbol: req.symbol }).await;
        });
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
    cfg: AsterConfig,
    rest: RestClient,
    shared: Arc<Shared>,
    sinks: Sinks,
    mut tracker: MetaTracker,
    changes: mpsc::Sender<Vec<String>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let every = Duration::from_secs(cfg.metadata_refresh_secs.max(10));
    loop {
        if sleep_or_shutdown(every, &mut shutdown).await {
            return Ok(());
        }
        match load_symbols(&rest).await {
            Ok((symbols, intervals)) => {
                let before = shared.recordable();
                *shared.symbols.write().unwrap_or_else(|e| e.into_inner()) = symbols;
                *shared.intervals.write().unwrap_or_else(|e| e.into_inner()) = intervals;
                let changed = tracker.emit(&shared.instruments(), &sinks).await?;
                if changed > 0 {
                    tracing::info!(venue = "aster", changed, "instrument metadata changed");
                }
                let after = shared.recordable();
                if after != before {
                    let _ = changes.try_send(after);
                }
            }
            Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
            Err(e) => {
                tracing::warn!(venue = "aster", error = %e, "metadata refresh failed");
                sinks.sys(EX, "rest", &SysEvent::RestError { ep: "exchangeInfo".into(), error: e.to_string() }).await?;
            }
        }
        // снимок прогнозов фандинга и цен — только для записи (поток markPrice даёт то же в реальном времени)
        match rest.get("/fapi/v3/premiumIndex", 1).await {
            Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
            Err(e) => tracing::debug!(venue = "aster", error = %e, "premiumIndex failed"),
            Ok(_) => {}
        }
    }
}

async fn open_interest_loop(
    cfg: AsterConfig,
    rest: RestClient,
    shared: Arc<Shared>,
    sinks: Sinks,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let every = Duration::from_secs(cfg.open_interest_secs.max(10));
    loop {
        for symbol in shared.recordable() {
            if *shutdown.borrow() {
                return Ok(());
            }
            // путь /fapi/v1: в документации v3 эндпоинта нет, но он работает (см. docs/venues/aster.md)
            match rest.get(&format!("/fapi/v1/openInterest?symbol={symbol}"), 1).await {
                Ok(body) => {
                    if let Ok(oi) = serde_json::from_str::<OpenInterest>(&body) {
                        let ev = MdEvent {
                            venue: normalize::venue(),
                            symbol: oi.symbol,
                            recv: sinks.now(),
                            venue_ts_ms: Some(oi.time),
                            kind: MdEventKind::OpenInterest { base: Some(oi.open_interest), usd: None },
                        };
                        sinks.md(ev).await?;
                    }
                }
                Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
                Err(e) => tracing::debug!(venue = "aster", %symbol, error = %e, "openInterest failed"),
            }
        }
        if sleep_or_shutdown(every, &mut shutdown).await {
            return Ok(());
        }
    }
}

async fn settled_loop(
    cfg: AsterConfig,
    rest: RestClient,
    shared: Arc<Shared>,
    sinks: Sinks,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let every = Duration::from_secs(cfg.settled_funding_secs.max(60));
    let mut last_seen: HashMap<String, i64> = HashMap::new();
    loop {
        for symbol in shared.recordable() {
            if *shutdown.borrow() {
                return Ok(());
            }
            match rest.get(&format!("/fapi/v3/fundingRate?symbol={symbol}&limit=3"), 1).await {
                Ok(body) => {
                    let Ok(items) = serde_json::from_str::<Vec<FundingRateItem>>(&body) else { continue };
                    let interval = shared.interval(&symbol);
                    let last = last_seen.get(&symbol).copied().unwrap_or(i64::MIN);
                    for it in items.iter().filter(|i| i.funding_time > last) {
                        let ev = MdEvent {
                            venue: normalize::venue(),
                            symbol: it.symbol.clone(),
                            recv: sinks.now(),
                            venue_ts_ms: Some(it.funding_time),
                            kind: MdEventKind::Funding(FundingUpdate {
                                settled_rate: Some(it.funding_rate),
                                settled_at_ms: Some(it.funding_time),
                                interval_hours: interval,
                                ..FundingUpdate::default()
                            }),
                        };
                        sinks.md(ev).await?;
                    }
                    if let Some(max) = items.iter().map(|i| i.funding_time).max() {
                        last_seen.insert(symbol, max);
                    }
                }
                Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
                Err(e) => tracing::debug!(venue = "aster", %symbol, error = %e, "fundingRate failed"),
            }
        }
        if sleep_or_shutdown(every, &mut shutdown).await {
            return Ok(());
        }
    }
}

async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => *shutdown.borrow(),
        _ = shutdown.changed() => true,
    }
}
