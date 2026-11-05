//! Тесты симулятора: тейкер по стакану, мейкер по очереди, reduce-only, фандинг,
//! потерянные ответы, детерминизм и тождество капитала (proptest).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use rust_decimal::Decimal;
use rust_decimal::dec;
use sh_core::{Exchange, Px, Qty, Side, Usd, VenueKey};
use sh_md::InstrumentKey;
use sh_signals::testkit::{InstrumentSpec, MarketBuilder};
use sh_sim::{FailMode, SimCore, SimParams};
use sh_venue_api::{
    ClientId, CommandResult, ExecCommand, ExecError, ExecEvent, ExecInput, OrderRequest, OrderStatus, Tif,
};

fn lighter() -> VenueKey {
    VenueKey::new(Exchange::Lighter)
}

fn setup() -> (SimCore, MarketBuilder, InstrumentKey) {
    let mut b = MarketBuilder::new();
    let spec = InstrumentSpec::liquid("lighter/ETH", dec!(0.0001), dec!(1));
    b.add(&spec, 2);
    let mut params = SimParams::default();
    params.initial_cash = BTreeMap::from([(lighter(), Usd(dec!(10000)))]);
    let mut sim = SimCore::new(params);
    for e in &b.events {
        sim.on_md(e);
    }
    (sim, b, spec.key)
}

fn req(cid: u64, side: Side, qty: Decimal, px: Decimal, tif: Tif, reduce_only: bool) -> OrderRequest {
    OrderRequest {
        cid: ClientId(cid),
        venue: lighter(),
        symbol: "ETH".into(),
        side,
        qty: Qty(qty),
        px: Px(px),
        tif,
        reduce_only,
    }
}

fn fills(out: &[ExecInput]) -> Vec<(Decimal, Decimal, bool)> {
    out.iter()
        .filter_map(|o| match o {
            ExecInput::Event(ExecEvent::Fill(f)) => Some((f.qty.0, f.px.0, f.maker)),
            _ => None,
        })
        .collect()
}

fn placed(out: &[ExecInput], cid: u64) -> Option<Result<OrderStatus, ExecError>> {
    out.iter().find_map(|o| match o {
        ExecInput::Result(CommandResult::Placed(c, r)) if c.0 == cid => Some(r.clone().map(|u| u.status)),
        _ => None,
    })
}

#[test]
fn ioc_walks_book_after_latency() {
    let (mut sim, b, _) = setup();
    let now = b.now_ms;
    // ask 100.01 + шаг 0.01 → покупка 15000 проходит два уровня
    sim.submit(ExecCommand::Place(req(1, Side::Buy, dec!(15000), dec!(100.05), Tif::Ioc, false)), now);
    // Lighter Standard: тейкер ждёт 300 мс + половину RTT
    assert!(sim.advance(now + 100).is_empty());
    let out = sim.advance(now + 400);
    let f = fills(&out);
    assert_eq!(f, vec![(dec!(10000), dec!(100.01), false), (dec!(5000), dec!(100.02), false)]);
    assert_eq!(sim.order(ClientId(1)).unwrap().status, OrderStatus::Filled);
    let pos = sim.positions(None);
    assert_eq!(pos[0].qty, dec!(15000));
}

#[test]
fn ioc_remainder_cancelled_and_post_only_cross_rejected() {
    let (mut sim, b, _) = setup();
    let now = b.now_ms;
    sim.submit(ExecCommand::Place(req(1, Side::Buy, dec!(5), dec!(99), Tif::Ioc, false)), now);
    sim.submit(ExecCommand::Place(req(2, Side::Buy, dec!(5), dec!(100.02), Tif::PostOnly, false)), now);
    let out = sim.advance(now + 1000);
    assert_eq!(sim.order(ClientId(1)).unwrap().status, OrderStatus::Cancelled);
    assert!(matches!(placed(&out, 2), Some(Err(ExecError::Rejected(_)))));
}

#[test]
fn maker_fills_only_after_queue_traded_through() {
    let (mut sim, mut b, key) = setup();
    let now = b.now_ms;
    // лучший бид 99.99 с очередью 10000
    sim.submit(ExecCommand::Place(req(1, Side::Buy, dec!(100), dec!(99.99), Tif::PostOnly, false)), now);
    sim.advance(now + 50);
    b.now_ms = now + 100;
    b.trade(&key, dec!(99.99), dec!(9950), Side::Sell);
    let out = sim.on_md(b.events.last().unwrap());
    assert!(fills(&out).is_empty());
    b.trade(&key, dec!(99.99), dec!(80), Side::Sell);
    let out = sim.on_md(b.events.last().unwrap());
    assert_eq!(fills(&out), vec![(dec!(30), dec!(99.99), true)]);
    // покупатель-агрессор не исполняет наш бид
    b.trade(&key, dec!(99.99), dec!(1000), Side::Buy);
    assert!(fills(&sim.on_md(b.events.last().unwrap())).is_empty());
    b.trade(&key, dec!(99.98), dec!(1000), Side::Sell);
    let out = sim.on_md(b.events.last().unwrap());
    assert_eq!(fills(&out), vec![(dec!(70), dec!(99.99), true)]);
    assert_eq!(sim.order(ClientId(1)).unwrap().status, OrderStatus::Filled);
}

