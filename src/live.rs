//! Оболочка live (M3): тот же движок, что в paper и backtest, но команды исполняют
//! приватные адаптеры площадок. Каждая команда — отдельная задача tokio с тайм-аутом;
//! её результат и события приватных потоков возвращаются в цикл движка по каналу.
//!
//! Защита: режим включается только при `engine.mode = "live"` и флагах
//! `--live --i-understand-real-money` (проверка в `check_live_gate`). Торговля на
//! площадке — только при `venues.<площадка>.trading_enabled = true`; Aster — с M4.
//! `--dry-run`: исполнение на симуляторе (как paper), а каждая команда дополнительно
//! строится и подписывается адаптером с `dry_run = true` и только журналируется.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use sh_core::{Clock, Exchange, Secrets};
use sh_obs::{Alerts, Level};
use sh_portfolio::{opportunities_csv, opportunity_rows};
use sh_risk::AlertLevel;
use sh_signals::SymbolMap;
use sh_store::{Db, LedgerRow};
use sh_venue_api::{
    CommandResult, ExecCommand, ExecError, ExecEvent, ExecInput, Execution, MdEvent, MdEventKind, MetaRegistry,
    ProxyConfig,
};
use tokio::sync::{mpsc, watch};

use crate::config::AppConfig;
use crate::ctl::{CtlEnvelope, CtlRequest, CtlResponse};
use crate::engine::{Engine, EngineMode, EngineOutput, EngineState, OperatorCommand};
use crate::runner::{ledger_to_row, write_atomic, write_report};
use crate::scan::ScanEngine;
use crate::trade::engine_settings;

/// Ключ состояния live-движка в базе (отдельно от paper).
pub const LIVE_KEY: &str = "engine_state_live";

/// Флаги командной строки live.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveFlags {
    /// `--live`.
    pub live: bool,
    /// `--i-understand-real-money`.
    pub real_money: bool,
    /// `--dry-run`.
    pub dry_run: bool,
}

/// Проверка защиты live: режим из конфига и флаги должны совпадать.
pub fn check_live_gate(mode: &str, f: LiveFlags) -> Result<(), String> {
    match (mode == "live", f.live) {
        (false, false) => {
            if f.dry_run || f.real_money {
                return Err("--dry-run and --i-understand-real-money only apply with --live".into());
            }
            Ok(())
        }
        (false, true) => Err(format!("--live given but engine.mode = \"{mode}\"; set engine.mode = \"live\"")),
        (true, false) => {
            Err("engine.mode = \"live\" requires --live (and --i-understand-real-money or --dry-run)".into())
        }
        (true, true) => {
            if f.dry_run || f.real_money {
                Ok(())
            } else {
                Err("live trading uses real money: add --i-understand-real-money (or --dry-run)".into())
            }
        }
    }
}

/// Дексы Hyperliquid из конфигурации и сопоставления символов.
pub fn hl_dexes(cfg: &AppConfig, symbols: &SymbolMap) -> Vec<String> {
    let mut d: BTreeSet<String> = cfg.execution.hyperliquid_dexes.iter().cloned().collect();
    for u in symbols.underlyings() {
        for m in &u.members {
            if m.venue.exchange == Exchange::Hyperliquid {
                d.insert(m.venue.dex.clone());
            }
        }
    }
    d.into_iter().collect()
}

/// Строит приватные адаптеры торгуемых площадок из секретов.
pub fn build_execs(
    cfg: &AppConfig,
    secrets: &mut Secrets,
    meta: &MetaRegistry,
    dexes: Vec<String>,
    proxy: Option<ProxyConfig>,
    dry_run: bool,
) -> anyhow::Result<BTreeMap<Exchange, Arc<dyn Execution>>> {
    let mut out: BTreeMap<Exchange, Arc<dyn Execution>> = BTreeMap::new();
    if cfg.venues.hyperliquid.trading_enabled {
        let s = secrets
            .hyperliquid
            .take()
            .context("venues.hyperliquid.trading_enabled but [hyperliquid] missing in secrets")?;
        let mut ec = sh_venue_hyperliquid::exec::HlExecConfig::for_network(true);
        ec.dexes = dexes;
        ec.dry_run = dry_run;
        if !cfg.execution.hyperliquid_usdc_token.is_empty() {
            ec.usdc_token.clone_from(&cfg.execution.hyperliquid_usdc_token);
        }
        ec.rest.clone_from(&cfg.venues.hyperliquid.rest);
        out.insert(
            Exchange::Hyperliquid,
            Arc::new(sh_venue_hyperliquid::exec::HlExecution::new(ec, &s, meta.clone(), proxy.clone())?),
        );
    }
    if cfg.venues.lighter.trading_enabled {
        let s = secrets.lighter.take().context("venues.lighter.trading_enabled but [lighter] missing in secrets")?;
        let mut ec =
            sh_venue_lighter::exec::LtExecConfig::for_network(true, cfg.execution.lighter_signer_lib.clone().into());
        ec.dry_run = dry_run;
        ec.rest.clone_from(&cfg.venues.lighter.rest);
        out.insert(Exchange::Lighter, Arc::new(sh_venue_lighter::exec::LtExecution::new(ec, &s, meta.clone(), proxy)?));
    }
    Ok(out)
}

