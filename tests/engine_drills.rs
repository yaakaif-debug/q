//! Движок с симулятором: вход S1 мейкером с хеджем, выход, и учения из приёмки M2 —
//! kill switch, отказ площадки, делистинг HIP-3, ADL, расхождение сверки, перезапуск.
//! Всё офлайн, на синтетическом рынке; живые и testnet-прогоны делает оператор.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};

use rust_decimal::Decimal;
use rust_decimal::dec;
use sh_core::{Exchange, Px, Side, Usd, VenueKey};
use sh_md::InstrumentKey;
use sh_portfolio::Lifecycle;
use sh_risk::StageCaps;
use sh_signals::testkit::{InstrumentSpec, MarketBuilder};
use sh_signals::{CostParams, FeesConfig, RiskLimits, ScannerParams, StrategyParams, SymbolMap};
use sh_sim::{SimCore, SimParams};
use sh_venue_api::{InstrumentStatus, MdEvent, MdEventKind};
use spread_hunter::engine::{Engine, EngineMode, EngineSettings, OperatorCommand};
use spread_hunter::scan::{ScanEngine, ScanSettings};
use spread_hunter::simdrive::SimDriver;

const SYMBOLS: &str = "[[underlying]]\nid = \"ETH\"\nmembers = [\"hyperliquid/ETH\", \"aster/ETHUSDT\"]\n";

fn settings() -> EngineSettings {
    let scan = ScanSettings {
        scanner: ScannerParams { interval_secs: 10, ..ScannerParams::default() },
        strategy: StrategyParams::default(),
        costs: CostParams::default(),
        fees: FeesConfig::default(),
        risk: RiskLimits::default(),
        reserve_frac: dec!(0.15),
    };
    EngineSettings {
        scan,
        mode: EngineMode::Backtest,
        trading_enabled: Exchange::ALL.iter().map(|e| (*e, true)).collect(),
        stage: StageCaps::default(),
        reconcile_tol: dec!(0.000001),
    }
}

struct World {
    b: MarketBuilder,
    d: SimDriver,
    aster: InstrumentKey,
    hl: InstrumentKey,
}

fn sim() -> SimCore {
    SimCore::new(SimParams {
        initial_cash: BTreeMap::from([
            (VenueKey::new(Exchange::Aster), Usd(dec!(50000))),
            (VenueKey::hyperliquid(""), Usd(dec!(50000))),
        ]),
        ..SimParams::default()
    })
}

fn engine(now: i64, state: Option<spread_hunter::engine::EngineState>) -> Engine {
    let s = settings();
    let scan =
        ScanEngine::new(s.scan.clone(), SymbolMap::from_toml_str(SYMBOLS).unwrap(), None, HashMap::new(), Vec::new());
    Engine::new(s, scan, state, now)
}

impl World {
    fn new() -> Self {
        let mut b = MarketBuilder::new();
        let a = InstrumentSpec::liquid("aster/ETHUSDT", dec!(0.0003), dec!(8));
        let h = InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.000001), dec!(1));
        b.add(&a, 8 * 24);
        b.add(&h, 8 * 24);
        let mut d = SimDriver::new(engine(b.now_ms, None), sim(), 1000);
        for e in b.events.clone() {
            d.on_md(&e);
        }
        Self { b, d, aster: a.key, hl: h.key }
    }

    /// Секунда: обновление стаканов (свежие данные), сделки через мейкер-уровни, такт.
    fn second(&mut self, sweep: bool, venues_alive: &[&InstrumentKey]) {
        self.b.now_ms += 1000;
        for k in venues_alive {
            let k = (*k).clone();
            self.b.set_book(&k, dec!(100), dec!(0.01), dec!(10000));
            let e = self.b.events.last().unwrap().clone();
            self.d.on_md(&e);
        }
        if sweep {
            let k = self.aster.clone();
            self.b.trade(&k, dec!(100.3), dec!(1000000), Side::Buy);
            let e = self.b.events.last().unwrap().clone();
            self.d.on_md(&e);
            self.b.trade(&k, dec!(99.7), dec!(1000000), Side::Sell);
            let e = self.b.events.last().unwrap().clone();
            self.d.on_md(&e);
        }
        self.d.tick(self.b.now_ms);
    }

    fn run(&mut self, secs: u32, sweep: bool) {
        let (a, h) = (self.aster.clone(), self.hl.clone());
        for _ in 0..secs {
            self.second(sweep, &[&a, &h]);
        }
    }

    fn pos(&self, k: &InstrumentKey) -> Decimal {
        self.d
            .sim
            .positions(None)
            .iter()
            .find(|p| p.venue == k.venue && p.symbol == k.symbol)
            .map(|p| p.qty)
            .unwrap_or_default()
    }

    fn pair_state(&self) -> Option<Lifecycle> {
        self.d.engine.book().pairs.values().next().map(|p| p.state)
    }

    fn enter(&mut self) {
        self.run(120, true);
        assert_eq!(self.pair_state(), Some(Lifecycle::Hedged), "{}", self.d.engine.status_json());
    }
}

