//! Тесты оценки пар на построенном состоянии рынка.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rust_decimal::Decimal;
use rust_decimal::dec;
use sh_core::{Collateral, Usd};
use sh_md::InstrumentKey;
use sh_signals::testkit::{HOUR_MS, InstrumentSpec, MarketBuilder};
use sh_signals::{
    CostParams, EvalContext, FeesConfig, PairKind, Reason, RiskLimits, SizeBudget, StrategyParams, SymbolMap, evaluate,
    pairs_from_symbols,
};

const SYMBOLS: &str = r#"
[[underlying]]
id = "ETH"
members = ["hyperliquid/ETH", "aster/ETHUSDT", "lighter/ETH"]

[[underlying]]
id = "NVDA"
members = ["hyperliquid/NVDA", "hyperliquid:xyz/xyz:NVDA"]
session = "us"

[calendars]
us = ["MON 13:30", "FRI 20:00"]
"#;

struct Fixture {
    b: MarketBuilder,
    symbols: SymbolMap,
    strategy: StrategyParams,
    costs: CostParams,
    fees: FeesConfig,
    risk: RiskLimits,
    tiers: Vec<Usd>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            b: MarketBuilder::new(),
            symbols: SymbolMap::from_toml_str(SYMBOLS).unwrap(),
            strategy: StrategyParams::default(),
            costs: CostParams::default(),
            fees: FeesConfig::default(),
            risk: RiskLimits::default(),
            tiers: vec![Usd(dec!(10000)), Usd(dec!(50000)), Usd(dec!(100000))],
        }
    }

    fn eval(&self, pair_idx: usize, budget: Option<SizeBudget>) -> sh_signals::PairEval {
        let pairs = pairs_from_symbols(&self.symbols);
        let none = |_: &InstrumentKey| None;
        let ctx = EvalContext {
            market: &self.b.market,
            symbols: &self.symbols,
            strategy: &self.strategy,
            costs: &self.costs,
            fees: &self.fees,
            risk: &self.risk,
            now_ms: self.b.now_ms,
            reserve_frac: dec!(0.15),
            target_roc: dec!(0.15),
            tiers: &self.tiers,
            first_seen_ms: &none,
            budget,
        };
        evaluate(&pairs[pair_idx], &ctx)
    }
}

fn idx_of(f: &Fixture, x: &str, y: &str) -> usize {
    pairs_from_symbols(&f.symbols)
        .iter()
        .position(|p| (p.a.id() == x && p.b.id() == y) || (p.a.id() == y && p.b.id() == x))
        .unwrap()
}

#[test]
fn pairs_cover_distinct_venues_and_kinds() {
    let f = Fixture::new();
    let pairs = pairs_from_symbols(&f.symbols);
    // ETH: 3 площадки → 3 пары S1; NVDA: ядро и xyz → 1 пара S2
    assert_eq!(pairs.len(), 4);
    assert_eq!(pairs.iter().filter(|p| p.kind == PairKind::S2).count(), 1);
}

#[test]
fn attractive_pair_is_entered_with_correct_direction() {
    let mut f = Fixture::new();
    // Aster платит шортам 0.03% за 8 ч (~32.85% годовых), HL — 0.0001% в час (~0.876%)
    f.b.add(&InstrumentSpec::liquid("aster/ETHUSDT", dec!(0.0003), dec!(8)), 8 * 24);
    f.b.add(&InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.000001), dec!(1)), 8 * 24);
    let idx = idx_of(&f, "aster/ETHUSDT", "hyperliquid/ETH");
    let budget = SizeBudget { pair_capital: Usd(dec!(25000)), room_margin: [Usd(dec!(60000)), Usd(dec!(60000))] };
    let e = f.eval(idx, Some(budget));
    assert_eq!(e.short.id(), "aster/ETHUSDT");
    assert_eq!(e.est.now, Some(dec!(0.3285) - dec!(0.00876)));
    assert!(e.est.persistent);
    assert!(e.eligible, "{:?}", e.reasons);
    assert!(e.entry_ok, "{:?}", e.reasons);
    let size = e.size.unwrap();
    // капитал 25k, резерв 15%, плечо 3/3 → номинал 25000 * 0.85 / (2/3) = 31875
    assert!(size.notional.0 <= dec!(31875) && size.notional.0 > dec!(31800));
    let at = e.at_size.unwrap();
    assert!(at.s_net.unwrap() > dec!(0.12));
    assert!(at.breakeven_h.unwrap() < dec!(72));
    // нужный чистый спред для 15% ROC при плече 3 и резерве 15% ≈ 11.76%
    assert!((e.required_net_spread.unwrap() - dec!(0.1176)).abs() < dec!(0.001));
}

