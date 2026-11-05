//! Движок сканера (M1): состояние рынка из событий, оценка всех пар раз в
//! `interval_secs`, теневой портфель, дневной отчёт на границе суток UTC.
//! Движок не делает I/O и не читает системное время — время приходит аргументом,
//! поэтому один и тот же код работает вживую и при повторе записей.

use std::collections::HashMap;

use chrono::DateTime;
use rust_decimal::Decimal;
use sh_core::Usd;
use sh_md::{InstrumentKey, MarketState};
use sh_portfolio::{DailyReport, LedgerEntry, ShadowParams, ShadowPortfolio};
use sh_signals::{
    CostParams, EvalContext, FeesConfig, Pair, PairEval, RiskLimits, ScannerParams, SizeBudget, StrategyParams,
    SymbolMap, evaluate, pairs_from_symbols, pct, suggest_mappings,
};
use sh_venue_api::{MdEvent, MdEventKind};

/// Настройки движка.
#[derive(Debug, Clone)]
pub struct ScanSettings {
    /// Сканер.
    pub scanner: ScannerParams,
    /// Стратегии.
    pub strategy: StrategyParams,
    /// Издержки.
    pub costs: CostParams,
    /// Комиссии.
    pub fees: FeesConfig,
    /// Риск.
    pub risk: RiskLimits,
    /// Доля резерва.
    pub reserve_frac: Decimal,
}

/// Результат такта.
#[derive(Debug, Default)]
pub struct TickOutput {
    /// Оценка пар выполнена в этом такте.
    pub scanned: bool,
    /// Новые записи журнала теневого портфеля.
    pub ledger: Vec<LedgerEntry>,
    /// Открытые пары.
    pub opened: Vec<String>,
    /// Закрытые пары с причиной.
    pub closed: Vec<(String, String)>,
    /// Отчёт за завершившиеся сутки.
    pub report: Option<DailyReport>,
    /// Инструменты, впервые увиденные (для базы).
    pub first_seen: Vec<String>,
}

const DAY_MS: i64 = 86_400_000;

/// Движок сканера.
pub struct ScanEngine {
    settings: ScanSettings,
    market: MarketState,
    symbols: SymbolMap,
    pairs: Vec<Pair>,
    shadow: Option<ShadowPortfolio>,
    first_seen: HashMap<String, i64>,
    pending_first_seen: Vec<String>,
    pending_ledger: Vec<LedgerEntry>,
    day_ledger: Vec<LedgerEntry>,
    last_scan_ms: Option<i64>,
    current_day_ms: Option<i64>,
    last_evals: Vec<PairEval>,
    tiers: Vec<Usd>,
}

impl ScanEngine {
    /// Создаёт движок. `shadow` — восстановленное состояние или `None` для нового портфеля.
    pub fn new(
        settings: ScanSettings,
        symbols: SymbolMap,
        shadow: Option<ShadowPortfolio>,
        first_seen: HashMap<String, i64>,
        day_ledger: Vec<LedgerEntry>,
    ) -> Self {
        let pairs = pairs_from_symbols(&symbols);
        let tiers = settings.scanner.notional_tiers_usd.iter().map(|v| Usd(*v)).collect();
        Self {
            settings,
            market: MarketState::new(),
            symbols,
            pairs,
            shadow,
            first_seen,
            pending_first_seen: Vec::new(),
            pending_ledger: Vec::new(),
            day_ledger,
            last_scan_ms: None,
            current_day_ms: None,
            last_evals: Vec::new(),
            tiers,
        }
    }

    /// Состояние рынка.
    pub fn market(&self) -> &MarketState {
        &self.market
    }

    /// Пары из сопоставления.
    pub fn pairs(&self) -> &[Pair] {
        &self.pairs
    }

    /// Теневой портфель.
    pub fn shadow(&self) -> Option<&ShadowPortfolio> {
        self.shadow.as_ref()
    }

    /// Последние оценки.
    pub fn last_evals(&self) -> &[PairEval] {
        &self.last_evals
    }

