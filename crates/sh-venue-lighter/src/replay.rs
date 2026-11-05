//! Повтор записанных сообщений Lighter: метаданные из `orderBookDetails`, интервалы из
//! `fundings` и `market_stats`, стаканы с той же проверкой `nonce`, что у живого адаптера.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use sh_core::{Exchange, RawRecord, RawSource, SysEvent};
use sh_md::{ApplyOutcome, SequencedBook};
use sh_venue_api::{FundingIntervalTracker, FundingUpdate, MdEvent, MdEventKind, Replay, query_param};

use crate::normalize::{self, LtMsg};
use crate::wire::{Fundings, OrderBookDetails, PerpDetail};

/// Повтор Lighter.
#[derive(Default)]
pub struct LtReplay {
    markets: HashMap<u32, PerpDetail>,
    trackers: HashMap<u32, FundingIntervalTracker>,
    books: HashMap<(String, u32), SequencedBook>,
}

impl LtReplay {
    /// Новый повтор.
    pub fn new() -> Self {
        Self::default()
    }

    fn symbol(&self, market: u32) -> String {
        self.markets.get(&market).map(|d| d.symbol.clone()).unwrap_or_else(|| format!("market{market}"))
    }

    fn invalidated(&self, market: u32, rec: &RawRecord) -> MdEvent {
        MdEvent {
            venue: normalize::venue(),
            symbol: self.symbol(market),
            recv: rec.recv,
            venue_ts_ms: None,
            kind: MdEventKind::BookInvalidated,
        }
    }
}

impl Replay for LtReplay {
    fn exchange(&self) -> Exchange {
        Exchange::Lighter
    }

