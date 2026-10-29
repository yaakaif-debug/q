//! Тесты разбора на записанных ответах Lighter (tests/fixtures).

// в интеграционных тестах паника на неожиданных данных — ожидаемое поведение
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rust_decimal::dec;
use sh_core::{Collateral, Px, Qty, RecvTs, Side, Usd};
use sh_md::{ApplyOutcome, SequencedBook};
use sh_venue_api::{FeeSource, InstrumentStatus, MdEventKind, PriceRule};
use sh_venue_lighter::normalize::{self, LtMsg, parse_ws};
use sh_venue_lighter::wire::{Fundings, OrderBookDetails};

fn fx(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

const RECV: RecvTs = RecvTs { wall_ns: 1, mono_ns: 2 };

#[test]
fn control_messages() {
    assert!(matches!(parse_ws(&fx("ws_connected.json")).unwrap(), LtMsg::Connected));
    assert!(matches!(parse_ws(&fx("ws_pong.json")).unwrap(), LtMsg::Pong));
}

#[test]
fn snapshot_then_consecutive_updates_stay_in_sync() {
    let LtMsg::OrderBook { market, snapshot, book, .. } = parse_ws(&fx("ws_order_book_snapshot.json")).unwrap() else {
        panic!()
    };
    assert_eq!(market, 0);
    assert!(snapshot);
    let mut sb = SequencedBook::new(16);
    let d = normalize::book_delta(&book);
    assert_eq!(sb.apply_snapshot(book.nonce, &d.bids, &d.asks), ApplyOutcome::Applied);
    for f in ["ws_order_book_update_1.json", "ws_order_book_update_2.json", "ws_order_book_update_3.json"] {
        let LtMsg::OrderBook { snapshot, book, .. } = parse_ws(&fx(f)).unwrap() else { panic!() };
        assert!(!snapshot);
        assert_eq!(sb.apply_delta(normalize::book_delta(&book)), ApplyOutcome::Applied, "{f}");
    }
    assert_eq!(sb.last_seq(), 26_023_132_440);
    assert!(!sb.book().is_crossed());
}

#[test]
fn skipped_update_is_a_gap() {
    let LtMsg::OrderBook { book, .. } = parse_ws(&fx("ws_order_book_snapshot.json")).unwrap() else { panic!() };
    let mut sb = SequencedBook::new(16);
    let d = normalize::book_delta(&book);
    sb.apply_snapshot(book.nonce, &d.bids, &d.asks);
    let LtMsg::OrderBook { book: u2, .. } = parse_ws(&fx("ws_order_book_update_2.json")).unwrap() else { panic!() };
    assert!(matches!(sb.apply_delta(normalize::book_delta(&u2)), ApplyOutcome::Gap { .. }));
}

#[test]
fn update_event_carries_nonces_and_zero_sizes() {
    let LtMsg::OrderBook { book, ts_ms, .. } = parse_ws(&fx("ws_order_book_update_1.json")).unwrap() else { panic!() };
    let ev = normalize::book_event("ETH", false, ts_ms, &book, RECV);
    let MdEventKind::BookDelta { asks, first_seq, last_seq, prev_seq, .. } = ev.kind else { panic!() };
    assert_eq!((first_seq, last_seq, prev_seq), (26_023_132_286, 26_023_132_340, Some(26_023_132_286)));
    // размер "0.0000" означает удаление уровня
    assert!(asks.iter().any(|l| l.px == Px(dec!(2338.76)) && l.qty.is_zero()));
    assert_eq!(ev.venue_ts_ms, Some(1_793_255_389_635));
}

#[test]
fn trades_include_liquidations() {
    let LtMsg::Trades { market, snapshot, trades } = parse_ws(&fx("ws_trade_snapshot.json")).unwrap() else { panic!() };
    assert_eq!(market, 0);
    assert!(snapshot);
    assert_eq!(trades.len(), 3);
    assert!(trades.iter().any(|t| t.kind == "liquidation"));
    let LtMsg::Trades { trades, .. } = parse_ws(&fx("ws_trade_update.json")).unwrap() else { panic!() };
    let ev = normalize::trades_event("ETH", &trades, RECV).unwrap();
    let MdEventKind::Trades(t) = ev.kind else { panic!() };
    assert_eq!(t[0].px, Px(dec!(2316.55)));
    assert_eq!(t[0].taker_side, Some(Side::Sell));
}

#[test]
fn market_stats_all_is_a_map_and_rates_are_percent() {
    let LtMsg::MarketStats(stats) = parse_ws(&fx("ws_market_stats_all_snapshot.json")).unwrap() else { panic!() };
    assert_eq!(stats.len(), 3);
    let eth = stats.iter().find(|s| s.market_id == 0).unwrap();
    assert_eq!(eth.symbol, "ETH");
    assert_eq!(eth.current_funding_rate, Some(dec!(0.0012)));
    let evs = normalize::stats_events(eth, Some(dec!(1)), RECV);
    let MdEventKind::Funding(f) = &evs[0].kind else { panic!() };
    // 0.0012% → 0.000012 за интервал
    assert_eq!(f.predicted_rate, Some(dec!(0.000012)));
    assert_eq!(f.settled_rate, Some(dec!(0.000009)));
    assert_eq!(f.settled_at_ms, Some(1_793_253_600_003));
    assert_eq!(f.next_funding_ms, Some(1_793_257_200_003));
    assert!(matches!(evs[2].kind, MdEventKind::OpenInterest { base: None, usd: Some(Usd(_)) }));
    let LtMsg::MarketStats(upd) = parse_ws(&fx("ws_market_stats_all_update.json")).unwrap() else { panic!() };
    assert!(!upd.is_empty());
}

#[test]
fn documented_single_market_stats_parse() {
    // пример `market_stats/0` из документации WebSocket
    let doc = r#"{"channel":"market_stats:0","market_stats":{"symbol":"ETH","market_id":0,"index_price":"1900.86","mark_price":"1899.99","mid_price":"1899.95","best_ask_price":"1899.95","best_bid_price":"1899.94","open_interest":"81885817.089747","open_interest_limit":"72057594037927936.000000","funding_clamp_small":"0.0500","funding_clamp_big":"4.0000","last_trade_price":"1899.98","current_funding_rate":"0.0008","funding_rate":"0.0008","funding_timestamp":1786370400000,"daily_base_token_volume":79940.2167,"daily_quote_token_volume":153191468.329297,"daily_price_low":1893.51,"daily_price_high":1937.78,"daily_price_change":-1.332599347722221,"base_interest_rate":"0.0100","premium":"-0.0429"},"timestamp":1786372092759,"type":"update/market_stats"}"#;
    let LtMsg::MarketStats(s) = parse_ws(doc).unwrap() else { panic!() };
    assert_eq!(s[0].mark_price, Some(Px(dec!(1899.99))));
}

#[test]
fn order_book_details_metadata() {
    let d: OrderBookDetails = serde_json::from_str(&fx("rest_order_book_details.json")).unwrap();
    let eth = d.order_book_details.iter().find(|m| m.market_id == 0).unwrap();
    let m = normalize::instrument(eth, Some(dec!(1)));
    assert_eq!(m.symbol, "ETH");
    assert_eq!(m.venue.to_string(), "lighter");
    assert_eq!(m.collateral, Collateral::Usdc);
    assert_eq!(m.price_rule, PriceRule::Tick { tick: Px(dec!(0.01)) });
    assert_eq!(m.lot_size, Qty(dec!(0.0001)));
    assert_eq!(m.fees.source, FeeSource::Metadata);
    assert_eq!(m.status, InstrumentStatus::Active);
    assert_eq!(m.funding_interval_hours, Some(dec!(1)));
    assert!(m.max_leverage.is_some());
    let inactive = d.order_book_details.iter().find(|m| m.status == "inactive").unwrap();
    assert_eq!(normalize::instrument(inactive, None).status, InstrumentStatus::Inactive);
}

#[test]
fn fundings_rest_seconds_infer_hourly() {
    let f: Fundings = serde_json::from_str(&fx("rest_fundings.json")).unwrap();
    let ts: Vec<i64> = f.fundings.iter().map(|x| x.timestamp * 1000).collect();
    assert_eq!(sh_venue_api::infer_interval_ms(&ts), Some(3_600_000));
}
