//! Производные Parquet из сырых записей за сутки UTC (SPEC.md: «A nightly job derives
//! Parquet tables»; перенесено в M1): BBO на конец каждой секунды, где он менялся,
//! прогнозы и расчёты фандинга, снимки 20 уровней стакана и ОИ раз в минуту.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use sh_core::{Exchange, Side};
use sh_md::{InstrumentKey, MarketState};
use sh_store::parquet_out::{
    BboRow, BookLevelRow, FundingRow, OiRow, write_bbo, write_book_levels, write_funding, write_oi,
};
use sh_venue_api::{MdEvent, MdEventKind};

use crate::replay_source::{ReplaySource, Replayers};

fn f(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(f64::NAN)
}

/// Сводка выгрузки.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeriveSummary {
    /// Записей прочитано.
    pub records: u64,
    /// Строк BBO.
    pub bbo_rows: usize,
    /// Строк фандинга.
    pub funding_rows: usize,
    /// Строк уровней стакана.
    pub book_rows: usize,
    /// Строк ОИ.
    pub oi_rows: usize,
    /// Каталог результата.
    pub out_dir: PathBuf,
}

/// Накопитель строк из потока событий.
#[derive(Default)]
pub struct Deriver {
    market: MarketState,
    dirty_bbo: HashSet<InstrumentKey>,
    last_predicted: HashMap<InstrumentKey, Decimal>,
    current_sec: Option<i64>,
    last_minute: Option<i64>,
    /// BBO.
    pub bbo: Vec<BboRow>,
    /// Фандинг.
    pub funding: Vec<FundingRow>,
    /// Уровни стаканов.
    pub book: Vec<BookLevelRow>,
    /// ОИ.
    pub oi: Vec<OiRow>,
}

impl Deriver {
    /// Обрабатывает событие с временем получения `ts_ms`.
    pub fn on_event(&mut self, ev: &MdEvent) {
        let ts_ms = ev.recv.wall_ns.div_euclid(1_000_000);
        let sec = ts_ms.div_euclid(1000);
        if self.current_sec.is_some_and(|s| sec > s) {
            self.flush_second();
        }
        self.current_sec = Some(sec);
        let minute = sec.div_euclid(60);
        if self.last_minute.is_some_and(|m| minute > m) {
            self.snapshot_minute(sec);
        }
        self.last_minute = Some(minute);

        let key = InstrumentKey::new(ev.venue.clone(), ev.symbol.clone());
        if let MdEventKind::Funding(fu) = &ev.kind {
            let interval = fu.interval_hours.or_else(|| self.market.get(&key).and_then(|s| s.interval_hours()));
            if let Some(r) = fu.predicted_rate
                && self.last_predicted.get(&key) != Some(&r)
            {
                self.last_predicted.insert(key.clone(), r);
                self.funding.push(FundingRow {
                    ts_ms,
                    venue: ev.venue.to_string(),
                    symbol: ev.symbol.clone(),
                    kind: "predicted".into(),
                    rate: f(r),
                    interval_h: interval.map(f),
                    next_ms: fu.next_funding_ms,
                });
            }
        }
        let new = self.market.apply(ev);
        if let Some(ns) = new {
            self.funding.push(FundingRow {
                ts_ms: ns.settlement.ts_ms,
                venue: ev.venue.to_string(),
                symbol: ev.symbol.clone(),
                kind: "settled".into(),
                rate: f(ns.settlement.rate),
                interval_h: Some(f(ns.settlement.interval_hours)),
                next_ms: None,
            });
        }
        if matches!(ev.kind, MdEventKind::BookSnapshot { .. } | MdEventKind::BookDelta { .. } | MdEventKind::Bbo { .. })
        {
            self.dirty_bbo.insert(key);
        }
    }

    fn flush_second(&mut self) {
        let Some(sec) = self.current_sec else { return };
        let mut keys: Vec<InstrumentKey> = self.dirty_bbo.drain().collect();
        keys.sort();
        for k in keys {
            let Some(st) = self.market.get(&k) else { continue };
            let (bid, ask) = st.best();
            self.bbo.push(BboRow {
                ts_s: sec,
                venue: k.venue.to_string(),
                symbol: k.symbol.clone(),
                bid: bid.map(|l| f(l.px.0)),
                bid_qty: bid.map(|l| f(l.qty.0)),
                ask: ask.map(|l| f(l.px.0)),
                ask_qty: ask.map(|l| f(l.qty.0)),
            });
        }
    }

    fn snapshot_minute(&mut self, sec: i64) {
        let mut books = Vec::new();
        let mut ois = Vec::new();
        for (k, st) in self.market.iter() {
            if st.book_valid {
                for (side, name) in [(Side::Buy, "bid"), (Side::Sell, "ask")] {
                    for (i, l) in st.book.top(side, 20).iter().enumerate() {
                        books.push(BookLevelRow {
                            ts_s: sec,
                            venue: k.venue.to_string(),
                            symbol: k.symbol.clone(),
                            side: name.into(),
                            level: u16::try_from(i).unwrap_or(u16::MAX),
                            px: f(l.px.0),
                            qty: f(l.qty.0),
                        });
                    }
                }
            }
            if st.oi_base.is_some() || st.oi_usd.is_some() {
                ois.push(OiRow {
                    ts_s: sec,
                    venue: k.venue.to_string(),
                    symbol: k.symbol.clone(),
                    oi_base: st.oi_base.map(f),
                    oi_usd: st.oi_usd.map(|u| f(u.0)),
                    mark: st.mark.map(|p| f(p.0)),
                });
            }
        }
        self.book.extend(books);
        self.oi.extend(ois);
    }

