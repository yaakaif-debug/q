//! Книга позиций (M2): пары и их жизненный цикл (SPEC.md, «Position lifecycle»),
//! позиции по инструментам из заполнений, балансы площадок, сверка с площадками и
//! атрибуция результата (фандинг, комиссии, проскальзывание от mid решения, ценовой
//! результат). Позиции считаются только по заполнениям; снимки площадок служат сверке.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Exchange, Px, Side, Usd, VenueKey, apply_fill};
use sh_md::{InstrumentKey, MarketState};
use sh_signals::PairKind;
use sh_venue_api::{Fill, FundingPayment, VenueBalance, VenuePosition};

use crate::shadow::{LedgerEntry, LedgerKind};

/// Состояние пары.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Вход идёт.
    Entering,
    /// Обе ноги в пределах `delta_tolerance`.
    Hedged,
    /// Перебалансировка маржи.
    Rebalancing,
    /// Выход идёт.
    Exiting,
    /// Хедж не удался или площадка деградировала.
    Unwinding,
    /// Остановлено сверкой или kill switch.
    Halted,
    /// Обе ноги закрыты и сверены.
    Closed,
}

/// Нога пары.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegPos {
    /// Инструмент.
    pub key: InstrumentKey,
    /// Сторона входа (короткая нога — продажа).
    pub entry_side: Side,
    /// Количество со знаком.
    pub qty: Decimal,
    /// Плечо.
    pub leverage: Decimal,
    /// Mid решения (для проскальзывания).
    pub decision_mid: Option<Px>,
}

/// Пара в книге.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairPosition {
    /// Идентификатор.
    pub pair_id: String,
    /// Вид.
    pub kind: PairKind,
    /// Состояние.
    pub state: Lifecycle,
    /// Ноги: короткая, длинная.
    pub legs: [LegPos; 2],
    /// Целевое количество на ногу.
    pub target_qty: Decimal,
    /// Капитал пары.
    pub capital: Usd,
    /// Время входа, мс.
    pub opened_ms: i64,
    /// Время закрытия, мс.
    pub closed_ms: Option<i64>,
    /// Фандинг.
    pub funding: Usd,
    /// Комиссии (плюс — уплачено).
    pub fees: Usd,
    /// Проскальзывание от mid решения (плюс — потеряно).
    pub slippage: Usd,
    /// Ценовой результат (по mid решения).
    pub price_pnl: Usd,
    /// Причина выхода.
    pub exit_reason: Option<String>,
}

impl PairPosition {
    /// Ноги ровные в пределах допуска `tol` (доля целевого количества).
    pub fn is_balanced(&self, tol: Decimal) -> bool {
        let d = (self.legs[0].qty + self.legs[1].qty).abs();
        d <= self.target_qty.abs() * tol
    }

    /// Обе ноги закрыты.
    pub fn is_flat(&self) -> bool {
        self.legs.iter().all(|l| l.qty.is_zero())
    }

    /// Итог пары.
    pub fn net(&self) -> Decimal {
        self.funding.0 - self.fees.0 - self.slippage.0 + self.price_pnl.0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct InstrPos {
    qty: Decimal,
    /// Цена входа по mid решения (атрибуция).
    entry_mid: Decimal,
}

/// Расхождение сверки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    /// Инструмент.
    pub key: String,
    /// В книге.
    pub book: Decimal,
    /// На площадке.
    pub venue: Decimal,
}

/// Книга.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Book {
    /// Пары (открытые и недавно закрытые).
    pub pairs: BTreeMap<String, PairPosition>,
    positions: BTreeMap<String, InstrPos>,
    /// Балансы по площадкам.
    pub balances: BTreeMap<String, VenueBalance>,
    /// Пик капитала.
    pub peak_equity: Usd,
    /// Капитал на начало суток.
    pub day_start_equity: Usd,
    /// Начало текущих суток UTC, мс.
    pub day_start_ms: i64,
    /// Подряд найденные расхождения по инструменту (сверка требует двух подряд).
    pending_mismatch: BTreeMap<String, u32>,
}

