//! Теневой портфель на построенном рынке.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use rust_decimal::Decimal;
use rust_decimal::dec;
use sh_core::Usd;
use sh_md::InstrumentKey;
use sh_portfolio::{DailyReport, LedgerKind, ShadowParams, ShadowPortfolio, opportunities_csv, opportunity_rows};
use sh_signals::testkit::{HOUR_MS, InstrumentSpec, MarketBuilder};
use sh_signals::{
    CostParams, EvalContext, FeesConfig, Pair, PairEval, RiskLimits, SizeBudget, StrategyParams, SymbolMap, evaluate,
    pairs_from_symbols,
};

const SYMBOLS: &str = r#"
[[underlying]]
id = "ETH"
members = ["hyperliquid/ETH", "aster/ETHUSDT"]
"#;

struct World {
    b: MarketBuilder,
    symbols: SymbolMap,
    strategy: StrategyParams,
    costs: CostParams,
    fees: FeesConfig,
    risk: RiskLimits,
}

impl World {
    fn new(aster_rate: Decimal) -> Self {
        let mut b = MarketBuilder::new();
        b.add(&InstrumentSpec::liquid("aster/ETHUSDT", aster_rate, dec!(8)), 8 * 24);
        b.add(&InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.000001), dec!(1)), 8 * 24);
        Self {
            b,
            symbols: SymbolMap::from_toml_str(SYMBOLS).unwrap(),
            strategy: StrategyParams::default(),
            costs: CostParams::default(),
            fees: FeesConfig::default(),
            risk: RiskLimits::default(),
        }
    }

    fn params(&self) -> ShadowParams {
        ShadowParams {
            capital: Usd(dec!(100000)),
            reserve_frac: dec!(0.15),
            s1: self.strategy.s1_rules(),
            s2: self.strategy.s2_rules(),
            risk: self.risk.clone(),
            costs: self.costs.clone(),
            fees: self.fees.clone(),
        }
    }

    fn eval(&self, pair: &Pair, budget: Option<SizeBudget>) -> PairEval {
        let none = |_: &InstrumentKey| None;
        let tiers = [Usd(dec!(10000))];
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
            tiers: &tiers,
            first_seen_ms: &none,
            budget,
        };
        evaluate(pair, &ctx)
    }

    fn step(&self, shadow: &mut ShadowPortfolio) -> sh_portfolio::StepOutcome {
        let pairs = pairs_from_symbols(&self.symbols);
        let evals: Vec<PairEval> = pairs.iter().map(|p| self.eval(p, None)).collect();
        let params = self.params();
        let f = |p: &Pair, b: SizeBudget| self.eval(p, Some(b));
        shadow.step(&evals, &self.b.market, &params, self.b.now_ms, &f)
    }
}

fn ledger_sum(entries: &[sh_portfolio::LedgerEntry]) -> Decimal {
    entries.iter().map(|e| e.amount.0).sum()
}

#[test]
fn opens_books_funding_and_exits() {
    let mut w = World::new(dec!(0.0003));
    let mut shadow = ShadowPortfolio::new(Usd(dec!(100000)), w.b.now_ms);
    let mut journal = Vec::new();

    let out = w.step(&mut shadow);
    assert_eq!(out.opened.len(), 1, "{out:?}");
    journal.extend(out.ledger.clone());
    let pos = shadow.positions.values().next().unwrap().clone();
    assert_eq!(pos.short.id(), "aster/ETHUSDT");
    // капитал пары ≤ 25% от $100k
    assert!(pos.capital.0 <= dec!(25000.01));
    assert!(out.ledger.iter().any(|e| e.kind == LedgerKind::Fee && e.amount.0 < Decimal::ZERO));

    // расчёт на Aster через 8 ч: шорт получает ставку × количество × цену маркировки
    w.b.now_ms += 8 * HOUR_MS;
    let key = InstrumentKey::parse("aster/ETHUSDT").unwrap();
    let ts = w.b.now_ms;
    w.b.settle(&key, ts, dec!(0.0003));
    let st = w.b.market.get(&key).unwrap();
    let ns = sh_md::NewSettlement { key: key.clone(), settlement: *st.funding.last_settled().unwrap() };
    let entries = shadow.on_settlement(&ns, &w.b.market);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].amount.0, dec!(0.0003) * pos.qty.0 * dec!(100));
    journal.extend(entries);
    // расчёт по длинной ноге HL с положительной ставкой — платим
    let hl = InstrumentKey::parse("hyperliquid/ETH").unwrap();
    w.b.settle(&hl, ts, dec!(0.000001));
    let ns = sh_md::NewSettlement {
        key: hl.clone(),
        settlement: *w.b.market.get(&hl).unwrap().funding.last_settled().unwrap(),
    };
    let paid = shadow.on_settlement(&ns, &w.b.market);
    assert!(paid[0].amount.0 < Decimal::ZERO);
    journal.extend(paid);

    // спред схлопнулся: три расчёта подряд ниже порога → выход
    for i in 1..=3 {
        w.b.now_ms += 8 * HOUR_MS;
        let t = w.b.now_ms;
        // EMA за 24 ч быстро уходит вниз при отрицательном фандинге Aster
        for h in 0..8 {
            w.b.settle(&hl, t - h * HOUR_MS, dec!(0.000001));
        }
        w.b.settle(&key, t, dec!(-0.0005));
        w.b.predict(&key, dec!(-0.0005));
        let out = w.step(&mut shadow);
        journal.extend(out.ledger.clone());
        if i < 3 {
            assert!(out.closed.is_empty(), "closed too early at {i}: {out:?}");
        } else {
            assert_eq!(out.closed.len(), 1, "{out:?}");
        }
    }
    assert!(shadow.positions.is_empty());
    // тождество капитала: начальный + сумма журнала (позиций нет, базис реализован)
    assert_eq!(shadow.equity(&w.b.market).0, dec!(100000) + ledger_sum(&journal));
}

