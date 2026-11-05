//! Построитель состояния рынка для тестов (здесь и в зависимых крейтах):
//! инструменты с метаданными, стаканами, ОИ и историей фандинга.

use rust_decimal::Decimal;
use rust_decimal::dec;
use sh_core::{Collateral, Exchange, Level, Px, Qty, RecvTs, Usd, VenueKey};
use sh_md::{InstrumentKey, MarketState};
use sh_venue_api::{
    FeeInfo, FeeSource, FundingUpdate, InstrumentMeta, InstrumentStatus, MdEvent, MdEventKind, PriceRule,
};

/// Час в миллисекундах.
pub const HOUR_MS: i64 = 3_600_000;

/// Построитель.
pub struct MarketBuilder {
    /// Состояние.
    pub market: MarketState,
    /// Текущее время, мс.
    pub now_ms: i64,
}

/// Описание инструмента.
#[derive(Debug, Clone)]
pub struct InstrumentSpec {
    /// Ключ.
    pub key: InstrumentKey,
    /// Mid.
    pub mid: Decimal,
    /// Полуспред.
    pub half_spread: Decimal,
    /// Количество на уровень.
    pub level_qty: Decimal,
    /// Ставка фандинга за интервал (прогноз и расчёты).
    pub rate: Decimal,
    /// Интервал, ч.
    pub interval_h: Decimal,
    /// ОИ, $.
    pub oi_usd: Decimal,
    /// Лот.
    pub lot: Decimal,
    /// Обеспечение.
    pub collateral: Collateral,
    /// Время листинга, мс.
    pub listed_at_ms: Option<i64>,
}

impl InstrumentSpec {
    /// Типичный ликвидный инструмент.
    pub fn liquid(key: &str, rate: Decimal, interval_h: Decimal) -> Self {
        Self {
            key: InstrumentKey::parse(key).unwrap_or_else(|| InstrumentKey::new(VenueKey::new(Exchange::Lighter), key)),
            mid: dec!(100),
            half_spread: dec!(0.01),
            level_qty: dec!(10000),
            rate,
            interval_h,
            oi_usd: dec!(50000000),
            lot: dec!(0.01),
            collateral: Collateral::Usdc,
            listed_at_ms: Some(0),
        }
    }
}

fn ev(key: &InstrumentKey, now_ms: i64, kind: MdEventKind) -> MdEvent {
    MdEvent {
        venue: key.venue.clone(),
        symbol: key.symbol.clone(),
        recv: RecvTs { wall_ns: now_ms * 1_000_000, mono_ns: u64::try_from(now_ms).unwrap_or(0) * 1_000_000 },
        venue_ts_ms: Some(now_ms),
        kind,
    }
}

impl MarketBuilder {
    /// Пустой рынок на момент `now_ms` (по умолчанию — 30 дней от эпохи).
    pub fn new() -> Self {
        Self { market: MarketState::new(), now_ms: 30 * 24 * HOUR_MS }
    }

    /// Добавляет инструмент с историей расчётов за `history_h` часов.
    pub fn add(&mut self, s: &InstrumentSpec, history_h: i64) -> &mut Self {
        let meta = InstrumentMeta {
            venue: s.key.venue.clone(),
            symbol: s.key.symbol.clone(),
            venue_id: None,
            base: s.key.symbol.split_once(':').map(|(_, b)| b).unwrap_or(&s.key.symbol).to_owned(),
            collateral: s.collateral.clone(),
            price_rule: PriceRule::Tick { tick: Px(dec!(0.01)) },
            lot_size: Qty(s.lot),
            min_qty: Some(Qty(s.lot)),
            min_notional: None,
            max_leverage: Some(dec!(20)),
            margin_mode: None,
            funding_interval_hours: Some(s.interval_h),
            fees: FeeInfo {
                maker: None,
                taker: None,
                source: FeeSource::AccountRequired,
                deployer_fee_scale: None,
                growth_mode: None,
            },
            status: InstrumentStatus::Active,
            listed_at_ms: s.listed_at_ms,
        };
        let now = self.now_ms;
        self.market.apply(&ev(&s.key, now, MdEventKind::Metadata(Box::new(meta))));
        self.set_book(&s.key, s.mid, s.half_spread, s.level_qty);
        self.market.apply(&ev(
            &s.key,
            now,
            MdEventKind::Mark { mark: Some(Px(s.mid)), index: Some(Px(s.mid)), oracle: Some(Px(s.mid)), mid: None },
        ));
        self.market.apply(&ev(&s.key, now, MdEventKind::OpenInterest { base: None, usd: Some(Usd(s.oi_usd)) }));
        let step = (s.interval_h * Decimal::from(HOUR_MS)).trunc().try_into().unwrap_or(HOUR_MS);
        let mut t = now - history_h * HOUR_MS;
        t -= t.rem_euclid(step);
        while t <= now {
            self.settle(&s.key, t, s.rate);
            t += step;
        }
        self.market.apply(&ev(
            &s.key,
            now,
            MdEventKind::Funding(FundingUpdate {
                predicted_rate: Some(s.rate),
                next_funding_ms: Some(t),
                ..FundingUpdate::default()
            }),
        ));
        self
    }

    /// Заменяет стакан: 20 уровней с шагом 0.01% цены.
    pub fn set_book(&mut self, key: &InstrumentKey, mid: Decimal, half_spread: Decimal, level_qty: Decimal) {
        let step = mid / dec!(10000);
        let bids: Vec<Level> =
            (0..20).map(|i| Level::new(Px(mid - half_spread - step * Decimal::from(i)), Qty(level_qty))).collect();
        let asks: Vec<Level> =
            (0..20).map(|i| Level::new(Px(mid + half_spread + step * Decimal::from(i)), Qty(level_qty))).collect();
        let now = self.now_ms;
        self.market.apply(&ev(key, now, MdEventKind::BookSnapshot { bids, asks, seq: None }));
    }

    /// Расчёт фандинга.
    pub fn settle(&mut self, key: &InstrumentKey, ts_ms: i64, rate: Decimal) {
        let now = self.now_ms;
        self.market.apply(&ev(
            key,
            now,
            MdEventKind::Funding(FundingUpdate {
                settled_rate: Some(rate),
                settled_at_ms: Some(ts_ms),
                ..FundingUpdate::default()
            }),
        ));
    }

    /// Прогноз фандинга.
    pub fn predict(&mut self, key: &InstrumentKey, rate: Decimal) {
        let now = self.now_ms;
        self.market.apply(&ev(
            key,
            now,
            MdEventKind::Funding(FundingUpdate { predicted_rate: Some(rate), ..FundingUpdate::default() }),
        ));
    }
}

impl Default for MarketBuilder {
    fn default() -> Self {
        Self::new()
    }
}
