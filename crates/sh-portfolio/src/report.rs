//! Дневной отчёт теневого портфеля и таблица возможностей сканера:
//! Markdown и CSV в `reports/`, короткая сводка для Telegram.

use rust_decimal::Decimal;
use serde::Serialize;
use sh_core::Usd;
use sh_md::MarketState;
use sh_signals::{PairEval, PairKind, Reason};

use crate::shadow::{LedgerEntry, LedgerKind, ShadowPortfolio};

/// Строка позиции.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PositionRow {
    /// Пара.
    pub pair_id: String,
    /// Вид (S1/S2).
    pub kind: String,
    /// Короткая нога.
    pub short: String,
    /// Длинная нога.
    pub long: String,
    /// Количество на ногу.
    pub qty: Decimal,
    /// Номинал на ногу.
    pub notional: Decimal,
    /// Капитал.
    pub capital: Decimal,
    /// Часов в позиции.
    pub hours_held: Decimal,
    /// Фандинг.
    pub funding: Decimal,
    /// Издержки.
    pub costs: Decimal,
    /// Нереализованный базис.
    pub unrealized_basis: Decimal,
}

/// Строка возможности.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpportunityRow {
    /// Пара.
    pub pair_id: String,
    /// Вид.
    pub kind: String,
    /// Короткая нога.
    pub short: String,
    /// Длинная нога.
    pub long: String,
    /// Спред по прогнозу, % годовых.
    pub s_now_pct: Option<Decimal>,
    /// EMA 24 ч, % годовых.
    pub s_ema_pct: Option<Decimal>,
    /// Реализованный 7 д, % годовых.
    pub s_7d_pct: Option<Decimal>,
    /// Знак устойчив.
    pub persistent: bool,
    /// Издержки входа и выхода на ступенях номинала, $ (пусто — не хватает глубины).
    pub tier_costs: Vec<Option<Decimal>>,
    /// Чистый спред на размере (или первой ступени), % годовых.
    pub s_net_pct: Option<Decimal>,
    /// Точка безубыточности, ч.
    pub breakeven_h: Option<Decimal>,
    /// ROC, %.
    pub roc_pct: Option<Decimal>,
    /// Нужный чистый спред для целевого ROC, %.
    pub required_net_pct: Option<Decimal>,
    /// Вход разрешён.
    pub entry_ok: bool,
    /// Причины отказа.
    pub reasons: String,
}

fn pct2(v: Option<Decimal>) -> Option<Decimal> {
    v.map(|x| (x * Decimal::ONE_HUNDRED).round_dp(2))
}

fn reason_str(r: &Reason) -> String {
    match r {
        Reason::MissingData(s) => format!("missing_data({s})"),
        Reason::Inactive(s) => format!("inactive({s})"),
        Reason::LowOpenInterest(s) => format!("low_oi({s})"),
        Reason::NewListing(s) => format!("new_listing({s})"),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Строки возможностей, отсортированные по чистому спреду.
pub fn opportunity_rows(evals: &[PairEval]) -> Vec<OpportunityRow> {
    let mut rows: Vec<OpportunityRow> = evals
        .iter()
        .map(|e| {
            let t = e.at_size.or_else(|| e.tiers.first().copied());
            OpportunityRow {
                pair_id: e.pair.id(),
                kind: match e.pair.kind {
                    PairKind::S1 => "S1".into(),
                    PairKind::S2 => "S2".into(),
                },
                short: e.short.id(),
                long: e.long.id(),
                s_now_pct: pct2(e.est.now),
                s_ema_pct: pct2(e.est.ema_24h),
                s_7d_pct: pct2(e.est.realized_7d),
                persistent: e.est.persistent,
                tier_costs: e.tiers.iter().map(|t| t.cost.map(|c| c.total.0.round_dp(2))).collect(),
                s_net_pct: pct2(t.and_then(|t| t.s_net)),
                breakeven_h: t.and_then(|t| t.breakeven_h).map(|h| h.round_dp(1)),
                roc_pct: pct2(t.and_then(|t| t.roc)),
                required_net_pct: pct2(e.required_net_spread),
                entry_ok: e.entry_ok,
                reasons: e.reasons.iter().map(reason_str).collect::<Vec<_>>().join(";"),
            }
        })
        .collect();
    rows.sort_by(|a, b| b.s_net_pct.unwrap_or(Decimal::MIN).cmp(&a.s_net_pct.unwrap_or(Decimal::MIN)));
    rows
}

fn opt(v: Option<Decimal>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_owned() }
}

/// CSV возможностей; `tiers` — номиналы ступеней для заголовков.
pub fn opportunities_csv(rows: &[OpportunityRow], tiers: &[Usd]) -> String {
    let mut out = String::from("pair_id,kind,short,long,s_now_pct,s_ema_pct,s_7d_pct,persistent,");
    for t in tiers {
        out.push_str(&format!("cost_{}_usd,", t.0.normalize()));
    }
    out.push_str("s_net_pct,breakeven_h,roc_pct,required_net_pct,entry_ok,reasons\n");
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},",
            csv_field(&r.pair_id),
            r.kind,
            csv_field(&r.short),
            csv_field(&r.long),
            opt(r.s_now_pct),
            opt(r.s_ema_pct),
            opt(r.s_7d_pct),
            r.persistent
        ));
        for i in 0..tiers.len() {
            out.push_str(&opt(r.tier_costs.get(i).copied().flatten()));
            out.push(',');
        }
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            opt(r.s_net_pct),
            opt(r.breakeven_h),
            opt(r.roc_pct),
            opt(r.required_net_pct),
            r.entry_ok,
            csv_field(&r.reasons)
        ));
    }
    out
}

