//! Запуск процесса: адаптеры площадок, рекордер и потребитель событий по режиму
//! (`record` — только запись, `scan` — сканер и теневой портфель). Повтор сканера
//! по записям — синхронный и детерминированный.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rust_decimal::Decimal;
use sh_core::{Clock, Exchange, Secrets, SystemClock, Usd};
use sh_obs::{Alerts, Level, Telegram};
use sh_portfolio::{DailyReport, LedgerEntry, LedgerKind, ShadowPortfolio, opportunities_csv, opportunity_rows};
use sh_signals::{SymbolMap, pct};
use sh_store::{Db, FORMAT, FileHeader, LedgerRow, Recorder};
use sh_venue_api::{MarketDataVenue, MdEvent, Sinks, VenueError};
use sh_venue_aster::AsterVenue;
use sh_venue_hyperliquid::HyperliquidVenue;
use sh_venue_lighter::LighterVenue;
use tokio::sync::{mpsc, watch};

use crate::config::AppConfig;
use crate::monitor::{self, MdStats};
use crate::replay_source::{ReplaySource, Replayers};
use crate::scan::{ScanEngine, ScanSettings, TickOutput};

/// Ключ состояния теневого портфеля в базе.
const SHADOW_KEY: &str = "shadow_state";

/// Один провайдер rustls на процесс; повторная установка безвредна.
pub fn install_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Имя хоста для заголовков и файлов.
pub fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_owned()))
        .unwrap_or_else(|| "unknown".into())
}

/// Настройки движка сканера из конфигурации.
pub fn scan_settings(cfg: &AppConfig) -> ScanSettings {
    ScanSettings {
        scanner: cfg.scanner.clone(),
        strategy: cfg.strategy.clone(),
        costs: cfg.costs.clone(),
        fees: cfg.fees.clone(),
        risk: cfg.risk.clone(),
        reserve_frac: pct(cfg.engine.equity_reserve_pct),
    }
}

/// Оповещения: Telegram при включённой настройке и наличии секретов, иначе лог.
pub fn make_alerts(cfg: &AppConfig, secrets: &mut Secrets) -> anyhow::Result<Alerts> {
    Ok(make_telegram(cfg, secrets)?.0)
}

/// Клиент команд Telegram и разрешённые пользователи.
pub type TelegramCommands = Option<(Telegram, Vec<i64>)>;

/// Оповещения и клиент команд Telegram с разрешёнными пользователями.
pub fn make_telegram(cfg: &AppConfig, secrets: &mut Secrets) -> anyhow::Result<(Alerts, TelegramCommands)> {
    if !cfg.telegram.enabled {
        return Ok((Alerts::log_only(), None));
    }
    let tg = secrets.telegram.take().context("telegram.enabled = true but [telegram] is missing in secrets")?;
    let allowed = tg.allowed_user_ids.clone();
    let client = Telegram::new(tg.bot_token, tg.chat_id).map_err(anyhow::Error::msg)?;
    let alerts = Alerts::spawn(client.clone(), Duration::from_secs(cfg.telegram.dedupe_secs), cfg.telegram.per_minute);
    Ok((alerts, Some((client, allowed))))
}

