//! Оценка пары инструментов: направление, три оценки спреда, устойчивость знака,
//! фильтры допуска, издержки на ступенях номинала и на рассчитанном размере,
//! `s_net`, точка безубыточности, ROC и решение о входе (SPEC.md, S1/S2).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Collateral, Exchange, Px, Side, UnderlyingId, Usd};
use sh_md::{InstrumentKey, InstrumentState, MarketState, Settlement};
use sh_venue_api::InstrumentStatus;

use crate::apr::funding_apr;
use crate::cost::{CostBreakdown, CostParams, LegCostInput, round_trip_cost};
use crate::economics::{breakeven_hours, capital_for, required_net_spread, roc, s_net};
use crate::estimators::{SpreadEstimates, ema_apr, realized_apr, sign_persistent, spread_series};
use crate::fees::FeesConfig;
use crate::params::{PairRules, RiskLimits, StrategyParams, pct};
use crate::sizing::{SizeDecision, SizingInputs, size_position};
use crate::symbols::SymbolMap;

/// Вид пары.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PairKind {
    /// Межплощадочная (S1).
    S1,
    /// Между дексами Hyperliquid (S2).
    S2,
}

/// Пара инструментов одного базового актива на разных площадках.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pair {
    /// Базовый актив.
    pub underlying: UnderlyingId,
    /// Первая нога (меньший ключ).
    pub a: InstrumentKey,
    /// Вторая нога.
    pub b: InstrumentKey,
    /// Вид.
    pub kind: PairKind,
}

impl Pair {
    /// Создаёт пару с упорядоченными ногами; S2, если обе ноги на Hyperliquid.
    pub fn new(underlying: UnderlyingId, x: InstrumentKey, y: InstrumentKey) -> Self {
        let (a, b) = if x <= y { (x, y) } else { (y, x) };
        let kind = if a.venue.exchange == Exchange::Hyperliquid && b.venue.exchange == Exchange::Hyperliquid {
            PairKind::S2
        } else {
            PairKind::S1
        };
        Self { underlying, a, b, kind }
    }

    /// Устойчивый идентификатор пары.
    pub fn id(&self) -> String {
        format!("{}:{}~{}", self.underlying.0, self.a.id(), self.b.id())
    }
}

/// Все пары из сопоставления: каждая пара инструментов на разных площадках.
pub fn pairs_from_symbols(map: &SymbolMap) -> Vec<Pair> {
    let mut out = Vec::new();
    for u in map.underlyings() {
        for (i, x) in u.members.iter().enumerate() {
            for y in &u.members[i + 1..] {
                if x.venue != y.venue {
                    out.push(Pair::new(u.id.clone(), x.clone(), y.clone()));
                }
            }
        }
    }
    out
}

/// Причина отказа во входе.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    /// Нет данных по ноге (метаданные, цена, стакан, интервал фандинга).
    MissingData(String),
    /// Нога не торгуется.
    Inactive(String),
    /// ОИ ниже минимума.
    LowOpenInterest(String),
    /// Листинг моложе минимума.
    NewListing(String),
    /// Маркировки расходятся сильнее допуска — вероятно, разные базовые активы.
    MarkDivergence,
    /// Направление не определено (нет оценок).
    NoDirection,
    /// Одна из оценок ниже минимального валового спреда.
    GrossBelowMin,
    /// Знак спреда неустойчив.
    NotPersistent,
    /// Не хватает глубины или капитала на размер.
    NoSize,
    /// Точка безубыточности дальше допустимой.
    BreakevenTooLong,
    /// Чистый спред ниже минимума.
    NetBelowMin,
    /// Близко смена сессии базового актива (S2).
    SessionGuard,
    /// Удержание короче времени до ближайшей выплаты получающей ноге.
    FundingTiming,
    /// Стратегия выключена.
    Disabled,
}

/// Издержки и экономика на одном номинале.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierEval {
    /// Номинал на ногу.
    pub notional: Usd,
    /// Издержки; `None` — глубины не хватает.
    pub cost: Option<CostBreakdown>,
    /// Чистый спред.
    pub s_net: Option<Decimal>,
    /// Точка безубыточности, ч.
    pub breakeven_h: Option<Decimal>,
    /// ROC.
    pub roc: Option<Decimal>,
}