    /// Ступени номинала.
    pub fn tiers(&self) -> &[Usd] {
        &self.tiers
    }

    /// Применяет событие рынка.
    pub fn on_event(&mut self, ev: &MdEvent) {
        if let MdEventKind::Metadata(m) = &ev.kind {
            let id = InstrumentKey::new(m.venue.clone(), m.symbol.clone()).id();
            if !self.first_seen.contains_key(&id) {
                self.first_seen.insert(id.clone(), ev.recv.wall_ns.div_euclid(1_000_000));
                self.pending_first_seen.push(id);
            }
        }
        if let Some(ns) = self.market.apply(ev)
            && let Some(shadow) = self.shadow.as_mut()
        {
            let entries = shadow.on_settlement(&ns, &self.market);
            self.pending_ledger.extend(entries);
        }
    }

    fn shadow_params(&self) -> ShadowParams {
        let s = &self.settings;
        ShadowParams {
            capital: Usd(s.scanner.shadow_capital_usd),
            reserve_frac: s.reserve_frac,
            s1: s.strategy.s1_rules(),
            s2: s.strategy.s2_rules(),
            risk: s.risk.clone(),
            costs: s.costs.clone(),
            fees: s.fees.clone(),
        }
    }

    /// Оценка пары под бюджет размера (движок исполнения).
    pub fn eval_with_budget(&self, pair: &Pair, now_ms: i64, budget: SizeBudget) -> PairEval {
        self.eval_pair(pair, now_ms, Some(budget))
    }

    /// Настройки.
    pub fn settings(&self) -> &ScanSettings {
        &self.settings
    }

    fn eval_pair(&self, pair: &Pair, now_ms: i64, budget: Option<SizeBudget>) -> PairEval {
        let first_seen = |k: &InstrumentKey| self.first_seen.get(&k.id()).copied();
        let s = &self.settings;
        let ctx = EvalContext {
            market: &self.market,
            symbols: &self.symbols,
            strategy: &s.strategy,
            costs: &s.costs,
            fees: &s.fees,
            risk: &s.risk,
            now_ms,
            reserve_frac: s.reserve_frac,
            target_roc: pct(s.scanner.target_roc_pct),
            tiers: &self.tiers,
            first_seen_ms: &first_seen,
            budget,
        };
        evaluate(pair, &ctx)
    }

    /// Такт движка: оценка пар по расписанию, шаг теневого портфеля, отчёт на границе суток.
    pub fn tick(&mut self, now_ms: i64) -> TickOutput {
        let mut out = TickOutput { first_seen: std::mem::take(&mut self.pending_first_seen), ..TickOutput::default() };
        // записи расчётов фандинга, пришедшие между тактами
        let mut new_entries = std::mem::take(&mut self.pending_ledger);
        self.day_ledger.extend(new_entries.iter().cloned());

        // отчёт за прошедшие сутки — до шага портфеля, который переносит начало суток
        let day = now_ms - now_ms.rem_euclid(DAY_MS);
        if let Some(prev) = self.current_day_ms
            && day > prev
        {
            if let Some(shadow) = &self.shadow {
                let date =
                    DateTime::from_timestamp_millis(prev).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
                let in_day: Vec<LedgerEntry> =
                    self.day_ledger.iter().filter(|e| e.ts_ms >= prev && e.ts_ms < day).cloned().collect();
                out.report =
                    Some(DailyReport::build(&date, &in_day, shadow, &self.market, &self.last_evals, now_ms, 20));
            }
            self.day_ledger.retain(|e| e.ts_ms >= day);
        }
        self.current_day_ms = Some(day);

        let interval_ms = i64::try_from(self.settings.scanner.interval_secs.max(1)).unwrap_or(60) * 1000;
        if self.last_scan_ms.is_none_or(|t| now_ms - t >= interval_ms) {
            self.last_scan_ms = Some(now_ms);
            out.scanned = true;
            let evals: Vec<PairEval> = self.pairs.iter().map(|p| self.eval_pair(p, now_ms, None)).collect();
            if let Some(mut shadow) = self.shadow.take() {
                let params = self.shadow_params();
                let f = |p: &Pair, b: SizeBudget| self.eval_pair(p, now_ms, Some(b));
                let step = shadow.step(&evals, &self.market, &params, now_ms, &f);
                self.day_ledger.extend(step.ledger.iter().cloned());
                new_entries.extend(step.ledger);
                out.opened = step.opened;
                out.closed = step.closed;
                self.shadow = Some(shadow);
            }
            self.last_evals = evals;
        }
        out.ledger = new_entries;
        out
    }