const DAY_MS: i64 = 86_400_000;

fn ledger(ts_ms: i64, pair_id: &str, leg: Option<String>, kind: LedgerKind, amount: Decimal) -> LedgerEntry {
    LedgerEntry { ts_ms, pair_id: pair_id.to_owned(), leg, kind, amount: Usd(amount) }
}

impl Book {
    /// Пустая книга.
    pub fn new() -> Self {
        Self::default()
    }

    /// Позиция по инструменту.
    pub fn position(&self, key: &InstrumentKey) -> Decimal {
        self.positions.get(&key.id()).map(|p| p.qty).unwrap_or_default()
    }

    /// Все ненулевые позиции.
    pub fn positions(&self) -> Vec<(InstrumentKey, Decimal)> {
        self.positions
            .iter()
            .filter(|(_, p)| !p.qty.is_zero())
            .filter_map(|(k, p)| InstrumentKey::parse(k).map(|k| (k, p.qty)))
            .collect()
    }

    /// Пара, в которой участвует инструмент (кроме закрытых).
    pub fn pair_of(&self, key: &InstrumentKey) -> Option<&PairPosition> {
        self.pairs.values().find(|p| p.state != Lifecycle::Closed && p.legs.iter().any(|l| l.key == *key))
    }

    /// Открывает пару в состоянии `Entering`.
    #[allow(clippy::too_many_arguments)]
    pub fn open_pair(
        &mut self,
        pair_id: &str,
        kind: PairKind,
        short: (InstrumentKey, Decimal, Option<Px>),
        long: (InstrumentKey, Decimal, Option<Px>),
        target_qty: Decimal,
        capital: Usd,
        now_ms: i64,
    ) {
        let leg = |(key, leverage, mid): (InstrumentKey, Decimal, Option<Px>), side| LegPos {
            key,
            entry_side: side,
            qty: Decimal::ZERO,
            leverage,
            decision_mid: mid,
        };
        self.pairs.insert(
            pair_id.to_owned(),
            PairPosition {
                pair_id: pair_id.to_owned(),
                kind,
                state: Lifecycle::Entering,
                legs: [leg(short, Side::Sell), leg(long, Side::Buy)],
                target_qty,
                capital,
                opened_ms: now_ms,
                closed_ms: None,
                funding: Usd(Decimal::ZERO),
                fees: Usd(Decimal::ZERO),
                slippage: Usd(Decimal::ZERO),
                price_pnl: Usd(Decimal::ZERO),
                exit_reason: None,
            },
        );
    }

    /// Меняет состояние пары.
    pub fn set_state(&mut self, pair_id: &str, state: Lifecycle, now_ms: i64) {
        if let Some(p) = self.pairs.get_mut(pair_id) {
            p.state = state;
            if state == Lifecycle::Closed {
                p.closed_ms = Some(now_ms);
            }
        }
    }

    /// Обновляет mid решения ноги (перед выходом).
    pub fn set_decision_mid(&mut self, pair_id: &str, market: &MarketState) {
        if let Some(p) = self.pairs.get_mut(pair_id) {
            for l in &mut p.legs {
                l.decision_mid = market.get(&l.key).and_then(|s| s.mid());
            }
        }
    }

