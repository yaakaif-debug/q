//! Задача пары S5 против симулятора: вход мейкером с хеджем на каждое заполнение,
//! разворот при отказе хеджа, выход reduce-only, сброс; инвариант непохеджированного объёма.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use rust_decimal::Decimal;
use rust_decimal::dec;
use sh_core::{Exchange, Px, Qty, Side, Usd, VenueKey};
use sh_exec::JobAction;
use sh_exec::{JobKind, JobLeg, JobParams, JobState, OrderTracker, PairJob, TrackerEvent};
use sh_md::InstrumentKey;
use sh_signals::testkit::{InstrumentSpec, MarketBuilder};
use sh_sim::{FailMode, SimCore, SimParams};
use sh_venue_api::{ClientId, ExecCommand, PriceRule};

struct Harness {
    b: MarketBuilder,
    sim: SimCore,
    tracker: OrderTracker,
    next: u64,
    maker_key: InstrumentKey,
    hedge_key: InstrumentKey,
}

fn leg(key: &InstrumentKey, side: Side, taker_fee: Decimal) -> JobLeg {
    JobLeg {
        venue: key.venue.clone(),
        symbol: key.symbol.clone(),
        side,
        lot: Qty(dec!(0.01)),
        min_qty: Qty(dec!(0.01)),
        price_rule: PriceRule::Tick { tick: Px(dec!(0.01)) },
        taker_fee,
    }
}

impl Harness {
    fn new() -> Self {
        let mut b = MarketBuilder::new();
        let m = InstrumentSpec::liquid("aster/ETHUSDT", dec!(0.0003), dec!(8));
        let h = InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.00001), dec!(1));
        b.add(&m, 2);
        b.add(&h, 2);
        let p = SimParams {
            initial_cash: BTreeMap::from([
                (VenueKey::new(Exchange::Aster), Usd(dec!(50000))),
                (VenueKey::hyperliquid(""), Usd(dec!(50000))),
            ]),
            ..SimParams::default()
        };
        let mut sim = SimCore::new(p);
        for e in &b.events {
            sim.on_md(e);
        }
        Self { b, sim, tracker: OrderTracker::new(), next: 1, maker_key: m.key, hedge_key: h.key }
    }

    fn job(&self, kind: JobKind, target: Decimal) -> PairJob {
        let (ms, hs) = match kind {
            JobKind::Entry => (Side::Sell, Side::Buy),
            _ => (Side::Buy, Side::Sell),
        };
        PairJob::new(
            1,
            "ETH:test",
            kind,
            [leg(&self.maker_key, ms, dec!(0.0004)), leg(&self.hedge_key, hs, dec!(0.00045))],
            0,
            target,
            JobParams::default(),
            self.b.now_ms,
        )
    }

    /// Один шаг на `dt` мс: задача → трекер → симулятор → трекер → задача.
    fn step(&mut self, job: &mut PairJob, dt: i64, allow: bool) {
        let now = self.b.now_ms;
        let mut next = self.next;
        let mut idgen = || {
            next += 1;
            ClientId(next)
        };
        let actions = job.step(now, &self.b.market, allow, &mut idgen);
        self.next = next;
        for a in actions {
            let cmd = match a {
                JobAction::Place(req) => Some(self.tracker.send(req, Some(job.id), now)),
                JobAction::Cancel(cid) => self.tracker.cancel(cid),
            };
            if let Some(c) = cmd {
                self.sim.submit(c, now);
            }
        }
        for c in self.tracker.tick(now) {
            self.sim.submit(c, now);
        }
        self.b.now_ms += dt;
        let out = self.sim.advance(self.b.now_ms);
        self.feed(job, &out);
    }

    fn feed(&mut self, job: &mut PairJob, out: &[sh_venue_api::ExecInput]) {
        for o in out {
            for ev in self.tracker.on_input(o, self.b.now_ms) {
                match ev {
                    TrackerEvent::Filled { fill, .. } => {
                        if let Some(c) = fill.cid {
                            job.on_fill(c, fill.qty.0);
                        }
                    }
                    TrackerEvent::Closed { cid, status, .. } => job.on_closed(cid, status),
                    _ => {}
                }
            }
        }
    }

    /// Покупатель-агрессор проходит по аскам мейкер-площадки до `px`.
    fn sweep_maker(&mut self, job: &mut PairJob, px: Decimal, qty: Decimal) {
        let key = self.maker_key.clone();
        self.b.trade(&key, px, qty, Side::Buy);
        let ev = self.b.events.last().unwrap().clone();
        let out = self.sim.on_md(&ev);
        self.feed(job, &out);
    }

    fn pos(&self, key: &InstrumentKey) -> Decimal {
        self.sim
            .positions(None)
            .iter()
            .find(|p| p.venue == key.venue && p.symbol == key.symbol)
            .map(|p| p.qty)
            .unwrap_or_default()
    }
}