    /// Предложения автосопоставления для одобрения оператором.
    pub fn suggestions(&self) -> String {
        sh_signals::suggestions_toml(&suggest_mappings(&self.market, &self.symbols))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use sh_signals::testkit::{InstrumentSpec, MarketBuilder};

    const SYMBOLS: &str = "[[underlying]]\nid = \"ETH\"\nmembers = [\"hyperliquid/ETH\", \"aster/ETHUSDT\"]\n";

    fn settings() -> ScanSettings {
        ScanSettings {
            scanner: ScannerParams::default(),
            strategy: StrategyParams::default(),
            costs: CostParams::default(),
            fees: FeesConfig::default(),
            risk: RiskLimits::default(),
            reserve_frac: dec!(0.15),
        }
    }

    /// События построителя рынка, проигранные через движок.
    fn engine_with_market(b: &MarketBuilder, now_ms: i64) -> ScanEngine {
        let mut e = ScanEngine::new(
            settings(),
            SymbolMap::from_toml_str(SYMBOLS).unwrap(),
            Some(ShadowPortfolio::new(Usd(dec!(100000)), now_ms)),
            HashMap::new(),
            Vec::new(),
        );
        e.market = b.market.clone();
        e
    }

    #[test]
    fn scans_opens_and_reports_at_day_boundary() {
        let mut b = MarketBuilder::new();
        b.add(&InstrumentSpec::liquid("aster/ETHUSDT", dec!(0.0003), dec!(8)), 8 * 24);
        b.add(&InstrumentSpec::liquid("hyperliquid/ETH", dec!(0.000001), dec!(1)), 8 * 24);
        let now = b.now_ms + 3_600_000;
        let mut e = engine_with_market(&b, now);
        let out = e.tick(now);
        assert!(out.scanned);
        assert_eq!(out.opened.len(), 1);
        assert!(!out.ledger.is_empty());
        // через 30 секунд — без оценки
        assert!(!e.tick(now + 30_000).scanned);
        // переход суток: отчёт за прошедшие
        let next_day = now - now.rem_euclid(DAY_MS) + DAY_MS + 1000;
        let out = e.tick(next_day);
        let rep = out.report.unwrap();
        assert_eq!(rep.positions.len(), 1);
        assert!(rep.fees < Decimal::ZERO);
        assert_eq!(e.last_evals().len(), 1);
    }

    #[test]
    fn first_seen_is_reported_once() {
        let mut e = ScanEngine::new(settings(), SymbolMap::default(), None, HashMap::new(), Vec::new());
        let mut b = MarketBuilder::new();
        b.add(&InstrumentSpec::liquid("aster/ETHUSDT", dec!(0.0003), dec!(8)), 8);
        // повторяем событие метаданных через движок
        let meta = b.market.get(&InstrumentKey::parse("aster/ETHUSDT").unwrap()).unwrap().meta.clone().unwrap();
        let ev = MdEvent {
            venue: meta.venue.clone(),
            symbol: meta.symbol.clone(),
            recv: sh_core::RecvTs { wall_ns: 5_000_000, mono_ns: 0 },
            venue_ts_ms: None,
            kind: MdEventKind::Metadata(Box::new(meta)),
        };
        e.on_event(&ev);
        e.on_event(&ev);
        assert_eq!(e.tick(10).first_seen, vec!["aster/ETHUSDT".to_owned()]);
        assert!(e.tick(20).first_seen.is_empty());
    }
}