    /// Заполнение: позиция инструмента, нога пары, журнал комиссии, проскальзывания и
    /// ценового результата (по mid решения).
    pub fn on_fill(&mut self, f: &Fill, pair_id: Option<&str>, fallback_mid: Option<Px>) -> Vec<LedgerEntry> {
        let key = InstrumentKey::new(f.venue.clone(), f.symbol.clone());
        let id = key.id();
        let dq = match f.side {
            Side::Buy => f.qty.0,
            Side::Sell => -f.qty.0,
        };
        let pair_id = pair_id.map(str::to_owned).or_else(|| self.pair_of(&key).map(|p| p.pair_id.clone()));
        let mid = pair_id
            .as_deref()
            .and_then(|pid| self.pairs.get(pid))
            .and_then(|p| p.legs.iter().find(|l| l.key == key))
            .and_then(|l| l.decision_mid)
            .or(fallback_mid)
            .unwrap_or(f.px);
        let pos = self.positions.entry(id.clone()).or_default();
        let ch = apply_fill(pos.qty, pos.entry_mid, dq, mid.0);
        pos.qty = ch.qty;
        pos.entry_mid = ch.entry_px;
        // проскальзывание: покупка выше mid — потеря
        let slip = (f.px.0 - mid.0) * dq;
        let tag = pair_id.clone().unwrap_or_else(|| "unpaired".into());
        let mut out = vec![ledger(f.ts_ms, &tag, Some(id.clone()), LedgerKind::Fee, -f.fee.0)];
        if !slip.is_zero() {
            out.push(ledger(f.ts_ms, &tag, Some(id.clone()), LedgerKind::Slippage, -slip));
        }
        if !ch.realized.is_zero() {
            out.push(ledger(f.ts_ms, &tag, Some(id.clone()), LedgerKind::Basis, ch.realized));
        }
        if let Some(p) = pair_id.as_deref().and_then(|pid| self.pairs.get_mut(pid))
            && let Some(l) = p.legs.iter_mut().find(|l| l.key == key)
        {
            l.qty += dq;
            p.fees.0 += f.fee.0;
            p.slippage.0 += slip;
            p.price_pnl.0 += ch.realized;
        }
        out
    }

    /// Платёж фандинга относится к паре, держащей инструмент.
    pub fn on_funding(&mut self, fp: &FundingPayment) -> Vec<LedgerEntry> {
        let key = InstrumentKey::new(fp.venue.clone(), fp.symbol.clone());
        let pid = self.pair_of(&key).map(|p| p.pair_id.clone());
        if let Some(p) = pid.as_deref().and_then(|id| self.pairs.get_mut(id)) {
            p.funding.0 += fp.amount.0;
        }
        vec![ledger(fp.ts_ms, pid.as_deref().unwrap_or("unpaired"), Some(key.id()), LedgerKind::Funding, fp.amount.0)]
    }

    /// Балансы площадок; обновляет пик и начало суток.
    pub fn on_balances(&mut self, bals: &[VenueBalance], now_ms: i64) {
        for b in bals {
            self.balances.insert(b.venue.to_string(), b.clone());
        }
        let eq = self.equity();
        if eq.0 > self.peak_equity.0 {
            self.peak_equity = eq;
        }
        let day = now_ms - now_ms.rem_euclid(DAY_MS);
        if day > self.day_start_ms || self.day_start_equity.0.is_zero() {
            self.day_start_ms = day;
            self.day_start_equity = eq;
        }
    }

    /// Капитал всех площадок.
    pub fn equity(&self) -> Usd {
        Usd(self.balances.values().map(|b| b.equity.0).sum())
    }

    /// Сверка позиций семейства площадок со снимком; расхождение засчитывается, если оно
    /// держится две проверки подряд (снимок и заполнения приходят разными путями).
    /// Инструменты из `skip` (с живыми ордерами) не проверяются.
    pub fn reconcile(
        &mut self,
        exchange: Exchange,
        venue: &[VenuePosition],
        skip: &[String],
        tol: Decimal,
    ) -> Vec<Mismatch> {
        let mut theirs: BTreeMap<String, Decimal> = BTreeMap::new();
        for p in venue.iter().filter(|p| p.venue.exchange == exchange) {
            *theirs.entry(InstrumentKey::new(p.venue.clone(), p.symbol.clone()).id()).or_default() += p.qty;
        }
        let mut keys: Vec<String> = theirs.keys().cloned().collect();
        for (k, p) in &self.positions {
            if !p.qty.is_zero()
                && InstrumentKey::parse(k).is_some_and(|ik| ik.venue.exchange == exchange)
                && !keys.contains(k)
            {
                keys.push(k.clone());
            }
        }
        let mut out = Vec::new();
        for k in keys {
            if skip.contains(&k) {
                continue;
            }
            let ours = self.positions.get(&k).map(|p| p.qty).unwrap_or_default();
            let v = theirs.get(&k).copied().unwrap_or_default();
            if (ours - v).abs() > tol {
                let n = self.pending_mismatch.entry(k.clone()).or_default();
                *n += 1;
                if *n >= 2 {
                    out.push(Mismatch { key: k, book: ours, venue: v });
                }
            } else {
                self.pending_mismatch.remove(&k);
            }
        }
        out
    }