    /// Завершает поток: последняя секунда.
    pub fn finish(&mut self) {
        self.flush_second();
    }
}

/// Строит Parquet за сутки `day_start_ms` из `raw_root` в `out_root/<YYYY-MM-DD>/`.
pub fn derive_day(raw_root: &Path, out_root: &Path, day_start_ms: i64) -> anyhow::Result<DeriveSummary> {
    let date = chrono::DateTime::from_timestamp_millis(day_start_ms)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    let out_dir = out_root.join(&date);
    let source = ReplaySource::open(raw_root, &Exchange::ALL, day_start_ms, day_start_ms + 86_400_000);
    let mut replayers = Replayers::new();
    let mut d = Deriver::default();
    let mut events = Vec::new();
    let mut records = 0u64;
    let mut per_venue: BTreeMap<&'static str, u64> = BTreeMap::new();
    for rec in source {
        records += 1;
        *per_venue.entry(rec.exchange.as_str()).or_default() += 1;
        events.clear();
        replayers.feed(&rec, &mut events);
        for ev in &events {
            d.on_event(ev);
        }
    }
    d.finish();
    tracing::info!(%date, records, ?per_venue, "derive input read");
    write_bbo(&out_dir.join("bbo_1s.parquet"), &d.bbo)?;
    write_funding(&out_dir.join("funding.parquet"), &d.funding)?;
    write_book_levels(&out_dir.join("book_snapshots.parquet"), &d.book)?;
    write_oi(&out_dir.join("oi.parquet"), &d.oi)?;
    Ok(DeriveSummary {
        records,
        bbo_rows: d.bbo.len(),
        funding_rows: d.funding.len(),
        book_rows: d.book.len(),
        oi_rows: d.oi.len(),
        out_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use sh_core::{Level, Px, Qty, RecvTs, VenueKey};
    use sh_venue_api::FundingUpdate;

    fn ev(ms: i64, kind: MdEventKind) -> MdEvent {
        MdEvent {
            venue: VenueKey::new(Exchange::Lighter),
            symbol: "ETH".into(),
            recv: RecvTs { wall_ns: ms * 1_000_000, mono_ns: 0 },
            venue_ts_ms: None,
            kind,
        }
    }

    fn book(px: Decimal) -> MdEventKind {
        MdEventKind::BookSnapshot {
            bids: vec![Level::new(Px(px - dec!(1)), Qty(dec!(1)))],
            asks: vec![Level::new(Px(px + dec!(1)), Qty(dec!(1)))],
            seq: None,
        }
    }

    #[test]
    fn samples_bbo_per_second_and_minute_snapshots() {
        let mut d = Deriver::default();
        d.on_event(&ev(1_000, book(dec!(100))));
        d.on_event(&ev(1_500, book(dec!(101))));
        d.on_event(&ev(2_100, book(dec!(102))));
        d.on_event(&ev(61_000, MdEventKind::OpenInterest { base: Some(Qty(dec!(5))), usd: None }));
        d.on_event(&ev(
            61_500,
            MdEventKind::Funding(FundingUpdate {
                interval_hours: Some(dec!(1)),
                predicted_rate: Some(dec!(0.0001)),
                ..FundingUpdate::default()
            }),
        ));
        d.on_event(&ev(
            61_600,
            MdEventKind::Funding(FundingUpdate { predicted_rate: Some(dec!(0.0001)), ..FundingUpdate::default() }),
        ));
        d.on_event(&ev(
            61_700,
            MdEventKind::Funding(FundingUpdate {
                settled_rate: Some(dec!(0.0002)),
                settled_at_ms: Some(60_000),
                ..FundingUpdate::default()
            }),
        ));
        d.on_event(&ev(121_000, MdEventKind::Trades(vec![])));
        d.finish();
        // секунда 1 → последний BBO 101, секунда 2 → 102
        assert_eq!(d.bbo.len(), 2);
        assert_eq!(d.bbo[0].bid, Some(100.0));
        assert_eq!(d.bbo[1].bid, Some(101.0));
        // прогноз без изменений не дублируется; расчёт — отдельной строкой
        assert_eq!(d.funding.iter().filter(|r| r.kind == "predicted").count(), 1);
        assert_eq!(d.funding.iter().filter(|r| r.kind == "settled").count(), 1);
        // снимки минут: на 61-й секунде (стакан) и на 121-й (стакан и ОИ)
        assert_eq!(d.book.len(), 4);
        assert_eq!(d.oi.len(), 1);
    }
}
