//! Задача пары S5 (SPEC.md, «Entry sequence (S1 with the S4 tactic)»).
//!
//! 1. Мейкер-нога выбирается заранее (меньшая мейкер-комиссия).
//! 2. Объём режется на дочерние ордера не больше `child_usd`.
//! 3. Дочерний post-only ставится по BBO хедж-площадки минус тейкер-комиссия хеджа,
//!    ожидаемое проскальзывание и `edge_bps`; переставляется, если цена ушла больше
//!    `reprice_bps`.
//! 4. На каждое заполнение — IOC на хедж-площадке ровно на заполненный объём с пределом
//!    BBO ± `max_slip_bps`.
//! 5. Хедж не завершён за `hedge_timeout_ms` — повтор с более широким пределом до
//!    `hedge_retries`; дальше непохеджированный объём закрывается на мейкер-площадке
//!    reduce-only IOC.
//! 6. Новые дочерние не отправляются, пока общий непохеджированный номинал выше
//!    `max_unhedged_usd` (флаг `allow_new_child` от риска).
//!
//! Выход — та же последовательность в обратную сторону, все ордера reduce-only.
//! «Сброс» (`kill --flatten`) — то же, но мейкер-нога тоже IOC, срезами `child_usd`,
//! так что дельта не растёт больше одного среза.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Px, Qty, Side, VenueKey};
use sh_md::{InstrumentKey, MarketState};
use sh_venue_api::{ClientId, OrderRequest, OrderStatus, PriceRule, Rounding, Tif};

/// Идентификатор задачи.
pub type JobId = u64;

/// Вид задачи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Вход в пару.
    Entry,
    /// Выход из пары (reduce-only).
    Exit,
    /// Аварийный сброс: обе ноги IOC reduce-only.
    Flatten,
}

/// Нога задачи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobLeg {
    /// Площадка.
    pub venue: VenueKey,
    /// Символ.
    pub symbol: String,
    /// Сторона сделок задачи на этой ноге.
    pub side: Side,
    /// Шаг количества.
    pub lot: Qty,
    /// Минимальное количество.
    pub min_qty: Qty,
    /// Правило цены.
    pub price_rule: PriceRule,
    /// Тейкер-комиссия (доля).
    pub taker_fee: Decimal,
}

impl JobLeg {
    fn key(&self) -> InstrumentKey {
        InstrumentKey::new(self.venue.clone(), self.symbol.clone())
    }
}

/// Параметры исполнения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobParams {
    /// Максимальный номинал дочернего ордера, $.
    pub child_usd: Decimal,
    /// Тайм-аут хеджа, мс.
    pub hedge_timeout_ms: i64,
    /// Повторы хеджа.
    pub hedge_retries: u32,
    /// Запас цены мейкера, bp.
    pub edge_bps: Decimal,
    /// Порог перестановки мейкера, bp.
    pub reprice_bps: Decimal,
    /// Предел проскальзывания хеджа, bp.
    pub max_slip_bps: Decimal,
    /// Ожидаемое проскальзывание хеджа, bp.
    pub expected_slip_bps: Decimal,
    /// Максимальная длительность входа или выхода, мс; дальше новые дочерние не ставятся.
    pub max_duration_ms: i64,
}

impl Default for JobParams {
    fn default() -> Self {
        Self {
            child_usd: Decimal::from(5000),
            hedge_timeout_ms: 1500,
            hedge_retries: 2,
            edge_bps: Decimal::ONE,
            reprice_bps: Decimal::from(2),
            max_slip_bps: Decimal::from(10),
            expected_slip_bps: Decimal::ONE,
            max_duration_ms: 30 * 60_000,
        }
    }
}

/// Состояние задачи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    /// Работает.
    Working,
    /// Хедж не удался: непохеджированный объём закрывается.
    Unwinding,
    /// Завершена.
    Done,
}

