//! Повтор записанных сообщений Hyperliquid в нормализованные события.
//! Метаданные дексов восстанавливаются из записанных ответов `perpDexs`, `spotMeta`
//! и `metaAndAssetCtxs`, интервал HIP-3 — из `fundingHistory`, интервал ядра —
//! из `predictedFundings`, как в живом адаптере.

use std::collections::{HashMap, HashSet};

use rust_decimal::Decimal;
use sh_core::{Exchange, RawRecord, RawSource, SysEvent, VenueKey};
use sh_venue_api::{FundingIntervalTracker, FundingUpdate, MdEvent, MdEventKind, Replay, rest_body};

use crate::normalize::{self, DexState, HlMsg};
use crate::wire::{FundingHistoryItem, MetaAndAssetCtxs, PerpDexInfo, PredictedFundings, SpotMeta};

/// Повтор Hyperliquid.
#[derive(Default)]
pub struct HlReplay {
    dex_index: HashMap<String, usize>,
    spot: Option<SpotMeta>,
    dexes: HashMap<String, DexState>,
    trackers: HashMap<String, FundingIntervalTracker>,
    core_interval: Option<Decimal>,
    synced: HashMap<String, HashSet<String>>,
}

impl HlReplay {
    /// Новый повтор.
    pub fn new() -> Self {
        Self::default()
    }

    fn interval_of(&self, dex: &str) -> Option<Decimal> {
        if dex.is_empty()
            && let Some(i) = self.core_interval
        {
            return Some(i);
        }
        // самый частый интервал по монетам декса
        let mut counts: HashMap<Decimal, usize> = HashMap::new();
        for (coin, t) in &self.trackers {
            if normalize::dex_of_coin(coin) == dex
                && let Some(i) = t.interval_hours()
            {
                *counts.entry(i).or_default() += 1;
            }
        }
        counts.into_iter().max_by_key(|(i, n)| (*n, std::cmp::Reverse(*i))).map(|(i, _)| i)
    }

    fn rest(&mut self, ep: &str, payload: &str, rec: &RawRecord, out: &mut Vec<MdEvent>) {
        let Some(body) = rest_body(ep) else { return };
        let kind = body["type"].as_str().unwrap_or_default();
        match kind {
            "perpDexs" => {
                if let Ok(d) = serde_json::from_str::<Vec<Option<PerpDexInfo>>>(payload) {
                    self.dex_index = d
                        .iter()
                        .enumerate()
                        .map(|(i, x)| (x.as_ref().map(|x| x.name.clone()).unwrap_or_default(), i))
                        .collect();
                }
            }
            "spotMeta" => self.spot = serde_json::from_str(payload).ok(),
            "metaAndAssetCtxs" => {
                let dex = body["dex"].as_str().unwrap_or_default().to_owned();
                let Ok(MetaAndAssetCtxs(meta, ctxs)) = serde_json::from_str(payload) else { return };
                let empty = SpotMeta { tokens: Vec::new() };
                let spot = self.spot.as_ref().unwrap_or(&empty);
                let index = self.dex_index.get(&dex).copied().unwrap_or(0);
                let state = normalize::dex_state(&dex, index, &meta, spot, self.interval_of(&dex));
                for m in state.instruments() {
                    out.push(ev(&m.venue, &m.symbol, rec, MdEventKind::Metadata(Box::new(m.clone()))));
                }
                for (i, ctx) in ctxs.iter().enumerate() {
                    if let Some(a) = state.universe.get(i) {
                        out.extend(normalize::ctx_events(&state, &a.name, ctx, rec.recv, None));
                    }
                }
                self.dexes.insert(dex, state);
            }
            "fundingHistory" => {
                let Ok(items) = serde_json::from_str::<Vec<FundingHistoryItem>>(payload) else { return };
                for it in items {
                    let t = self.trackers.entry(it.coin.clone()).or_default();
                    t.observe(it.time);
                    let interval = t.interval_hours();
                    out.push(ev(
                        &normalize::venue_of_coin(&it.coin),
                        &it.coin,
                        rec,
                        MdEventKind::Funding(FundingUpdate {
                            settled_rate: Some(it.funding_rate),
                            settled_at_ms: Some(it.time),
                            interval_hours: interval,
                            ..FundingUpdate::default()
                        }),
                    ));
                }
            }
            "predictedFundings" => {
                let Ok(pred) = serde_json::from_str::<PredictedFundings>(payload) else { return };
                for (coin, venues) in pred {
                    let Some(hl) = venues.iter().find(|(v, _)| v == "HlPerp").and_then(|(_, f)| f.as_ref()) else {
                        continue;
                    };
                    self.core_interval = self.core_interval.or(hl.funding_interval_hours);
                    out.push(ev(
                        &VenueKey::hyperliquid(""),
                        &coin,
                        rec,
                        MdEventKind::Funding(FundingUpdate {
                            predicted_rate: Some(hl.funding_rate),
                            next_funding_ms: Some(hl.next_funding_time),
                            interval_hours: hl.funding_interval_hours,
                            ..FundingUpdate::default()
                        }),
                    ));
                }
            }
            _ => {}
        }
    }
}

