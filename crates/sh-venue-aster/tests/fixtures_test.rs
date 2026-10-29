//! Тесты разбора на записанных ответах Aster (tests/fixtures) и примерах документации.

// в интеграционных тестах паника на неожиданных данных — ожидаемое поведение
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rust_decimal::dec;
use sh_core::{Collateral, Px, Qty, RecvTs, Side};
use sh_md::{ApplyOutcome, SequencedBook};
use sh_venue_api::{FeeSource, InstrumentStatus, MdEventKind, PriceRule};
use sh_venue_aster::normalize::{self, AsMsg, parse_ws};
use sh_venue_aster::wire::{DepthSnapshot, ExchangeInfo, FundingInfo, FundingRateItem, OpenInterest, PremiumIndex};

fn fx(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

const RECV: RecvTs = RecvTs { wall_ns: 1, mono_ns: 2 };

fn depth(name: &str) -> sh_venue_aster::wire::DepthUpdate {
    match parse_ws(&fx(name)).unwrap() {
        AsMsg::Depth(d) => d,
        other => panic!("{other:?}"),
    }
}

#[test]
fn live_depth_chain_is_contiguous_by_pu() {
    let d1 = depth("ws_depth_update_1.json");
    let d2 = depth("ws_depth_update_2.json");
    let d3 = depth("ws_depth_update_3.json");
    assert_eq!(d2.prev_id, d1.last_id);
    assert_eq!(d3.prev_id, d2.last_id);
    // снимок, совпадающий с последним id первого события: дальше цепочка по pu
    let mut b = SequencedBook::new(16);
    b.apply_snapshot(d1.last_id, &[], &[]);
    assert_eq!(b.apply_delta(normalize::book_delta(&d2)), ApplyOutcome::Applied);
    assert_eq!(b.apply_delta(normalize::book_delta(&d3)), ApplyOutcome::Applied);
    // пропуск d2: d3 после снимка на d1 — разрыв
    let mut b = SequencedBook::new(16);
    b.apply_snapshot(d1.last_id, &[], &[]);
    assert!(matches!(b.apply_delta(normalize::book_delta(&d3)), ApplyOutcome::Gap { .. }));
    // повтор уже применённого события — устаревшее
    let mut b = SequencedBook::new(16);
    b.apply_snapshot(d1.last_id, &[], &[]);
    b.apply_delta(normalize::book_delta(&d2));
    assert_eq!(b.apply_delta(normalize::book_delta(&d1)), ApplyOutcome::Stale);
}

#[test]
fn documented_sync_procedure() {
    // пример depthUpdate из документации (U=157, u=160, pu=149) и снимок lastUpdateId=158:
    // U <= lastUpdateId <= u → первое событие применяется
    let doc = r#"{"stream":"btcusdt@depth","data":{"e":"depthUpdate","E":123456789,"T":123456788,"s":"BTCUSDT","U":157,"u":160,"pu":149,"b":[["0.0024","10"]],"a":[["0.0026","100"]]}}"#;
    let AsMsg::Depth(d) = parse_ws(doc).unwrap() else { panic!() };
    let mut b = SequencedBook::new(16);
    assert_eq!(b.apply_delta(normalize::book_delta(&d)), ApplyOutcome::Buffered);
    let snap: DepthSnapshot =
        serde_json::from_str(r#"{"lastUpdateId":158,"E":1,"T":1,"bids":[["0.0023","5"]],"asks":[["0.0026","1"]]}"#)
            .unwrap();
    let (bids, asks) = normalize::snapshot_levels(&snap);
    assert_eq!(b.apply_snapshot(snap.last_update_id, &bids, &asks), ApplyOutcome::Applied);
    assert_eq!(b.last_seq(), 160);
    assert_eq!(b.book().best_ask().unwrap().qty, Qty(dec!(100)));
    assert_eq!(b.book().best_bid().unwrap().px, Px(dec!(0.0024)));
}

#[test]
fn delta_event_and_zero_quantities() {
    let d = depth("ws_depth_update_1.json");
    let ev = normalize::delta_event(&d, RECV);
    let MdEventKind::BookDelta { bids, prev_seq, .. } = ev.kind else { panic!() };
    assert_eq!(prev_seq, Some(625_798_089_023));
    assert!(bids.iter().any(|l| l.qty.is_zero()));
    assert_eq!(ev.venue_ts_ms, Some(1_793_255_420_050));
}

#[test]
fn rest_depth_snapshot() {
    let s: DepthSnapshot = serde_json::from_str(&fx("rest_depth_btc.json")).unwrap();
    let ev = normalize::snapshot_event("BTCUSDT", &s, RECV);
    let MdEventKind::BookSnapshot { bids, asks, seq } = ev.kind else { panic!() };
    assert_eq!(seq, Some(s.last_update_id));
    assert!(bids[0].px < asks[0].px);
}

#[test]
fn agg_trade_side() {
    let AsMsg::AggTrade(t) = parse_ws(&fx("ws_agg_trade.json")).unwrap() else { panic!() };
    let MdEventKind::Trades(v) = normalize::trade_event(&t, RECV).kind else { panic!() };
    // m = true: покупатель — мейкер, агрессор продавал
    assert_eq!(v[0].taker_side, Some(Side::Sell));
    assert_eq!(v[0].px, Px(dec!(80077.7)));
}

#[test]
fn mark_price_array_and_book_ticker() {
    let AsMsg::MarkPrices(v) = parse_ws(&fx("ws_mark_price_arr.json")).unwrap() else { panic!() };
    assert_eq!(v.len(), 3);
    let btc = v.iter().find(|m| m.symbol == "BTCUSDT").unwrap();
    let evs = normalize::mark_events(btc, Some(dec!(8)), RECV);
    let MdEventKind::Funding(f) = &evs[0].kind else { panic!() };
    assert_eq!(f.predicted_rate, Some(dec!(0.00005371)));
    assert_eq!(f.interval_hours, Some(dec!(8)));
    assert_eq!(f.next_funding_ms, Some(1_793_260_800_000));
    let AsMsg::BookTicker(b) = parse_ws(&fx("ws_book_ticker.json")).unwrap() else { panic!() };
    let MdEventKind::Bbo { bid, ask } = normalize::bbo_event(&b, RECV).kind else { panic!() };
    assert!(bid.unwrap().px < ask.unwrap().px);
}

#[test]
fn exchange_info_filters_and_collateral() {
    let ei: ExchangeInfo = serde_json::from_str(&fx("rest_exchange_info.json")).unwrap();
    let btc = ei.symbols.iter().find(|s| s.symbol == "BTCUSDT").unwrap();
    let m = normalize::instrument(btc, Some(dec!(8))).unwrap();
    assert!(matches!(m.price_rule, PriceRule::Tick { .. }));
    assert!(m.lot_size.is_positive());
    assert!(m.min_notional.is_some());
    assert_eq!(m.collateral, Collateral::Usdt);
    assert_eq!(m.fees.source, FeeSource::AccountRequired);
    assert_eq!(m.status, InstrumentStatus::Active);
    // не все контракты маржинальны в USDT
    let usd1 = ei.symbols.iter().find(|s| s.symbol == "SOXLUSD1").unwrap();
    assert_eq!(normalize::instrument(usd1, None).unwrap().collateral, Collateral::Other("USD1".into()));
    let halted = ei.symbols.iter().find(|s| s.status != "TRADING").unwrap();
    assert!(!normalize::is_recordable(halted));
}

#[test]
fn funding_rest_endpoints() {
    let fi: Vec<FundingInfo> = serde_json::from_str(&fx("rest_funding_info.json")).unwrap();
    assert!(fi.iter().any(|f| f.symbol == "BTCUSDT" && f.funding_interval_hours == dec!(8)));
    let pi: Vec<PremiumIndex> = serde_json::from_str(&fx("rest_premium_index.json")).unwrap();
    assert_eq!(pi.len(), 2);
    let fr: Vec<FundingRateItem> = serde_json::from_str(&fx("rest_funding_rate_btc.json")).unwrap();
    let ts: Vec<i64> = fr.iter().map(|f| f.funding_time).collect();
    assert_eq!(sh_venue_api::infer_interval_ms(&ts), Some(8 * 3_600_000));
    let oi: OpenInterest = serde_json::from_str(&fx("rest_open_interest_btc.json")).unwrap();
    assert!(oi.open_interest.is_positive());
}
