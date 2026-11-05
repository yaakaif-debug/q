//! Стратегии (SPEC.md, «Strategy modules»). Стратегия не обращается к площадкам и не
//! читает часы: по оценкам пар (`sh-signals::evaluate`) и удерживаемым парам она выдаёт
//! намерения войти или выйти. Риск и исполнение решают, что из этого будет сделано.
//!
//! S1 (межбиржевой фандинг) и S2 (между дексами Hyperliquid) — одна логика с разными
//! правилами (`PairRules`) и фильтром вида пары.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_md::{InstrumentKey, MarketState};
use sh_signals::{PairEval, PairKind, PairRules};
use sh_venue_api::InstrumentStatus;

/// Удерживаемая пара (ориентация на момент входа).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldPair {
    /// Идентификатор пары.
    pub pair_id: String,
    /// Вид.
    pub kind: PairKind,
    /// Короткая нога.
    pub short: InstrumentKey,
    /// Длинная нога.
    pub long: InstrumentKey,
}

/// Намерение стратегии.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Войти в пару (кандидаты по убыванию `s_net`; размер уточняет движок под бюджет).
    Enter(Box<PairEval>),
    /// Выйти из пары.
    Exit {
        /// Пара.
        pair_id: String,
        /// Причина.
        reason: String,
    },
}

/// Стратегия.
pub trait Strategy: Send {
    /// Имя для журналов.
    fn name(&self) -> &'static str;

    /// Решения после очередной оценки пар.
    fn on_scan(&mut self, evals: &[PairEval], held: &[HeldPair], market: &MarketState, now_ms: i64) -> Vec<Intent>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ExitTracker {
    below: u32,
    last_check_ms: i64,
}

/// Стратегия переноса фандинга (S1 или S2).
#[derive(Debug, Clone)]
pub struct CarryStrategy {
    kind: PairKind,
    rules: PairRules,
    enabled: bool,
    exits: BTreeMap<String, ExitTracker>,
}

fn net_of(e: &PairEval) -> Decimal {
    e.at_size.or_else(|| e.tiers.first().copied()).and_then(|t| t.s_net).unwrap_or(Decimal::MIN)
}

impl CarryStrategy {
    /// S1 или S2 по виду пары.
    pub fn new(kind: PairKind, rules: PairRules, enabled: bool) -> Self {
        Self { kind, rules, enabled, exits: BTreeMap::new() }
    }

    /// Причина выхода по удерживаемой паре или `None`.
    fn exit_reason(&mut self, held: &HeldPair, eval: Option<&PairEval>, market: &MarketState) -> Option<String> {
        for k in [&held.short, &held.long] {
            let status = market.get(k).and_then(|s| s.meta.as_ref()).map(|m| m.status);
            if status != Some(InstrumentStatus::Active) {
                return Some(format!("leg {} inactive or delisted", k.id()));
            }
        }
        let eval = eval?;
        // EMA спреда в ориентации позиции; оценка могла перевернуть ноги
        let ema = eval.est.ema_24h.map(|v| if eval.short == held.short { v } else { -v });
        let last_settle = [&held.short, &held.long]
            .iter()
            .filter_map(|k| market.get(k).and_then(|s| s.funding.last_settled()).map(|s| s.ts_ms))
            .max()
            .unwrap_or(0);
        let t = self.exits.entry(held.pair_id.clone()).or_default();
        if last_settle > t.last_check_ms {
            t.last_check_ms = last_settle;
            if ema.is_none_or(|v| v < self.rules.exit) {
                t.below += 1;
            } else {
                t.below = 0;
            }
            if t.below >= self.rules.m_intervals {
                return Some(format!("ema below exit for {} intervals", t.below));
            }
        }
        None
    }
}

impl Strategy for CarryStrategy {
    fn name(&self) -> &'static str {
        match self.kind {
            PairKind::S1 => "s1_funding_carry",
            PairKind::S2 => "s2_cross_dex",
        }
    }

    fn on_scan(&mut self, evals: &[PairEval], held: &[HeldPair], market: &MarketState, _now_ms: i64) -> Vec<Intent> {
        let mut out = Vec::new();
        let by_id: BTreeMap<String, &PairEval> = evals.iter().map(|e| (e.pair.id(), e)).collect();
        let mine: Vec<&HeldPair> = held.iter().filter(|h| h.kind == self.kind).collect();
        for h in &mine {
            if let Some(reason) = self.exit_reason(h, by_id.get(&h.pair_id).copied(), market) {
                out.push(Intent::Exit { pair_id: h.pair_id.clone(), reason });
            }
        }
        self.exits.retain(|id, _| mine.iter().any(|h| &h.pair_id == id));
        if !self.enabled {
            return out;
        }
        let busy = |e: &PairEval| {
            held.iter().any(|h| {
                h.pair_id == e.pair.id() || [&h.short, &h.long].iter().any(|k| **k == e.pair.a || **k == e.pair.b)
            })
        };
        let mut cands: Vec<&PairEval> =
            evals.iter().filter(|e| e.pair.kind == self.kind && e.entry_ok && !busy(e)).collect();
        cands.sort_by_key(|e| std::cmp::Reverse(net_of(e)));
        out.extend(cands.into_iter().map(|e| Intent::Enter(Box::new(e.clone()))));
        out
    }
}
