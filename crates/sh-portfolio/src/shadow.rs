//! Теневой портфель M1. Позиции открываются и закрываются по тем же правилам, что
//! будут у стратегий S1/S2 (оценка пары из `sh-signals`), без ордеров. Фандинг
//! начисляется в моменты фактических расчётов площадок по количеству позиции и цене
//! расчёта площадки; комиссии и проскальзывание — по стаканам в момент входа и выхода.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Collateral, Exchange, Px, Qty, Side, Usd};
use sh_md::{InstrumentKey, MarketState, NewSettlement};
use sh_signals::cost::{LegCostInput, round_trip_cost};
use sh_signals::{
    CostParams, FeesConfig, Pair, PairEval, PairKind, PairRules, RiskLimits, SizeBudget, capital_for, pct,
};

/// Вид записи журнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// Фандинг (получен — плюс, уплачен — минус).
    Funding,
    /// Комиссии.
    Fee,
    /// Проскальзывание от mid.
    Slippage,
    /// Переводы на перебалансировку и конвертация обеспечения.
    Transfer,
    /// Ценовой (базисный) результат при закрытии: должен быть около нуля.
    Basis,
}

/// Запись журнала теневого портфеля.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Время, мс.
    pub ts_ms: i64,
    /// Пара.
    pub pair_id: String,
    /// Нога, если запись к ней относится.
    pub leg: Option<String>,
    /// Вид.
    pub kind: LedgerKind,
    /// Сумма со знаком (плюс — доход).
    pub amount: Usd,
}

/// Позиция по паре.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowPosition {
    /// Пара.
    pub pair: Pair,
    /// Короткая нога.
    pub short: InstrumentKey,
    /// Длинная нога.
    pub long: InstrumentKey,
    /// Количество на ногу.
    pub qty: Qty,
    /// Номинал на ногу при входе.
    pub notional: Usd,
    /// Плечо короткой и длинной ноги.
    pub leverage: (Decimal, Decimal),
    /// Капитал под пару (маржа и резерв).
    pub capital: Usd,
    /// Время входа, мс.
    pub entry_ms: i64,
    /// Mid короткой и длинной ноги при входе.
    pub entry_mid: (Px, Px),
    /// Накопленный фандинг.
    pub funding: Usd,
    /// Накопленные издержки (комиссии, проскальзывание, переводы), положительные.
    pub costs: Usd,
    /// Подряд оценок EMA ниже порога выхода.
    pub below_exit: u32,
    /// Отметка последнего расчёта, по которому проверялся выход.
    pub last_exit_check_ms: i64,
}

impl ShadowPosition {
    /// Нереализованный базисный результат по текущим mid.
    pub fn unrealized_basis(&self, market: &MarketState) -> Usd {
        let mid = |k: &InstrumentKey| market.get(k).and_then(|s| s.mid());
        match (mid(&self.short), mid(&self.long)) {
            (Some(s), Some(l)) => Usd(self.qty.0 * ((l.0 - self.entry_mid.1.0) - (s.0 - self.entry_mid.0.0))),
            _ => Usd::ZERO,
        }
    }

    /// Маржа ноги на площадке.
    fn margin_on(&self, exchange: Exchange) -> Decimal {
        let mut m = Decimal::ZERO;
        if self.short.venue.exchange == exchange {
            m += self.notional.0 / self.leverage.0;
        }
        if self.long.venue.exchange == exchange {
            m += self.notional.0 / self.leverage.1;
        }
        m
    }
}

/// Параметры теневого портфеля.
#[derive(Debug, Clone)]
pub struct ShadowParams {
    /// Начальный капитал.
    pub capital: Usd,
    /// Доля резерва.
    pub reserve_frac: Decimal,
    /// Правила S1.
    pub s1: PairRules,
    /// Правила S2.
    pub s2: PairRules,
    /// Лимиты риска.
    pub risk: RiskLimits,
    /// Издержки.
    pub costs: CostParams,
    /// Комиссии.
    pub fees: FeesConfig,
}

impl ShadowParams {
    fn rules(&self, kind: PairKind) -> &PairRules {
        match kind {
            PairKind::S1 => &self.s1,
            PairKind::S2 => &self.s2,
        }
    }
}

/// Что сделал шаг.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepOutcome {
    /// Открытые пары.
    pub opened: Vec<String>,
    /// Закрытые пары с причиной.
    pub closed: Vec<(String, String)>,
    /// Новые записи журнала.
    pub ledger: Vec<LedgerEntry>,
}