#[test]
fn small_spread_and_flipping_sign_are_rejected() {
    let mut f = Fixture::new();
    f.b.add(&InstrumentSpec::liquid("aster/ETHUSDT", dec!(0.0001), dec!(8)), 8 * 24);
    f.b.add(&InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.0000125), dec!(1)), 8 * 24);
    let idx = idx_of(&f, "aster/ETHUSDT", "hyperliquid/ETH");
    let e = f.eval(idx, None);
    // 10.95% − 10.95% ≈ 0
    assert!(e.reasons.contains(&Reason::GrossBelowMin));
    assert!(!e.entry_ok);

    // знак менялся: последние расчёты отрицательные, прогноз положительный
    let key = InstrumentKey::parse("aster/ETHUSDT").unwrap();
    let mut g = Fixture::new();
    g.b.add(&InstrumentSpec::liquid("aster/ETHUSDT", dec!(-0.0003), dec!(8)), 8 * 24);
    g.b.add(&InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.000001), dec!(1)), 8 * 24);
    g.b.predict(&key, dec!(0.0003));
    let e = g.eval(idx, None);
    assert!(e.reasons.contains(&Reason::NotPersistent));
}

#[test]
fn filters_low_oi_new_listing_and_mark_divergence() {
    let mut f = Fixture::new();
    let mut a = InstrumentSpec::liquid("aster/ETHUSDT", dec!(0.0003), dec!(8));
    a.oi_usd = dec!(1000000);
    a.listed_at_ms = Some(f.b.now_ms - 10 * HOUR_MS);
    let mut h = InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.000001), dec!(1));
    h.mid = dec!(103);
    f.b.add(&a, 8 * 24).add(&h, 8 * 24);
    let idx = idx_of(&f, "aster/ETHUSDT", "hyperliquid/ETH");
    let e = f.eval(idx, None);
    assert!(!e.eligible);
    assert!(e.reasons.contains(&Reason::LowOpenInterest("aster/ETHUSDT".into())));
    assert!(e.reasons.contains(&Reason::NewListing("aster/ETHUSDT".into())));
    assert!(e.reasons.contains(&Reason::MarkDivergence));
}

#[test]
fn thin_book_gives_no_size() {
    let mut f = Fixture::new();
    // мейкер — Aster (0% мейкер), хедж-нога HL проходится по стакану: её и делаем тонкой
    let mut h = InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.000001), dec!(1));
    h.level_qty = dec!(1);
    f.b.add(&InstrumentSpec::liquid("aster/ETHUSDT", dec!(0.0003), dec!(8)), 8 * 24).add(&h, 8 * 24);
    let idx = idx_of(&f, "aster/ETHUSDT", "hyperliquid/ETH");
    let e = f.eval(idx, None);
    // на $10k глубины 20 * 1 * $100 = $2000 не хватает
    assert!(e.tiers[0].cost.is_none());
    assert!(e.reasons.contains(&Reason::NoSize));
}

#[test]
fn s2_pair_uses_s2_rules_conversion_and_session_guard() {
    let mut f = Fixture::new();
    let mut xyz = InstrumentSpec::liquid("hyperliquid:xyz/xyz:NVDA", dec!(0.00005), dec!(1));
    xyz.collateral = Collateral::Usdh;
    f.b.add(&xyz, 8 * 24).add(&InstrumentSpec::liquid("hyperliquid/NVDA", dec!(0.000001), dec!(1)), 8 * 24);
    let pairs = pairs_from_symbols(&f.symbols);
    let idx = pairs.iter().position(|p| p.kind == PairKind::S2).unwrap();
    let e = f.eval(idx, None);
    assert_eq!(e.short.id(), "hyperliquid:xyz/xyz:NVDA");
    let cost = e.tiers[0].cost.unwrap();
    assert!(cost.conversion.0 > Decimal::ZERO);
    assert_eq!(cost.rebalance, Usd::ZERO);
    // время фикстуры — 31 января 1970 (суббота), до смен далеко
    assert!(!e.reasons.contains(&Reason::SessionGuard));
    let mut g = Fixture::new();
    g.b.now_ms = 4 * 24 * HOUR_MS + 13 * HOUR_MS; // понедельник 5 января 1970, 13:00 UTC
    g.b.add(&xyz, 8 * 24).add(&InstrumentSpec::liquid("hyperliquid/NVDA", dec!(0.000001), dec!(1)), 8 * 24);
    assert!(g.eval(idx, None).reasons.contains(&Reason::SessionGuard));
}
