//! Рабочая часть адаптера Hyperliquid: соединения WebSocket, план подписок
//! и опросчики REST (дексы, метаданные, прогнозный и расчётный фандинг).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rust_decimal::Decimal;
use serde_json::json;
use sh_core::{Exchange, RecvTs, SysEvent, VenueKey};
use sh_venue_api::conn::{ConnSpec, Outbox, PingKind, Session, run_connection};
use sh_venue_api::{
    FundingIntervalTracker, FundingUpdate, InstrumentMeta, MarketDataVenue, MdEvent, MdEventKind, PlanDiff,
    ProxyConfig, RestClient, Sinks, SubPlan, VenueError, WeightLimiter,
};
use tokio::sync::{mpsc, watch};

use crate::config::HyperliquidConfig;
use crate::normalize::{self, DexState, HlMsg};
use crate::subs::{HlSub, desired_subs};
use crate::wire::{FundingHistoryItem, MetaAndAssetCtxs, PerpDexInfo, PredictedFundings, SpotMeta};

const EX: Exchange = Exchange::Hyperliquid;
/// Вес `info`-запросов, кроме перечисленных в документации как лёгкие.
const INFO_WEIGHT: u32 = 20;

/// Состояние, общее для соединений и опросчиков.
#[derive(Default)]
struct Shared {
    /// Дексы; элемент с `name == ""` — ядро.
    dexes: RwLock<Vec<DexState>>,
    /// Ожидаемое время следующего расчёта фандинга по дексу, мс.
    next_funding: RwLock<HashMap<String, i64>>,
}

impl Shared {
    fn active_coins(&self) -> Vec<String> {
        let dexes = self.dexes.read().unwrap_or_else(|e| e.into_inner());
        dexes.iter().flat_map(|d| d.active_coins().map(str::to_owned).collect::<Vec<_>>()).collect()
    }

    fn instruments(&self) -> Vec<InstrumentMeta> {
        let dexes = self.dexes.read().unwrap_or_else(|e| e.into_inner());
        dexes.iter().flat_map(DexState::instruments).collect()
    }
}

/// Адаптер Hyperliquid.
pub struct HyperliquidVenue {
    cfg: HyperliquidConfig,
    proxy: Option<ProxyConfig>,
}