/// Теневой портфель.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowPortfolio {
    /// Начальный капитал.
    pub initial_capital: Usd,
    /// Реализованный результат (фандинг минус издержки плюс базис закрытых позиций).
    pub realized: Usd,
    /// Открытые позиции по идентификатору пары.
    pub positions: BTreeMap<String, ShadowPosition>,
    /// Пиковый капитал.
    pub peak_equity: Usd,
    /// Входы запрещены до этого времени (дневной лимит убытка), мс.
    pub entries_blocked_until_ms: i64,
    /// Капитал на начало текущих суток UTC.
    pub day_start_equity: Usd,
    /// Начало текущих суток, мс.
    pub day_start_ms: i64,
    /// Максимальная просадка от пика за всё время (доля).
    #[serde(default)]
    pub max_drawdown: Decimal,
    /// Время создания портфеля, мс.
    #[serde(default)]
    pub started_ms: i64,
}

const DAY_MS: i64 = 86_400_000;

impl ShadowPortfolio {
    /// Новый портфель.
    pub fn new(capital: Usd, now_ms: i64) -> Self {
        Self {
            initial_capital: capital,
            realized: Usd::ZERO,
            positions: BTreeMap::new(),
            peak_equity: capital,
            entries_blocked_until_ms: 0,
            day_start_equity: capital,
            day_start_ms: now_ms - now_ms.rem_euclid(DAY_MS),
            max_drawdown: Decimal::ZERO,
            started_ms: now_ms,
        }
    }

    /// Капитал: начальный + реализованный + нереализованный базис.
    pub fn equity(&self, market: &MarketState) -> Usd {
        let unreal: Decimal = self.positions.values().map(|p| p.unrealized_basis(market).0).sum();
        Usd(self.initial_capital.0 + self.realized.0 + unreal)
    }

    /// Занятый капитал.
    pub fn used_capital(&self) -> Usd {
        Usd(self.positions.values().map(|p| p.capital.0).sum())
    }

    /// Начисляет фандинг по новому расчёту; только позициям, открытым до расчёта.
    pub fn on_settlement(&mut self, ns: &NewSettlement, market: &MarketState) -> Vec<LedgerEntry> {
        let mut out = Vec::new();
        let price = market.get(&ns.key).and_then(|s| s.funding_price(ns.key.venue.exchange));
        let Some(price) = price else { return out };
        for (id, pos) in self.positions.iter_mut() {
            if ns.settlement.ts_ms < pos.entry_ms {
                continue;
            }
            // при положительной ставке лонги платят шортам
            let sign = if pos.short == ns.key {
                Decimal::ONE
            } else if pos.long == ns.key {
                -Decimal::ONE
            } else {
                continue;
            };
            let amount = Usd(sign * ns.settlement.rate * pos.qty.0 * price.0);
            pos.funding.0 += amount.0;
            self.realized.0 += amount.0;
            out.push(LedgerEntry {
                ts_ms: ns.settlement.ts_ms,
                pair_id: id.clone(),
                leg: Some(ns.key.id()),
                kind: LedgerKind::Funding,
                amount,
            });
        }
        out
    }

    /// Шаг: выходы, затем входы. `evaluate_with_budget` оценивает пару с бюджетом размера.
    pub fn step(
        &mut self,
        evals: &[PairEval],
        market: &MarketState,
        params: &ShadowParams,
        now_ms: i64,
        evaluate_with_budget: &dyn Fn(&Pair, SizeBudget) -> PairEval,
    ) -> StepOutcome {
        let mut out = StepOutcome::default();
        self.roll_day(market, now_ms, params);
        let equity = self.equity(market);
        if equity.0 > self.peak_equity.0 {
            self.peak_equity = equity;
        }
        let drawdown = if self.peak_equity.0 > Decimal::ZERO {
            (self.peak_equity.0 - equity.0) / self.peak_equity.0
        } else {
            Decimal::ZERO
        };
        self.max_drawdown = self.max_drawdown.max(drawdown);
        let drawdown_hit = drawdown > pct(params.risk.max_drawdown_pct);

        // выходы
        let by_id: BTreeMap<String, &PairEval> = evals.iter().map(|e| (e.pair.id(), e)).collect();
        let ids: Vec<String> = self.positions.keys().cloned().collect();
        for id in ids {
            let reason = {
                let Some(pos) = self.positions.get_mut(&id) else { continue };
                exit_reason(pos, by_id.get(&id).copied(), market, params, drawdown_hit)
            };
            if let Some(reason) = reason {
                out.ledger.extend(self.close(&id, market, params, now_ms));
                out.closed.push((id, reason));
            }
        }

        // входы
        if drawdown_hit || now_ms < self.entries_blocked_until_ms {
            return out;
        }
        let mut candidates: Vec<&PairEval> =
            evals.iter().filter(|e| e.entry_ok && !self.positions.contains_key(&e.pair.id())).collect();
        candidates.sort_by_key(|e| std::cmp::Reverse(net_of(e)));
        for cand in candidates {
            let equity = self.equity(market);
            // одна позиция на инструмент: ноги разных пар не должны сливаться
            let busy = self
                .positions
                .values()
                .any(|p| [&p.short, &p.long].iter().any(|k| **k == cand.pair.a || **k == cand.pair.b));
            if busy {
                continue;
            }
            let Some(budget) = self.budget_for(cand, equity, params) else { continue };
            let e = evaluate_with_budget(&cand.pair, budget);
            if !e.entry_ok {
                continue;
            }
            if let Some(entries) = self.open(&e, market, params, now_ms) {
                out.opened.push(e.pair.id());
                out.ledger.extend(entries);
            }
        }
        out
    }