/// Действие задачи для движка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobAction {
    /// Отправить ордер.
    Place(OrderRequest),
    /// Отменить ордер.
    Cancel(ClientId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Role {
    Maker,
    Hedge,
    Unwind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LiveOrder {
    cid: ClientId,
    px: Px,
    qty: Decimal,
    filled: Decimal,
    sent_ms: i64,
    cancel_sent: bool,
}

/// Задача пары.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairJob {
    /// Идентификатор.
    pub id: JobId,
    /// Пара.
    pub pair_id: String,
    /// Вид.
    pub kind: JobKind,
    /// Ноги.
    pub legs: [JobLeg; 2],
    /// Индекс мейкер-ноги.
    pub maker: usize,
    /// Целевое количество на каждую ногу.
    pub target: Decimal,
    /// Параметры.
    pub params: JobParams,
    /// Состояние.
    pub state: JobState,
    /// Исполнено по ногам (в направлении задачи).
    pub filled: [Decimal; 2],
    /// Время старта, мс.
    pub started_ms: i64,
    /// Новые дочерние больше не ставятся.
    pub stopped: bool,
    /// Номер попытки текущего хеджа.
    pub hedge_attempt: u32,
    /// Остаток, который не удалось округлить до лота (журнал).
    pub residual: Decimal,
    /// Прервана: только отмена живых ордеров, новых нет.
    #[serde(default)]
    pub aborted: bool,
    roles: BTreeMap<ClientId, Role>,
    maker_order: Option<LiveOrder>,
    hedge_order: Option<LiveOrder>,
    unwind_order: Option<LiveOrder>,
}

/// Общее кратное двух лотов (десятичных).
pub fn common_lot(a: Qty, b: Qty) -> Qty {
    let (x, y) = (a.0.normalize(), b.0.normalize());
    if x <= Decimal::ZERO {
        return Qty(y);
    }
    if y <= Decimal::ZERO {
        return Qty(x);
    }
    let scale = x.scale().max(y.scale());
    let m = Decimal::from(10u64.pow(scale.min(18)));
    let (xi, yi) = ((x * m).trunc(), (y * m).trunc());
    let (mut p, mut q) = (xi, yi);
    while !q.is_zero() {
        let r = p % q;
        p = q;
        q = r;
    }
    if p.is_zero() {
        return Qty(x.max(y));
    }
    Qty((xi / p * yi / m).normalize())
}

fn floor_to(v: Decimal, lot: Qty) -> Decimal {
    if lot.0 <= Decimal::ZERO {
        return v;
    }
    ((v / lot.0).floor() * lot.0).normalize()
}

fn bps(v: Decimal) -> Decimal {
    v / Decimal::from(10_000)
}

impl PairJob {
    /// Новая задача.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: JobId,
        pair_id: impl Into<String>,
        kind: JobKind,
        legs: [JobLeg; 2],
        maker: usize,
        target: Decimal,
        params: JobParams,
        now_ms: i64,
    ) -> Self {
        Self {
            id,
            pair_id: pair_id.into(),
            kind,
            legs,
            maker: maker.min(1),
            target,
            params,
            state: JobState::Working,
            filled: [Decimal::ZERO; 2],
            started_ms: now_ms,
            stopped: false,
            hedge_attempt: 0,
            residual: Decimal::ZERO,
            aborted: false,
            roles: BTreeMap::new(),
            maker_order: None,
            hedge_order: None,
            unwind_order: None,
        }
    }

    fn hedge(&self) -> usize {
        1 - self.maker
    }

    /// Непохеджированный объём (мейкер исполнено минус хедж исполнено).
    pub fn unhedged(&self) -> Decimal {
        self.filled[self.maker] - self.filled[self.hedge()]
    }

    /// Задача завершена.
    pub fn is_done(&self) -> bool {
        self.state == JobState::Done
    }

    /// Живые ордера задачи.
    pub fn live_orders(&self) -> Vec<ClientId> {
        [&self.maker_order, &self.hedge_order, &self.unwind_order]
            .iter()
            .filter_map(|o| o.as_ref().map(|o| o.cid))
            .collect()
    }

    /// Останавливает постановку новых дочерних (выход по сигналу риска, тайм-аут).
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// Прерывает задачу: живые ордера отменяются, новые не отправляются
    /// (замена другой задачей, остановка Halted).
    pub fn abort(&mut self) {
        self.aborted = true;
        self.stopped = true;
    }

    /// Сворачивает задачу безопасно: вход разворачивает непохеджированный объём на
    /// мейкер-ноге reduce-only, выход прерывается (kill switch без сброса).
    pub fn wind_down(&mut self) {
        self.stopped = true;
        if self.kind == JobKind::Entry && !self.unhedged().is_zero() {
            self.state = JobState::Unwinding;
        } else if self.kind != JobKind::Entry {
            self.aborted = true;
        }
    }

    /// Ордер отклонён риском до отправки.
    pub fn on_rejected(&mut self, cid: ClientId) {
        self.on_closed(cid, OrderStatus::Rejected);
    }

    /// Заполнение ордера задачи.
    pub fn on_fill(&mut self, cid: ClientId, qty: Decimal) {
        let Some(role) = self.roles.get(&cid).copied() else { return };
        match role {
            Role::Maker => {
                self.filled[self.maker] += qty;
                if let Some(o) = self.maker_order.as_mut().filter(|o| o.cid == cid) {
                    o.filled += qty;
                }
            }
            Role::Hedge => {
                let h = self.hedge();
                self.filled[h] += qty;
                if let Some(o) = self.hedge_order.as_mut().filter(|o| o.cid == cid) {
                    o.filled += qty;
                }
            }
            Role::Unwind => {
                // разворот уменьшает объём мейкер-ноги в направлении задачи
                self.filled[self.maker] -= qty;
                if let Some(o) = self.unwind_order.as_mut().filter(|o| o.cid == cid) {
                    o.filled += qty;
                }
            }
        }
    }

    /// Ордер задачи закрыт.
    pub fn on_closed(&mut self, cid: ClientId, _status: OrderStatus) {
        match self.roles.get(&cid).copied() {
            Some(Role::Maker) if self.maker_order.as_ref().is_some_and(|o| o.cid == cid) => self.maker_order = None,
            Some(Role::Hedge) if self.hedge_order.as_ref().is_some_and(|o| o.cid == cid) => {
                if let Some(o) = self.hedge_order.take() {
                    // хедж исполнен не полностью — следующий с более широким пределом
                    if o.filled < o.qty {
                        self.hedge_attempt += 1;
                    } else {
                        self.hedge_attempt = 0;
                    }
                }
            }
            Some(Role::Unwind) if self.unwind_order.as_ref().is_some_and(|o| o.cid == cid) => self.unwind_order = None,
            _ => {}
        }
    }

    fn reduce_only(&self) -> bool {
        self.kind != JobKind::Entry
    }

    /// Цена мейкер-ноги от BBO хедж-площадки.
    fn maker_price(&self, market: &MarketState) -> Option<Px> {
        let m = &self.legs[self.maker];
        let h = &self.legs[self.hedge()];
        let (hb, ha) = market.get(&h.key())?.best();
        let (mb, ma) = market.get(&m.key())?.best();
        let adj = h.taker_fee + bps(self.params.expected_slip_bps + self.params.edge_bps);
        let (raw, mode) = match m.side {
            // мейкер покупает → хедж продаёт по биду хеджа
            Side::Buy => (hb?.px.0 * (Decimal::ONE - adj), Rounding::Down),
            Side::Sell => (ha?.px.0 * (Decimal::ONE + adj), Rounding::Up),
        };
        let mut px = m.price_rule.round(Px(raw), mode)?;
        // post-only не должен пересекать стакан мейкер-площадки: тогда встаём в лучший уровень своей стороны
        match m.side {
            Side::Buy => {
                if let Some(a) = ma
                    && px.0 >= a.px.0
                {
                    px = mb?.px;
                }
            }
            Side::Sell => {
                if let Some(b) = mb
                    && px.0 <= b.px.0
                {
                    px = ma?.px;
                }
            }
        }
        Some(px)
    }

    /// Агрессивная цена IOC на ноге с пределом проскальзывания `slip_bps`.
    fn taker_price(&self, leg: usize, market: &MarketState, slip_bps: Decimal) -> Option<Px> {
        let l = &self.legs[leg];
        let (b, a) = market.get(&l.key())?.best();
        let (raw, mode) = match l.side {
            Side::Buy => (a?.px.0 * (Decimal::ONE + bps(slip_bps)), Rounding::Down),
            Side::Sell => (b?.px.0 * (Decimal::ONE - bps(slip_bps)), Rounding::Up),
        };
        l.price_rule.round(Px(raw), mode)
    }

    #[allow(clippy::too_many_arguments)]
    fn order(
        &mut self,
        leg: usize,
        side: Side,
        qty: Decimal,
        px: Px,
        tif: Tif,
        reduce_only: bool,
        cid: ClientId,
        role: Role,
        now: i64,
    ) -> JobAction {
        let l = &self.legs[leg];
        self.roles.insert(cid, role);
        let live = LiveOrder { cid, px, qty, filled: Decimal::ZERO, sent_ms: now, cancel_sent: false };
        match role {
            Role::Maker => self.maker_order = Some(live),
            Role::Hedge => self.hedge_order = Some(live),
            Role::Unwind => self.unwind_order = Some(live),
        }
        JobAction::Place(OrderRequest {
            cid,
            venue: l.venue.clone(),
            symbol: l.symbol.clone(),
            side,
            qty: Qty(qty),
            px,
            tif,
            reduce_only,
        })
    }

    /// Шаг задачи: хедж, разворот, постановка и перестановка мейкера, завершение.
    pub fn step(
        &mut self,
        now_ms: i64,
        market: &MarketState,
        allow_new_child: bool,
        next_cid: &mut dyn FnMut() -> ClientId,
    ) -> Vec<JobAction> {
        let mut out = Vec::new();
        if self.state == JobState::Done {
            return out;
        }
        if self.aborted {
            for o in [&mut self.maker_order, &mut self.hedge_order, &mut self.unwind_order].into_iter().flatten() {
                if !o.cancel_sent {
                    o.cancel_sent = true;
                    out.push(JobAction::Cancel(o.cid));
                }
            }
            if self.maker_order.is_none() && self.hedge_order.is_none() && self.unwind_order.is_none() {
                self.residual = self.unhedged();
                self.state = JobState::Done;
            }
            return out;
        }
        if now_ms - self.started_ms > self.params.max_duration_ms {
            self.stopped = true;
        }
        let h = self.hedge();
        let mk = self.maker;
        let unhedged = self.unhedged();

        // хедж: тайм-аут живого хеджа — отмена (IOC обычно закрывается сам)
        if let Some(o) = self.hedge_order.as_mut()
            && !o.cancel_sent
            && now_ms - o.sent_ms > self.params.hedge_timeout_ms
        {
            o.cancel_sent = true;
            out.push(JobAction::Cancel(o.cid));
        }
        if self.hedge_attempt > self.params.hedge_retries && self.kind == JobKind::Entry {
            self.state = JobState::Unwinding;
            self.stopped = true;
        }
        let hedge_qty = floor_to(unhedged, self.legs[h].lot);
        if self.state == JobState::Working || self.kind != JobKind::Entry {
            if self.hedge_attempt > self.params.hedge_retries {
                // выход: хедж повторяется с максимальным пределом, состояние — разворот (тревога)
                self.state = JobState::Unwinding;
            }
            if self.hedge_order.is_none() && hedge_qty > Decimal::ZERO && hedge_qty >= self.legs[h].min_qty.0 {
                let attempt = Decimal::from(self.hedge_attempt.min(self.params.hedge_retries) + 1);
                if let Some(px) = self.taker_price(h, market, self.params.max_slip_bps * attempt) {
                    let side = self.legs[h].side;
                    let ro = self.reduce_only();
                    let cid = next_cid();
                    out.push(self.order(h, side, hedge_qty, px, Tif::Ioc, ro, cid, Role::Hedge, now_ms));
                }
            }
        }

        // разворот входа: непохеджированный объём закрывается на мейкер-ноге reduce-only
        if self.state == JobState::Unwinding && self.kind == JobKind::Entry {
            if let Some(o) = &self.maker_order
                && !o.cancel_sent
            {
                let cid = o.cid;
                if let Some(o) = self.maker_order.as_mut() {
                    o.cancel_sent = true;
                }
                out.push(JobAction::Cancel(cid));
            }
            let q = floor_to(unhedged, self.legs[mk].lot);
            if self.unwind_order.is_none() && self.hedge_order.is_none() && q > Decimal::ZERO {
                let side = match self.legs[mk].side {
                    Side::Buy => Side::Sell,
                    Side::Sell => Side::Buy,
                };
                let slip = self.params.max_slip_bps * Decimal::from(self.params.hedge_retries + 1);
                let l = &self.legs[mk];
                let (b, a) = market.get(&l.key()).map(|s| s.best()).unwrap_or((None, None));
                let raw = match side {
                    Side::Buy => a.map(|a| (a.px.0 * (Decimal::ONE + bps(slip)), Rounding::Down)),
                    Side::Sell => b.map(|b| (b.px.0 * (Decimal::ONE - bps(slip)), Rounding::Up)),
                };
                if let Some(px) = raw.and_then(|(r, m)| l.price_rule.round(Px(r), m)) {
                    let cid = next_cid();
                    out.push(self.order(mk, side, q, px, Tif::Ioc, true, cid, Role::Unwind, now_ms));
                }
            }
            if q.is_zero() && self.unwind_order.is_none() && self.maker_order.is_none() && self.hedge_order.is_none() {
                self.residual = unhedged;
                self.state = JobState::Done;
            }
            return out;
        }

        // мейкер-нога
        let lot = common_lot(self.legs[0].lot, self.legs[1].lot);
        let backlog = hedge_qty > Decimal::ZERO || self.hedge_order.is_some();
        if let Some(o) = &self.maker_order {
            let mut cancel = !o.cancel_sent && (self.stopped || !allow_new_child);
            if !cancel
                && !o.cancel_sent
                && self.kind != JobKind::Flatten
                && let Some(px) = self.maker_price(market)
                && o.px.0 > Decimal::ZERO
                && ((px.0 - o.px.0).abs() / o.px.0) > bps(self.params.reprice_bps)
            {
                cancel = true;
            }
            if cancel {
                let cid = o.cid;
                if let Some(o) = self.maker_order.as_mut() {
                    o.cancel_sent = true;
                }
                out.push(JobAction::Cancel(cid));
            }
        } else if !self.stopped && allow_new_child && !backlog {
            let left = floor_to(self.target - self.filled[mk], lot);
            let ref_px = market.get(&self.legs[mk].key()).and_then(|s| s.mid());
            if left > Decimal::ZERO
                && let Some(mid) = ref_px
                && mid.0 > Decimal::ZERO
            {
                let child = floor_to((self.params.child_usd / mid.0).min(left), lot);
                let child = if child.is_zero() { left.min(lot.0) } else { child };
                let side = self.legs[mk].side;
                let ro = self.reduce_only();
                let px = if self.kind == JobKind::Flatten {
                    self.taker_price(mk, market, self.params.max_slip_bps)
                } else {
                    self.maker_price(market)
                };
                if let Some(px) = px
                    && child >= self.legs[mk].min_qty.0
                {
                    let tif = if self.kind == JobKind::Flatten { Tif::Ioc } else { Tif::PostOnly };
                    let cid = next_cid();
                    out.push(self.order(mk, side, child, px, tif, ro, cid, Role::Maker, now_ms));
                } else if child < self.legs[mk].min_qty.0 {
                    // остаток меньше минимального ордера: не форсируем, только журнал
                    self.residual = left;
                    self.stopped = true;
                }
            }
        }

        // завершение: целевой объём набран или остановлено, все ордера закрыты, хедж догнал
        let target_met = self.filled[mk] >= self.target - lot.0 / Decimal::from(2);
        if (target_met || self.stopped)
            && self.maker_order.is_none()
            && self.hedge_order.is_none()
            && self.unwind_order.is_none()
            && hedge_qty.is_zero()
        {
            if !unhedged.is_zero() {
                self.residual = unhedged;
            }
            self.state = JobState::Done;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn common_lot_values() {
        assert_eq!(common_lot(Qty(dec!(0.01)), Qty(dec!(0.001))), Qty(dec!(0.01)));
        assert_eq!(common_lot(Qty(dec!(0.02)), Qty(dec!(0.03))), Qty(dec!(0.06)));
        assert_eq!(common_lot(Qty(dec!(1)), Qty(dec!(0.5))), Qty(dec!(1)));
        assert_eq!(common_lot(Qty(dec!(0.25)), Qty(dec!(0.1))), Qty(dec!(0.5)));
    }
}