fn exchange_of(cmd: &ExecCommand) -> Exchange {
    match cmd {
        ExecCommand::Place(r) => r.venue.exchange,
        ExecCommand::Cancel { venue, .. } | ExecCommand::Query { venue, .. } => venue.exchange,
        ExecCommand::CancelAll(e) => *e,
    }
}

/// Тайм-аут команды: дольше — исход неизвестен.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

fn to<T>(r: Result<Result<T, ExecError>, tokio::time::error::Elapsed>) -> Result<T, ExecError> {
    r.unwrap_or(Err(ExecError::Timeout))
}

/// Исполняет команду адаптером; результат — в канал движка.
pub fn dispatch(exec: Arc<dyn Execution>, cmd: ExecCommand, tx: mpsc::Sender<ExecInput>) {
    tokio::spawn(async move {
        let res = match cmd {
            ExecCommand::Place(req) => {
                let cid = req.cid;
                let t0 = std::time::Instant::now();
                let ex = req.venue.exchange.as_str();
                let r = to(tokio::time::timeout(COMMAND_TIMEOUT, exec.place(req)).await);
                metrics::histogram!("order_ack_ms", "venue" => ex).record(t0.elapsed().as_secs_f64() * 1000.0);
                CommandResult::Placed(cid, r)
            }
            ExecCommand::Cancel { venue, symbol, cid } => CommandResult::Cancelled(
                cid,
                to(tokio::time::timeout(COMMAND_TIMEOUT, exec.cancel(venue, symbol, cid)).await),
            ),
            ExecCommand::CancelAll(e) => {
                CommandResult::CancelledAll(e, to(tokio::time::timeout(COMMAND_TIMEOUT, exec.cancel_all()).await))
            }
            ExecCommand::Query { venue, symbol, cid } => CommandResult::Queried(
                cid,
                to(tokio::time::timeout(COMMAND_TIMEOUT, exec.query_order(venue, symbol, cid)).await),
            ),
        };
        let _ = tx.send(ExecInput::Result(res)).await;
    });
}

/// Снимки позиций и балансов семейства для сверки.
pub fn snapshot(exec: Arc<dyn Execution>, tx: mpsc::Sender<ExecInput>) {
    tokio::spawn(async move {
        let ex = exec.exchange();
        match tokio::time::timeout(COMMAND_TIMEOUT, exec.positions()).await {
            Ok(Ok(positions)) => {
                let _ = tx.send(ExecInput::Event(ExecEvent::Positions { exchange: ex, positions })).await;
            }
            other => tracing::warn!(venue = %ex, error = ?other.err().map(|_| "timeout"), "positions snapshot failed"),
        }
        if let Ok(Ok(b)) = tokio::time::timeout(COMMAND_TIMEOUT, exec.balances()).await {
            let _ = tx.send(ExecInput::Event(ExecEvent::Balances(b))).await;
        }
    });
}

/// Регистрирует метаданные инструмента для адаптеров.
pub fn register_meta(reg: &MetaRegistry, ev: &MdEvent) {
    if let MdEventKind::Metadata(m) = &ev.kind
        && let Ok(mut g) = reg.write()
    {
        g.insert((m.venue.clone(), m.symbol.clone()), (**m).clone());
    }
}

fn level(l: AlertLevel) -> Level {
    match l {
        AlertLevel::Info => Level::Info,
        AlertLevel::Warn => Level::Warn,
        AlertLevel::Crit => Level::Crit,
    }
}