    fn roll_day(&mut self, market: &MarketState, now_ms: i64, params: &ShadowParams) {
        let day = now_ms - now_ms.rem_euclid(DAY_MS);
        let equity = self.equity(market);
        if day > self.day_start_ms {
            self.day_start_ms = day;
            self.day_start_equity = equity;
        }
        let loss = self.day_start_equity.0 - equity.0;
        if self.day_start_equity.0 > Decimal::ZERO
            && loss / self.day_start_equity.0 > pct(params.risk.daily_loss_limit_pct)
        {
            self.entries_blocked_until_ms = self.entries_blocked_until_ms.max(now_ms + DAY_MS);
        }
    }

    /// Бюджет размера для кандидата с учётом долей пары, платформ и S2.
    fn budget_for(&self, e: &PairEval, equity: Usd, params: &ShadowParams) -> Option<SizeBudget> {
        let free = equity.0 - self.used_capital().0;
        let mut pair_capital = free.min(equity.0 * pct(params.risk.max_pair_share_pct));
        if e.pair.kind == PairKind::S2 {
            let s2_used: Decimal =
                self.positions.values().filter(|p| p.pair.kind == PairKind::S2).map(|p| p.capital.0).sum();
            pair_capital = pair_capital.min(equity.0 * pct(params.risk.max_s2_share_pct) - s2_used);
        }
        if pair_capital <= Decimal::ZERO {
            return None;
        }
        let room = |k: &InstrumentKey| {
            let ex = k.venue.exchange;
            let used: Decimal = self.positions.values().map(|p| p.margin_on(ex)).sum();
            let mut r = equity.0 * params.risk.platform_share(ex) - used;
            // обе ноги на одной платформе делят её остаток
            if e.pair.a.venue.exchange == e.pair.b.venue.exchange {
                r /= Decimal::TWO;
            }
            Usd(r.max(Decimal::ZERO))
        };
        Some(SizeBudget { pair_capital: Usd(pair_capital), room_margin: [room(&e.pair.a), room(&e.pair.b)] })
    }

    fn open(
        &mut self,
        e: &PairEval,
        market: &MarketState,
        params: &ShadowParams,
        now_ms: i64,
    ) -> Option<Vec<LedgerEntry>> {
        let size = e.size?;
        let at = e.at_size?;
        let cost = at.cost?;
        let mid = |k: &InstrumentKey| market.get(k).and_then(|s| s.mid());
        let entry_mid = (mid(&e.short)?, mid(&e.long)?);
        let capital = capital_for(size.notional, e.leverage.0, e.leverage.1, params.reserve_frac)?;
        let id = e.pair.id();
        let entries = vec![
            entry(now_ms, &id, LedgerKind::Fee, Usd(-cost.fees_in.0)),
            entry(now_ms, &id, LedgerKind::Slippage, Usd(-cost.slip_in.0)),
            // перевод и половина конвертации — на входе, остальное — на выходе
            entry(now_ms, &id, LedgerKind::Transfer, Usd(-(cost.rebalance.0 + cost.conversion.0 / Decimal::TWO))),
        ];
        let paid: Decimal = entries.iter().map(|x| -x.amount.0).sum();
        self.realized.0 -= paid;
        self.positions.insert(
            id,
            ShadowPosition {
                pair: e.pair.clone(),
                short: e.short.clone(),
                long: e.long.clone(),
                qty: size.qty,
                notional: size.notional,
                leverage: e.leverage,
                capital,
                entry_ms: now_ms,
                entry_mid,
                funding: Usd::ZERO,
                costs: Usd(paid),
                below_exit: 0,
                last_exit_check_ms: now_ms,
            },
        );
        Some(entries.into_iter().filter(|x| !x.amount.is_zero()).collect())
    }