    fn feed(&mut self, rec: &RawRecord, out: &mut Vec<MdEvent>) {
        match &rec.source {
            RawSource::Rest { ep } => {
                if ep.contains("/orderBookDetails") {
                    let Ok(d) = serde_json::from_str::<OrderBookDetails>(&rec.payload) else { return };
                    for m in d.order_book_details {
                        let interval = self.trackers.get(&m.market_id).and_then(FundingIntervalTracker::interval_hours);
                        let meta = normalize::instrument(&m, interval);
                        out.push(MdEvent {
                            venue: normalize::venue(),
                            symbol: m.symbol.clone(),
                            recv: rec.recv,
                            venue_ts_ms: None,
                            kind: MdEventKind::Metadata(Box::new(meta)),
                        });
                        self.markets.insert(m.market_id, m);
                    }
                } else if ep.contains("/fundings") {
                    let Some(market) = query_param(ep, "market_id").and_then(|m| m.parse::<u32>().ok()) else { return };
                    let Ok(f) = serde_json::from_str::<Fundings>(&rec.payload) else { return };
                    let symbol = self.symbol(market);
                    let t = self.trackers.entry(market).or_default();
                    for it in &f.fundings {
                        t.observe(it.timestamp * 1000);
                    }
                    let interval = t.interval_hours();
                    for it in f.fundings {
                        out.push(MdEvent {
                            venue: normalize::venue(),
                            symbol: symbol.clone(),
                            recv: rec.recv,
                            venue_ts_ms: Some(it.timestamp * 1000),
                            kind: MdEventKind::Funding(FundingUpdate {
                                settled_rate: Some(normalize::pct(it.rate)),
                                settled_at_ms: Some(it.timestamp * 1000),
                                interval_hours: interval,
                                ..FundingUpdate::default()
                            }),
                        });
                    }
                }
            }
            RawSource::Sys => {
                if let Ok(SysEvent::ConnDown { .. }) = serde_json::from_str::<SysEvent>(&rec.payload) {
                    let gone: Vec<(String, u32)> = self.books.keys().filter(|(c, _)| *c == rec.conn).cloned().collect();
                    for k in gone {
                        if self.books.remove(&k).is_some_and(|b| b.is_synced()) {
                            out.push(self.invalidated(k.1, rec));
                        }
                    }
                }
            }
            RawSource::Out => {}
            RawSource::Ws => match normalize::parse_ws(&rec.payload) {
                Ok(LtMsg::OrderBook { market, snapshot, ts_ms, book }) => {
                    let symbol = self.symbol(market);
                    let sb = self.books.entry((rec.conn.clone(), market)).or_insert_with(|| SequencedBook::new(256));
                    let d = normalize::book_delta(&book);
                    if snapshot {
                        sb.apply_snapshot(book.nonce, &d.bids, &d.asks);
                        out.push(normalize::book_event(&symbol, true, ts_ms, &book, rec.recv));
                    } else {
                        match sb.apply_delta(d) {
                            ApplyOutcome::Applied => {
                                out.push(normalize::book_event(&symbol, false, ts_ms, &book, rec.recv))
                            }
                            ApplyOutcome::Gap { .. } => out.push(self.invalidated(market, rec)),
                            ApplyOutcome::Buffered | ApplyOutcome::Stale => {}
                        }
                    }
                }
                Ok(LtMsg::Trades { market, trades, .. }) => {
                    if let Some(e) = normalize::trades_event(&self.symbol(market), &trades, rec.recv) {
                        out.push(e);
                    }
                }
                Ok(LtMsg::MarketStats(stats)) => {
                    for s in &stats {
                        let t = self.trackers.entry(s.market_id).or_default();
                        if let Some(ts) = s.funding_timestamp.filter(|t| *t > 0) {
                            t.observe(ts);
                        }
                        out.extend(normalize::stats_events(s, t.interval_hours(), rec.recv));
                    }
                }
                Ok(LtMsg::Ticker { market, ts_ms, ticker }) => {
                    out.push(normalize::ticker_event(&self.symbol(market), ts_ms, &ticker, rec.recv));
                }
                _ => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sh_core::RecvTs;

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn rec(source: RawSource, conn: &str, payload: String) -> RawRecord {
        RawRecord {
            exchange: Exchange::Lighter,
            source,
            conn: conn.into(),
            recv: RecvTs { wall_ns: 1, mono_ns: 1 },
            payload,
        }
    }

    #[test]
    fn replays_books_stats_and_metadata() {
        let mut r = LtReplay::new();
        let mut out = Vec::new();
        r.feed(
            &rec(
                RawSource::Rest { ep: "GET /api/v1/fundings?market_id=0&resolution=1h".into() },
                "rest",
                fx("rest_fundings.json"),
            ),
            &mut out,
        );
        assert!(!out.is_empty());
        out.clear();
        r.feed(
            &rec(
                RawSource::Rest { ep: "GET /api/v1/orderBookDetails?filter=perp".into() },
                "rest",
                fx("rest_order_book_details.json"),
            ),
            &mut out,
        );
        let eth = out.iter().find(|e| e.symbol == "ETH").unwrap();
        let MdEventKind::Metadata(m) = &eth.kind else { panic!() };
        assert_eq!(m.funding_interval_hours, Some(rust_decimal::Decimal::ONE));
        out.clear();
        for f in [
            "ws_order_book_snapshot.json",
            "ws_order_book_update_1.json",
            "ws_order_book_update_2.json",
            "ws_order_book_update_3.json",
        ] {
            r.feed(&rec(RawSource::Ws, "lt-ws-0", fx(f)), &mut out);
        }
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|e| e.symbol == "ETH"));
        out.clear();
        r.feed(&rec(RawSource::Ws, "lt-ws-0", fx("ws_market_stats_all_snapshot.json")), &mut out);
        assert_eq!(out.len(), 9);
        out.clear();
        r.feed(&rec(RawSource::Sys, "lt-ws-0", SysEvent::ConnDown { reason: "x".into() }.to_json()), &mut out);
        assert!(matches!(out[0].kind, MdEventKind::BookInvalidated));
    }
}
