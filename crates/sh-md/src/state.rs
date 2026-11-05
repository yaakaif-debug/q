//! Состояние рынка по инструментам, собранное из нормализованных событий:
//! стакан, BBO, цены маркировки/индекса/оракула, открытый интерес, прогнозный
//! и расчётный фандинг, метаданные. Используется сканером, paper и live.

use std::collections::{BTreeMap, VecDeque};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Exchange, Level, Px, Usd, VenueKey};
use sh_venue_api::{InstrumentMeta, MdEvent, MdEventKind};

use crate::book::OrderBook;

/// Ключ инструмента: площадка и символ площадки.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstrumentKey {
    /// Площадка.
    pub venue: VenueKey,
    /// Символ площадки.
    pub symbol: String,
}

impl InstrumentKey {
    /// Создаёт ключ.
    pub fn new(venue: VenueKey, symbol: impl Into<String>) -> Self {
        Self { venue, symbol: symbol.into() }
    }

    /// Строковая форма `venue/symbol` (`hyperliquid:xyz/xyz:NVDA`), как в `config/symbols.toml`.
    pub fn id(&self) -> String {
        format!("{}/{}", self.venue, self.symbol)
    }

    /// Разбор строки `venue/symbol`.
    pub fn parse(s: &str) -> Option<Self> {
        let (venue, symbol) = s.split_once('/')?;
        let (ex, dex) = venue.split_once(':').unwrap_or((venue, ""));
        let exchange: Exchange = ex.parse().ok()?;
        if symbol.is_empty() || (exchange != Exchange::Hyperliquid && !dex.is_empty()) {
            return None;
        }
        Some(Self { venue: VenueKey { exchange, dex: dex.to_owned() }, symbol: symbol.to_owned() })
    }
}

impl std::fmt::Display for InstrumentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.venue, self.symbol)
    }
}

/// Один расчёт фандинга.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settlement {
    /// Время расчёта, мс.
    pub ts_ms: i64,
    /// Ставка за интервал (доля).
    pub rate: Decimal,
    /// Интервал, ч (на момент расчёта).
    pub interval_hours: Decimal,
}

/// Фандинг инструмента.
#[derive(Debug, Clone, Default)]
pub struct FundingState {
    /// Прогноз ставки на текущий интервал (доля).
    pub predicted: Option<Decimal>,
    /// Время получения прогноза, мс.
    pub predicted_at_ms: Option<i64>,
    /// Время следующего расчёта, мс.
    pub next_funding_ms: Option<i64>,
    /// Интервал, ч.
    pub interval_hours: Option<Decimal>,
    /// Расчёты по возрастанию времени, без повторов.
    pub settled: VecDeque<Settlement>,
}

/// Сколько хранить расчёты: 8 суток покрывают окно в 7 дней.
const KEEP_SETTLED_MS: i64 = 8 * 24 * 3_600_000;

impl FundingState {
    /// Добавляет расчёт; возвращает истину, если он новый.
    pub fn push_settlement(&mut self, s: Settlement) -> bool {
        if self.settled.iter().any(|x| x.ts_ms == s.ts_ms) {
            return false;
        }
        let pos = self.settled.iter().position(|x| x.ts_ms > s.ts_ms).unwrap_or(self.settled.len());
        self.settled.insert(pos, s);
        if let Some(newest) = self.settled.back().map(|x| x.ts_ms) {
            while self.settled.front().is_some_and(|x| newest - x.ts_ms > KEEP_SETTLED_MS) {
                self.settled.pop_front();
            }
        }
        true
    }

    /// Последний расчёт.
    pub fn last_settled(&self) -> Option<&Settlement> {
        self.settled.back()
    }
}

/// Состояние одного инструмента.
#[derive(Debug, Clone, Default)]
pub struct InstrumentState {
    /// Метаданные.
    pub meta: Option<InstrumentMeta>,
    /// Стакан (верхние уровни для площадок со снимками).
    pub book: OrderBook,
    /// Стакан действителен (синхронизирован).
    pub book_valid: bool,
    /// Последний BBO из отдельного потока.
    pub bbo: Option<(Option<Level>, Option<Level>)>,
    /// Цена маркировки.
    pub mark: Option<Px>,
    /// Индекс.
    pub index: Option<Px>,
    /// Оракул.
    pub oracle: Option<Px>,
    /// Mid площадки.
    pub venue_mid: Option<Px>,
    /// ОИ одной стороны в базовом активе.
    pub oi_base: Option<Decimal>,
    /// ОИ одной стороны в долларах.
    pub oi_usd: Option<Usd>,
    /// Фандинг.
    pub funding: FundingState,
    /// Настенное время последнего события, мс.
    pub last_event_ms: Option<i64>,
    /// Монотонное время последнего события стакана, нс.
    pub last_book_mono_ns: Option<u64>,
}

impl InstrumentState {
    /// Лучшие цены: из стакана, если он действителен, иначе из потока BBO.
    pub fn best(&self) -> (Option<Level>, Option<Level>) {
        if self.book_valid && !self.book.is_empty() {
            return (self.book.best_bid(), self.book.best_ask());
        }
        self.bbo.unwrap_or((None, None))
    }