/// Результат оценки пары.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairEval {
    /// Пара.
    pub pair: Pair,
    /// Короткая нога (больший фандинг).
    pub short: InstrumentKey,
    /// Длинная нога.
    pub long: InstrumentKey,
    /// Оценки спреда.
    pub est: EstimatesView,
    /// Плечо коротой и длинной ноги.
    pub leverage: (Decimal, Decimal),
    /// Нужный чистый спред для целевого ROC при этом плече.
    pub required_net_spread: Option<Decimal>,
    /// Ступени номинала.
    pub tiers: Vec<TierEval>,
    /// Размер при доступном капитале.
    pub size: Option<SizeDecision>,
    /// Экономика на этом размере.
    pub at_size: Option<TierEval>,
    /// Пара проходит фильтры допуска (ОИ, возраст, маркировки, данные).
    pub eligible: bool,
    /// Вход разрешён.
    pub entry_ok: bool,
    /// Причины отказа.
    pub reasons: Vec<Reason>,
    /// Цена для перевода номинала в количество.
    pub price: Option<Px>,
}

/// Сериализуемый вид оценок спреда.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EstimatesView {
    /// По прогнозу.
    pub now: Option<Decimal>,
    /// EMA 24 ч.
    pub ema_24h: Option<Decimal>,
    /// Реализованное 7 д.
    pub realized_7d: Option<Decimal>,
    /// Знак устойчив.
    pub persistent: bool,
}

impl From<SpreadEstimates> for EstimatesView {
    fn from(e: SpreadEstimates) -> Self {
        Self { now: e.now, ema_24h: e.ema_24h, realized_7d: e.realized_7d, persistent: e.persistent }
    }
}

/// Капитал под пару для расчёта размера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeBudget {
    /// Капитал, который можно выделить паре, $.
    pub pair_capital: Usd,
    /// Остаток лимита платформы по маржинальному капиталу для ног a и b, $.
    pub room_margin: [Usd; 2],
}

/// Контекст оценки.
pub struct EvalContext<'a> {
    /// Рынок.
    pub market: &'a MarketState,
    /// Сопоставление символов.
    pub symbols: &'a SymbolMap,
    /// Параметры стратегий.
    pub strategy: &'a StrategyParams,
    /// Параметры издержек.
    pub costs: &'a CostParams,
    /// Комиссии.
    pub fees: &'a FeesConfig,
    /// Лимиты риска.
    pub risk: &'a RiskLimits,
    /// Текущее время, мс.
    pub now_ms: i64,
    /// Доля резерва капитала.
    pub reserve_frac: Decimal,
    /// Целевой ROC (доля).
    pub target_roc: Decimal,
    /// Ступени номинала.
    pub tiers: &'a [Usd],
    /// Время первого появления инструмента, если листинг площадкой не сообщается.
    pub first_seen_ms: &'a dyn Fn(&InstrumentKey) -> Option<i64>,
    /// Бюджет размера; `None` — только ступени.
    pub budget: Option<SizeBudget>,
}

impl EvalContext<'_> {
    /// Правила для вида пары.
    pub fn rules(&self, kind: PairKind) -> PairRules {
        match kind {
            PairKind::S1 => self.strategy.s1_rules(),
            PairKind::S2 => self.strategy.s2_rules(),
        }
    }
}

struct LegApr {
    now: Option<Decimal>,
    ema: Option<Decimal>,
    real: Option<Decimal>,
    settled: Vec<Settlement>,
}

fn leg_apr(st: &InstrumentState, now_ms: i64) -> LegApr {
    let settled: Vec<Settlement> = st.funding.settled.iter().copied().collect();
    let now = match (st.funding.predicted, st.interval_hours()) {
        (Some(r), Some(h)) => funding_apr(r, h),
        _ => None,
    };
    LegApr { now, ema: ema_apr(&settled, now_ms, 24), real: realized_apr(&settled, now_ms, 168), settled }
}

fn diff(a: Option<Decimal>, b: Option<Decimal>) -> Option<Decimal> {
    Some(a? - b?)
}

