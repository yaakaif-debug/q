//! Повтор записанных сообщений Aster: метаданные из `exchangeInfo` и `fundingInfo`,
//! стаканы по дельтам с синхронизацией по записанным REST-снимкам `depth`,
//! ОИ из `openInterest`, расчёты из `fundingRate`.

use std::collections::HashMap;

use rust_decimal::Decimal;
use sh_core::{Exchange, RawRecord, RawSource, SysEvent};
use sh_md::{ApplyOutcome, SequencedBook};
use sh_venue_api::{FundingUpdate, MdEvent, MdEventKind, Replay, query_param};

use crate::normalize::{self, AsMsg};
use crate::wire::{DepthSnapshot, ExchangeInfo, FundingInfo, FundingRateItem, OpenInterest, SymbolInfo};

/// Повтор Aster.
#[derive(Default)]
pub struct AsReplay {
    symbols: Vec<SymbolInfo>,
    intervals: HashMap<String, Decimal>,
    books: HashMap<String, SequencedBook>,
    owner: HashMap<String, String>,
}

impl AsReplay {
    /// Новый повтор.
    pub fn new() -> Self {
        Self::default()
    }

    fn emit_metadata(&self, rec: &RawRecord, out: &mut Vec<MdEvent>) {
        for s in &self.symbols {
            if let Some(m) = normalize::instrument(s, self.intervals.get(&s.symbol).copied()) {
                out.push(MdEvent {
                    venue: normalize::venue(),
                    symbol: s.symbol.clone(),
                    recv: rec.recv,
                    venue_ts_ms: None,
                    kind: MdEventKind::Metadata(Box::new(m)),
                });
            }
        }
    }

    fn event(symbol: &str, rec: &RawRecord, kind: MdEventKind) -> MdEvent {
        MdEvent { venue: normalize::venue(), symbol: symbol.to_owned(), recv: rec.recv, venue_ts_ms: None, kind }
    }
}

impl Replay for AsReplay {
    fn exchange(&self) -> Exchange {
        Exchange::Aster
    }