    /// Средняя цена: стакан → BBO → mid площадки → маркировка.
    pub fn mid(&self) -> Option<Px> {
        match self.best() {
            (Some(b), Some(a)) => Some(Px((b.px.0 + a.px.0) / Decimal::TWO)),
            _ => self.venue_mid.or(self.mark),
        }
    }

    /// Цена, по которой площадка считает платёж фандинга:
    /// Hyperliquid — оракул, Lighter — индекс, Aster — маркировка (UNVERIFIED для Aster).
    pub fn funding_price(&self, exchange: Exchange) -> Option<Px> {
        let preferred = match exchange {
            Exchange::Hyperliquid => self.oracle,
            Exchange::Lighter => self.index,
            Exchange::Aster => self.mark,
        };
        preferred.or(self.mark).or_else(|| self.mid())
    }

    /// ОИ одной стороны в долларах (из долларового значения или базового × маркировка).
    pub fn open_interest_usd(&self) -> Option<Usd> {
        self.oi_usd.or_else(|| Some(Usd(self.oi_base? * self.mark.or_else(|| self.mid())?.0)))
    }

    /// Интервал фандинга: из событий или из метаданных.
    pub fn interval_hours(&self) -> Option<Decimal> {
        self.funding.interval_hours.or_else(|| self.meta.as_ref().and_then(|m| m.funding_interval_hours))
    }
}

/// Новый расчёт фандинга, обнаруженный при применении события.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSettlement {
    /// Инструмент.
    pub key: InstrumentKey,
    /// Расчёт.
    pub settlement: Settlement,
}

/// Состояние всех инструментов.
#[derive(Debug, Clone, Default)]
pub struct MarketState {
    instruments: BTreeMap<InstrumentKey, InstrumentState>,
}

impl MarketState {
    /// Пустое состояние.
    pub fn new() -> Self {
        Self::default()
    }

    /// Инструмент по ключу.
    pub fn get(&self, key: &InstrumentKey) -> Option<&InstrumentState> {
        self.instruments.get(key)
    }

    /// Все инструменты.
    pub fn iter(&self) -> impl Iterator<Item = (&InstrumentKey, &InstrumentState)> {
        self.instruments.iter()
    }

    /// Число инструментов.
    pub fn len(&self) -> usize {
        self.instruments.len()
    }

    /// Истина, если инструментов нет.
    pub fn is_empty(&self) -> bool {
        self.instruments.is_empty()
    }