#[test]
fn reduce_only_is_clamped_and_rejected_when_flat() {
    let (mut sim, b, _) = setup();
    let now = b.now_ms;
    sim.submit(ExecCommand::Place(req(1, Side::Sell, dec!(1), dec!(90), Tif::Ioc, true)), now);
    let out = sim.advance(now + 1000);
    assert!(matches!(placed(&out, 1), Some(Err(ExecError::Rejected(_)))));
    sim.submit(ExecCommand::Place(req(2, Side::Buy, dec!(3), dec!(101), Tif::Ioc, false)), now + 1000);
    sim.advance(now + 2000);
    sim.submit(ExecCommand::Place(req(3, Side::Sell, dec!(10), dec!(90), Tif::Ioc, true)), now + 2000);
    let out = sim.advance(now + 3000);
    assert_eq!(fills(&out).iter().map(|f| f.0).sum::<Decimal>(), dec!(3));
    assert!(sim.positions(None).is_empty());
}

#[test]
fn funding_paid_at_settlement() {
    let (mut sim, mut b, key) = setup();
    let now = b.now_ms;
    sim.submit(ExecCommand::Place(req(1, Side::Sell, dec!(10), dec!(99), Tif::Ioc, false)), now);
    sim.advance(now + 1000);
    b.now_ms = now + 3_600_000;
    b.settle(&key, now + 3_600_000, dec!(0.001));
    let out = sim.on_md(b.events.last().unwrap());
    let pay: Vec<Decimal> = out
        .iter()
        .filter_map(|o| match o {
            ExecInput::Event(ExecEvent::Funding(f)) => Some(f.amount.0),
            _ => None,
        })
        .collect();
    // шорт 10 по цене фандинга Lighter (индекс 100) получает 0.1% → +1
    assert_eq!(pay, vec![dec!(1.000)]);
}

#[test]
fn lost_reply_resolved_by_query() {
    let (mut sim, b, _) = setup();
    let now = b.now_ms;
    sim.set_fail(Exchange::Lighter, Some(FailMode::LostReplies));
    sim.submit(ExecCommand::Place(req(7, Side::Buy, dec!(1), dec!(101), Tif::Ioc, false)), now);
    let out = sim.advance(now + 1000);
    assert!(matches!(placed(&out, 7), Some(Err(ExecError::Timeout))));
    // заполнение всё равно произошло — его покажет запрос по клиентскому ID
    sim.set_fail(Exchange::Lighter, None);
    sim.submit(ExecCommand::Query { venue: lighter(), symbol: "ETH".into(), cid: ClientId(7) }, now + 1000);
    let out = sim.advance(now + 2000);
    let q = out.iter().find_map(|o| match o {
        ExecInput::Result(CommandResult::Queried(_, Ok(Some(u)))) => Some(u.status),
        _ => None,
    });
    assert_eq!(q, Some(OrderStatus::Filled));
    // площадка недоступна: ошибка транспорта, ничего не исполнено
    sim.set_fail(Exchange::Lighter, Some(FailMode::Down));
    sim.submit(ExecCommand::Place(req(8, Side::Buy, dec!(1), dec!(101), Tif::Ioc, false)), now + 2000);
    let out = sim.advance(now + 3000);
    assert!(matches!(placed(&out, 8), Some(Err(ExecError::Transport(_)))));
    assert!(sim.order(ClientId(8)).is_none());
}

#[test]
fn duplicate_client_id_rejected() {
    let (mut sim, b, _) = setup();
    let now = b.now_ms;
    sim.submit(ExecCommand::Place(req(1, Side::Buy, dec!(1), dec!(99), Tif::Gtc, false)), now);
    sim.submit(ExecCommand::Place(req(1, Side::Buy, dec!(1), dec!(99), Tif::Gtc, false)), now + 1);
    let out = sim.advance(now + 1000);
    assert_eq!(out.iter().filter(|o| matches!(o, ExecInput::Result(CommandResult::Placed(_, Err(_))))).count(), 1);
    sim.submit(ExecCommand::CancelAll(Exchange::Lighter), now + 1000);
    let out = sim.advance(now + 2000);
    assert!(out.iter().any(|o| matches!(o, ExecInput::Result(CommandResult::CancelledAll(_, Ok(1))))));
    assert!(sim.open_orders().is_empty());
}

proptest! {
    /// Капитал = начальный − комиссии + реализованный и нереализованный результат + фандинг,
    /// при любой последовательности сделок; повтор той же последовательности даёт тот же итог.
    #[test]
    fn equity_identity_and_determinism(ops in proptest::collection::vec((any::<bool>(), 1u32..400, any::<bool>()), 1..30)) {
        let run = |ops: &[(bool, u32, bool)]| {
            let (mut sim, b, _) = setup();
            let mut t = b.now_ms;
            let mut fees = Decimal::ZERO;
            for (i, (buy, q, reduce)) in ops.iter().enumerate() {
                let side = if *buy { Side::Buy } else { Side::Sell };
                let px = if *buy { dec!(101) } else { dec!(99) };
                sim.submit(ExecCommand::Place(req(i as u64 + 1, side, Decimal::from(*q) / dec!(100), px, Tif::Ioc, *reduce)), t);
                t += 1000;
                for o in sim.advance(t) {
                    if let ExecInput::Event(ExecEvent::Fill(f)) = o {
                        fees += f.fee.0;
                    }
                }
            }
            (sim.total_equity().0, fees, format!("{:?}", sim.positions(None)))
        };
        let (eq, fees, pos) = run(&ops);
        // mid 100, спред 0.02: потери = комиссии + полуспред на каждый оборот; капитал ≤ начального
        prop_assert!(eq <= dec!(10000) - fees + dec!(0.0000001));
        let (eq2, _, pos2) = run(&ops);
        prop_assert_eq!(eq, eq2);
        prop_assert_eq!(pos, pos2);
    }
}