/// Запуск в режиме из конфигурации (`record` или `scan`).
pub async fn run_live(cfg: AppConfig) -> anyhow::Result<()> {
    sh_obs::init_logging(&cfg.log).context("logging")?;
    install_crypto();
    sh_obs::init_metrics(&cfg.metrics).map_err(anyhow::Error::msg).context("metrics")?;
    let proxy = cfg.network.proxy().map_err(anyhow::Error::msg)?;
    if let Some(p) = &proxy {
        tracing::warn!(proxy = %format!("{}:{}", p.host, p.port), "websocket traffic goes through an HTTP proxy");
    }
    let mut secrets = Secrets::load(&cfg.secrets.source().map_err(anyhow::Error::msg)?).context("secrets")?;
    let (alerts, tg_commands) = make_telegram(&cfg, &mut secrets)?;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let recorder = if cfg.recorder.enabled {
        let start = clock.wall_ns();
        let header = FileHeader {
            format: FORMAT.into(),
            session: format!("{start:x}-{}", std::process::id()),
            host: host_name(),
            session_start_wall_ns: start,
            version: env!("CARGO_PKG_VERSION").into(),
        };
        tracing::info!(session = %header.session, dir = %cfg.recorder.dir.display(), "recorder starting");
        Some(Recorder::start(cfg.recorder.clone(), header).context("recorder")?)
    } else {
        None
    };

    let (md_tx, md_rx) = mpsc::channel(65_536);
    let sinks = Sinks::new(recorder.as_ref().map(Recorder::sender), Some(md_tx), clock.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let consumer = match cfg.engine.mode.as_str() {
        "scan" => {
            let state = ScanState::open(&cfg, clock.wall_ms()).await?;
            tokio::spawn(scan_loop(md_rx, clock.clone(), state, alerts.clone()))
        }
        "paper" => {
            let state = crate::trade::PaperState::open(&cfg, clock.wall_ms()).await?;
            let (ctl_tx, ctl_rx) = mpsc::channel(16);
            let sock = PathBuf::from(&cfg.control.socket);
            let tx = ctl_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::ctl::serve(sock, tx).await {
                    tracing::error!(error = %e, "control socket failed");
                }
            });
            if let Some((tg, allowed)) = tg_commands.clone() {
                tokio::spawn(crate::ctl::telegram_commands(tg, allowed, ctl_tx.clone()));
            }
            drop(ctl_tx);
            tokio::spawn(crate::trade::paper_loop(md_rx, ctl_rx, clock.clone(), state, alerts.clone()))
        }
        _ => tokio::spawn(monitor::run(md_rx, clock.clone())),
    };

    let mut venues = tokio::task::JoinSet::new();
    let v = &cfg.venues;
    if v.hyperliquid.enabled {
        spawn_venue(&mut venues, HyperliquidVenue::new(v.hyperliquid.clone(), proxy.clone()), &sinks, &shutdown_rx);
    }
    if v.lighter.enabled {
        spawn_venue(&mut venues, LighterVenue::new(v.lighter.clone(), proxy.clone()), &sinks, &shutdown_rx);
    }
    if v.aster.enabled {
        spawn_venue(&mut venues, AsterVenue::new(v.aster.clone(), proxy.clone()), &sinks, &shutdown_rx);
    }
    drop(sinks);
    if venues.is_empty() {
        anyhow::bail!("no venues enabled");
    }
    alerts.send(Level::Info, "startup", &format!("spread-hunter started in mode {}", cfg.engine.mode));

    let mut fatal: Option<anyhow::Error> = None;
    tokio::select! {
        _ = shutdown_signal() => tracing::info!("shutdown signal received"),
        Some(res) = venues.join_next() => {
            fatal = Some(match res {
                Ok((ex, Ok(()))) => anyhow::anyhow!("venue {ex} stopped unexpectedly"),
                Ok((ex, Err(e))) => anyhow::anyhow!("venue {ex} failed: {e}"),
                Err(e) => anyhow::anyhow!("venue task panicked: {e}"),
            });
        }
    }
    let _ = shutdown_tx.send(true);
    // адаптеры закрывают соединения и пишут ConnDown; ждём ограниченное время
    let drained = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(res) = venues.join_next().await {
            if let Ok((ex, Err(e))) = res {
                tracing::warn!(venue = %ex, error = %e, "venue stopped with error");
            }
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("venues did not stop within 15s, aborting");
        venues.abort_all();
        while venues.join_next().await.is_some() {}
    }
    let _ = consumer.await;
    if let Some(recorder) = recorder {
        let stats = tokio::task::spawn_blocking(move || recorder.finish()).await?;
        tracing::info!(records = ?stats.records, raw_bytes = stats.raw_bytes, compressed_bytes = stats.compressed_bytes, write_errors = stats.write_errors, "recorder stopped");
    }
    if let Some(e) = &fatal {
        alerts.send(Level::Crit, "fatal", &format!("spread-hunter stopped: {e}"));
        // даём задаче оповещений шанс отправить
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    match fatal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn spawn_venue<V: MarketDataVenue>(
    set: &mut tokio::task::JoinSet<(Exchange, Result<(), VenueError>)>,
    venue: V,
    sinks: &Sinks,
    shutdown: &watch::Receiver<bool>,
) {
    let (sinks, shutdown) = (sinks.clone(), shutdown.clone());
    set.spawn(async move {
        let ex = venue.exchange();
        tracing::info!(venue = %ex, "venue starting");
        (ex, venue.run(sinks, shutdown).await)
    });
}

/// Сигнал остановки: SIGINT или SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

/// Движок сканера с базой и путями отчётов.
pub struct ScanState {
    engine: ScanEngine,
    db: Option<Db>,
    report_dir: PathBuf,
    suggestions_path: PathBuf,
    telegram_summary: bool,
}

fn kind_str(k: LedgerKind) -> &'static str {
    match k {
        LedgerKind::Funding => "funding",
        LedgerKind::Fee => "fee",
        LedgerKind::Slippage => "slippage",
        LedgerKind::Transfer => "transfer",
        LedgerKind::Basis => "basis",
    }
}