/// Дневной отчёт.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailyReport {
    /// Дата UTC.
    pub date: String,
    /// Капитал на начало суток.
    pub start_equity: Decimal,
    /// Капитал на конец.
    pub end_equity: Decimal,
    /// Фандинг за сутки.
    pub funding: Decimal,
    /// Комиссии.
    pub fees: Decimal,
    /// Проскальзывание.
    pub slippage: Decimal,
    /// Переводы и конвертация.
    pub transfers: Decimal,
    /// Реализованный базис.
    pub basis: Decimal,
    /// Доходность за сутки в годовых, %.
    pub day_annualized_pct: Decimal,
    /// Доходность с начала в годовых, %.
    pub since_start_annualized_pct: Decimal,
    /// Дней с начала.
    pub days_since_start: Decimal,
    /// Максимальная просадка, %.
    pub max_drawdown_pct: Decimal,
    /// Позиции.
    pub positions: Vec<PositionRow>,
    /// Лучшие возможности.
    pub opportunities: Vec<OpportunityRow>,
}

impl DailyReport {
    /// Строит отчёт за сутки по журналу этих суток.
    pub fn build(
        date: &str,
        day_ledger: &[LedgerEntry],
        shadow: &ShadowPortfolio,
        market: &MarketState,
        evals: &[PairEval],
        now_ms: i64,
        top_n: usize,
    ) -> Self {
        let sum = |k: LedgerKind| -> Decimal { day_ledger.iter().filter(|e| e.kind == k).map(|e| e.amount.0).sum() };
        let end = shadow.equity(market).0;
        let start = shadow.day_start_equity.0;
        let days = Decimal::from((now_ms - shadow.started_ms).max(1)) / Decimal::from(86_400_000);
        let init = shadow.initial_capital.0;
        let ann = |ret: Decimal, d: Decimal| if d > Decimal::ZERO { ret * Decimal::from(365) / d * Decimal::ONE_HUNDRED } else { Decimal::ZERO };
        let positions = shadow
            .positions
            .iter()
            .map(|(id, p)| PositionRow {
                pair_id: id.clone(),
                kind: format!("{:?}", p.pair.kind),
                short: p.short.id(),
                long: p.long.id(),
                qty: p.qty.0,
                notional: p.notional.0.round_dp(2),
                capital: p.capital.0.round_dp(2),
                hours_held: (Decimal::from(now_ms - p.entry_ms) / Decimal::from(3_600_000)).round_dp(1),
                funding: p.funding.0.round_dp(2),
                costs: p.costs.0.round_dp(2),
                unrealized_basis: p.unrealized_basis(market).0.round_dp(2),
            })
            .collect();
        let mut opportunities = opportunity_rows(evals);
        opportunities.truncate(top_n);
        Self {
            date: date.to_owned(),
            start_equity: start.round_dp(2),
            end_equity: end.round_dp(2),
            funding: sum(LedgerKind::Funding).round_dp(2),
            fees: sum(LedgerKind::Fee).round_dp(2),
            slippage: sum(LedgerKind::Slippage).round_dp(2),
            transfers: sum(LedgerKind::Transfer).round_dp(2),
            basis: sum(LedgerKind::Basis).round_dp(2),
            day_annualized_pct: if start > Decimal::ZERO { ann((end - start) / start, Decimal::ONE).round_dp(2) } else { Decimal::ZERO },
            since_start_annualized_pct: if init > Decimal::ZERO { ann((end - init) / init, days).round_dp(2) } else { Decimal::ZERO },
            days_since_start: days.round_dp(2),
            max_drawdown_pct: (shadow.max_drawdown * Decimal::ONE_HUNDRED).round_dp(2),
            positions,
            opportunities,
        }
    }

