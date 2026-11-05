//! Движок исполнения (M2): один синхронный цикл, владеющий всем изменяемым состоянием
//! (SPEC.md, «Concurrency model»). Входы — события рынка, ответы и события исполнения,
//! такты времени и команды оператора; выходы — команды площадкам, оповещения и записи
//! журнала. Движок не делает I/O и не читает часы, поэтому одинаково работает в paper,
//! backtest и live: меняется только оболочка, исполняющая команды.

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use sh_core::{Exchange, Px, Qty, Side, Usd, VenueKey};
use sh_exec::{JobAction, JobId, JobKind, JobLeg, JobParams, JobState, OrderTracker, PairJob, TrackerEvent};
use sh_md::{InstrumentKey, MarketState};
use sh_portfolio::{Book, LedgerEntry, Lifecycle, PairPosition};
use sh_risk::{AlertLevel, BookView, EntryIntent, LegExposure, PairExposure, RiskAction, RiskState, StageCaps, Veto};
use sh_signals::{Pair, PairEval, PairKind, SizeBudget, capital_for, pct};
use sh_strategy::{CarryStrategy, HeldPair, Intent, Strategy};
use sh_venue_api::{
    ClientId, ClientIdGen, CommandResult, ExecCommand, ExecEvent, ExecInput, InstrumentStatus, MdEvent, MdEventKind,
    OrderRequest, Rounding, Tif, VenuePosition,
};

use crate::scan::{ScanEngine, ScanSettings};

/// Режим движка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineMode {
    /// Живые данные, симулированное исполнение.
    Paper,
    /// Повтор записей, симулированное исполнение.
    Backtest,
    /// Реальные ордера.
    Live,
}

/// Настройки движка.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    /// Сканер, стратегии, издержки, комиссии, риск.
    pub scan: ScanSettings,
    /// Режим.
    pub mode: EngineMode,
    /// Торговля разрешена на семействе (`venues.*.trading_enabled`; в paper и backtest — все).
    pub trading_enabled: BTreeMap<Exchange, bool>,
    /// Ограничения ступени (M3/M4).
    pub stage: StageCaps,
    /// Допуск сверки позиций (в количестве).
    pub reconcile_tol: Decimal,
}

/// Команда оператора (spread-ctl, файл `run/KILL`, Telegram).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum OperatorCommand {
    /// Kill switch: отмена всех ордеров, запрет новых; `flatten` — закрыть все позиции.
    Kill {
        /// Закрыть позиции.
        flatten: bool,
        /// Источник (ctl, file, telegram).
        source: String,
    },
    /// Снять kill switch.
    ReleaseKill,
    /// Снять Halted (после разбора расхождения).
    Resume,
    /// Принять позиции площадки как истину (после ручного разбора).
    AdoptPositions {
        /// Семейство.
        exchange: Exchange,
    },
    /// Приостановить или возобновить новые входы.
    PauseEntries {
        /// Пауза.
        paused: bool,
    },
    /// Выйти из пары.
    ExitPair {
        /// Пара.
        pair_id: String,
    },
}

/// Оповещение движка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineAlert {
    /// Уровень.
    pub level: AlertLevel,
    /// Ключ дедупликации.
    pub key: String,
    /// Текст.
    pub text: String,
}

/// Выход такта.
#[derive(Debug, Default)]
pub struct EngineOutput {
    /// Команды площадкам.
    pub commands: Vec<ExecCommand>,
    /// Оповещения.
    pub alerts: Vec<EngineAlert>,
    /// Записи журнала.
    pub ledger: Vec<LedgerEntry>,
    /// Оценка пар выполнена.
    pub scanned: bool,
    /// Отчёт сканера за прошедшие сутки.
    pub report: Option<sh_portfolio::DailyReport>,
    /// Впервые увиденные инструменты.
    pub first_seen: Vec<String>,
    /// Нужны снимки позиций и балансов (сверка).
    pub want_snapshots: bool,
    /// Заполнения (наши и чужие) с парой — для учёта и экспорта.
    pub fills: Vec<(sh_venue_api::Fill, Option<String>)>,
}

/// Сохраняемое состояние движка.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineState {
    /// Книга.
    pub book: Book,
    /// Ордера.
    pub tracker: OrderTracker,
    /// Задачи.
    pub jobs: BTreeMap<JobId, PairJob>,
    /// Риск (без лимитов).
    pub risk: RiskState,
    /// Следующий номер задачи.
    pub next_job: JobId,
    /// Новые входы приостановлены оператором.
    pub paused: bool,
    /// Время последнего CRIT-оповещения (ступени капитала: «14 чистых дней»).
    #[serde(default)]
    pub last_crit_ms: Option<i64>,
    /// Начало отсчёта чистых дней (первый запуск).
    #[serde(default)]
    pub started_ms: Option<i64>,
}

/// Движок.
pub struct Engine {
    settings: EngineSettings,
    scan: ScanEngine,
    tracker: OrderTracker,
    jobs: BTreeMap<JobId, PairJob>,
    book: Book,
    risk: RiskState,
    strategies: Vec<Box<dyn Strategy>>,
    ids: ClientIdGen,
    next_job: JobId,
    paused: bool,
    /// Время последнего события рынка по площадке.
    last_md_ms: BTreeMap<VenueKey, i64>,
    /// Последние снимки позиций площадок (для цены ликвидации).
    venue_positions: BTreeMap<Exchange, Vec<VenuePosition>>,
    /// Семейства, прошедшие стартовую сверку.
    reconciled: BTreeSet<Exchange>,
    last_snapshot_ms: Option<i64>,
    /// Приватный поток семейства недоступен.
    stream_down: BTreeSet<Exchange>,
    out: EngineOutput,
    now_ms: i64,
    last_crit_ms: Option<i64>,
    started_ms: i64,
}

fn alert(level: AlertLevel, key: impl Into<String>, text: impl Into<String>) -> EngineAlert {
    EngineAlert { level, key: key.into(), text: text.into() }
}