fn kind_from(s: &str) -> Option<LedgerKind> {
    Some(match s {
        "funding" => LedgerKind::Funding,
        "fee" => LedgerKind::Fee,
        "slippage" => LedgerKind::Slippage,
        "transfer" => LedgerKind::Transfer,
        "basis" => LedgerKind::Basis,
        _ => return None,
    })
}

/// Строка базы из записи журнала.
pub fn ledger_to_row(e: &LedgerEntry) -> LedgerRow {
    to_row(e)
}

fn to_row(e: &LedgerEntry) -> LedgerRow {
    LedgerRow {
        ts_ms: e.ts_ms,
        pair_id: e.pair_id.clone(),
        leg: e.leg.clone(),
        kind: kind_str(e.kind).into(),
        amount: e.amount.0.to_string(),
    }
}

fn from_row(r: &LedgerRow) -> Option<LedgerEntry> {
    Some(LedgerEntry {
        ts_ms: r.ts_ms,
        pair_id: r.pair_id.clone(),
        leg: r.leg.clone(),
        kind: kind_from(&r.kind)?,
        amount: Usd(r.amount.parse::<Decimal>().ok()?),
    })
}

impl ScanState {
    /// Открывает базу и восстанавливает теневой портфель, времена появления и журнал текущих суток.
    pub async fn open(cfg: &AppConfig, now_ms: i64) -> anyhow::Result<Self> {
        let symbols = SymbolMap::load(Path::new(&cfg.symbols.path)).context("symbols")?;
        let db = Db::open(Path::new(&cfg.database.path)).await.context("database")?;
        let shadow = if cfg.scanner.shadow_enabled {
            match db.kv_get(SHADOW_KEY).await? {
                Some(json) => Some(serde_json::from_str::<ShadowPortfolio>(&json).context("stored shadow state")?),
                None => Some(ShadowPortfolio::new(Usd(cfg.scanner.shadow_capital_usd), now_ms)),
            }
        } else {
            None
        };
        let first_seen = db.first_seen_all().await?;
        let day = now_ms - now_ms.rem_euclid(86_400_000);
        let day_ledger = db.ledger_between(day, i64::MAX).await?.iter().filter_map(from_row).collect();
        tracing::info!(pairs = symbols.underlyings().len(), restored_shadow = shadow.is_some(), "scanner state opened");
        Ok(Self {
            engine: ScanEngine::new(scan_settings(cfg), symbols, shadow, first_seen, day_ledger),
            db: Some(db),
            report_dir: PathBuf::from(&cfg.scanner.report_dir),
            suggestions_path: PathBuf::from(&cfg.scanner.suggestions_path),
            telegram_summary: cfg.scanner.telegram_summary,
        })
    }