#[test]
fn no_entry_without_opportunity_and_daily_loss_blocks() {
    let w = World::new(dec!(0.0000125));
    let mut shadow = ShadowPortfolio::new(Usd(dec!(100000)), w.b.now_ms);
    assert!(w.step(&mut shadow).opened.is_empty());

    let w = World::new(dec!(0.0003));
    let mut shadow = ShadowPortfolio::new(Usd(dec!(100000)), w.b.now_ms);
    // имитация дневного убытка 2% → входы заблокированы на сутки
    shadow.realized = Usd(dec!(-2000));
    assert!(w.step(&mut shadow).opened.is_empty());
    assert!(shadow.entries_blocked_until_ms > w.b.now_ms);
}

#[test]
fn report_and_csv() {
    let w = World::new(dec!(0.0003));
    let mut shadow = ShadowPortfolio::new(Usd(dec!(100000)), w.b.now_ms - 86_400_000);
    let out = w.step(&mut shadow);
    let pairs = pairs_from_symbols(&w.symbols);
    let evals: Vec<PairEval> = pairs.iter().map(|p| w.eval(p, None)).collect();
    let rep = DailyReport::build("1970-01-31", &out.ledger, &shadow, &w.b.market, &evals, w.b.now_ms, 10);
    assert_eq!(rep.positions.len(), 1);
    assert!(rep.fees < Decimal::ZERO);
    let md = rep.to_markdown();
    assert!(md.contains("## Позиции") && md.contains("aster/ETHUSDT"));
    assert!(rep.positions_csv().lines().count() == 2);
    assert!(rep.telegram_summary().starts_with("spread-hunter shadow 1970-01-31"));
    let csv = opportunities_csv(&opportunity_rows(&evals), &[Usd(dec!(10000))]);
    assert!(csv.starts_with("pair_id,kind,short,long"));
    assert!(csv.contains("cost_10000_usd"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn equity_identity_holds(rates in proptest::collection::vec(-30i64..60, 1..12), moves in proptest::collection::vec(-200i64..200, 1..12)) {
        let mut w = World::new(dec!(0.0003));
        let mut shadow = ShadowPortfolio::new(Usd(dec!(100000)), w.b.now_ms);
        let mut journal = Vec::new();
        journal.extend(w.step(&mut shadow).ledger);
        let aster = InstrumentKey::parse("aster/ETHUSDT").unwrap();
        let hl = InstrumentKey::parse("hyperliquid/ETH").unwrap();
        for (i, r) in rates.iter().enumerate() {
            w.b.now_ms += 8 * HOUR_MS;
            let t = w.b.now_ms;
            let rate = Decimal::new(*r, 5);
            w.b.settle(&aster, t, rate);
            w.b.predict(&aster, rate);
            let ns = sh_md::NewSettlement { key: aster.clone(), settlement: *w.b.market.get(&aster).unwrap().funding.last_settled().unwrap() };
            journal.extend(shadow.on_settlement(&ns, &w.b.market));
            // сдвиг цены на одной ноге создаёт базис
            let mv = Decimal::new(*moves.get(i).unwrap_or(&0), 3);
            w.b.set_book(&hl, dec!(100) + mv, dec!(0.01), dec!(10000));
            journal.extend(w.step(&mut shadow).ledger);
            let unreal: Decimal = shadow.positions.values().map(|p| p.unrealized_basis(&w.b.market).0).sum();
            prop_assert_eq!(shadow.equity(&w.b.market).0, dec!(100000) + ledger_sum(&journal) + unreal);
            // доля пары никогда не превышает 25% начального капитала с учётом дохода
            for p in shadow.positions.values() {
                prop_assert!(p.capital.0 <= shadow.equity(&w.b.market).0 * dec!(0.25) + dec!(1000));
            }
        }
    }
}
