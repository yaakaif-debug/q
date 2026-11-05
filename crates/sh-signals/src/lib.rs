//! Сигналы и экономика спреда: нормализация фандинга к APR, оценки спреда
//! (прогноз, EMA за 24 ч, реализованное среднее за 7 д), модель издержек,
//! `s_net`, ROC, точка безубыточности, размер позиции, сопоставление символов,
//! календари сессий и оценка пар. Чистые функции без I/O; деньги — только `Decimal`.

pub mod apr;
pub mod cost;
pub mod economics;
pub mod estimators;
pub mod eval;
pub mod fees;
pub mod params;
pub mod sessions;
pub mod sizing;
pub mod symbols;
pub mod testkit;

pub use apr::{HOURS_PER_YEAR, funding_apr};
pub use cost::{CostBreakdown, CostParams, LegCostInput, round_trip_cost};
pub use economics::{breakeven_hours, capital_for, required_net_spread, roc, s_net};
pub use estimators::{SpreadEstimates, ema_apr, realized_apr, sign_persistent, spread_series};
pub use eval::{EstimatesView, EvalContext, Pair, PairEval, PairKind, Reason, SizeBudget, TierEval, evaluate, pairs_from_symbols};
pub use fees::{FeeRates, FeesConfig, VenueFees};
pub use params::{PairRules, RiskLimits, S1Params, S2Params, ScannerParams, StrategyParams, pct};
pub use sessions::{Calendar, CalendarError};
pub use sizing::{SizeConstraint, SizeDecision, SizingInputs, common_lot, size_position};
pub use symbols::{SymbolMap, SymbolsError, Underlying, suggest_mappings, suggestions_toml};
