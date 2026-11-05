//! Торговый цикл paper (M2; live — M3): живые рыночные данные, движок, симулятор
//! исполнения, сохранение состояния в SQLite, журнал, оповещения, `spread-ctl`,
//! файл kill switch и команды Telegram.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rust_decimal::Decimal;
use sh_core::{Clock, Exchange, Usd, VenueKey};
use sh_obs::{Alerts, Level};
use sh_portfolio::{LedgerEntry, opportunities_csv, opportunity_rows};
use sh_risk::{AlertLevel, StageCaps};
use sh_signals::SymbolMap;
use sh_sim::{SimCore, SimParams, SimVenueParams};
use sh_store::{Db, LedgerRow};
use sh_venue_api::MdEvent;
use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::ctl::{CtlEnvelope, CtlRequest, CtlResponse};
use crate::engine::{Engine, EngineMode, EngineOutput, EngineSettings, EngineState, OperatorCommand};
use crate::runner::{scan_settings, write_atomic, write_report};
use crate::scan::ScanEngine;
use crate::simdrive::SimDriver;

/// Ключ состояния движка в базе.
pub const ENGINE_KEY: &str = "engine_state";

/// Параметры симулятора из конфигурации.
pub fn sim_params(cfg: &AppConfig) -> SimParams {
    let premium = cfg.fees.lighter_account_tier == "premium";
    let venues = Exchange::ALL
        .iter()
        .map(|e| {
            let mut p = SimVenueParams::default_for(*e, premium);
            if let Some(r) = cfg.paper.rtt_ms.get(e.as_str()) {
                p.rtt_ms = *r;
            }
            (*e, p)
        })
        .collect();
    let initial_cash = cfg
        .paper
        .cash_usd
        .iter()
        .filter_map(|(k, v)| k.parse::<Exchange>().ok().map(|e| (VenueKey::new(e), Usd(*v))))
        .collect();
    SimParams { venues, fees: cfg.fees.clone(), initial_cash, leverage: cfg.risk.max_leverage_leg }
}

/// Настройки движка; в paper и backtest торговля разрешена на всех площадках симулятора.
pub fn engine_settings(cfg: &AppConfig, mode: EngineMode) -> EngineSettings {
    let trading_enabled: BTreeMap<Exchange, bool> = match mode {
        EngineMode::Paper | EngineMode::Backtest => Exchange::ALL.iter().map(|e| (*e, true)).collect(),
        EngineMode::Live => BTreeMap::from([
            (Exchange::Hyperliquid, cfg.venues.hyperliquid.trading_enabled),
            (Exchange::Lighter, cfg.venues.lighter.trading_enabled),
            // Aster в live — только с M4
            (Exchange::Aster, false),
        ]),
    };
    EngineSettings {
        scan: scan_settings(cfg),
        mode,
        trading_enabled,
        // ступень капитала действует в live; в paper и backtest — без ограничений ступени
        stage: match mode {
            EngineMode::Live => {
                StageCaps { max_capital_usd: Some(cfg.stage.max_capital_usd), max_pairs: Some(cfg.stage.max_pairs) }
            }
            _ => StageCaps::default(),
        },
        reconcile_tol: Decimal::new(1, 9),
    }
}

fn level(l: AlertLevel) -> Level {
    match l {
        AlertLevel::Info => Level::Info,
        AlertLevel::Warn => Level::Warn,
        AlertLevel::Crit => Level::Crit,
    }
}

fn ledger_row(e: &LedgerEntry) -> LedgerRow {
    crate::runner::ledger_to_row(e)
}

/// Состояние торгового цикла paper.
pub struct PaperState {
    driver: SimDriver,
    db: Db,
    report_dir: PathBuf,
    kill_file: PathBuf,
    kill_file_seen: bool,
    last_persist_ms: i64,
    key: &'static str,
    dry_run: Option<crate::live::DryRunExecs>,
}

impl PaperState {
    /// `live --dry-run`: те же данные и симулятор, отдельное состояние, а каждая команда
    /// подписывается приватным адаптером без отправки.
    pub async fn open_dry_run(cfg: &AppConfig, now_ms: i64, execs: crate::live::DryRunExecs) -> anyhow::Result<Self> {
        let mut s = Self::open_with_key(cfg, now_ms, "engine_state_dry_run").await?;
        s.dry_run = Some(execs);
        Ok(s)
    }

    /// Открывает базу, восстанавливает движок и симулятор.
    pub async fn open(cfg: &AppConfig, now_ms: i64) -> anyhow::Result<Self> {
        Self::open_with_key(cfg, now_ms, ENGINE_KEY).await
    }