    /// Закрывает позицию: издержки выхода по текущим стаканам и базисный результат.
    pub fn close(&mut self, id: &str, market: &MarketState, params: &ShadowParams, now_ms: i64) -> Vec<LedgerEntry> {
        let Some(pos) = self.positions.remove(id) else { return Vec::new() };
        let mut entries = Vec::new();
        let basis = pos.unrealized_basis(market);
        entries.push(entry(now_ms, id, LedgerKind::Basis, basis));
        if let (Some(ss), Some(sl)) = (market.get(&pos.short), market.get(&pos.long)) {
            let price = pos.notional.0 / pos.qty.0.max(Decimal::new(1, 18));
            let now_notional = match (ss.mid(), sl.mid()) {
                (Some(a), Some(b)) => Usd(pos.qty.0 * (a.0 + b.0) / Decimal::TWO),
                _ => Usd(pos.qty.0 * price),
            };
            let cost = round_trip_cost(
                &leg_input(ss, Side::Sell, pos.leverage.0, &params.fees),
                &leg_input(sl, Side::Buy, pos.leverage.1, &params.fees),
                now_notional,
                Usd::ZERO,
                &params.costs,
            );
            match cost {
                Some(c) => {
                    entries.push(entry(now_ms, id, LedgerKind::Fee, Usd(-c.fees_out.0)));
                    entries.push(entry(now_ms, id, LedgerKind::Slippage, Usd(-c.slip_out.0)));
                    entries.push(entry(now_ms, id, LedgerKind::Transfer, Usd(-c.conversion.0 / Decimal::TWO)));
                }
                None => {
                    // стакана нет или он тонкий: консервативно — комиссии тейкера и 50 bp проскальзывания
                    let fees = |k: &InstrumentKey| {
                        market
                            .get(k)
                            .and_then(|s| s.meta.as_ref())
                            .map(|m| params.fees.resolve(m).taker)
                            .unwrap_or_default()
                    };
                    let f = now_notional.0 * (fees(&pos.short) + fees(&pos.long));
                    entries.push(entry(now_ms, id, LedgerKind::Fee, Usd(-f)));
                    entries.push(entry(now_ms, id, LedgerKind::Slippage, Usd(-now_notional.0 * Decimal::new(100, 4))));
                }
            }
        }
        let total: Decimal = entries.iter().map(|e| e.amount.0).sum();
        self.realized.0 += total;
        entries.into_iter().filter(|x| !x.amount.is_zero()).collect()
    }
}

fn leg_input<'a>(st: &'a sh_md::InstrumentState, side: Side, lev: Decimal, fees: &FeesConfig) -> LegCostInput<'a> {
    let meta = st.meta.as_ref();
    LegCostInput {
        book: &st.book,
        fees: meta.map(|m| fees.resolve(m)).unwrap_or_default(),
        entry_side: side,
        leverage: lev,
        needs_conversion: meta.is_some_and(|m| {
            m.venue.exchange == Exchange::Hyperliquid && !m.venue.dex.is_empty() && m.collateral != Collateral::Usdc
        }),
    }
}

fn entry(ts_ms: i64, id: &str, kind: LedgerKind, amount: Usd) -> LedgerEntry {
    LedgerEntry { ts_ms, pair_id: id.to_owned(), leg: None, kind, amount }
}

fn net_of(e: &PairEval) -> Decimal {
    e.at_size.or_else(|| e.tiers.first().copied()).and_then(|t| t.s_net).unwrap_or(Decimal::MIN)
}

/// Причина выхода или `None`.
fn exit_reason(
    pos: &mut ShadowPosition,
    eval: Option<&PairEval>,
    market: &MarketState,
    params: &ShadowParams,
    drawdown_hit: bool,
) -> Option<String> {
    if drawdown_hit {
        return Some("max drawdown".into());
    }
    for k in [&pos.short, &pos.long] {
        let active = market
            .get(k)
            .and_then(|s| s.meta.as_ref())
            .is_some_and(|m| m.status == sh_venue_api::InstrumentStatus::Active);
        if !active {
            return Some(format!("leg {k} inactive or delisted"));
        }
    }
    let eval = eval?;
    // EMA спреда в ориентации позиции: если оценка перевернула ноги, знак меняется
    let ema = eval.est.ema_24h.map(|v| if eval.short == pos.short { v } else { -v });
    // проверяем по расчётам: «m интервалов подряд»
    let last_settle = [&pos.short, &pos.long]
        .iter()
        .filter_map(|k| market.get(k).and_then(|s| s.funding.last_settled()).map(|s| s.ts_ms))
        .max()
        .unwrap_or(0);
    if last_settle > pos.last_exit_check_ms {
        pos.last_exit_check_ms = last_settle;
        let rules = params.rules(pos.pair.kind);
        if ema.is_none_or(|v| v < rules.exit) {
            pos.below_exit += 1;
        } else {
            pos.below_exit = 0;
        }
        if pos.below_exit >= rules.m_intervals {
            return Some(format!("ema below exit for {} intervals", pos.below_exit));
        }
    }
    None
}