    /// Принимает позиции площадки как истину (оператор после разбора расхождения).
    pub fn adopt_venue_positions(&mut self, exchange: Exchange, venue: &[VenuePosition]) {
        self.positions.retain(|k, _| InstrumentKey::parse(k).is_none_or(|ik| ik.venue.exchange != exchange));
        for p in venue.iter().filter(|p| p.venue.exchange == exchange) {
            let k = InstrumentKey::new(p.venue.clone(), p.symbol.clone()).id();
            self.positions.insert(k, InstrPos { qty: p.qty, entry_mid: p.entry_px.map(|x| x.0).unwrap_or_default() });
        }
        self.pending_mismatch.clear();
    }

    /// Капитал пар на платформе.
    pub fn platform_capital(&self) -> BTreeMap<Exchange, Usd> {
        let mut m: BTreeMap<Exchange, Usd> = BTreeMap::new();
        for p in self.pairs.values().filter(|p| p.state != Lifecycle::Closed) {
            for l in &p.legs {
                // капитал пары делится между ногами пропорционально марже (N/L)
                let inv: Decimal = p.legs.iter().map(|x| Decimal::ONE / x.leverage.max(Decimal::ONE)).sum();
                let share =
                    if inv.is_zero() { Decimal::ZERO } else { (Decimal::ONE / l.leverage.max(Decimal::ONE)) / inv };
                m.entry(l.key.venue.exchange).or_insert(Usd(Decimal::ZERO)).0 += p.capital.0 * share;
            }
        }
        m
    }

    /// Удаляет закрытые пары старше `keep_ms`.
    pub fn prune(&mut self, now_ms: i64, keep_ms: i64) {
        self.pairs.retain(|_, p| p.closed_ms.is_none_or(|t| now_ms - t < keep_ms));
    }