impl HyperliquidVenue {
    /// Создаёт адаптер; конфигурация должна быть проверена `validate()`.
    pub fn new(cfg: HyperliquidConfig, proxy: Option<ProxyConfig>) -> Self {
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

    fn desired(&self, shared: &Shared) -> Vec<HlSub> {
        desired_subs(
            &shared.active_coins(),
            self.cfg.subscribe_books,
            self.cfg.subscribe_trades,
            self.cfg.subscribe_bbo,
        )
    }

    fn conn_spec(&self, i: usize) -> ConnSpec {
        ConnSpec {
            exchange: EX,
            conn_id: format!("hl-ws-{i}"),
            ping: PingKind::Text(json!({"method": "ping"}).to_string()),
            ping_every: Duration::from_secs(self.cfg.ping_secs),
            idle_timeout: Duration::from_secs(self.cfg.idle_timeout_secs),
            max_age: None,
            connect_timeout: Duration::from_secs(15),
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// Запрос `info`.
async fn info(rest: &RestClient, body: serde_json::Value) -> Result<String, VenueError> {
    rest.post_json("/info", &body, INFO_WEIGHT).await
}

/// Загружает список дексов, спот-токены и метаданные всех дексов.
/// Интервалы фандинга переносятся из `prev`, если декс уже известен.
async fn load_universe(rest: &RestClient, denylist: &[String], prev: &[DexState]) -> Result<Vec<DexState>, VenueError> {
    let dexes: Vec<Option<PerpDexInfo>> = serde_json::from_str(&info(rest, json!({"type": "perpDexs"})).await?)?;
    let spot: SpotMeta = serde_json::from_str(&info(rest, json!({"type": "spotMeta"})).await?)?;
    let mut out = Vec::with_capacity(dexes.len());
    for (index, d) in dexes.iter().enumerate() {
        let name = d.as_ref().map(|d| d.name.clone()).unwrap_or_default();
        if denylist.contains(&name) {
            continue;
        }
        let interval = prev.iter().find(|p| p.name == name).and_then(|p| p.interval_hours);
        out.push(load_dex(rest, &name, index, &spot, interval).await?);
    }
    Ok(out)
}

async fn load_dex(
    rest: &RestClient,
    name: &str,
    index: usize,
    spot: &SpotMeta,
    interval: Option<Decimal>,
) -> Result<DexState, VenueError> {
    let body = if name.is_empty() {
        json!({"type": "metaAndAssetCtxs"})
    } else {
        json!({"type": "metaAndAssetCtxs", "dex": name})
    };
    let MetaAndAssetCtxs(meta, _ctxs) = serde_json::from_str(&info(rest, body).await?)?;
    Ok(normalize::dex_state(name, index, &meta, spot, interval))
}

impl MarketDataVenue for HyperliquidVenue {
    fn exchange(&self) -> Exchange {
        EX
    }

    async fn fetch_metadata(&self, sinks: &Sinks) -> Result<Vec<InstrumentMeta>, VenueError> {
        let rest = self.rest(sinks.clone())?;
        let dexes = load_universe(&rest, &self.cfg.dex_denylist, &[]).await?;
        Ok(dexes.iter().flat_map(DexState::instruments).collect())
    }

    async fn run(&self, sinks: Sinks, mut shutdown: watch::Receiver<bool>) -> Result<(), VenueError> {
        let rest = self.rest(sinks.clone())?;
        let shared = Arc::new(Shared::default());
        let dexes = loop {
            match load_universe(&rest, &self.cfg.dex_denylist, &[]).await {
                Ok(d) => break d,
                Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
                Err(e) => {
                    tracing::warn!(venue = "hyperliquid", error = %e, "initial metadata load failed, retrying");
                    sinks
                        .sys(EX, "rest", &SysEvent::RestError { ep: "load_universe".into(), error: e.to_string() })
                        .await?;
                    if sleep_or_shutdown(Duration::from_secs(10), &mut shutdown).await {
                        return Ok(());
                    }
                }
            }
        };
        tracing::info!(
            venue = "hyperliquid",
            dexes = ?dexes.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            "dexes discovered"
        );
        *shared.dexes.write().unwrap_or_else(|e| e.into_inner()) = dexes;
        let mut meta_tracker = MetaTracker::default();
        meta_tracker.emit(&shared.instruments(), &sinks).await?;

        let n = self.cfg.connections();
        let mut plan = SubPlan::new(n, self.cfg.subs_per_connection, self.cfg.max_subscriptions);
        let first = plan.apply(&self.desired(&shared));
        report_uncovered(&first, &sinks).await?;
        let ws_limiter = WeightLimiter::with_burst(self.cfg.ws_messages_per_minute, self.cfg.ws_message_burst);
        let mut cmd_txs = Vec::with_capacity(n);
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..n {
            let (tx, rx) = mpsc::channel(256);
            let session = HlSession {
                url: self.cfg.ws.clone(),
                conn_id: format!("hl-ws-{i}"),
                subs: plan.conn_subs(i).cloned().unwrap_or_default(),
                shared: shared.clone(),
                sinks: sinks.clone(),
                synced: HashSet::new(),
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
            meta_tracker,
            replan_tx,
            shutdown.clone(),
        ));
        tasks.spawn(predicted_loop(self.cfg.clone(), rest.clone(), shared.clone(), sinks.clone(), shutdown.clone()));
        if self.cfg.settled_funding_poll {
            tasks.spawn(settled_loop(self.cfg.clone(), rest.clone(), shared.clone(), sinks.clone(), shutdown.clone()));
        }

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                Some(()) = replan_rx.recv() => {
                    let diff = plan.apply(&self.desired(&shared));
                    if !diff.is_empty() {
                        tracing::info!(venue = "hyperliquid", total = plan.total(), "subscription plan changed");
                    }
                    for (i, subs) in diff.removes.iter() {
                        let _ = cmd_txs[*i].send(HlCmd::Unsubscribe(subs.clone())).await;
                    }
                    for (i, subs) in diff.adds.iter() {
                        let _ = cmd_txs[*i].send(HlCmd::Subscribe(subs.clone())).await;
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
        while let Some(res) = tasks.join_next().await {
            if let Ok(Err(e)) = res {
                tracing::debug!(venue = "hyperliquid", error = %e, "task ended with error during shutdown");
            }
        }
        Ok(())
    }
}

async fn report_uncovered(diff: &PlanDiff<HlSub>, sinks: &Sinks) -> Result<(), VenueError> {
    metrics::gauge!("md_uncovered_streams", "venue" => "hyperliquid").set(diff.uncovered.len() as f64);
    if !diff.uncovered.is_empty() {
        let streams: Vec<String> = diff.uncovered.iter().map(HlSub::label).collect();
        tracing::warn!(venue = "hyperliquid", count = streams.len(), "streams not subscribed: subscription limit");
        sinks.sys(EX, "plan", &SysEvent::Uncovered { streams }).await?;
    }
    Ok(())
}

/// Команды соединению.
#[derive(Debug)]
pub enum HlCmd {
    /// Подписаться.
    Subscribe(Vec<HlSub>),
    /// Отписаться.
    Unsubscribe(Vec<HlSub>),
}

struct HlSession {
    url: String,
    conn_id: String,
    subs: BTreeSet<HlSub>,
    shared: Arc<Shared>,
    sinks: Sinks,
    /// Монеты, по которым после подключения уже пришёл снимок стакана.
    synced: HashSet<String>,
}

impl HlSession {
    async fn handle_ctxs(&self, all: crate::wire::AllDexsAssetCtxs, recv: RecvTs) -> Result<(), VenueError> {
        // события собираем под блокировкой, отправляем без неё
        let events: Vec<MdEvent> = {
            let dexes = self.shared.dexes.read().unwrap_or_else(|e| e.into_inner());
            let next = self.shared.next_funding.read().unwrap_or_else(|e| e.into_inner());
            let mut evs = Vec::new();
            for (dex_name, ctxs) in &all.ctxs {
                let Some(dex) = dexes.iter().find(|d| &d.name == dex_name) else { continue };
                let next_ms = next.get(dex_name).copied();
                for (i, ctx) in ctxs.iter().enumerate() {
                    // контексты идут в порядке universe; новые активы появятся после обновления метаданных
                    if let Some(asset) = dex.universe.get(i) {
                        evs.extend(normalize::ctx_events(dex, &asset.name, ctx, recv, next_ms));
                    }
                }
            }
            evs
        };
        for ev in events {
            self.sinks.md(ev).await?;
        }
        Ok(())
    }
}

impl Session for HlSession {
    type Cmd = HlCmd;

    fn url(&self) -> String {
        self.url.clone()
    }

    async fn on_connected(&mut self, out: &mut Outbox) -> Result<(), VenueError> {
        self.synced.clear();
        for s in &self.subs {
            out.send(s.subscribe_msg());
        }
        Ok(())
    }

    async fn on_text(&mut self, text: &str, recv: RecvTs, _out: &mut Outbox) -> Result<(), VenueError> {
        match normalize::parse_ws(text)? {
            HlMsg::Book(b) => {
                if self.synced.insert(b.coin.clone()) {
                    let ev = SysEvent::BookSynced { symbol: b.coin.clone(), seq: None };
                    self.sinks.sys(EX, &self.conn_id, &ev).await?;
                }
                self.sinks.md(normalize::book_event(&b, recv)).await?;
            }
            HlMsg::Trades(t) => {
                for ev in normalize::trade_events(&t, recv) {
                    self.sinks.md(ev).await?;
                }
            }
            HlMsg::Bbo(b) => {
                self.sinks.md(normalize::bbo_event(&b, recv)).await?;
            }
            HlMsg::AllDexsAssetCtxs(all) => self.handle_ctxs(all, recv).await?,
            HlMsg::Error(e) => {
                tracing::warn!(venue = "hyperliquid", conn = %self.conn_id, error = %e, "venue error message")
            }
            HlMsg::SubscriptionResponse | HlMsg::Pong | HlMsg::Other(_) => {}
        }
        Ok(())
    }

    async fn on_command(&mut self, cmd: HlCmd, out: &mut Outbox) -> Result<(), VenueError> {
        match cmd {
            HlCmd::Subscribe(subs) => {
                for s in subs {
                    if !self.subs.contains(&s) {
                        out.send(s.subscribe_msg());
                        self.subs.insert(s);
                    }
                }
            }
            HlCmd::Unsubscribe(subs) => {
                for s in subs {
                    if self.subs.remove(&s) {
                        out.send(s.unsubscribe_msg());
                        if let HlSub::L2Book(c) = &s {
                            self.synced.remove(c);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn on_disconnected(&mut self, _reason: &str) -> Result<(), VenueError> {
        for coin in self.synced.drain() {
            let ev = MdEvent {
                venue: normalize::venue_of_coin(&coin),
                symbol: coin,
                recv: self.sinks.now(),
                venue_ts_ms: None,
                kind: MdEventKind::BookInvalidated,
            };
            self.sinks.md(ev).await?;
        }
        Ok(())
    }
}

/// Отслеживает изменения метаданных и отправляет события только по изменившимся инструментам.
#[derive(Default)]
struct MetaTracker {
    last: HashMap<(VenueKey, String), InstrumentMeta>,
}

impl MetaTracker {
    async fn emit(&mut self, instruments: &[InstrumentMeta], sinks: &Sinks) -> Result<usize, VenueError> {
        let mut changed = 0;
        for m in instruments {
            let key = (m.venue.clone(), m.symbol.clone());
            if self.last.get(&key) != Some(m) {
                self.last.insert(key, m.clone());
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

/// Снимки метаданных каждые `metadata_refresh_secs`, обнаружение дексов каждые `dex_discovery_secs`.
async fn metadata_loop(
    cfg: HyperliquidConfig,
    rest: RestClient,
    shared: Arc<Shared>,
    sinks: Sinks,
    mut tracker: MetaTracker,
    replan: mpsc::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let refresh = Duration::from_secs(cfg.metadata_refresh_secs.max(10));
    let discovery_every = (cfg.dex_discovery_secs / refresh.as_secs()).max(1);
    let mut tick: u64 = 0;
    loop {
        if sleep_or_shutdown(refresh, &mut shutdown).await {
            return Ok(());
        }
        tick += 1;
        let prev: Vec<DexState> = shared.dexes.read().unwrap_or_else(|e| e.into_inner()).clone();
        let result = if tick.is_multiple_of(discovery_every) {
            load_universe(&rest, &cfg.dex_denylist, &prev).await
        } else {
            refresh_known(&rest, &prev).await
        };
        match result {
            Ok(dexes) => {
                let old_names: Vec<String> = prev.iter().map(|d| d.name.clone()).collect();
                let new_names: Vec<String> = dexes.iter().map(|d| d.name.clone()).collect();
                if old_names != new_names {
                    tracing::info!(venue = "hyperliquid", old = ?old_names, new = ?new_names, "dex set changed");
                }
                let old_coins = shared.active_coins();
                {
                    let mut w = shared.dexes.write().unwrap_or_else(|e| e.into_inner());
                    // интервалы могли обновиться опросчиком фандинга, пока шёл запрос
                    let intervals: HashMap<String, Option<Decimal>> =
                        w.iter().map(|d| (d.name.clone(), d.interval_hours)).collect();
                    *w = dexes
                        .into_iter()
                        .map(|mut d| {
                            if let Some(i) = intervals.get(&d.name) {
                                d.interval_hours = *i;
                            }
                            d
                        })
                        .collect();
                }
                let changed = tracker.emit(&shared.instruments(), &sinks).await?;
                if changed > 0 {
                    tracing::info!(venue = "hyperliquid", changed, "instrument metadata changed");
                }
                if shared.active_coins() != old_coins {
                    let _ = replan.try_send(());
                }
            }
            Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
            Err(e) => {
                tracing::warn!(venue = "hyperliquid", error = %e, "metadata refresh failed");
                sinks.sys(EX, "rest", &SysEvent::RestError { ep: "metadata".into(), error: e.to_string() }).await?;
            }
        }
    }
}

/// Обновляет метаданные известных дексов без запроса `perpDexs`.
async fn refresh_known(rest: &RestClient, prev: &[DexState]) -> Result<Vec<DexState>, VenueError> {
    let spot: SpotMeta = serde_json::from_str(&info(rest, json!({"type": "spotMeta"})).await?)?;
    let mut out = Vec::with_capacity(prev.len());
    for d in prev {
        out.push(load_dex(rest, &d.name, d.index, &spot, d.interval_hours).await?);
    }
    Ok(out)
}

/// `predictedFundings`: интервал и время расчёта для ядра (документация: только первый декс).
async fn predicted_loop(
    cfg: HyperliquidConfig,
    rest: RestClient,
    shared: Arc<Shared>,
    sinks: Sinks,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let every = Duration::from_secs(cfg.predicted_funding_secs.max(10));
    loop {
        match info(&rest, json!({"type": "predictedFundings"}))
            .await
            .and_then(|b| Ok(serde_json::from_str::<PredictedFundings>(&b)?))
        {
            Ok(pred) => {
                let recv = sinks.now();
                let mut interval = None;
                let mut next = None;
                let mut events = Vec::new();
                for (coin, venues) in &pred {
                    let Some(hl) = venues.iter().find(|(v, _)| v == "HlPerp").and_then(|(_, f)| f.as_ref()) else {
                        continue;
                    };
                    interval = interval.or(hl.funding_interval_hours);
                    next = next.max(Some(hl.next_funding_time));
                    events.push(MdEvent {
                        venue: VenueKey::hyperliquid(""),
                        symbol: coin.clone(),
                        recv,
                        venue_ts_ms: None,
                        kind: MdEventKind::Funding(FundingUpdate {
                            predicted_rate: Some(hl.funding_rate),
                            next_funding_ms: Some(hl.next_funding_time),
                            interval_hours: hl.funding_interval_hours,
                            ..FundingUpdate::default()
                        }),
                    });
                }
                if let Some(i) = interval {
                    let mut w = shared.dexes.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(core) = w.iter_mut().find(|d| d.name.is_empty()) {
                        core.interval_hours = Some(i);
                    }
                }
                if let Some(n) = next {
                    shared.next_funding.write().unwrap_or_else(|e| e.into_inner()).insert(String::new(), n);
                }
                for ev in events {
                    sinks.md(ev).await?;
                }
            }
            Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
            Err(e) => {
                tracing::warn!(venue = "hyperliquid", error = %e, "predictedFundings failed");
                sinks
                    .sys(EX, "rest", &SysEvent::RestError { ep: "predictedFundings".into(), error: e.to_string() })
                    .await?;
            }
        }
        if sleep_or_shutdown(every, &mut shutdown).await {
            return Ok(());
        }
    }
}

/// Расчётный фандинг по каждой монете (`fundingHistory`) и вывод интервала по дексам.
async fn settled_loop(
    cfg: HyperliquidConfig,
    rest: RestClient,
    shared: Arc<Shared>,
    sinks: Sinks,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), VenueError> {
    let trackers: Mutex<HashMap<String, FundingIntervalTracker>> = Mutex::new(HashMap::new());
    let every = Duration::from_secs(cfg.settled_funding_secs.max(60));
    loop {
        // по одной монете каждого декса вперёд, чтобы интервалы появились быстро
        let coins: Vec<String> = {
            let dexes = shared.dexes.read().unwrap_or_else(|e| e.into_inner());
            let mut firsts = Vec::new();
            let mut rest_coins = Vec::new();
            for d in dexes.iter() {
                let mut it = d.active_coins().map(str::to_owned);
                firsts.extend(it.next());
                rest_coins.extend(it);
            }
            firsts.extend(rest_coins);
            firsts
        };
        for coin in coins {
            if *shutdown.borrow() {
                return Ok(());
            }
            let last = trackers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&coin)
                .and_then(FundingIntervalTracker::last_settlement_ms);
            let lookback_ms = i64::try_from(cfg.settled_funding_lookback_hours).unwrap_or(6) * 3_600_000;
            let start = last.map(|t| t + 1).unwrap_or_else(|| sinks.clock().wall_ms() - lookback_ms);
            let body = json!({"type": "fundingHistory", "coin": coin, "startTime": start});
            let items: Vec<FundingHistoryItem> = match rest.post_json("/info", &body, INFO_WEIGHT + 1).await {
                Ok(b) => match serde_json::from_str(&b) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(venue = "hyperliquid", %coin, error = %e, "fundingHistory parse failed");
                        continue;
                    }
                },
                Err(VenueError::SinkClosed) => return Err(VenueError::SinkClosed),
                Err(e) => {
                    tracing::debug!(venue = "hyperliquid", %coin, error = %e, "fundingHistory failed");
                    continue;
                }
            };
            let recv = sinks.now();
            let (interval, events) = {
                let mut t = trackers.lock().unwrap_or_else(|e| e.into_inner());
                let tr = t.entry(coin.clone()).or_default();
                let mut evs = Vec::new();
                for it in items.iter().filter(|i| i.coin == coin) {
                    tr.observe(it.time);
                    evs.push(it.clone());
                }
                let interval = tr.interval_hours();
                let events: Vec<MdEvent> = evs
                    .into_iter()
                    .map(|it| MdEvent {
                        venue: normalize::venue_of_coin(&coin),
                        symbol: coin.clone(),
                        recv,
                        venue_ts_ms: Some(it.time),
                        kind: MdEventKind::Funding(FundingUpdate {
                            settled_rate: Some(it.funding_rate),
                            settled_at_ms: Some(it.time),
                            interval_hours: interval,
                            ..FundingUpdate::default()
                        }),
                    })
                    .collect();
                (interval, events)
            };
            if let Some(i) = interval {
                update_dex_interval(&shared, &trackers, normalize::dex_of_coin(&coin), i);
            }
            for ev in events {
                sinks.md(ev).await?;
            }
        }
        if sleep_or_shutdown(every, &mut shutdown).await {
            return Ok(());
        }
    }
}

/// Интервал декса — самый частый среди его монет (документация: интервал не зависит от актива).
fn update_dex_interval(
    shared: &Shared,
    trackers: &Mutex<HashMap<String, FundingIntervalTracker>>,
    dex: &str,
    fallback: Decimal,
) {
    let (interval, next) = {
        let t = trackers.lock().unwrap_or_else(|e| e.into_inner());
        let mut counts: HashMap<Decimal, usize> = HashMap::new();
        let mut next = None;
        for tr in t.iter().filter(|(c, _)| normalize::dex_of_coin(c) == dex).map(|(_, tr)| tr) {
            if let Some(i) = tr.interval_hours() {
                *counts.entry(i).or_default() += 1;
            }
            next = next.max(tr.next_settlement_ms());
        }
        let interval = counts.into_iter().max_by_key(|(i, n)| (*n, std::cmp::Reverse(*i))).map(|(i, _)| i);
        (interval.unwrap_or(fallback), next)
    };
    let mut w = shared.dexes.write().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = w.iter_mut().find(|d| d.name == dex) {
        // для ядра приоритет у `predictedFundings`, если он уже дал интервал
        if !(d.name.is_empty() && d.interval_hours.is_some()) {
            d.interval_hours = Some(interval);
        }
    }
    drop(w);
    if let Some(n) = next
        && !dex.is_empty()
    {
        shared.next_funding.write().unwrap_or_else(|e| e.into_inner()).insert(dex.to_owned(), n);
    }
}

async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => *shutdown.borrow(),
        _ = shutdown.changed() => true,
    }
}