fn ev(venue: &VenueKey, symbol: &str, rec: &RawRecord, kind: MdEventKind) -> MdEvent {
    MdEvent { venue: venue.clone(), symbol: symbol.to_owned(), recv: rec.recv, venue_ts_ms: None, kind }
}

impl Replay for HlReplay {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    fn feed(&mut self, rec: &RawRecord, out: &mut Vec<MdEvent>) {
        match &rec.source {
            RawSource::Rest { ep } => self.rest(ep, &rec.payload, rec, out),
            RawSource::Sys => {
                if let Ok(SysEvent::ConnDown { .. }) = serde_json::from_str::<SysEvent>(&rec.payload) {
                    for coin in self.synced.remove(&rec.conn).unwrap_or_default() {
                        out.push(ev(&normalize::venue_of_coin(&coin), &coin, rec, MdEventKind::BookInvalidated));
                    }
                }
            }
            RawSource::Out => {}
            RawSource::Ws => match normalize::parse_ws(&rec.payload) {
                Ok(HlMsg::Book(b)) => {
                    self.synced.entry(rec.conn.clone()).or_default().insert(b.coin.clone());
                    out.push(normalize::book_event(&b, rec.recv));
                }
                Ok(HlMsg::Trades(t)) => out.extend(normalize::trade_events(&t, rec.recv)),
                Ok(HlMsg::Bbo(b)) => out.push(normalize::bbo_event(&b, rec.recv)),
                Ok(HlMsg::AllDexsAssetCtxs(all)) => {
                    for (dex, ctxs) in &all.ctxs {
                        let Some(state) = self.dexes.get(dex) else { continue };
                        for (i, ctx) in ctxs.iter().enumerate() {
                            if let Some(a) = state.universe.get(i) {
                                out.extend(normalize::ctx_events(state, &a.name, ctx, rec.recv, None));
                            }
                        }
                    }
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
            exchange: Exchange::Hyperliquid,
            source,
            conn: conn.into(),
            recv: RecvTs { wall_ns: 1, mono_ns: 1 },
            payload,
        }
    }

    fn rest(body: &str, payload: String) -> RawRecord {
        rec(RawSource::Rest { ep: format!("POST /info {body}") }, "rest", payload)
    }

    #[test]
    fn rebuilds_metadata_books_and_ctxs() {
        let mut r = HlReplay::new();
        let mut out = Vec::new();
        r.feed(&rest(r#"{"type":"perpDexs"}"#, fx("rest_perp_dexs.json")), &mut out);
        r.feed(&rest(r#"{"type":"spotMeta"}"#, fx("rest_spot_meta.json")), &mut out);
        r.feed(&rest(r#"{"dex":"xyz","type":"metaAndAssetCtxs"}"#, fx("rest_meta_and_asset_ctxs_xyz.json")), &mut out);
        let metas: Vec<&MdEvent> = out.iter().filter(|e| matches!(e.kind, MdEventKind::Metadata(_))).collect();
        assert_eq!(metas.len(), 3);
        let MdEventKind::Metadata(m) = &metas[0].kind else { panic!() };
        // индекс декса xyz из perpDexs = 1
        assert_eq!(m.venue_id, Some(110_000));
        out.clear();
        r.feed(&rec(RawSource::Ws, "hl-ws-0", fx("ws_all_dexs_asset_ctxs.json")), &mut out);
        assert!(out.iter().any(|e| e.symbol == "xyz:XYZ100" && matches!(e.kind, MdEventKind::Funding(_))));
        out.clear();
        r.feed(&rec(RawSource::Ws, "hl-ws-0", fx("ws_l2book_btc.json")), &mut out);
        assert!(matches!(out[0].kind, MdEventKind::BookSnapshot { .. }));
        out.clear();
        r.feed(&rec(RawSource::Sys, "hl-ws-0", SysEvent::ConnDown { reason: "x".into() }.to_json()), &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].kind, MdEventKind::BookInvalidated));
    }

    #[test]
    fn funding_history_infers_interval() {
        let mut r = HlReplay::new();
        let mut out = Vec::new();
        r.feed(
            &rest(
                r#"{"coin":"xyz:NVDA","startTime":0,"type":"fundingHistory"}"#,
                fx("rest_funding_history_xyz_nvda.json"),
            ),
            &mut out,
        );
        assert_eq!(out.len(), 4);
        let MdEventKind::Funding(f) = &out[3].kind else { panic!() };
        assert_eq!(f.interval_hours, Some(Decimal::ONE));
        assert_eq!(r.interval_of("xyz"), Some(Decimal::ONE));
        out.clear();
        r.feed(&rest(r#"{"type":"predictedFundings"}"#, fx("rest_predicted_fundings.json")), &mut out);
        assert_eq!(r.interval_of(""), Some(Decimal::ONE));
    }
}