#[test]
fn entry_posts_children_and_hedges_each_fill() {
    let mut h = Harness::new();
    let mut job = h.job(JobKind::Entry, dec!(120));
    for _ in 0..200 {
        h.step(&mut job, 100, true);
        h.sweep_maker(&mut job, dec!(100.2), dec!(100000));
        if job.is_done() {
            break;
        }
    }
    assert_eq!(job.state, JobState::Done);
    assert_eq!(job.filled, [dec!(120), dec!(120)]);
    assert_eq!(h.pos(&h.maker_key.clone()), dec!(-120));
    assert_eq!(h.pos(&h.hedge_key.clone()), dec!(120));
    // все дочерние — post-only не крупнее $5k
    let makers: Vec<_> = h.tracker.iter().filter(|o| o.req.venue.exchange == Exchange::Aster).collect();
    assert_eq!(makers.len(), 3);
    assert!(makers.iter().all(|o| o.req.tif == sh_venue_api::Tif::PostOnly && o.req.qty.0 <= dec!(50)));
}

#[test]
fn hedge_failure_unwinds_maker_leg() {
    let mut h = Harness::new();
    let mut job = h.job(JobKind::Entry, dec!(30));
    h.step(&mut job, 100, true);
    // хедж-площадка упала: IOC не доходит
    h.sim.set_fail(Exchange::Hyperliquid, Some(FailMode::Down));
    h.sweep_maker(&mut job, dec!(100.2), dec!(100000));
    assert_eq!(job.unhedged(), dec!(30));
    for _ in 0..50 {
        h.step(&mut job, 500, true);
        if job.is_done() {
            break;
        }
    }
    assert_eq!(job.state, JobState::Done);
    assert_eq!(h.pos(&h.maker_key.clone()), Decimal::ZERO);
    assert_eq!(job.unhedged(), Decimal::ZERO);
    // разворот — reduce-only IOC
    assert!(h.tracker.iter().any(|o| o.req.venue.exchange == Exchange::Aster && o.req.reduce_only));
}

#[test]
fn flatten_closes_both_legs_reduce_only() {
    let mut h = Harness::new();
    let mut job = h.job(JobKind::Entry, dec!(60));
    for _ in 0..100 {
        h.step(&mut job, 100, true);
        h.sweep_maker(&mut job, dec!(100.2), dec!(100000));
        if job.is_done() {
            break;
        }
    }
    let mut fl = h.job(JobKind::Flatten, dec!(60));
    fl.id = 2;
    for _ in 0..100 {
        h.step(&mut fl, 400, true);
        if fl.is_done() {
            break;
        }
    }
    assert_eq!(fl.state, JobState::Done);
    assert_eq!(h.pos(&h.maker_key.clone()), Decimal::ZERO);
    assert_eq!(h.pos(&h.hedge_key.clone()), Decimal::ZERO);
    let flatten_orders: Vec<_> = h.tracker.iter().filter(|o| o.job == Some(2)).collect();
    assert!(flatten_orders.iter().all(|o| o.req.reduce_only));
}

#[test]
fn no_new_child_while_risk_blocks() {
    let mut h = Harness::new();
    let mut job = h.job(JobKind::Entry, dec!(60));
    for _ in 0..10 {
        h.step(&mut job, 100, false);
    }
    assert_eq!(h.tracker.iter().count(), 0);
}

#[test]
fn lost_ack_goes_unknown_and_resolves_by_query() {
    let mut h = Harness::new();
    let mut job = h.job(JobKind::Entry, dec!(10));
    h.sim.set_fail(Exchange::Aster, Some(FailMode::Blackout));
    h.step(&mut job, 100, true);
    let cid = h.tracker.iter().next().unwrap().req.cid;
    assert_eq!(h.tracker.get(cid).unwrap().status, sh_venue_api::OrderStatus::Unknown);
    h.sim.set_fail(Exchange::Aster, None);
    // запрос уходит на следующем шаге; новый дочерний не ставится, пока ордер жив
    h.step(&mut job, 100, true);
    assert_eq!(h.tracker.get(cid).unwrap().status, sh_venue_api::OrderStatus::Acked);
    assert_eq!(h.tracker.iter().filter(|o| matches!(o.req.venue.exchange, Exchange::Aster)).count(), 1);
    let _ = ExecCommand::CancelAll(Exchange::Aster);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    /// При любых объёмах сделок по мейкер-ноге непохеджированный объём после хеджа
    /// не превышает одного дочернего ордера, а итог входа нейтрален по дельте.
    #[test]
    fn unhedged_bounded_by_one_child(sweeps in proptest::collection::vec(0u32..30_000, 5..40)) {
        let mut h = Harness::new();
        let mut job = h.job(JobKind::Entry, dec!(150));
        for q in sweeps {
            h.step(&mut job, 200, true);
            h.sweep_maker(&mut job, dec!(100.2), Decimal::from(q));
            prop_assert!(job.unhedged() <= dec!(50));
        }
        for _ in 0..50 {
            h.step(&mut job, 200, true);
            h.sweep_maker(&mut job, dec!(100.2), dec!(100000));
            if job.is_done() { break; }
        }
        prop_assert_eq!(job.state, JobState::Done);
        prop_assert_eq!(h.pos(&h.maker_key.clone()) + h.pos(&h.hedge_key.clone()), Decimal::ZERO);
    }
}