/// Состояние цикла live.
pub struct LiveState {
    engine: Engine,
    execs: BTreeMap<Exchange, Arc<dyn Execution>>,
    meta: MetaRegistry,
    db: Db,
    report_dir: PathBuf,
    kill_file: PathBuf,
    kill_file_seen: bool,
    last_persist_ms: i64,
}

impl LiveState {
    /// Открывает базу и восстанавливает движок live.
    pub async fn open(
        cfg: &AppConfig,
        execs: BTreeMap<Exchange, Arc<dyn Execution>>,
        meta: MetaRegistry,
        now_ms: i64,
    ) -> anyhow::Result<Self> {
        let symbols = SymbolMap::load(Path::new(&cfg.symbols.path)).context("symbols")?;
        let db = Db::open(Path::new(&cfg.database.path)).await.context("database")?;
        let state = match db.kv_get(LIVE_KEY).await? {
            Some(json) => Some(serde_json::from_str::<EngineState>(&json).context("stored live engine state")?),
            None => None,
        };
        let first_seen = db.first_seen_all().await?;
        let settings = engine_settings(cfg, EngineMode::Live);
        let scan = ScanEngine::new(settings.scan.clone(), symbols, None, first_seen, Vec::new());
        let engine = Engine::new(settings, scan, state, now_ms);
        Ok(Self {
            engine,
            execs,
            meta,
            db,
            report_dir: PathBuf::from(&cfg.scanner.report_dir),
            kill_file: PathBuf::from(&cfg.control.kill_file),
            kill_file_seen: false,
            last_persist_ms: 0,
        })
    }

    fn run_commands(&self, cmds: Vec<ExecCommand>, tx: &mpsc::Sender<ExecInput>) {
        for c in cmds {
            match self.execs.get(&exchange_of(&c)) {
                Some(e) => dispatch(e.clone(), c, tx.clone()),
                None => tracing::error!(cmd = ?c, "no execution adapter for command; trading disabled on venue"),
            }
        }
    }

    async fn handle(
        &mut self,
        out: EngineOutput,
        now_ms: i64,
        alerts: &Alerts,
        tx: &mpsc::Sender<ExecInput>,
    ) -> anyhow::Result<()> {
        self.run_commands(out.commands, tx);
        if out.want_snapshots {
            for e in self.execs.values() {
                snapshot(e.clone(), tx.clone());
            }
        }
        for a in &out.alerts {
            alerts.send(level(a.level), &a.key, &a.text);
        }
        if !out.first_seen.is_empty() {
            self.db.first_seen_insert(&out.first_seen, now_ms).await?;
        }
        let rows: Vec<LedgerRow> = out.ledger.iter().map(ledger_to_row).collect();
        self.db.append_ledger(&rows).await?;
        if out.scanned {
            let scan = self.engine.scan();
            write_atomic(
                &self.report_dir.join("scan-latest.csv"),
                &opportunities_csv(&opportunity_rows(scan.last_evals()), scan.tiers()),
            )?;
        }
        if let Some(rep) = &out.report {
            write_report(&self.report_dir, rep, self.engine.scan().tiers())?;
        }
        if now_ms - self.last_persist_ms >= 2000 || !out.ledger.is_empty() {
            self.persist(now_ms).await?;
        }
        Ok(())
    }

    async fn persist(&mut self, now_ms: i64) -> anyhow::Result<()> {
        self.db.kv_put(LIVE_KEY, &serde_json::to_string(&self.engine.state())?, now_ms).await?;
        self.last_persist_ms = now_ms;
        Ok(())
    }
}

