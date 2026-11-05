//! Оболочка live против симулятора, подключённого как приватные адаптеры
//! (`SimExecution`): старт со сверкой, вход S1 на Hyperliquid + Lighter в пределах ступени,
//! kill switch через канал управления. Реальные площадки здесь не используются.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;
use rust_decimal::dec;
use sh_core::{Clock, Exchange, Side, SystemClock, Usd, VenueKey};
use sh_obs::Alerts;
use sh_signals::testkit::{InstrumentSpec, MarketBuilder};
use sh_sim::{SimCore, SimExecution, SimParams};
use sh_venue_api::{Execution, MdEvent};
use spread_hunter::config::AppConfig;
use spread_hunter::ctl::{CtlRequest, CtlResponse};
use spread_hunter::engine::OperatorCommand;
use spread_hunter::live::{LiveState, live_loop, new_registry};
use tokio::sync::{mpsc, oneshot, watch};

async fn ask(ctl: &mpsc::Sender<spread_hunter::ctl::CtlEnvelope>, req: CtlRequest) -> CtlResponse {
    let (tx, rx) = oneshot::channel();
    ctl.send((req, tx)).await.unwrap();
    rx.await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_shell_enters_within_stage_and_kill_flattens() {
    let dir = std::env::temp_dir().join(format!("sh-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let symbols = dir.join("symbols.toml");
    std::fs::write(&symbols, "[[underlying]]\nid = \"ETH\"\nmembers = [\"hyperliquid/ETH\", \"lighter/ETH\"]\n")
        .unwrap();
    let mut cfg = AppConfig::default();
    cfg.symbols.path = symbols.display().to_string();
    cfg.database.path = dir.join("state.sqlite").display().to_string();
    cfg.scanner.report_dir = dir.join("reports").display().to_string();
    cfg.control.kill_file = dir.join("KILL").display().to_string();
    cfg.scanner.interval_secs = 1;
    cfg.engine.mode = "live".into();
    cfg.venues.hyperliquid.trading_enabled = true;
    cfg.venues.lighter.trading_enabled = true;

    let clock = SystemClock::new();
    let mut b = MarketBuilder::new();
    b.now_ms = clock.wall_ms();
    let lt = InstrumentSpec::liquid("lighter/ETH", dec!(0.0002), dec!(1));
    let hl = InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.000001), dec!(1));
    b.add(&lt, 8 * 24);
    b.add(&hl, 8 * 24);

    let core = Arc::new(Mutex::new(SimCore::new(SimParams {
        initial_cash: BTreeMap::from([
            (VenueKey::new(Exchange::Lighter), Usd(dec!(20000))),
            (VenueKey::hyperliquid(""), Usd(dec!(20000))),
        ]),
        ..SimParams::default()
    })));
    let clk: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut execs: BTreeMap<Exchange, Arc<dyn Execution>> = BTreeMap::new();
    // события рынка идут в симулятор через адаптер Lighter: его поток отдаёт заполнения
    let lt_exec = Arc::new(SimExecution::new(Exchange::Lighter, core.clone(), clk.clone()));
    execs.insert(Exchange::Hyperliquid, Arc::new(SimExecution::new(Exchange::Hyperliquid, core.clone(), clk.clone())));
    execs.insert(Exchange::Lighter, lt_exec.clone());
    let st = LiveState::open(&cfg, execs, new_registry(), clock.wall_ms()).await.unwrap();
    let (md_tx, md_rx) = mpsc::channel::<MdEvent>(65_536);
    let (ctl_tx, ctl_rx) = mpsc::channel(16);
    let (stop_tx, stop_rx) = watch::channel(false);
    let lp = tokio::spawn(live_loop(md_rx, ctl_rx, clk.clone(), st, Alerts::log_only(), stop_rx));
    for e in &b.events {
        core.lock().unwrap().on_md(e);
        md_tx.send(e.clone()).await.unwrap();
    }

    // поток рынка: свежие стаканы и сделки через мейкер-уровни Lighter
    let feed = |b: &mut MarketBuilder, sweep: bool| {
        b.now_ms = SystemClock::new().wall_ms();
        let mut evs = Vec::new();
        for k in [lt.key.clone(), hl.key.clone()] {
            b.set_book(&k, dec!(100), dec!(0.01), dec!(10000));
            evs.push(b.events.last().unwrap().clone());
        }
        if sweep {
            b.trade(&lt.key, dec!(100.3), dec!(1000000), Side::Buy);
            evs.push(b.events.last().unwrap().clone());
            b.trade(&lt.key, dec!(99.7), dec!(1000000), Side::Sell);
            evs.push(b.events.last().unwrap().clone());
        }
        evs
    };
    let pos = |core: &Arc<Mutex<SimCore>>, ex: Exchange| -> Decimal {
        core.lock().unwrap().positions(Some(ex)).iter().map(|p| p.qty).sum()
    };
    let mut entered = false;
    for _ in 0..60 {
        for e in feed(&mut b, true) {
            lt_exec.on_md(&e);
            md_tx.send(e).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (l, h) = (pos(&core, Exchange::Lighter), pos(&core, Exchange::Hyperliquid));
        if !l.is_zero() && l + h == Decimal::ZERO {
            entered = true;
            break;
        }
    }
    let status = ask(&ctl_tx, CtlRequest::Status).await;
    assert!(entered, "{}\n{:?}", status.body, core.lock().unwrap().open_orders());
    // ступень M3: капитал пар не больше $5000
    let notional = pos(&core, Exchange::Lighter).abs() * dec!(100);
    assert!(notional <= dec!(5000) * dec!(3), "notional {notional}");
    let r = ask(&ctl_tx, CtlRequest::Command(OperatorCommand::Kill { flatten: true, source: "test".into() })).await;
    assert!(r.ok);
    for _ in 0..60 {
        for e in feed(&mut b, false) {
            lt_exec.on_md(&e);
            md_tx.send(e).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        if pos(&core, Exchange::Lighter).is_zero() && pos(&core, Exchange::Hyperliquid).is_zero() {
            break;
        }
    }
    assert_eq!(pos(&core, Exchange::Lighter), Decimal::ZERO);
    assert_eq!(pos(&core, Exchange::Hyperliquid), Decimal::ZERO);
    let _ = stop_tx.send(true);
    drop(md_tx);
    let _ = tokio::time::timeout(Duration::from_secs(10), lp).await;
    let _ = std::fs::remove_dir_all(&dir);
}