#[test]
fn enters_s1_maker_first_and_stays_neutral() {
    let mut w = World::new();
    w.enter();
    let (sa, sh) = (w.pos(&w.aster.clone()), w.pos(&w.hl.clone()));
    assert!(sa < Decimal::ZERO, "aster short expected");
    assert_eq!(sa + sh, Decimal::ZERO);
    // книга совпадает с площадкой
    assert_eq!(w.d.engine.book().position(&w.aster.clone()), sa);
    // мейкер-нога — Aster (0% мейкер), все её ордера post-only
    let aster_orders: Vec<_> =
        w.d.engine.tracker().iter().filter(|o| o.req.venue.exchange == Exchange::Aster).collect();
    assert!(!aster_orders.is_empty());
    assert!(aster_orders.iter().all(|o| o.req.tif == sh_venue_api::Tif::PostOnly));
    assert_eq!(w.d.engine.tracker().unknown_count(), 0);
    assert!(w.d.engine.risk().halted.is_none());
}

#[test]
fn kill_switch_cancels_and_blocks_then_flatten_closes() {
    let mut w = World::new();
    w.run(15, false);
    // вход начался: стоит мейкер-ордер, сделок нет
    assert!(w.d.engine.tracker().live().count() > 0);
    let now = w.b.now_ms;
    w.d.command(&OperatorCommand::Kill { flatten: false, source: "test".into() }, now);
    w.run(5, false);
    assert_eq!(w.d.engine.tracker().live().count(), 0);
    let before = w.d.engine.tracker().iter().count();
    w.run(30, true);
    assert_eq!(w.d.engine.tracker().iter().count(), before, "no new orders while killed");
    assert_eq!(w.pos(&w.aster.clone()), Decimal::ZERO, "{}", w.d.engine.status_json());

    let mut w = World::new();
    w.enter();
    let now = w.b.now_ms;
    w.d.command(&OperatorCommand::Kill { flatten: true, source: "test".into() }, now);
    w.run(60, false);
    assert_eq!(w.pos(&w.aster.clone()), Decimal::ZERO);
    assert_eq!(w.pos(&w.hl.clone()), Decimal::ZERO);
    assert_eq!(w.pair_state(), Some(Lifecycle::Closed));
    // все ордера сброса — reduce-only
    let flatten_orders: Vec<_> = w.d.engine.tracker().iter().filter(|o| o.sent_ms >= now).collect();
    assert!(!flatten_orders.is_empty());
    assert!(flatten_orders.iter().all(|o| o.req.reduce_only));
}

#[test]
fn venue_outage_marks_degraded_and_stops_children() {
    let mut w = World::new();
    w.run(15, false);
    assert!(w.d.engine.tracker().live().count() > 0);
    // Hyperliquid молчит 5 секунд: данные устарели (md_stale_ms = 2000)
    let a = w.aster.clone();
    for _ in 0..5 {
        w.second(false, &[&a]);
    }
    assert!(w.d.engine.risk().degraded.contains_key("hyperliquid"));
    assert_eq!(w.d.engine.tracker().live().count(), 0, "maker child cancelled while hedge venue is degraded");
    // восстановление
    w.run(3, false);
    assert!(w.d.engine.risk().degraded.is_empty());
}