/// Цикл live.
pub async fn live_loop(
    mut md: mpsc::Receiver<MdEvent>,
    mut ctl: mpsc::Receiver<CtlEnvelope>,
    clock: Arc<dyn Clock>,
    mut st: LiveState,
    alerts: Alerts,
    shutdown: watch::Receiver<bool>,
) {
    let (tx, mut rx) = mpsc::channel::<ExecInput>(16_384);
    let (stream_tx, mut stream_rx) = mpsc::channel::<ExecEvent>(16_384);
    for e in st.execs.values() {
        let (e, t, s) = (e.clone(), stream_tx.clone(), shutdown.clone());
        tokio::spawn(async move {
            if let Err(err) = e.run_stream(t, s).await {
                tracing::error!(venue = %e.exchange(), error = %err, "private stream stopped");
            }
        });
    }
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = md.recv() => {
                let Some(ev) = ev else { break };
                register_meta(&st.meta, &ev);
                st.engine.on_md(&ev);
            }
            Some(input) = rx.recv() => st.engine.on_exec(&input, clock.wall_ms()),
            Some(ev) = stream_rx.recv() => st.engine.on_exec(&ExecInput::Event(ev), clock.wall_ms()),
            Some((req, reply)) = ctl.recv() => {
                let now = clock.wall_ms();
                let resp = match req {
                    CtlRequest::Status => CtlResponse { ok: true, body: st.engine.status_json() },
                    CtlRequest::Command(c) => CtlResponse { ok: true, body: st.engine.on_command(&c, now).into() },
                };
                let _ = reply.send(resp);
            }
            _ = tick.tick() => {
                let now = clock.wall_ms();
                if st.kill_file.exists() {
                    if !st.kill_file_seen {
                        st.kill_file_seen = true;
                        let flatten = std::fs::read_to_string(&st.kill_file).map(|s| s.trim() == "flatten").unwrap_or(false);
                        st.engine.on_command(&OperatorCommand::Kill { flatten, source: "kill file".into() }, now);
                    }
                } else {
                    st.kill_file_seen = false;
                }
                let out = st.engine.tick(now);
                if let Err(e) = st.handle(out, now, &alerts, &tx).await {
                    alerts.send(Level::Warn, "live_persist", &format!("live persistence failed: {e}"));
                }
            }
        }
    }
    // SIGTERM: новые входы останавливаются, мейкер-ордера снимаются, позиции остаются
    let now = clock.wall_ms();
    st.engine.on_command(&OperatorCommand::PauseEntries { paused: true }, now);
    let cancels: Vec<ExecCommand> = st
        .engine
        .tracker()
        .live()
        .filter(|o| o.req.tif == sh_venue_api::Tif::PostOnly)
        .map(|o| ExecCommand::Cancel { venue: o.req.venue.clone(), symbol: o.req.symbol.clone(), cid: o.req.cid })
        .collect();
    let n = cancels.len();
    st.run_commands(cancels, &tx);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut done = 0;
    while done < n && tokio::time::Instant::now() < deadline {
        if let Ok(Some(input)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            st.engine.on_exec(&input, clock.wall_ms());
            done += 1;
        }
    }
    if let Err(e) = st.persist(now).await {
        tracing::warn!(error = %e, "final live state save failed");
    }
    tracing::info!(cancelled = n, "live loop stopped");
}

/// Набор адаптеров для `--dry-run`: подписывают и журналируют команды, не отправляя.
pub type DryRunExecs = BTreeMap<Exchange, Arc<dyn Execution>>;

/// Отправляет команды paper-движка в адаптеры dry-run (только подпись и журнал).
pub fn dry_run_sign(execs: &DryRunExecs, cmds: &[ExecCommand]) {
    for c in cmds {
        if let (ExecCommand::Place(_) | ExecCommand::Cancel { .. }, Some(e)) = (c, execs.get(&exchange_of(c))) {
            let (e, c) = (e.clone(), c.clone());
            tokio::spawn(async move {
                let r = match c {
                    ExecCommand::Place(req) => e.place(req).await.map(|_| ()),
                    ExecCommand::Cancel { venue, symbol, cid } => e.cancel(venue, symbol, cid).await,
                    _ => Ok(()),
                };
                match r {
                    Err(ExecError::NotSupported(_)) => {}
                    Err(err) => tracing::warn!(error = %err, "dry run: building or signing failed"),
                    Ok(()) => tracing::error!("dry run adapter sent a request; this must not happen"),
                }
            });
        }
    }
}

/// Пустой реестр метаданных.
pub fn new_registry() -> MetaRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_gate() {
        let f = |live, real_money, dry_run| LiveFlags { live, real_money, dry_run };
        assert!(check_live_gate("paper", f(false, false, false)).is_ok());
        assert!(check_live_gate("paper", f(true, true, false)).is_err());
        assert!(check_live_gate("live", f(false, false, false)).is_err());
        assert!(check_live_gate("live", f(true, false, false)).is_err());
        assert!(check_live_gate("live", f(true, true, false)).is_ok());
        assert!(check_live_gate("live", f(true, false, true)).is_ok());
        assert!(check_live_gate("scan", f(false, false, true)).is_err());
    }
}