    /// Обрабатывает результат такта: база, файлы, метрики, оповещения.
    pub async fn handle(&mut self, out: TickOutput, now_ms: i64, alerts: &Alerts) -> anyhow::Result<()> {
        if let Some(db) = &self.db {
            if !out.first_seen.is_empty() {
                db.first_seen_insert(&out.first_seen, now_ms).await?;
            }
            let rows: Vec<LedgerRow> = out.ledger.iter().map(to_row).collect();
            db.append_ledger(&rows).await?;
        }
        for id in &out.opened {
            alerts.send(Level::Info, &format!("shadow_open:{id}"), &format!("shadow opened {id}"));
        }
        for (id, why) in &out.closed {
            alerts.send(Level::Info, &format!("shadow_close:{id}"), &format!("shadow closed {id}: {why}"));
        }
        if out.scanned {
            let rows = opportunity_rows(self.engine.last_evals());
            write_atomic(&self.report_dir.join("scan-latest.csv"), &opportunities_csv(&rows, self.engine.tiers()))?;
            for e in self.engine.last_evals().iter().filter(|e| e.entry_ok) {
                if let Some(n) = e.at_size.or_else(|| e.tiers.first().copied()).and_then(|t| t.s_net) {
                    use rust_decimal::prelude::ToPrimitive;
                    metrics::gauge!("s_net_apr", "pair" => e.pair.id()).set(n.to_f64().unwrap_or(0.0));
                }
            }
            if let (Some(db), Some(shadow)) = (&self.db, self.engine.shadow()) {
                db.kv_put(SHADOW_KEY, &serde_json::to_string(shadow)?, now_ms).await?;
            }
        }
        if let Some(rep) = &out.report {
            write_report(&self.report_dir, rep, self.engine.tiers())?;
            write_atomic(&self.suggestions_path, &self.engine.suggestions())?;
            if self.telegram_summary {
                alerts.send(Level::Info, &format!("daily:{}", rep.date), &rep.telegram_summary());
            }
        }
        Ok(())
    }
}

/// Запись через временный файл и переименование, чтобы читатель не видел половину файла.
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// Пишет дневной отчёт: `<date>.md`, `<date>-positions.csv`, `<date>-opportunities.csv`.
pub fn write_report(dir: &Path, rep: &DailyReport, tiers: &[Usd]) -> std::io::Result<()> {
    write_atomic(&dir.join(format!("{}.md", rep.date)), &rep.to_markdown())?;
    write_atomic(&dir.join(format!("{}-positions.csv", rep.date)), &rep.positions_csv())?;
    write_atomic(&dir.join(format!("{}-opportunities.csv", rep.date)), &opportunities_csv(&rep.opportunities, tiers))?;
    tracing::info!(date = %rep.date, equity = %rep.end_equity, "daily report written");
    Ok(())
}

async fn scan_loop(mut rx: mpsc::Receiver<MdEvent>, clock: Arc<dyn Clock>, mut state: ScanState, alerts: Alerts) {
    let mut stats = MdStats::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut log_tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                stats.on_event(&ev);
                state.engine.on_event(&ev);
            }
            _ = tick.tick() => {
                stats.staleness(clock.mono_ns());
                let now = clock.wall_ms();
                let out = state.engine.tick(now);
                if let Err(e) = state.handle(out, now, &alerts).await {
                    alerts.send(Level::Warn, "scan_persist", &format!("scanner persistence failed: {e}"));
                }
            }
            _ = log_tick.tick() => stats.log_summary(),
        }
    }
    // последнее сохранение состояния при остановке
    if let (Some(db), Some(shadow)) = (&state.db, state.engine.shadow())
        && let Ok(json) = serde_json::to_string(shadow)
        && let Err(e) = db.kv_put(SHADOW_KEY, &json, clock.wall_ms()).await
    {
        tracing::warn!(error = %e, "final shadow state save failed");
    }
}

