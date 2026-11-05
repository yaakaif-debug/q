//! `selftest` против симулятора: все шаги проходят на исправной «площадке», а отказ
//! площадки виден в таблице. Реальный прогон (testnet/mainnet) делает оператор.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rust_decimal::dec;
use sh_core::{Exchange, Px, SystemClock, Usd, VenueKey};
use sh_md::InstrumentKey;
use sh_signals::testkit::{InstrumentSpec, MarketBuilder};
use sh_sim::{FailMode, SimCore, SimExecution, SimParams};
use spread_hunter::selftest::{Outcome, VenueCheck, check_venue, table};

fn setup(key: &str, ex: Exchange, venue: VenueKey) -> (Arc<Mutex<SimCore>>, VenueCheck) {
    let mut b = MarketBuilder::new();
    let spec = InstrumentSpec::liquid(key, dec!(0.0001), dec!(1));
    b.add(&spec, 2);
    let mut cash = BTreeMap::from([(venue.clone(), Usd(dec!(10000)))]);
    cash.insert(VenueKey::hyperliquid("xyz"), Usd(dec!(1000)));
    let core = Arc::new(Mutex::new(SimCore::new(SimParams { initial_cash: cash, ..SimParams::default() })));
    for e in &b.events {
        core.lock().unwrap().on_md(e);
    }
    let meta = b.market.get(&InstrumentKey::parse(key).unwrap()).unwrap().meta.clone();
    let exec = Arc::new(SimExecution::new(ex, core.clone(), Arc::new(SystemClock::new())));
    let check = VenueCheck {
        exec,
        venue: spec.key.venue.clone(),
        symbol: spec.key.symbol.clone(),
        meta,
        quote: Arc::new(|| Box::pin(async { Some((Px(dec!(99.99)), Px(dec!(100.01)))) })),
        max_notional: dec!(500),
        transfer: (ex == Exchange::Hyperliquid).then(|| (String::new(), "xyz".to_owned())),
        wait_ms: 300,
    };
    (core, check)
}

#[tokio::test]
async fn all_steps_pass_on_a_healthy_venue() {
    let (_core, c) = setup("hyperliquid/ETH", Exchange::Hyperliquid, VenueKey::hyperliquid(""));
    let res = check_venue(&c).await;
    let t = table(&res);
    assert!(res.iter().all(|r| r.outcome != Outcome::Fail), "{t}");
    for step in [
        "metadata",
        "balances",
        "post_only_place",
        "cancel_confirmed",
        "ioc_place",
        "stream_fill",
        "query_by_client_id",
        "position",
        "reduce_only_close",
        "funding_read",
        "transfer_out",
        "transfer_back",
        "kill_cancel_all",
        "kill_confirmed",
        "flat",
    ] {
        assert!(res.iter().any(|r| r.step == step && r.outcome == Outcome::Pass), "{step} missing\n{t}");
    }
}

#[tokio::test]
async fn venue_down_is_reported_and_nothing_is_left_open() {
    let (core, c) = setup("lighter/ETH", Exchange::Lighter, VenueKey::new(Exchange::Lighter));
    core.lock().unwrap().set_fail(Exchange::Lighter, Some(FailMode::Down));
    let res = check_venue(&c).await;
    assert!(res.iter().any(|r| r.step == "ioc_place" && r.outcome == Outcome::Fail));
    assert!(core.lock().unwrap().positions(None).is_empty());
    assert!(table(&res).contains("FAIL"));
}

#[tokio::test]
async fn too_small_notional_stops_before_orders() {
    let (core, mut c) = setup("lighter/ETH", Exchange::Lighter, VenueKey::new(Exchange::Lighter));
    c.max_notional = dec!(0.5);
    let res = check_venue(&c).await;
    assert!(res.iter().any(|r| r.step == "sizing" && r.outcome == Outcome::Fail));
    assert!(core.lock().unwrap().open_orders().is_empty());
}