    fn feed(&mut self, rec: &RawRecord, out: &mut Vec<MdEvent>) {
        match &rec.source {
            RawSource::Rest { ep } => {
                if ep.contains("/exchangeInfo") {
                    if let Ok(ei) = serde_json::from_str::<ExchangeInfo>(&rec.payload) {
                        self.symbols = ei.symbols;
                        self.emit_metadata(rec, out);
                    }
                } else if ep.contains("/fundingInfo") {
                    if let Ok(fi) = serde_json::from_str::<Vec<FundingInfo>>(&rec.payload) {
                        self.intervals = fi.into_iter().map(|f| (f.symbol, f.funding_interval_hours)).collect();
                        self.emit_metadata(rec, out);
                    }
                } else if ep.contains("/depth") {
                    let Some(symbol) = query_param(ep, "symbol") else { return };
                    let Ok(snap) = serde_json::from_str::<DepthSnapshot>(&rec.payload) else { return };
                    let (bids, asks) = normalize::snapshot_levels(&snap);
                    let book = self.books.entry(symbol.to_owned()).or_insert_with(|| SequencedBook::new(2048));
                    if !matches!(book.apply_snapshot(snap.last_update_id, &bids, &asks), ApplyOutcome::Gap { .. }) {
                        out.push(normalize::snapshot_event(symbol, &snap, rec.recv));
                    }
                } else if ep.contains("/openInterest") {
                    if let Ok(oi) = serde_json::from_str::<OpenInterest>(&rec.payload) {
                        out.push(Self::event(&oi.symbol.clone(), rec, MdEventKind::OpenInterest { base: Some(oi.open_interest), usd: None }));
                    }
                } else if ep.contains("/fundingRate")
                    && let Ok(items) = serde_json::from_str::<Vec<FundingRateItem>>(&rec.payload)
                {
                    for it in items {
                        let interval = self.intervals.get(&it.symbol).copied();
                        out.push(Self::event(
                            &it.symbol,
                            rec,
                            MdEventKind::Funding(FundingUpdate {
                                settled_rate: Some(it.funding_rate),
                                settled_at_ms: Some(it.funding_time),
                                interval_hours: interval,
                                ..FundingUpdate::default()
                            }),
                        ));
                    }
                }
            }
            RawSource::Sys => {
                if let Ok(SysEvent::ConnDown { .. }) = serde_json::from_str::<SysEvent>(&rec.payload) {
                    let gone: Vec<String> = self.owner.iter().filter(|(_, c)| **c == rec.conn).map(|(s, _)| s.clone()).collect();
                    for s in gone {
                        self.owner.remove(&s);
                        if self.books.remove(&s).is_some_and(|b| b.is_synced()) {
                            out.push(Self::event(&s, rec, MdEventKind::BookInvalidated));
                        }
                    }
                }
            }
            RawSource::Out => {}
            RawSource::Ws => match normalize::parse_ws(&rec.payload) {
                Ok(AsMsg::Depth(d)) => {
                    self.owner.insert(d.symbol.clone(), rec.conn.clone());
                    let delta = normalize::book_delta(&d);
                    let book = self.books.entry(d.symbol.clone()).or_insert_with(|| SequencedBook::new(2048));
                    match book.apply_delta(delta.clone()) {
                        ApplyOutcome::Applied => out.push(normalize::delta_event(&d, rec.recv)),
                        ApplyOutcome::Gap { .. } => {
                            book.apply_delta(delta);
                            out.push(Self::event(&d.symbol, rec, MdEventKind::BookInvalidated));
                        }
                        ApplyOutcome::Buffered | ApplyOutcome::Stale => {}
                    }
                }
                Ok(AsMsg::AggTrade(t)) => out.push(normalize::trade_event(&t, rec.recv)),
                Ok(AsMsg::MarkPrices(v)) => {
                    for m in &v {
                        out.extend(normalize::mark_events(m, self.intervals.get(&m.symbol).copied(), rec.recv));
                    }
                }
                Ok(AsMsg::BookTicker(b)) => out.push(normalize::bbo_event(&b, rec.recv)),
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

    fn rec(source: RawSource, payload: String) -> RawRecord {
        RawRecord { exchange: Exchange::Aster, source, conn: "as-ws-1".into(), recv: RecvTs { wall_ns: 1, mono_ns: 1 }, payload }
    }

    #[test]
    fn replays_metadata_depth_sync_and_funding() {
        let mut r = AsReplay::new();
        let mut out = Vec::new();
        r.feed(&rec(RawSource::Rest { ep: "GET /fapi/v3/fundingInfo".into() }, fx("rest_funding_info.json")), &mut out);
        out.clear();
        r.feed(&rec(RawSource::Rest { ep: "GET /fapi/v3/exchangeInfo".into() }, fx("rest_exchange_info.json")), &mut out);
        let btc = out.iter().find(|e| e.symbol == "BTCUSDT").unwrap();
        let MdEventKind::Metadata(m) = &btc.kind else { panic!() };
        assert_eq!(m.funding_interval_hours, Some(Decimal::from(8)));
        out.clear();
        // дельта до снимка буферизуется; снимок с id, совпадающим с u первой дельты, синхронизирует
        r.feed(&rec(RawSource::Ws, fx("ws_depth_update_1.json")), &mut out);
        assert!(out.is_empty());
        let d1 = normalize::book_delta(&match normalize::parse_ws(&fx("ws_depth_update_1.json")).unwrap() {
            AsMsg::Depth(d) => d,
            _ => panic!(),
        });
        let snap = format!(r#"{{"lastUpdateId":{},"E":1,"T":1,"bids":[["80000","1"]],"asks":[["80200","1"]]}}"#, d1.seq.last);
        r.feed(&rec(RawSource::Rest { ep: "GET /fapi/v3/depth?symbol=BTCUSDT&limit=50".into() }, snap), &mut out);
        assert!(matches!(out[0].kind, MdEventKind::BookSnapshot { .. }));
        out.clear();
        r.feed(&rec(RawSource::Ws, fx("ws_depth_update_2.json")), &mut out);
        assert!(matches!(out[0].kind, MdEventKind::BookDelta { .. }));
        out.clear();
        r.feed(&rec(RawSource::Rest { ep: "GET /fapi/v3/fundingRate?symbol=BTCUSDT&limit=3".into() }, fx("rest_funding_rate_btc.json")), &mut out);
        assert_eq!(out.len(), 4);
        out.clear();
        r.feed(&rec(RawSource::Sys, SysEvent::ConnDown { reason: "x".into() }.to_json()), &mut out);
        assert!(matches!(out[0].kind, MdEventKind::BookInvalidated));
    }
}