    /// Задаёт позицию инструмента (восстановление симулятора paper из книги и тесты).
    pub fn venue_keys(&self) -> Vec<VenueKey> {
        let mut v: Vec<VenueKey> =
            self.positions.keys().filter_map(|k| InstrumentKey::parse(k)).map(|k| k.venue).collect();
        v.sort();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::dec;
    use sh_core::Qty;

    fn key(s: &str) -> InstrumentKey {
        InstrumentKey::parse(s).unwrap()
    }

    fn fill(k: &str, side: Side, qty: Decimal, px: Decimal, fee: Decimal, n: u32) -> Fill {
        let k = key(k);
        Fill {
            cid: None,
            venue: k.venue,
            symbol: k.symbol,
            side,
            qty: Qty(qty),
            px: Px(px),
            fee: Usd(fee),
            maker: false,
            trade_id: format!("t{n}"),
            ts_ms: i64::from(n),
            liquidation: false,
        }
    }

    fn book_with_pair() -> Book {
        let mut b = Book::new();
        b.open_pair(
            "ETH:p",
            PairKind::S1,
            (key("aster/ETHUSDT"), dec!(3), Some(Px(dec!(100)))),
            (key("lighter/ETH"), dec!(2), Some(Px(dec!(100)))),
            dec!(10),
            Usd(dec!(1000)),
            0,
        );
        b
    }

    #[test]
    fn attribution_splits_slippage_and_price() {
        let mut b = book_with_pair();
        let e = b.on_fill(&fill("aster/ETHUSDT", Side::Sell, dec!(10), dec!(99.9), dec!(0.4), 1), Some("ETH:p"), None);
        assert!(e.iter().any(|x| x.kind == LedgerKind::Slippage && x.amount.0 == dec!(-1.0)));
        b.on_fill(&fill("lighter/ETH", Side::Buy, dec!(10), dec!(100), dec!(0), 2), Some("ETH:p"), None);
        assert!(b.pairs["ETH:p"].is_balanced(dec!(0.005)));
        // выход: mid решения 110
        if let Some(p) = b.pairs.get_mut("ETH:p") {
            p.legs[0].decision_mid = Some(Px(dec!(110)));
        }
        let e = b.on_fill(&fill("aster/ETHUSDT", Side::Buy, dec!(10), dec!(110.1), dec!(0), 3), Some("ETH:p"), None);
        // ценовой результат шорта по mid: (100 − 110) × 10 = −100; проскальзывание −1
        assert!(e.iter().any(|x| x.kind == LedgerKind::Basis && x.amount.0 == dec!(-100)));
        let p = &b.pairs["ETH:p"];
        assert_eq!(p.slippage.0, dec!(2.0));
        assert_eq!(p.price_pnl.0, dec!(-100));
    }

    #[test]
    fn reconcile_needs_two_consecutive_mismatches() {
        let mut b = book_with_pair();
        b.on_fill(&fill("lighter/ETH", Side::Buy, dec!(5), dec!(100), dec!(0), 1), Some("ETH:p"), None);
        let snap = vec![VenuePosition {
            venue: VenueKey::new(Exchange::Lighter),
            symbol: "ETH".into(),
            qty: dec!(4),
            entry_px: None,
            liq_px: None,
            margin: None,
        }];
        assert!(b.reconcile(Exchange::Lighter, &snap, &[], dec!(0.000001)).is_empty());
        let m = b.reconcile(Exchange::Lighter, &snap, &[], dec!(0.000001));
        assert_eq!(m, vec![Mismatch { key: "lighter/ETH".into(), book: dec!(5), venue: dec!(4) }]);
        // позиция на площадке, которой нет в книге
        assert!(b.reconcile(Exchange::Aster, &[], &[], dec!(0)).is_empty());
        b.adopt_venue_positions(Exchange::Lighter, &snap);
        assert!(b.reconcile(Exchange::Lighter, &snap, &[], dec!(0)).is_empty());
    }

    proptest! {
        /// Сумма записей журнала (кроме фандинга) равна денежному потоку сделок при
        /// закрытии в ноль: −Σ комиссий + Σ (продажи − покупки).
        #[test]
        fn ledger_matches_cash_flow(trades in proptest::collection::vec((any::<bool>(), 1u32..100, 90u32..110, 0u32..5), 1..40)) {
            let mut b = Book::new();
            let mut cash = Decimal::ZERO;
            let mut total = Decimal::ZERO;
            let mut n = 0u32;
            let mut mid = Px(dec!(100));
            for (buy, q, px, fee) in trades {
                n += 1;
                let side = if buy { Side::Buy } else { Side::Sell };
                let (q, px, fee) = (Decimal::from(q), Decimal::from(px), Decimal::from(fee) / dec!(10));
                cash += if buy { -q * px } else { q * px } - fee;
                for e in b.on_fill(&fill("lighter/ETH", side, q, px, fee, n), None, Some(mid)) {
                    total += e.amount.0;
                }
                mid = Px(px);
            }
            // закрываем остаток по mid
            let pos = b.position(&key("lighter/ETH"));
            if !pos.is_zero() {
                n += 1;
                let side = if pos > Decimal::ZERO { Side::Sell } else { Side::Buy };
                cash += pos * mid.0;
                for e in b.on_fill(&fill("lighter/ETH", side, pos.abs(), mid.0, Decimal::ZERO, n), None, Some(mid)) {
                    total += e.amount.0;
                }
            }
            // средняя цена входа — деление; остаток округления ниже 1e-12
            prop_assert!((total - cash).abs() < dec!(0.000000000001), "{} vs {}", total, cash);
        }
    }
}
