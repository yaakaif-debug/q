//! Бэктест (SPEC.md, «Backtester»): сырые записи проигрываются через парсеры адаптеров,
//! движок со стратегиями и риском и симулятор площадок. Время берётся из записей,
//! случайности нет: одни и те же данные дают побайтно одинаковый результат.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use rust_decimal::Decimal;
use sh_core::Exchange;
use sh_portfolio::LedgerEntry;
use sh_signals::SymbolMap;
use sh_sim::SimCore;

use crate::config::AppConfig;
use crate::engine::{Engine, EngineMode};
use crate::replay_source::{ReplaySource, Replayers};
use crate::runner::{write_atomic, write_report};
use crate::scan::ScanEngine;
use crate::simdrive::SimDriver;
use crate::trade::{engine_settings, sim_params};

/// Итог бэктеста.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacktestSummary {
    /// Записей прочитано.
    pub records: u64,
    /// Ордеров отправлено.
    pub orders: usize,
    /// Пар открыто.
    pub pairs: usize,
    /// Начальный капитал.
    pub start_equity: Decimal,
    /// Конечный капитал.
    pub end_equity: Decimal,
    /// Сумма журнала по видам.
    pub by_kind: Vec<(String, Decimal)>,
    /// Ордеров в `Unknown` на конец.
    pub unknown_orders: usize,
}

/// CSV журнала.
pub fn ledger_csv(entries: &[LedgerEntry]) -> String {
    let mut s = String::from("ts_ms,pair,leg,kind,amount_usd\n");
    for e in entries {
        s.push_str(&format!(
            "{},{},{},{:?},{}\n",
            e.ts_ms,
            e.pair_id,
            e.leg.clone().unwrap_or_default(),
            e.kind,
            e.amount.0
        ));
    }
    s
}

/// Прогон бэктеста за `[from_ms, to_ms)`; результаты — в `out_dir`.
pub fn run_backtest(
    cfg: &AppConfig,
    raw: &Path,
    from_ms: i64,
    to_ms: i64,
    out_dir: &Path,
) -> anyhow::Result<BacktestSummary> {
    let symbols = SymbolMap::load(Path::new(&cfg.symbols.path)).context("symbols")?;
    let settings = engine_settings(cfg, EngineMode::Backtest);
    let scan = ScanEngine::new(settings.scan.clone(), symbols, None, HashMap::new(), Vec::new());
    let engine = Engine::new(settings, scan, None, from_ms);
    let mut d = SimDriver::new(engine, SimCore::new(sim_params(cfg)), cfg.paper.tick_ms);
    let start_equity = d.sim.total_equity().0;
    let mut replayers = Replayers::new();
    let mut events = Vec::new();
    let mut ledger: Vec<LedgerEntry> = Vec::new();
    let mut records = 0u64;
    let mut reports = 0usize;
    let mut alerts = String::new();
    for rec in ReplaySource::open(raw, &Exchange::ALL, from_ms, to_ms) {
        records += 1;
        events.clear();
        replayers.feed(&rec, &mut events);
        for ev in &events {
            d.on_md(ev);
        }
        let out = d.take_output();
        ledger.extend(out.ledger);
        for a in out.alerts {
            alerts.push_str(&format!("{:?} {} {}\n", a.level, a.key, a.text));
        }
        if let Some(rep) = &out.report {
            write_report(out_dir, rep, d.engine.scan().tiers())?;
            reports += 1;
        }
    }
    let out = d.tick(to_ms);
    ledger.extend(out.ledger);
    let end_equity = d.sim.total_equity().0;
    let mut by: std::collections::BTreeMap<String, Decimal> = std::collections::BTreeMap::new();
    for e in ledger.iter().filter(|e| !e.pair_id.starts_with("shadow:")) {
        *by.entry(format!("{:?}", e.kind)).or_default() += e.amount.0;
    }
    std::fs::create_dir_all(out_dir)?;
    write_atomic(&out_dir.join("ledger.csv"), &ledger_csv(&ledger))?;
    write_atomic(&out_dir.join("alerts.log"), &alerts)?;
    let status = serde_json::to_string_pretty(&d.engine.status_json())?;
    write_atomic(&out_dir.join("final-status.json"), &status)?;
    tracing::info!(records, reports, "backtest finished");
    Ok(BacktestSummary {
        records,
        orders: d.engine.tracker().iter().count(),
        pairs: d.engine.book().pairs.len(),
        start_equity,
        end_equity,
        by_kind: by.into_iter().collect(),
        unknown_orders: d.engine.tracker().unknown_count(),
    })
}