fn opposite(s: Side) -> Side {
    match s {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

impl Engine {
    /// Новый движок. `state` — восстановленное состояние или `None`.
    pub fn new(settings: EngineSettings, scan: ScanEngine, state: Option<EngineState>, now_ms: i64) -> Self {
        let limits = settings.scan.risk.clone();
        let strat = &settings.scan.strategy;
        let strategies: Vec<Box<dyn Strategy>> = vec![
            Box::new(CarryStrategy::new(PairKind::S1, strat.s1_rules(), strat.s1_funding_carry.enabled)),
            Box::new(CarryStrategy::new(PairKind::S2, strat.s2_rules(), strat.s2_cross_dex.enabled)),
        ];
        let (book, mut tracker, jobs, mut risk, next_job, paused, last_crit_ms, started_ms) = match state {
            Some(s) => (
                s.book,
                s.tracker,
                s.jobs,
                s.risk,
                s.next_job,
                s.paused,
                s.last_crit_ms,
                s.started_ms.unwrap_or(now_ms),
            ),
            None => (
                Book::new(),
                OrderTracker::new(),
                BTreeMap::new(),
                RiskState::new(limits.clone()),
                1,
                false,
                None,
                now_ms,
            ),
        };
        // ордера, живые на момент остановки, уточняются запросом по клиентскому ID
        tracker.mark_live_unknown();
        risk.limits = limits;
        risk.trading_enabled.clone_from(&settings.trading_enabled);
        risk.stage = settings.stage.clone();
        Self {
            settings,
            scan,
            tracker,
            jobs,
            book,
            risk,
            strategies,
            ids: ClientIdGen::new(now_ms),
            next_job,
            paused,
            last_md_ms: BTreeMap::new(),
            venue_positions: BTreeMap::new(),
            reconciled: BTreeSet::new(),
            last_snapshot_ms: None,
            stream_down: BTreeSet::new(),
            out: EngineOutput::default(),
            now_ms,
            last_crit_ms,
            started_ms,
        }
    }

    /// Полных суток без CRIT-оповещений (с первого запуска или последнего CRIT).
    pub fn clean_days(&self) -> i64 {
        (self.now_ms - self.last_crit_ms.unwrap_or(self.started_ms)).max(0) / 86_400_000
    }

    /// Состояние для сохранения.
    pub fn state(&self) -> EngineState {
        EngineState {
            book: self.book.clone(),
            tracker: self.tracker.clone(),
            jobs: self.jobs.clone(),
            risk: self.risk.clone(),
            next_job: self.next_job,
            paused: self.paused,
            last_crit_ms: self.last_crit_ms,
            started_ms: Some(self.started_ms),
        }
    }

    /// Книга.
    pub fn book(&self) -> &Book {
        &self.book
    }

    /// Риск.
    pub fn risk(&self) -> &RiskState {
        &self.risk
    }

    /// Трекер ордеров.
    pub fn tracker(&self) -> &OrderTracker {
        &self.tracker
    }

    /// Задачи.
    pub fn jobs(&self) -> &BTreeMap<JobId, PairJob> {
        &self.jobs
    }

    /// Состояние рынка.
    pub fn market(&self) -> &MarketState {
        self.scan.market()
    }

    /// Сканер.
    pub fn scan(&self) -> &ScanEngine {
        &self.scan
    }

    /// Семейства площадок, участвующие в торговле.
    fn active_exchanges(&self) -> Vec<Exchange> {
        self.settings.trading_enabled.iter().filter(|(_, on)| **on).map(|(e, _)| *e).collect()
    }

    /// Старт разрешён: все торгуемые семейства прошли сверку.
    pub fn startup_complete(&self) -> bool {
        self.active_exchanges().iter().all(|e| self.reconciled.contains(e))
    }

    fn cid(&self) -> ClientId {
        self.ids.next(self.now_ms)
    }

    // ---------------------------------------------------------------- рынок

    /// Событие рынка.
    pub fn on_md(&mut self, ev: &MdEvent) {
        let ts = ev.recv.wall_ns.div_euclid(1_000_000);
        self.last_md_ms.insert(ev.venue.clone(), ts);
        if let MdEventKind::Metadata(m) = &ev.kind {
            let key = InstrumentKey::new(m.venue.clone(), m.symbol.clone());
            let prev = self.scan.market().get(&key).and_then(|s| s.meta.clone());
            if let (Some(prev), Some(pair)) = (prev, self.book.pair_of(&key).map(|p| p.pair_id.clone())) {
                // действия деплоера HIP-3 и делистинг: пара уходит в выход
                let changed = prev.status != m.status
                    || prev.max_leverage != m.max_leverage
                    || prev.margin_mode != m.margin_mode
                    || prev.lot_size != m.lot_size;
                if changed {
                    self.out.alerts.push(alert(
                        AlertLevel::Warn,
                        format!("meta:{}", key.id()),
                        format!(
                            "metadata change on held {}: status {:?}, max leverage {:?}",
                            key.id(),
                            m.status,
                            m.max_leverage
                        ),
                    ));
                    if m.status != InstrumentStatus::Active || m.max_leverage < prev.max_leverage {
                        self.start_exit(&pair, "metadata change", false);
                    }
                }
            }
        }
        self.scan.on_event(ev);
    }

    // ------------------------------------------------------------ исполнение

    /// Ответ или событие исполнения.
    pub fn on_exec(&mut self, input: &ExecInput, now_ms: i64) {
        self.now_ms = self.now_ms.max(now_ms);
        match input {
            ExecInput::Event(ExecEvent::Funding(fp)) => {
                let e = self.book.on_funding(fp);
                metrics::counter!("funding_events_total", "venue" => fp.venue.exchange.as_str()).increment(1);
                self.out.ledger.extend(e);
                return;
            }
            ExecInput::Event(ExecEvent::Positions { exchange, positions }) => {
                self.on_positions(*exchange, positions);
                return;
            }
            ExecInput::Event(ExecEvent::Balances(b)) => {
                self.book.on_balances(b, now_ms);
                return;
            }
            ExecInput::Event(ExecEvent::StreamDown(ex)) => {
                self.stream_down.insert(*ex);
                return;
            }
            ExecInput::Event(ExecEvent::StreamUp(ex)) => {
                self.stream_down.remove(ex);
                return;
            }
            ExecInput::Result(CommandResult::Placed(_, Err(e))) => {
                metrics::counter!("order_errors_total", "kind" => if e.outcome_unknown() { "timeout" } else { "error" })
                    .increment(1);
            }
            _ => {}
        }
        for ev in self.tracker.on_input(input, now_ms) {
            match ev {
                TrackerEvent::Filled { job, fill } => {
                    let pair = job.and_then(|j| self.jobs.get(&j)).map(|j| j.pair_id.clone());
                    if let (Some(j), Some(c)) = (job.and_then(|j| self.jobs.get_mut(&j)), fill.cid) {
                        j.on_fill(c, fill.qty.0);
                    }
                    let mid = self
                        .scan
                        .market()
                        .get(&InstrumentKey::new(fill.venue.clone(), fill.symbol.clone()))
                        .and_then(|s| s.mid());
                    let e = self.book.on_fill(&fill, pair.as_deref(), mid);
                    self.out.fills.push((fill.clone(), pair.clone()));
                    metrics::counter!("fills_total", "venue" => fill.venue.exchange.as_str()).increment(1);
                    metrics::counter!("fees_usd", "venue" => fill.venue.exchange.as_str())
                        .increment(fill.fee.0.abs().to_u64().unwrap_or(0));
                    self.out.ledger.extend(e);
                }
                TrackerEvent::Closed { job, cid, status } => {
                    metrics::counter!("orders_total", "status" => format!("{status:?}")).increment(1);
                    if let Some(j) = job.and_then(|j| self.jobs.get_mut(&j)) {
                        j.on_closed(cid, status);
                    }
                }
                TrackerEvent::ForeignFill(f) => self.on_foreign_fill(&f),
                TrackerEvent::Ignored { cid, why } => {
                    tracing::debug!(cid = cid.0, %why, "order update ignored");
                }
            }
        }
    }

    /// Заполнение не по нашему ордеру: ликвидация или ADL. Вторая нога сокращается сразу.
    fn on_foreign_fill(&mut self, f: &sh_venue_api::Fill) {
        let key = InstrumentKey::new(f.venue.clone(), f.symbol.clone());
        let mid = self.scan.market().get(&key).and_then(|s| s.mid());
        let e = self.book.on_fill(f, None, mid);
        self.out.ledger.extend(e);
        let pair = self.book.pair_of(&key).map(|p| p.pair_id.clone());
        self.out.fills.push((f.clone(), pair.clone()));
        self.out.alerts.push(alert(
            AlertLevel::Crit,
            format!("adl:{}", key.id()),
            format!(
                "position change not caused by our orders on {} ({:?} {}): treating as ADL/liquidation",
                key.id(),
                f.side,
                f.qty.0
            ),
        ));
        if let Some(pid) = pair {
            self.start_exit(&pid, "ADL or liquidation on a leg", true);
        }
    }

    fn on_positions(&mut self, exchange: Exchange, positions: &[VenuePosition]) {
        self.venue_positions.insert(exchange, positions.to_vec());
        // инструменты с живыми ордерами не сверяем: позиция ещё меняется
        let skip: Vec<String> = self
            .tracker
            .live()
            .filter(|o| o.req.venue.exchange == exchange)
            .map(|o| InstrumentKey::new(o.req.venue.clone(), o.req.symbol.clone()).id())
            .collect();
        let mism = self.book.reconcile(exchange, positions, &skip, self.settings.reconcile_tol);
        if mism.is_empty() {
            if self.reconciled.insert(exchange) {
                tracing::info!(venue = %exchange, "startup reconciliation passed");
            }
            return;
        }
        let text =
            mism.iter().map(|m| format!("{}: book {} venue {}", m.key, m.book, m.venue)).collect::<Vec<_>>().join("; ");
        self.out.alerts.push(alert(
            AlertLevel::Crit,
            format!("reconcile:{exchange}"),
            format!("reconciliation mismatch on {exchange}: {text}"),
        ));
        self.halt(&format!("reconciliation mismatch on {exchange}"));
    }

    fn halt(&mut self, reason: &str) {
        let acts = self.risk.halt(reason);
        self.apply_risk_actions(acts);
        let ids: Vec<String> =
            self.book.pairs.values().filter(|p| p.state != Lifecycle::Closed).map(|p| p.pair_id.clone()).collect();
        for id in ids {
            self.book.set_state(&id, Lifecycle::Halted, self.now_ms);
        }
        for j in self.jobs.values_mut() {
            j.abort();
        }
    }

    // ------------------------------------------------------------ оператор

    /// Команда оператора.
    pub fn on_command(&mut self, cmd: &OperatorCommand, now_ms: i64) -> String {
        self.now_ms = self.now_ms.max(now_ms);
        match cmd {
            OperatorCommand::Kill { flatten, source } => {
                let acts =
                    self.risk.engage_kill(format!("{source}{}", if *flatten { " (flatten)" } else { "" }), now_ms);
                self.apply_risk_actions(acts);
                for j in self.jobs.values_mut() {
                    if *flatten {
                        j.abort();
                    } else {
                        j.wind_down();
                    }
                }
                if *flatten {
                    let ids: Vec<String> =
                        self.book.pairs.values().filter(|p| !p.is_flat()).map(|p| p.pair_id.clone()).collect();
                    for id in ids {
                        self.start_exit(&id, "kill --flatten", true);
                    }
                }
                format!("kill switch engaged (flatten = {flatten})")
            }
            OperatorCommand::ReleaseKill => {
                self.risk.release_kill();
                self.out.alerts.push(alert(AlertLevel::Warn, "kill_release", "kill switch released by operator"));
                "kill switch released".into()
            }
            OperatorCommand::Resume => {
                self.risk.resume();
                let ids: Vec<String> = self
                    .book
                    .pairs
                    .values()
                    .filter(|p| p.state == Lifecycle::Halted)
                    .map(|p| p.pair_id.clone())
                    .collect();
                for id in ids {
                    let st = if self.book.pairs.get(&id).is_some_and(PairPosition::is_flat) {
                        Lifecycle::Closed
                    } else {
                        Lifecycle::Hedged
                    };
                    self.book.set_state(&id, st, now_ms);
                }
                self.out.alerts.push(alert(AlertLevel::Warn, "resume", "halted state cleared by operator"));
                "resumed".into()
            }
            OperatorCommand::AdoptPositions { exchange } => {
                let pos = self.venue_positions.get(exchange).cloned().unwrap_or_default();
                self.book.adopt_venue_positions(*exchange, &pos);
                format!("adopted {} venue positions on {exchange}", pos.len())
            }
            OperatorCommand::PauseEntries { paused } => {
                self.paused = *paused;
                format!("entries paused = {paused}")
            }
            OperatorCommand::ExitPair { pair_id } => {
                if self.book.pairs.contains_key(pair_id) {
                    self.start_exit(pair_id, "operator", false);
                    format!("exit started for {pair_id}")
                } else {
                    format!("unknown pair {pair_id}")
                }
            }
        }
    }

    fn apply_risk_actions(&mut self, acts: Vec<RiskAction>) {
        for a in acts {
            match a {
                RiskAction::Alert { level, key, text } => self.out.alerts.push(EngineAlert { level, key, text }),
                RiskAction::CancelAll => {
                    for ex in Exchange::ALL {
                        if self.tracker.live().any(|o| o.req.venue.exchange == ex) {
                            self.out.commands.push(ExecCommand::CancelAll(ex));
                        }
                    }
                }
                RiskAction::ExitAll(reason) => {
                    let ids: Vec<String> =
                        self.book.pairs.values().filter(|p| !p.is_flat()).map(|p| p.pair_id.clone()).collect();
                    for id in ids {
                        self.start_exit(&id, &reason, false);
                    }
                }
                RiskAction::ReducePair { pair_id, reason } => self.start_exit(&pair_id, &reason, false),
                RiskAction::HedgeDelta(_) => {
                    // избыточная дельта вне задач — это остатки ног: закрываем их reduce-only
                    self.close_orphans();
                }
            }
        }
    }

    // --------------------------------------------------------------- такт

    /// Такт движка.
    pub fn tick(&mut self, now_ms: i64) -> EngineOutput {
        self.now_ms = self.now_ms.max(now_ms);
        let t = self.scan.tick(now_ms);
        self.out.scanned = t.scanned;
        self.out.report = t.report;
        self.out.first_seen = t.first_seen;
        self.out.ledger.extend(t.ledger.into_iter().map(|mut e| {
            e.pair_id = format!("shadow:{}", e.pair_id);
            e
        }));

        self.check_staleness(now_ms);

        // сверка: снимки позиций по расписанию
        let every = i64::try_from(self.settings.scan.risk.reconcile_every_s.max(1)).unwrap_or(10) * 1000;
        if self.last_snapshot_ms.is_none_or(|t| now_ms - t >= every) {
            self.last_snapshot_ms = Some(now_ms);
            self.out.want_snapshots = true;
        }

        if t.scanned && self.startup_complete() {
            self.run_strategies(now_ms);
        }

        let view = self.book_view();
        let acts = self.risk.monitor(&view, now_ms);
        self.apply_risk_actions(acts);
        metrics::gauge!("unhedged_usd").set(view.unhedged_usd.0.to_f64().unwrap_or(0.0));
        metrics::gauge!("net_delta_usd").set(view.net_delta_usd().to_f64().unwrap_or(0.0));
        metrics::gauge!("equity_usd_total").set(view.equity.0.to_f64().unwrap_or(0.0));

        self.step_jobs(now_ms);
        for c in self.tracker.tick(now_ms) {
            self.out.commands.push(c);
        }
        self.tracker.prune(now_ms, 86_400_000);
        self.book.prune(now_ms, 7 * 86_400_000);
        metrics::gauge!("orders_unknown").set(self.tracker.unknown_count() as f64);
        if self.out.alerts.iter().any(|a| a.level == AlertLevel::Crit) {
            self.last_crit_ms = Some(now_ms);
        }
        metrics::gauge!("clean_days").set(self.clean_days() as f64);
        std::mem::take(&mut self.out)
    }

    fn check_staleness(&mut self, now_ms: i64) {
        let stale = i64::try_from(self.settings.scan.risk.md_stale_ms).unwrap_or(2000);
        // площадки, на которых есть позиции или задачи
        let mut venues: BTreeSet<VenueKey> = BTreeSet::new();
        for p in self.book.pairs.values().filter(|p| p.state != Lifecycle::Closed) {
            for l in &p.legs {
                venues.insert(l.key.venue.clone());
            }
        }
        for j in self.jobs.values() {
            for l in &j.legs {
                venues.insert(l.venue.clone());
            }
        }
        for v in venues {
            let has_pos = self.book.positions().iter().any(|(k, _)| k.venue == v);
            let reason = match self.last_md_ms.get(&v) {
                None => Some("no market data".to_owned()),
                Some(t) if now_ms - t > stale => Some(format!("market data stale for {} ms", now_ms - t)),
                _ if self.stream_down.contains(&v.exchange) => Some("private stream down".to_owned()),
                _ => None,
            };
            let acts = self.risk.set_degraded(&v, reason, has_pos);
            self.apply_risk_actions(acts);
        }
    }

    fn held_pairs(&self) -> Vec<HeldPair> {
        self.book
            .pairs
            .values()
            .filter(|p| p.state != Lifecycle::Closed)
            .map(|p| HeldPair {
                pair_id: p.pair_id.clone(),
                kind: p.kind,
                short: p.legs[0].key.clone(),
                long: p.legs[1].key.clone(),
            })
            .collect()
    }

    fn run_strategies(&mut self, now_ms: i64) {
        let held = self.held_pairs();
        let evals: Vec<PairEval> = self.scan.last_evals().to_vec();
        let mut intents = Vec::new();
        for s in &mut self.strategies {
            intents.extend(s.on_scan(&evals, &held, self.scan.market(), now_ms));
        }
        // предупреждение о смене знака фандинга на удерживаемой паре
        for h in &held {
            if let Some(e) = evals.iter().find(|e| e.pair.id() == h.pair_id)
                && let Some(now) = e.est.now
            {
                let oriented = if e.short == h.short { now } else { -now };
                if oriented < Decimal::ZERO {
                    self.out.alerts.push(alert(
                        AlertLevel::Warn,
                        format!("flip:{}", h.pair_id),
                        format!("funding spread sign flipped on held {}", h.pair_id),
                    ));
                }
            }
        }
        for i in intents {
            match i {
                Intent::Exit { pair_id, reason } => self.start_exit(&pair_id, &reason, false),
                Intent::Enter(e) => {
                    if self.paused || self.risk.kill.engaged || self.risk.halted.is_some() {
                        continue;
                    }
                    self.try_enter(&e, now_ms);
                }
            }
        }
    }

    fn budget_for(&self, pair: &Pair, equity: Decimal) -> Option<SizeBudget> {
        let risk = &self.settings.scan.risk;
        let used: Decimal =
            self.book.pairs.values().filter(|p| p.state != Lifecycle::Closed).map(|p| p.capital.0).sum();
        let mut cap = (equity - used).min(equity * pct(risk.max_pair_share_pct));
        if pair.kind == PairKind::S2 {
            let s2: Decimal = self
                .book
                .pairs
                .values()
                .filter(|p| p.state != Lifecycle::Closed && p.kind == PairKind::S2)
                .map(|p| p.capital.0)
                .sum();
            cap = cap.min(equity * pct(risk.max_s2_share_pct) - s2);
        }
        if let Some(c) = self.settings.stage.max_capital_usd {
            cap = cap.min(c - used);
        }
        if cap <= Decimal::ZERO {
            return None;
        }
        let pc = self.book.platform_capital();
        let room = |k: &InstrumentKey| {
            let ex = k.venue.exchange;
            let mut r = equity * risk.platform_share(ex) - pc.get(&ex).map(|u| u.0).unwrap_or_default();
            if pair.a.venue.exchange == pair.b.venue.exchange {
                r /= Decimal::TWO;
            }
            Usd(r.max(Decimal::ZERO))
        };
        Some(SizeBudget { pair_capital: Usd(cap), room_margin: [room(&pair.a), room(&pair.b)] })
    }

    fn try_enter(&mut self, cand: &PairEval, now_ms: i64) {
        let equity = self.book.equity().0;
        if equity <= Decimal::ZERO {
            return;
        }
        let Some(budget) = self.budget_for(&cand.pair, equity) else { return };
        let e = self.scan.eval_with_budget(&cand.pair, now_ms, budget);
        if !e.entry_ok {
            return;
        }
        let Some(size) = e.size else { return };
        let reserve = self.settings.scan.reserve_frac;
        let Some(capital) = capital_for(size.notional, e.leverage.0, e.leverage.1, reserve) else { return };
        let market = self.scan.market();
        let meta = |k: &InstrumentKey| market.get(k).and_then(|s| s.meta.clone());
        let (Some(ms), Some(ml)) = (meta(&e.short), meta(&e.long)) else { return };
        let margin = |lev: Decimal| Usd(size.notional.0 / lev);
        let intent = EntryIntent {
            pair_id: e.pair.id(),
            kind: e.pair.kind,
            capital,
            legs: [
                (
                    e.short.venue.clone(),
                    e.short.symbol.clone(),
                    e.leverage.0,
                    margin(e.leverage.0),
                    ms.collateral.clone(),
                ),
                (
                    e.long.venue.clone(),
                    e.long.symbol.clone(),
                    e.leverage.1,
                    margin(e.leverage.1),
                    ml.collateral.clone(),
                ),
            ],
        };
        let view = self.book_view();
        if let Err(v) = self.risk.check_entry(&intent, &view, now_ms) {
            if !matches!(v, Veto::InstrumentBusy | Veto::TradingDisabled(_)) {
                tracing::info!(pair = %intent.pair_id, veto = %v, "entry vetoed");
            }
            return;
        }
        let fees = &self.settings.scan.fees;
        let (fs, fl) = (fees.resolve(&ms), fees.resolve(&ml));
        let legs = [
            JobLeg {
                venue: e.short.venue.clone(),
                symbol: e.short.symbol.clone(),
                side: Side::Sell,
                lot: ms.lot_size,
                min_qty: ms.min_qty.unwrap_or(ms.lot_size),
                price_rule: ms.price_rule.clone(),
                taker_fee: fs.taker,
            },
            JobLeg {
                venue: e.long.venue.clone(),
                symbol: e.long.symbol.clone(),
                side: Side::Buy,
                lot: ml.lot_size,
                min_qty: ml.min_qty.unwrap_or(ml.lot_size),
                price_rule: ml.price_rule.clone(),
                taker_fee: fl.taker,
            },
        ];
        // мейкер — нога с меньшей мейкер-комиссией (при равенстве — короткая)
        let maker = usize::from(fl.maker < fs.maker);
        let mids = (market.get(&e.short).and_then(|s| s.mid()), market.get(&e.long).and_then(|s| s.mid()));
        let pid = e.pair.id();
        self.book.open_pair(
            &pid,
            e.pair.kind,
            (e.short.clone(), e.leverage.0, mids.0),
            (e.long.clone(), e.leverage.1, mids.1),
            size.qty.0,
            capital,
            now_ms,
        );
        let id = self.next_job;
        self.next_job += 1;
        let params = self.job_params(e.pair.kind);
        self.jobs.insert(id, PairJob::new(id, pid.clone(), JobKind::Entry, legs, maker, size.qty.0, params, now_ms));
        self.out.alerts.push(alert(
            AlertLevel::Info,
            format!("enter:{pid}"),
            format!(
                "entering {pid}: {} per leg, s_net {:?}",
                size.notional.0.round_dp(0),
                e.at_size.and_then(|t| t.s_net)
            ),
        ));
        tracing::info!(pair = %pid, qty = %size.qty.0, notional = %size.notional.0, "entry job started");
    }

    fn job_params(&self, kind: PairKind) -> JobParams {
        let s1 = &self.settings.scan.strategy.s1_funding_carry;
        let s2 = &self.settings.scan.strategy.s2_cross_dex;
        let timeout = match kind {
            PairKind::S1 => s1.hedge_timeout_ms,
            PairKind::S2 => s2.hedge_timeout_ms.unwrap_or(s1.hedge_timeout_ms),
        };
        JobParams {
            child_usd: s1.child_usd,
            hedge_timeout_ms: i64::try_from(timeout).unwrap_or(1500),
            hedge_retries: s1.hedge_retries,
            edge_bps: s1.edge_bps,
            reprice_bps: s1.reprice_bps,
            max_slip_bps: s1.max_slip_bps,
            expected_slip_bps: Decimal::ONE,
            max_duration_ms: 30 * 60_000,
        }
    }

    /// Запускает выход из пары (или сброс при `flatten`).
    fn start_exit(&mut self, pair_id: &str, reason: &str, flatten: bool) {
        let Some(p) = self.book.pairs.get(pair_id).cloned() else { return };
        if p.state == Lifecycle::Closed {
            return;
        }
        // уже выходим тем же способом
        let existing: Vec<JobId> =
            self.jobs.iter().filter(|(_, j)| j.pair_id == pair_id && !j.is_done()).map(|(id, _)| *id).collect();
        if existing.iter().any(|id| {
            self.jobs.get(id).is_some_and(|j| j.kind == JobKind::Flatten || (!flatten && j.kind == JobKind::Exit))
        }) {
            return;
        }
        for id in existing {
            if let Some(j) = self.jobs.get_mut(&id) {
                j.abort();
            }
        }
        self.book.set_decision_mid(pair_id, self.scan.market());
        if let Some(pp) = self.book.pairs.get_mut(pair_id) {
            pp.exit_reason = Some(reason.to_owned());
        }
        self.book.set_state(pair_id, Lifecycle::Exiting, self.now_ms);
        let market = self.scan.market();
        let fees = &self.settings.scan.fees;
        let mut legs = Vec::new();
        let mut maker_fees = Vec::new();
        for l in &p.legs {
            let Some(m) = market.get(&l.key).and_then(|s| s.meta.clone()) else { return };
            let f = fees.resolve(&m);
            maker_fees.push(f.maker);
            legs.push(JobLeg {
                venue: l.key.venue.clone(),
                symbol: l.key.symbol.clone(),
                side: opposite(l.entry_side),
                lot: m.lot_size,
                min_qty: m.min_qty.unwrap_or(m.lot_size),
                price_rule: m.price_rule.clone(),
                taker_fee: f.taker,
            });
        }
        let target = p.legs[0].qty.abs().min(p.legs[1].qty.abs());
        let Ok(legs) = <[JobLeg; 2]>::try_from(legs) else { return };
        let maker = usize::from(maker_fees.get(1) < maker_fees.first());
        let id = self.next_job;
        self.next_job += 1;
        let kind = if flatten { JobKind::Flatten } else { JobKind::Exit };
        let params = self.job_params(p.kind);
        self.jobs.insert(id, PairJob::new(id, pair_id, kind, legs, maker, target, params, self.now_ms));
        self.out.alerts.push(alert(
            if flatten { AlertLevel::Crit } else { AlertLevel::Info },
            format!("exit:{pair_id}"),
            format!("exiting {pair_id}: {reason}"),
        ));
        tracing::info!(pair = %pair_id, %reason, flatten, "exit job started");
    }

    /// Закрывает остатки ног вне пар и непарные остатки пар (reduce-only IOC).
    fn close_orphans(&mut self) {
        let in_jobs: BTreeSet<InstrumentKey> = self
            .jobs
            .values()
            .filter(|j| !j.is_done())
            .flat_map(|j| j.legs.iter().map(|l| InstrumentKey::new(l.venue.clone(), l.symbol.clone())))
            .collect();
        let live: BTreeSet<InstrumentKey> =
            self.tracker.live().map(|o| InstrumentKey::new(o.req.venue.clone(), o.req.symbol.clone())).collect();
        let mut residual: Vec<(InstrumentKey, Decimal)> = Vec::new();
        for (k, q) in self.book.positions() {
            if in_jobs.contains(&k) || live.contains(&k) {
                continue;
            }
            let pair = self.book.pair_of(&k);
            let excess = match pair {
                None => q,
                Some(p) => {
                    // избыток ноги над противоположной
                    let other = p.legs.iter().find(|l| l.key != k).map(|l| l.qty).unwrap_or_default();
                    let net = q + other;
                    if (net > Decimal::ZERO) == (q > Decimal::ZERO) { net } else { Decimal::ZERO }
                }
            };
            if !excess.is_zero() {
                residual.push((k, excess));
            }
        }
        for (k, q) in residual {
            self.send_reduce(&k, q, "orphan residual");
        }
    }

    /// Reduce-only IOC на инструменте против позиции `q` (знак — текущая позиция).
    fn send_reduce(&mut self, k: &InstrumentKey, q: Decimal, why: &str) {
        let Some(st) = self.scan.market().get(k) else { return };
        let Some(m) = st.meta.clone() else { return };
        let qty = (q.abs() / m.lot_size.0).floor() * m.lot_size.0;
        if qty <= Decimal::ZERO {
            return;
        }
        let side = if q > Decimal::ZERO { Side::Sell } else { Side::Buy };
        let slip = self.settings.scan.strategy.s1_funding_carry.max_slip_bps * Decimal::from(3) / Decimal::from(10_000);
        let (b, a) = st.best();
        let raw = match side {
            Side::Buy => a.map(|a| (a.px.0 * (Decimal::ONE + slip), Rounding::Down)),
            Side::Sell => b.map(|b| (b.px.0 * (Decimal::ONE - slip), Rounding::Up)),
        };
        let Some(px) = raw.and_then(|(r, md)| m.price_rule.round(Px(r), md)) else { return };
        let req = OrderRequest {
            cid: self.cid(),
            venue: k.venue.clone(),
            symbol: k.symbol.clone(),
            side,
            qty: Qty(qty.normalize()),
            px,
            tif: Tif::Ioc,
            reduce_only: true,
        };
        let view = self.book_view();
        if self.risk.check_order(&req, false, &view).is_ok() {
            tracing::warn!(instrument = %k.id(), qty = %qty, %why, "closing residual");
            let c = self.tracker.send(req, None, self.now_ms);
            self.out.commands.push(c);
        }
    }

    fn step_jobs(&mut self, now_ms: i64) {
        let view = self.book_view();
        let allow_entry = self.risk.allow_new_child(&view) && self.startup_complete();
        let below_unhedged_cap = view.unhedged_usd.0 < self.settings.scan.risk.max_unhedged_usd;
        let ids: Vec<JobId> = self.jobs.keys().copied().collect();
        for id in ids {
            let Some(mut job) = self.jobs.remove(&id) else { continue };
            let degraded = job.legs.iter().any(|l| self.risk.degraded.contains_key(&l.venue.to_string()));
            // выход reduce-only разрешён и при kill switch; сброс — и на деградировавшей площадке
            let allow = match job.kind {
                JobKind::Entry => allow_entry && !degraded,
                JobKind::Exit => below_unhedged_cap && !degraded && self.risk.halted.is_none(),
                JobKind::Flatten => true,
            };
            let ids_gen = &self.ids;
            let mut next = || ids_gen.next(now_ms);
            let actions = job.step(now_ms, self.scan.market(), allow, &mut next);
            for a in actions {
                match a {
                    JobAction::Place(req) => {
                        let is_child = !req.reduce_only && req.tif == Tif::PostOnly;
                        match self.risk.check_order(&req, is_child, &view) {
                            Ok(()) => {
                                let c = self.tracker.send(req, Some(id), now_ms);
                                self.out.commands.push(c);
                            }
                            Err(v) => {
                                tracing::warn!(job = id, cid = req.cid.0, veto = %v, "order vetoed");
                                // задача увидит отказ как закрытый ордер
                                job.on_rejected(req.cid);
                            }
                        }
                    }
                    JobAction::Cancel(cid) => {
                        if let Some(c) = self.tracker.cancel(cid) {
                            self.out.commands.push(c);
                        }
                    }
                }
            }
            if job.state == JobState::Unwinding {
                self.out.alerts.push(alert(
                    AlertLevel::Crit,
                    format!("unwind:{}", job.pair_id),
                    format!("hedge failed on {}: unwinding", job.pair_id),
                ));
                self.book.set_state(&job.pair_id, Lifecycle::Unwinding, now_ms);
            }
            if job.is_done() {
                self.finish_job(&job, now_ms);
            } else {
                self.jobs.insert(id, job);
            }
        }
    }

    fn finish_job(&mut self, job: &PairJob, now_ms: i64) {
        let tol = pct(self.settings.scan.strategy.s1_funding_carry.delta_tolerance_pct);
        let Some(p) = self.book.pairs.get(&job.pair_id).cloned() else { return };
        if !job.residual.is_zero() {
            tracing::info!(pair = %job.pair_id, residual = %job.residual, "job finished with residual below lot");
        }
        let state = if p.is_flat() {
            Lifecycle::Closed
        } else if p.state == Lifecycle::Halted {
            Lifecycle::Halted
        } else if job.kind == JobKind::Entry && p.is_balanced(tol) {
            Lifecycle::Hedged
        } else if job.kind == JobKind::Entry {
            // вход не довёл ноги до допуска: остаток закроет монитор дельты
            Lifecycle::Hedged
        } else {
            // выход оставил непарный остаток: закрываем его отдельно
            Lifecycle::Exiting
        };
        if let Some(pp) = self.book.pairs.get_mut(&job.pair_id)
            && job.kind == JobKind::Entry
        {
            pp.target_qty = job.filled[0].min(job.filled[1]);
        }
        self.book.set_state(&job.pair_id, state, now_ms);
        if state == Lifecycle::Closed {
            self.out.alerts.push(alert(
                AlertLevel::Info,
                format!("closed:{}", job.pair_id),
                format!("{} closed, net {}", job.pair_id, p.net().round_dp(2)),
            ));
        }
        if state == Lifecycle::Exiting {
            for l in &p.legs {
                if !l.qty.is_zero() {
                    self.send_reduce(&l.key, l.qty, "exit residual");
                }
            }
        }
    }

    /// Снимок книги для риска.
    pub fn book_view(&self) -> BookView {
        let market = self.scan.market();
        let mark = |k: &InstrumentKey| market.get(k).and_then(|s| s.mark.or_else(|| s.mid()));
        let liq = |k: &InstrumentKey| {
            self.venue_positions
                .get(&k.venue.exchange)
                .and_then(|v| v.iter().find(|p| p.venue == k.venue && p.symbol == k.symbol))
                .and_then(|p| p.liq_px)
        };
        let mut pairs = Vec::new();
        let mut paired: BTreeSet<InstrumentKey> = BTreeSet::new();
        for p in self.book.pairs.values().filter(|p| p.state != Lifecycle::Closed) {
            let legs = p
                .legs
                .iter()
                .map(|l| {
                    paired.insert(l.key.clone());
                    LegExposure {
                        venue: l.key.venue.clone(),
                        symbol: l.key.symbol.clone(),
                        qty: l.qty,
                        mark: mark(&l.key),
                        liq_px: liq(&l.key),
                        adr_frac: None,
                    }
                })
                .collect();
            pairs.push(PairExposure { pair_id: p.pair_id.clone(), kind: p.kind, capital: p.capital, legs });
        }
        let orphan_legs = self
            .book
            .positions()
            .into_iter()
            .filter(|(k, _)| !paired.contains(k))
            .map(|(k, q)| LegExposure {
                venue: k.venue.clone(),
                symbol: k.symbol.clone(),
                qty: q,
                mark: mark(&k),
                liq_px: liq(&k),
                adr_frac: None,
            })
            .collect();
        let unhedged: Decimal = self
            .jobs
            .values()
            .map(|j| {
                let k = InstrumentKey::new(j.legs[j.maker].venue.clone(), j.legs[j.maker].symbol.clone());
                j.unhedged().abs() * mark(&k).map(|m| m.0).unwrap_or_default()
            })
            .sum();
        BookView {
            equity: self.book.equity(),
            peak_equity: self.book.peak_equity,
            day_start_equity: self.book.day_start_equity,
            platform_capital: self.book.platform_capital(),
            pairs,
            unhedged_usd: Usd(unhedged),
            orphan_legs,
        }
    }

    /// План перебалансировки по балансам площадок и марже ног (неподписанные данные).
    pub fn rebalance_plan(&self, p: &sh_portfolio::rebalance::PlanParams) -> sh_portfolio::rebalance::RebalancePlan {
        use sh_portfolio::rebalance::{VenueFunds, plan};
        let market = self.scan.market();
        let reserve = self.settings.scan.reserve_frac;
        let ratio = self.settings.scan.strategy.s1_funding_carry.rebalance_margin_ratio;
        let mut required: BTreeMap<String, Decimal> = BTreeMap::new();
        for pp in self.book.pairs.values().filter(|x| x.state != Lifecycle::Closed) {
            for l in &pp.legs {
                let mark = market.get(&l.key).and_then(|s| s.mark.or_else(|| s.mid())).map(|m| m.0).unwrap_or_default();
                let margin = l.qty.abs() * mark / l.leverage.max(Decimal::ONE) / (Decimal::ONE - reserve);
                *required.entry(l.key.venue.to_string()).or_default() += margin;
            }
        }
        let mut funds: BTreeMap<String, VenueFunds> = BTreeMap::new();
        for b in self.book.balances.values() {
            let key = b.venue.to_string();
            let f = funds.entry(key.clone()).or_insert(VenueFunds {
                venue: b.venue.clone(),
                equity: Usd(Decimal::ZERO),
                required: Usd(required.get(&key).copied().unwrap_or_default()),
                urgent: false,
            });
            f.equity.0 += b.equity.0;
            if let Some(mm) = b.maintenance_margin
                && b.equity.0 > Decimal::ZERO
                && mm.0 / b.equity.0 >= ratio
            {
                f.urgent = true;
            }
        }
        for (k, r) in &required {
            if !funds.contains_key(k)
                && let Some(ik) =
                    self.book.pairs.values().flat_map(|x| x.legs.iter()).find(|l| &l.key.venue.to_string() == k)
            {
                funds.insert(
                    k.clone(),
                    VenueFunds {
                        venue: ik.key.venue.clone(),
                        equity: Usd(Decimal::ZERO),
                        required: Usd(*r),
                        urgent: true,
                    },
                );
            }
        }
        let v: Vec<VenueFunds> = funds.into_values().collect();
        plan(&v, p)
    }

    /// Сводка состояния для `spread-ctl status` (JSON).
    pub fn status_json(&self) -> serde_json::Value {
        let view = self.book_view();
        serde_json::json!({
            "mode": self.settings.mode,
            "startup_complete": self.startup_complete(),
            "kill": self.risk.kill,
            "halted": self.risk.halted,
            "paused": self.paused,
            "clean_days": self.clean_days(),
            "stage": self.settings.stage,
            "degraded": self.risk.degraded,
            "equity_usd": view.equity.0.round_dp(2).to_string(),
            "peak_equity_usd": view.peak_equity.0.round_dp(2).to_string(),
            "unhedged_usd": view.unhedged_usd.0.round_dp(2).to_string(),
            "net_delta_usd": view.net_delta_usd().round_dp(2).to_string(),
            "pairs": self.book.pairs.values().map(|p| serde_json::json!({
                "pair": p.pair_id,
                "kind": format!("{:?}", p.kind),
                "state": p.state,
                "legs": p.legs.iter().map(|l| format!("{} {}", l.key.id(), l.qty)).collect::<Vec<_>>(),
                "capital": p.capital.0.round_dp(2).to_string(),
                "funding": p.funding.0.round_dp(2).to_string(),
                "fees": p.fees.0.round_dp(2).to_string(),
                "slippage": p.slippage.0.round_dp(2).to_string(),
                "price_pnl": p.price_pnl.0.round_dp(2).to_string(),
                "exit_reason": p.exit_reason,
            })).collect::<Vec<_>>(),
            "jobs": self.jobs.values().map(|j| serde_json::json!({
                "id": j.id, "pair": j.pair_id, "kind": j.kind, "state": j.state,
                "filled": [j.filled[0].to_string(), j.filled[1].to_string()], "target": j.target.to_string(),
            })).collect::<Vec<_>>(),
            "live_orders": self.tracker.live().count(),
            "unknown_orders": self.tracker.unknown_count(),
        })
    }
}