/// Оценивает пару.
pub fn evaluate(pair: &Pair, ctx: &EvalContext<'_>) -> PairEval {
    let rules = ctx.rules(pair.kind);
    let mut reasons = Vec::new();
    let mut out = PairEval {
        pair: pair.clone(),
        short: pair.a.clone(),
        long: pair.b.clone(),
        est: EstimatesView::default(),
        leverage: (Decimal::ZERO, Decimal::ZERO),
        required_net_spread: None,
        tiers: Vec::new(),
        size: None,
        at_size: None,
        eligible: false,
        entry_ok: false,
        reasons: Vec::new(),
        price: None,
    };
    let enabled = match pair.kind {
        PairKind::S1 => ctx.strategy.s1_funding_carry.enabled,
        PairKind::S2 => ctx.strategy.s2_cross_dex.enabled,
    };
    if !enabled {
        reasons.push(Reason::Disabled);
    }
    let (Some(sa), Some(sb)) = (ctx.market.get(&pair.a), ctx.market.get(&pair.b)) else {
        out.reasons = vec![Reason::MissingData("instrument state".into())];
        return out;
    };
    let (Some(ma), Some(mb)) = (sa.meta.as_ref(), sb.meta.as_ref()) else {
        out.reasons = vec![Reason::MissingData("metadata".into())];
        return out;
    };

    // допуск пары
    let underlying = ctx.symbols.underlying_of(&pair.a);
    let mut eligible = true;
    for (key, st, meta) in [(&pair.a, sa, ma), (&pair.b, sb, mb)] {
        if meta.status != InstrumentStatus::Active {
            reasons.push(Reason::Inactive(key.id()));
            eligible = false;
        }
        if st.mid().is_none() || st.interval_hours().is_none() {
            reasons.push(Reason::MissingData(key.id()));
            eligible = false;
        }
        match st.open_interest_usd() {
            Some(oi) if oi.0 >= ctx.risk.min_oi_usd => {}
            _ => {
                reasons.push(Reason::LowOpenInterest(key.id()));
                eligible = false;
            }
        }
        let listed = meta.listed_at_ms.or_else(|| (ctx.first_seen_ms)(key));
        let young = listed.is_none_or(|t| ctx.now_ms - t < ctx.risk.min_listing_age_h * 3_600_000);
        if young && !underlying.is_some_and(|u| u.allow_new_listing) {
            reasons.push(Reason::NewListing(key.id()));
            eligible = false;
        }
    }
    if let (Some(pa), Some(pb)) = (sa.mark.or_else(|| sa.mid()), sb.mark.or_else(|| sb.mid())) {
        let div = (pa.0 / pb.0 - Decimal::ONE).abs();
        if div > pct(ctx.risk.max_mark_divergence_pct) && !underlying.is_some_and(|u| u.allow_mark_divergence) {
            reasons.push(Reason::MarkDivergence);
            eligible = false;
        }
    }
    out.eligible = eligible;

    // оценки и направление: коротко там, где фандинг выше
    let (la, lb) = (leg_apr(sa, ctx.now_ms), leg_apr(sb, ctx.now_ms));
    let d_now = diff(la.now, lb.now);
    let d_ema = diff(la.ema, lb.ema);
    let Some(direction) = d_now.or(d_ema) else {
        reasons.push(Reason::NoDirection);
        out.reasons = reasons;
        return out;
    };
    let a_is_short = direction >= Decimal::ZERO;
    let (short_key, long_key, ss, sl, ms, ml, lshort, llong) = if a_is_short {
        (&pair.a, &pair.b, sa, sb, ma, mb, &la, &lb)
    } else {
        (&pair.b, &pair.a, sb, sa, mb, ma, &lb, &la)
    };
    out.short = short_key.clone();
    out.long = long_key.clone();
    let series = spread_series(&lshort.settled, &llong.settled);
    let est = SpreadEstimates {
        now: diff(lshort.now, llong.now),
        ema_24h: diff(lshort.ema, llong.ema),
        realized_7d: diff(lshort.real, llong.real),
        persistent: sign_persistent(&series, rules.k_settlements),
    };
    out.est = est.into();
    if ![est.now, est.ema_24h, est.realized_7d].iter().all(|e| e.is_some_and(|v| v >= rules.min_gross)) {
        reasons.push(Reason::GrossBelowMin);
    }
    if !est.persistent {
        reasons.push(Reason::NotPersistent);
    }

    // плечо, капитал, издержки
    let lev = |m: &sh_venue_api::InstrumentMeta| {
        let cap = ctx.risk.leverage_cap(m.venue.exchange).min(rules.leverage);
        m.max_leverage.map_or(cap, |v| v.min(cap))
    };
    let (lev_s, lev_l) = (lev(ms), lev(ml));
    out.leverage = (lev_s, lev_l);
    let unit_capital = capital_for(Usd(Decimal::ONE), lev_s, lev_l, ctx.reserve_frac);
    out.required_net_spread = unit_capital.and_then(|c| required_net_spread(ctx.target_roc, Usd(Decimal::ONE), c));
    let conv = |m: &sh_venue_api::InstrumentMeta| {
        m.venue.exchange == Exchange::Hyperliquid && !m.venue.dex.is_empty() && m.collateral != Collateral::Usdc
    };
    let short_leg = LegCostInput {
        book: &ss.book,
        fees: ctx.fees.resolve(ms),
        entry_side: Side::Sell,
        leverage: lev_s,
        needs_conversion: conv(ms),
    };
    let long_leg = LegCostInput {
        book: &sl.book,
        fees: ctx.fees.resolve(ml),
        entry_side: Side::Buy,
        leverage: lev_l,
        needs_conversion: conv(ml),
    };
    let rebalance = Usd(match pair.kind {
        PairKind::S1 => ctx.costs.rebalance_usd_s1,
        PairKind::S2 => ctx.costs.rebalance_usd_s2,
    });
    let s_ema = est.ema_24h.unwrap_or(Decimal::ZERO);
    let tier = |n: Usd| -> TierEval {
        let cost = if ss.book_valid && sl.book_valid {
            round_trip_cost(&short_leg, &long_leg, n, rebalance, ctx.costs)
        } else {
            None
        };
        let net = cost.and_then(|c| s_net(s_ema, c.total, n, rules.hold_hours));
        let be = cost.and_then(|c| breakeven_hours(c.total, s_ema, n));
        let r = match (net, capital_for(n, lev_s, lev_l, ctx.reserve_frac)) {
            (Some(net), Some(c)) => roc(net, n, c),
            _ => None,
        };
        TierEval { notional: n, cost, s_net: net, breakeven_h: be, roc: r }
    };
    out.tiers = ctx.tiers.iter().map(|n| tier(*n)).collect();

    let price = match (ss.mid(), sl.mid()) {
        (Some(a), Some(b)) => Some(Px((a.0 + b.0) / Decimal::TWO)),
        _ => None,
    };
    out.price = price;
    if let (Some(budget), Some(price)) = (ctx.budget, price) {
        let per_notional = |l: Decimal| Decimal::ONE / l;
        let margin_per_notional = per_notional(lev_s) + per_notional(lev_l);
        let capital_notional = Usd(budget.pair_capital.0 * (Decimal::ONE - ctx.reserve_frac) / margin_per_notional);
        // остаток лимита платформы в марже переводим в номинал ноги
        let (room_s, room_l) = if a_is_short {
            (budget.room_margin[0], budget.room_margin[1])
        } else {
            (budget.room_margin[1], budget.room_margin[0])
        };
        let depth = |st: &InstrumentState, side: Side| -> Usd {
            let q = st.book.depth_within_bps(side, Decimal::TEN).map(|q| q.0).unwrap_or(Decimal::ZERO);
            Usd(q * price.0)
        };
        let inputs = SizingInputs {
            capital_notional,
            oi_usd: [ss.open_interest_usd().unwrap_or(Usd::ZERO), sl.open_interest_usd().unwrap_or(Usd::ZERO)],
            // короткая нога продаёт в bids, длинная покупает из asks
            depth_usd: [depth(ss, Side::Buy), depth(sl, Side::Sell)],
            venue_room: [Usd(room_s.0 * lev_s), Usd(room_l.0 * lev_l)],
            price,
            lots: [ms.lot_size, ml.lot_size],
        };
        out.size = size_position(&inputs);
        out.at_size = out.size.map(|d| tier(d.notional));
    }

    match out.at_size.or_else(|| out.tiers.first().copied()) {
        Some(t) if t.cost.is_some() => {
            if t.breakeven_h.is_none_or(|h| h > rules.max_breakeven_hours) {
                reasons.push(Reason::BreakevenTooLong);
            }
            if t.s_net.is_none_or(|n| n < rules.min_net) {
                reasons.push(Reason::NetBelowMin);
            }
        }
        _ => reasons.push(Reason::NoSize),
    }
    if ctx.budget.is_some() && out.size.is_none() && !reasons.contains(&Reason::NoSize) {
        reasons.push(Reason::NoSize);
    }

    if let (Some(guard), Some(u)) = (rules.session_guard_min, underlying) {
        let now: DateTime<Utc> = DateTime::from_timestamp_millis(ctx.now_ms).unwrap_or_default();
        let near = u
            .session
            .as_deref()
            .and_then(|s| ctx.symbols.calendar(s))
            .and_then(|c| c.minutes_to_nearest_change(now))
            .is_some_and(|m| m < guard);
        if near {
            reasons.push(Reason::SessionGuard);
        }
    }
    // получающая нога — короткая при положительном фандинге; удержание должно покрыть ближайшую выплату
    let next_ms = if ss.funding.predicted.unwrap_or_default() >= Decimal::ZERO {
        ss.funding.next_funding_ms
    } else {
        sl.funding.next_funding_ms
    };
    if let Some(next) = next_ms {
        let hold_ms = (rules.hold_hours * Decimal::from(3_600_000)).trunc();
        if Decimal::from(next - ctx.now_ms) > hold_ms {
            reasons.push(Reason::FundingTiming);
        }
    }
    out.entry_ok = out.eligible && reasons.is_empty();
    out.reasons = reasons;
    out
}