    /// Применяет событие. Возвращает новый расчёт фандинга, если событие его принесло.
    pub fn apply(&mut self, ev: &MdEvent) -> Option<NewSettlement> {
        let key = InstrumentKey::new(ev.venue.clone(), ev.symbol.clone());
        let st = self.instruments.entry(key.clone()).or_default();
        st.last_event_ms = Some(ev.recv.wall_ns.div_euclid(1_000_000));
        match &ev.kind {
            MdEventKind::BookSnapshot { bids, asks, .. } => {
                st.book.replace(bids, asks);
                st.book.touch(ev.recv.mono_ns, ev.venue_ts_ms);
                st.book_valid = true;
                st.last_book_mono_ns = Some(ev.recv.mono_ns);
            }
            MdEventKind::BookDelta { bids, asks, .. } => {
                // последовательность уже проверил адаптер; дельта до снимка не применяется
                if st.book_valid {
                    st.book.apply_levels(bids, asks);
                    st.book.touch(ev.recv.mono_ns, ev.venue_ts_ms);
                    st.last_book_mono_ns = Some(ev.recv.mono_ns);
                }
            }
            MdEventKind::BookInvalidated => {
                st.book.clear();
                st.book_valid = false;
            }
            MdEventKind::Bbo { bid, ask } => st.bbo = Some((*bid, *ask)),
            MdEventKind::Trades(_) => {}
            MdEventKind::Mark { mark, index, oracle, mid } => {
                st.mark = mark.or(st.mark);
                st.index = index.or(st.index);
                st.oracle = oracle.or(st.oracle);
                st.venue_mid = mid.or(st.venue_mid);
            }
            MdEventKind::OpenInterest { base, usd } => {
                if let Some(b) = base {
                    st.oi_base = Some(b.0);
                }
                if let Some(u) = usd {
                    st.oi_usd = Some(*u);
                }
            }
            MdEventKind::Metadata(m) => {
                if m.funding_interval_hours.is_some() {
                    st.funding.interval_hours = m.funding_interval_hours;
                }
                st.meta = Some((**m).clone());
            }
            MdEventKind::Funding(f) => {
                if let Some(i) = f.interval_hours {
                    st.funding.interval_hours = Some(i);
                }
                if f.predicted_rate.is_some() {
                    st.funding.predicted = f.predicted_rate;
                    st.funding.predicted_at_ms = st.last_event_ms;
                }
                if f.next_funding_ms.is_some() {
                    st.funding.next_funding_ms = f.next_funding_ms;
                }
                if let (Some(rate), Some(ts)) = (f.settled_rate, f.settled_at_ms) {
                    let interval = st.interval_hours()?;
                    let s = Settlement { ts_ms: ts, rate, interval_hours: interval };
                    if st.funding.push_settlement(s) {
                        return Some(NewSettlement { key, settlement: s });
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use sh_core::{Qty, RecvTs};
    use sh_venue_api::FundingUpdate;

    fn ev(kind: MdEventKind) -> MdEvent {
        MdEvent {
            venue: VenueKey::hyperliquid("xyz"),
            symbol: "xyz:NVDA".into(),
            recv: RecvTs { wall_ns: 5_000_000, mono_ns: 1 },
            venue_ts_ms: None,
            kind,
        }
    }

    fn lvl(px: Decimal, q: Decimal) -> Level {
        Level::new(Px(px), Qty(q))
    }

    #[test]
    fn key_roundtrip() {
        let k = InstrumentKey::parse("hyperliquid:xyz/xyz:NVDA").unwrap();
        assert_eq!(k.venue, VenueKey::hyperliquid("xyz"));
        assert_eq!(k.id(), "hyperliquid:xyz/xyz:NVDA");
        assert_eq!(InstrumentKey::parse("aster/BTCUSDT").unwrap().venue, VenueKey::new(Exchange::Aster));
        assert!(InstrumentKey::parse("aster:x/BTC").is_none());
        assert!(InstrumentKey::parse("binance/BTC").is_none());
        assert!(InstrumentKey::parse("lighter/").is_none());
    }

    #[test]
    fn books_and_invalidation() {
        let mut m = MarketState::new();
        m.apply(&ev(MdEventKind::BookDelta {
            bids: vec![lvl(dec!(1), dec!(1))],
            asks: vec![],
            first_seq: 1,
            last_seq: 1,
            prev_seq: None,
        }));
        let k = InstrumentKey::new(VenueKey::hyperliquid("xyz"), "xyz:NVDA");
        // дельта до снимка игнорируется
        assert!(m.get(&k).unwrap().book.is_empty());
        m.apply(&ev(MdEventKind::BookSnapshot {
            bids: vec![lvl(dec!(99), dec!(1))],
            asks: vec![lvl(dec!(101), dec!(1))],
            seq: None,
        }));
        assert_eq!(m.get(&k).unwrap().mid(), Some(Px(dec!(100))));
        m.apply(&ev(MdEventKind::BookInvalidated));
        let st = m.get(&k).unwrap();
        assert!(!st.book_valid && st.mid().is_none());
    }

    #[test]
    fn settlements_dedupe_and_need_interval() {
        let mut m = MarketState::new();
        let settled = |ts| {
            MdEventKind::Funding(FundingUpdate {
                settled_rate: Some(dec!(0.0001)),
                settled_at_ms: Some(ts),
                ..FundingUpdate::default()
            })
        };
        // без интервала расчёт не учитывается: APR не из чего считать
        assert!(m.apply(&ev(settled(1000))).is_none());
        m.apply(&ev(MdEventKind::Funding(FundingUpdate { interval_hours: Some(dec!(1)), ..FundingUpdate::default() })));
        assert!(m.apply(&ev(settled(1000))).is_some());
        assert!(m.apply(&ev(settled(1000))).is_none());
        assert!(m.apply(&ev(settled(500))).is_some());
        let k = InstrumentKey::new(VenueKey::hyperliquid("xyz"), "xyz:NVDA");
        let ts: Vec<i64> = m.get(&k).unwrap().funding.settled.iter().map(|s| s.ts_ms).collect();
        assert_eq!(ts, vec![500, 1000]);
    }

    #[test]
    fn old_settlements_are_pruned() {
        let mut f = FundingState::default();
        let s = |ts| Settlement { ts_ms: ts, rate: Decimal::ZERO, interval_hours: Decimal::ONE };
        f.push_settlement(s(0));
        f.push_settlement(s(KEEP_SETTLED_MS + 1));
        assert_eq!(f.settled.len(), 1);
    }

    #[test]
    fn oi_usd_from_base_and_mark() {
        let mut m = MarketState::new();
        m.apply(&ev(MdEventKind::OpenInterest { base: Some(Qty(dec!(10))), usd: None }));
        m.apply(&ev(MdEventKind::Mark {
            mark: Some(Px(dec!(150))),
            index: None,
            oracle: Some(Px(dec!(149))),
            mid: None,
        }));
        let st = m.get(&InstrumentKey::new(VenueKey::hyperliquid("xyz"), "xyz:NVDA")).unwrap();
        assert_eq!(st.open_interest_usd(), Some(Usd(dec!(1500))));
        assert_eq!(st.funding_price(Exchange::Hyperliquid), Some(Px(dec!(149))));
        assert_eq!(st.funding_price(Exchange::Aster), Some(Px(dec!(150))));
    }
}