/// Итог повтора сканера.
#[derive(Debug, Clone)]
pub struct ReplaySummary {
    /// Записей прочитано.
    pub records: u64,
    /// Отчётов записано.
    pub reports: usize,
    /// Капитал в конце.
    pub final_equity: Decimal,
    /// Максимальная просадка (доля).
    pub max_drawdown: Decimal,
    /// Позиций открыто за повтор.
    pub opened: usize,
}

/// Повтор сканера и теневого портфеля по записям `[from_ms, to_ms)`; отчёты — в `out_dir`.
pub fn run_replay(
    cfg: &AppConfig,
    raw: &Path,
    from_ms: i64,
    to_ms: i64,
    out_dir: &Path,
) -> anyhow::Result<ReplaySummary> {
    let symbols = SymbolMap::load(Path::new(&cfg.symbols.path)).context("symbols")?;
    let shadow = ShadowPortfolio::new(Usd(cfg.scanner.shadow_capital_usd), from_ms);
    let mut engine = ScanEngine::new(scan_settings(cfg), symbols, Some(shadow), HashMap::new(), Vec::new());
    let interval_ms = i64::try_from(cfg.scanner.interval_secs.max(1)).unwrap_or(60) * 1000;
    let mut next_tick = from_ms + interval_ms;
    let mut replayers = Replayers::new();
    let mut events = Vec::new();
    let mut summary =
        ReplaySummary { records: 0, reports: 0, final_equity: Decimal::ZERO, max_drawdown: Decimal::ZERO, opened: 0 };
    let handle = |out: TickOutput, engine: &ScanEngine, summary: &mut ReplaySummary| -> anyhow::Result<()> {
        summary.opened += out.opened.len();
        if let Some(rep) = &out.report {
            write_report(out_dir, rep, engine.tiers())?;
            summary.reports += 1;
        }
        Ok(())
    };
    for rec in ReplaySource::open(raw, &Exchange::ALL, from_ms, to_ms) {
        summary.records += 1;
        let t = rec.recv.wall_ns.div_euclid(1_000_000);
        while next_tick <= t {
            let out = engine.tick(next_tick);
            handle(out, &engine, &mut summary)?;
            next_tick += interval_ms;
        }
        events.clear();
        replayers.feed(&rec, &mut events);
        for ev in &events {
            engine.on_event(ev);
        }
    }
    // последний такт закрывает сутки, если повтор дошёл до границы
    let out = engine.tick(to_ms);
    handle(out, &engine, &mut summary)?;
    if let Some(shadow) = engine.shadow() {
        summary.final_equity = shadow.equity(engine.market()).0;
        summary.max_drawdown = shadow.max_drawdown;
    }
    write_atomic(
        &out_dir.join("scan-latest.csv"),
        &opportunities_csv(&opportunity_rows(engine.last_evals()), engine.tiers()),
    )?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_rows_roundtrip() {
        let e = LedgerEntry {
            ts_ms: 5,
            pair_id: "p".into(),
            leg: Some("l".into()),
            kind: LedgerKind::Slippage,
            amount: Usd(Decimal::new(-12345, 4)),
        };
        assert_eq!(from_row(&to_row(&e)), Some(e));
        assert!(
            from_row(&LedgerRow { ts_ms: 1, pair_id: "p".into(), leg: None, kind: "x".into(), amount: "1".into() })
                .is_none()
        );
    }

    #[test]
    fn atomic_write_replaces() {
        let dir = std::env::temp_dir().join(format!("sh-runner-{}", std::process::id()));
        let p = dir.join("a/b.csv");
        write_atomic(&p, "1").unwrap();
        write_atomic(&p, "2").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
