//! Портфель: в M1 — теневой портфель сканера (распределение $100k по правилам S1/S2
//! и лимитам риска, фандинг по фактическим расчётам, издержки по стаканам) и дневные
//! отчёты; позиции по площадкам, сверка и перебалансировка добавляются в M2–M4.

pub mod report;
pub mod shadow;

pub use report::{DailyReport, OpportunityRow, PositionRow, opportunities_csv, opportunity_rows};
pub use shadow::{LedgerEntry, LedgerKind, ShadowParams, ShadowPortfolio, ShadowPosition, StepOutcome};