    async fn open_with_key(cfg: &AppConfig, now_ms: i64, key: &'static str) -> anyhow::Result<Self> {
        let symbols = SymbolMap::load(Path::new(&cfg.symbols.path)).context("symbols")?;
        let db = Db::open(Path::new(&cfg.database.path)).await.context("database")?;
        let state = match db.kv_get(key).await? {
            Some(json) => Some(serde_json::from_str::<EngineState>(&json).context("stored engine state")?),
            None => None,
        };
        let restored = state.is_some();
        let first_seen = db.first_seen_all().await?;
        let settings = engine_settings(cfg, EngineMode::Paper);
        let scan = ScanEngine::new(settings.scan.clone(), symbols, None, first_seen, Vec::new());
        let engine = Engine::new(settings, scan, state, now_ms);
        let mut driver = SimDriver::new(engine, SimCore::new(sim_params(cfg)), cfg.paper.tick_ms);
        if restored {
            // позиции симулятора восстанавливаются из книги: сверка после старта их подтвердит
            driver.seed_from_book();
        }
        tracing::info!(restored, "paper engine opened");
        Ok(Self {
            driver,
            db,
            report_dir: PathBuf::from(&cfg.scanner.report_dir),
            kill_file: PathBuf::from(&cfg.control.kill_file),
            kill_file_seen: false,
            last_persist_ms: 0,
            key,
            dry_run: None,
        })
    }

    async fn handle(&mut self, out: EngineOutput, now_ms: i64, alerts: &Alerts) -> anyhow::Result<()> {
        if let Some(d) = &self.dry_run {
            crate::live::dry_run_sign(d, &out.commands);
        }
        for a in &out.alerts {
            alerts.send(level(a.level), &a.key, &a.text);
        }
        if !out.first_seen.is_empty() {
            self.db.first_seen_insert(&out.first_seen, now_ms).await?;
        }
        let rows: Vec<LedgerRow> = out.ledger.iter().map(ledger_row).collect();
        self.db.append_ledger(&rows).await?;
        if out.scanned {
            let scan = self.driver.engine.scan();
            let rows = opportunity_rows(scan.last_evals());
            write_atomic(&self.report_dir.join("scan-latest.csv"), &opportunities_csv(&rows, scan.tiers()))?;
        }
        if let Some(rep) = &out.report {
            write_report(&self.report_dir, rep, self.driver.engine.scan().tiers())?;
        }
        if now_ms - self.last_persist_ms >= 5000 || !out.ledger.is_empty() {
            self.persist(now_ms).await?;
        }
        Ok(())
    }

    /// Сохраняет состояние движка.
    pub async fn persist(&mut self, now_ms: i64) -> anyhow::Result<()> {
        let json = serde_json::to_string(&self.driver.engine.state())?;
        self.db.kv_put(self.key, &json, now_ms).await?;
        self.last_persist_ms = now_ms;
        Ok(())
    }

    fn check_kill_file(&mut self, now_ms: i64) {
        if self.kill_file.exists() {
            if !self.kill_file_seen {
                self.kill_file_seen = true;
                let flatten = std::fs::read_to_string(&self.kill_file).map(|s| s.trim() == "flatten").unwrap_or(false);
                let reply = self.driver.command(&OperatorCommand::Kill { flatten, source: "kill file".into() }, now_ms);
                tracing::warn!(%reply, "kill file detected");
            }
        } else {
            self.kill_file_seen = false;
        }
    }

    fn on_ctl(&mut self, req: CtlRequest, now_ms: i64) -> CtlResponse {
        match req {
            CtlRequest::Status => CtlResponse { ok: true, body: self.driver.engine.status_json() },
            CtlRequest::Command(c) => {
                let msg = self.driver.command(&c, now_ms);
                CtlResponse { ok: true, body: msg.into() }
            }
        }
    }
}

/// Цикл paper: события рынка, такт движка, команды управления.
pub async fn paper_loop(
    mut rx: mpsc::Receiver<MdEvent>,
    mut ctl: mpsc::Receiver<CtlEnvelope>,
    clock: Arc<dyn Clock>,
    mut state: PaperState,
    alerts: Alerts,
) {
    let tick_ms = u64::try_from(state.driver.tick_every_ms).unwrap_or(1000);
    let mut tick = tokio::time::interval(Duration::from_millis(tick_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                state.driver.on_md(&ev);
            }
            Some((req, reply)) = ctl.recv() => {
                let now = clock.wall_ms();
                let resp = state.on_ctl(req, now);
                let _ = reply.send(resp);
                let out = state.driver.take_output();
                if let Err(e) = state.handle(out, now, &alerts).await {
                    alerts.send(Level::Warn, "paper_persist", &format!("paper persistence failed: {e}"));
                }
            }
            _ = tick.tick() => {
                let now = clock.wall_ms();
                state.check_kill_file(now);
                let mut out = state.driver.take_output();
                let o = state.driver.tick(now);
                out.commands.extend(o.commands);
                out.alerts.extend(o.alerts);
                out.ledger.extend(o.ledger);
                out.scanned |= o.scanned;
                out.first_seen.extend(o.first_seen);
                if o.report.is_some() {
                    out.report = o.report;
                }
                if let Err(e) = state.handle(out, now, &alerts).await {
                    alerts.send(Level::Warn, "paper_persist", &format!("paper persistence failed: {e}"));
                }
            }
        }
    }
    // SIGTERM: новые ордера не ставятся, мейкер-ордера снимаются, позиции остаются
    let now = clock.wall_ms();
    let reply = state.driver.command(&OperatorCommand::PauseEntries { paused: true }, now);
    tracing::info!(%reply, "shutdown: entries paused");
    if let Err(e) = state.persist(now).await {
        tracing::warn!(error = %e, "final engine state save failed");
    }
}