#[test]
fn hip3_style_delisting_exits_pair() {
    let mut w = World::new();
    w.enter();
    let mut meta = w.d.engine.market().get(&w.hl.clone()).unwrap().meta.clone().unwrap();
    meta.status = InstrumentStatus::Delisted;
    let ev = MdEvent {
        venue: w.hl.venue.clone(),
        symbol: w.hl.symbol.clone(),
        recv: sh_core::RecvTs { wall_ns: (w.b.now_ms + 500) * 1_000_000, mono_ns: 0 },
        venue_ts_ms: None,
        kind: MdEventKind::Metadata(Box::new(meta)),
    };
    w.b.push(ev.clone());
    w.d.on_md(&ev);
    let out = w.d.tick(w.b.now_ms + 600);
    assert!(out.alerts.iter().any(|a| a.key.starts_with("meta:")), "{:?}", out.alerts);
    w.run(120, true);
    assert_eq!(w.pair_state(), Some(Lifecycle::Closed), "{}", w.d.engine.status_json());
    assert_eq!(w.pos(&w.aster.clone()), Decimal::ZERO);
    assert_eq!(w.pos(&w.hl.clone()), Decimal::ZERO);
}

#[test]
fn adl_on_one_leg_reduces_the_other() {
    let mut w = World::new();
    w.enter();
    let hl = w.hl.clone();
    let q = w.pos(&hl);
    // ADL на площадке — кратно лоту
    let half = (q / dec!(2) / dec!(0.01)).floor() * dec!(0.01);
    let out = w.d.sim.force_position_change(&hl.venue, &hl.symbol, -half, Px(dec!(100)));
    for o in &out {
        w.d.engine.on_exec(o, w.b.now_ms);
    }
    let o = w.d.tick(w.b.now_ms);
    assert!(o.alerts.iter().any(|a| a.key.starts_with("adl:")));
    w.run(60, false);
    assert_eq!(w.pos(&w.aster.clone()), Decimal::ZERO);
    assert_eq!(w.pos(&hl), Decimal::ZERO);
}

#[test]
fn reconciliation_mismatch_halts() {
    let mut w = World::new();
    w.enter();
    // площадка «потеряла» часть позиции без событий
    let k = w.aster.clone();
    let q = w.pos(&k);
    let eq: Vec<(VenueKey, Usd)> = w.d.sim.balances(None).iter().map(|b| (b.venue.clone(), b.equity)).collect();
    let hl_q = w.pos(&w.hl.clone());
    w.d.sim.seed(
        &[
            (k.venue.clone(), k.symbol.clone(), q / dec!(2), Some(Px(dec!(100)))),
            (w.hl.venue.clone(), w.hl.symbol.clone(), hl_q, Some(Px(dec!(100)))),
        ],
        &eq,
    );
    w.run(25, false);
    assert!(w.d.engine.risk().halted.is_some());
    assert_eq!(w.pair_state(), Some(Lifecycle::Halted));
    // в Halted новые входы и ордера не отправляются
    let n = w.d.engine.tracker().iter().count();
    w.run(20, true);
    assert_eq!(w.d.engine.tracker().iter().count(), n);
}

#[test]
fn restart_restores_book_and_reconciles() {
    let mut w = World::new();
    w.enter();
    let state = w.d.engine.state();
    let json = serde_json::to_string(&state).unwrap();
    let restored: spread_hunter::engine::EngineState = serde_json::from_str(&json).unwrap();
    let positions_before = w.d.sim.positions(None);
    // новый процесс: движок из состояния, симулятор восстановлен из книги
    let mut d = SimDriver::new(engine(w.b.now_ms, Some(restored)), sim(), 1000);
    // как в paper: симулятор восстанавливается из книги до первых данных
    d.seed_from_book();
    for e in w.b.events.clone() {
        d.on_md(&e);
    }
    let positions_after = d.sim.positions(None);
    assert_eq!(
        positions_before.iter().map(|p| p.qty).collect::<Vec<_>>(),
        positions_after.iter().map(|p| p.qty).collect::<Vec<_>>()
    );
    let mut alerts = Vec::new();
    for i in 0..15 {
        alerts.extend(d.tick(w.b.now_ms + 1000 * (i + 1)).alerts);
    }
    assert!(d.engine.risk().halted.is_none(), "{alerts:?}");
    assert!(d.engine.startup_complete());
}