    /// Отчёт в Markdown.
    pub fn to_markdown(&self) -> String {
        let mut s = format!(
            "# Теневой портфель — {}\n\n\
             | Показатель | Значение |\n| --- | --- |\n\
             | Капитал на начало суток, $ | {} |\n| Капитал на конец, $ | {} |\n\
             | Фандинг, $ | {} |\n| Комиссии, $ | {} |\n| Проскальзывание, $ | {} |\n\
             | Переводы и конвертация, $ | {} |\n| Реализованный базис, $ | {} |\n\
             | Доходность за сутки, % годовых | {} |\n| Доходность с начала, % годовых | {} |\n\
             | Дней с начала | {} |\n| Максимальная просадка, % | {} |\n\n",
            self.date,
            self.start_equity,
            self.end_equity,
            self.funding,
            self.fees,
            self.slippage,
            self.transfers,
            self.basis,
            self.day_annualized_pct,
            self.since_start_annualized_pct,
            self.days_since_start,
            self.max_drawdown_pct
        );
        s.push_str("## Позиции\n\n| Пара | Вид | Шорт | Лонг | Номинал, $ | Капитал, $ | Часов | Фандинг, $ | Издержки, $ | Базис, $ |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for p in &self.positions {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                p.pair_id, p.kind, p.short, p.long, p.notional, p.capital, p.hours_held, p.funding, p.costs, p.unrealized_basis
            ));
        }
        s.push_str("\n## Лучшие возможности\n\n| Пара | Вид | Шорт | s сейчас, % | s EMA, % | s 7д, % | s_net, % | Безубыточность, ч | ROC, % | Нужно для цели, % | Вход | Причины |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for o in &self.opportunities {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                o.pair_id,
                o.kind,
                o.short,
                opt(o.s_now_pct),
                opt(o.s_ema_pct),
                opt(o.s_7d_pct),
                opt(o.s_net_pct),
                opt(o.breakeven_h),
                opt(o.roc_pct),
                opt(o.required_net_pct),
                if o.entry_ok { "да" } else { "нет" },
                o.reasons
            ));
        }
        s
    }

    /// CSV позиций.
    pub fn positions_csv(&self) -> String {
        let mut out = String::from("pair_id,kind,short,long,qty,notional,capital,hours_held,funding,costs,unrealized_basis\n");
        for p in &self.positions {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{}\n",
                csv_field(&p.pair_id),
                p.kind,
                csv_field(&p.short),
                csv_field(&p.long),
                p.qty,
                p.notional,
                p.capital,
                p.hours_held,
                p.funding,
                p.costs,
                p.unrealized_basis
            ));
        }
        out
    }

    /// Короткая сводка для Telegram (текст на английском, как логи).
    pub fn telegram_summary(&self) -> String {
        format!(
            "spread-hunter shadow {}: equity ${} (day {} % ann., since start {} % ann.), funding ${}, costs ${}, max DD {} %, positions {}",
            self.date,
            self.end_equity,
            self.day_annualized_pct,
            self.since_start_annualized_pct,
            self.funding,
            (self.fees + self.slippage + self.transfers).round_dp(2),
            self.max_drawdown_pct,
            self.positions.len()
        )
    }
}
