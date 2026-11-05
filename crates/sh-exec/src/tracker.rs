//! Автомат состояний ордера. Переходы применяются только по ответам и событиям
//! площадки; тайм-аут переводит ордер в `Unknown`, который разрешается запросом
//! по клиентскому идентификатору. Ордер никогда не переотправляется вслепую.

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Px, Qty, Usd};
use sh_venue_api::{
    ClientId, CommandResult, ExecCommand, ExecError, ExecEvent, ExecInput, Fill, OrderRequest, OrderStatus, OrderUpdate,
};

use crate::job::JobId;

/// Отслеживаемый ордер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedOrder {
    /// Запрос.
    pub req: OrderRequest,
    /// Состояние.
    pub status: OrderStatus,
    /// Исполнено (по заполнениям).
    pub filled: Qty,
    /// Сумма `qty × px` заполнений.
    pub notional: Decimal,
    /// Комиссии.
    pub fees: Usd,
    /// Задача-владелец.
    pub job: Option<JobId>,
    /// Время отправки, мс.
    pub sent_ms: i64,
    /// Последнее изменение, мс.
    pub updated_ms: i64,
    /// Последний запрос состояния, мс.
    pub last_query_ms: Option<i64>,
    /// Отмена запрошена.
    pub cancel_requested: bool,
    /// Причина отказа или отмены.
    pub reason: Option<String>,
}

impl TrackedOrder {
    /// Средняя цена исполнения.
    pub fn avg_px(&self) -> Option<Px> {
        (self.filled.0 > Decimal::ZERO).then(|| Px(self.notional / self.filled.0))
    }

    /// Остаток.
    pub fn remaining(&self) -> Decimal {
        self.req.qty.0 - self.filled.0
    }

    /// Ордер может ещё исполниться (не конечный).
    pub fn is_live(&self) -> bool {
        !self.status.is_terminal()
    }
}

/// Событие трекера для задач и портфеля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerEvent {
    /// Новое заполнение нашего ордера.
    Filled {
        /// Задача.
        job: Option<JobId>,
        /// Заполнение.
        fill: Fill,
    },
    /// Ордер перешёл в конечное состояние.
    Closed {
        /// Задача.
        job: Option<JobId>,
        /// Клиентский идентификатор.
        cid: ClientId,
        /// Конечное состояние.
        status: OrderStatus,
    },
    /// Заполнение не по нашему ордеру (ликвидация, ADL, ручная сделка).
    ForeignFill(Fill),
    /// Переход отклонён как неподтверждённый или противоречивый (для журнала).
    Ignored {
        /// Клиентский идентификатор.
        cid: ClientId,
        /// Причина.
        why: String,
    },
}

/// Трекер ордеров.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OrderTracker {
    orders: BTreeMap<ClientId, TrackedOrder>,
    seen_trades: BTreeSet<String>,
    /// Интервал повторного запроса `Unknown`, мс.
    pub query_every_ms: i64,
    /// Тайм-аут подтверждения отправки, мс.
    pub ack_timeout_ms: i64,
    /// Через сколько `Unknown`, которого площадка не знает, считается не дошедшим, мс.
    pub not_found_grace_ms: i64,
}

/// Допустимость перехода между состояниями.
fn allowed(from: OrderStatus, to: OrderStatus) -> bool {
    use OrderStatus::*;
    if from == to {
        return true;
    }
    match from {
        // конечные состояния не меняются
        Filled | Cancelled | Rejected | Expired => false,
        New => true,
        Sent | Unknown => to != New && to != Sent,
        Acked => !matches!(to, New | Sent | Unknown),
        PartiallyFilled => !matches!(to, New | Sent | Acked | Unknown | Rejected),
    }
}

impl OrderTracker {
    /// Трекер с тайм-аутами по умолчанию.
    pub fn new() -> Self {
        Self { query_every_ms: 1000, ack_timeout_ms: 5000, not_found_grace_ms: 10_000, ..Self::default() }
    }

    /// Ордер по идентификатору.
    pub fn get(&self, cid: ClientId) -> Option<&TrackedOrder> {
        self.orders.get(&cid)
    }

    /// Все ордера.
    pub fn iter(&self) -> impl Iterator<Item = &TrackedOrder> {
        self.orders.values()
    }

    /// Живые ордера.
    pub fn live(&self) -> impl Iterator<Item = &TrackedOrder> {
        self.orders.values().filter(|o| o.is_live())
    }

    /// Число ордеров в `Unknown`.
    pub fn unknown_count(&self) -> usize {
        self.orders.values().filter(|o| o.status == OrderStatus::Unknown).count()
    }

    /// Регистрирует отправку ордера и возвращает команду.
    pub fn send(&mut self, req: OrderRequest, job: Option<JobId>, now_ms: i64) -> ExecCommand {
        self.orders.insert(
            req.cid,
            TrackedOrder {
                req: req.clone(),
                status: OrderStatus::Sent,
                filled: Qty(Decimal::ZERO),
                notional: Decimal::ZERO,
                fees: Usd(Decimal::ZERO),
                job,
                sent_ms: now_ms,
                updated_ms: now_ms,
                last_query_ms: None,
                cancel_requested: false,
                reason: None,
            },
        );
        ExecCommand::Place(req)
    }

    /// Команда отмены живого ордера (повторная отмена не отправляется).
    pub fn cancel(&mut self, cid: ClientId) -> Option<ExecCommand> {
        let o = self.orders.get_mut(&cid)?;
        if !o.is_live() || o.cancel_requested {
            return None;
        }
        o.cancel_requested = true;
        Some(ExecCommand::Cancel { venue: o.req.venue.clone(), symbol: o.req.symbol.clone(), cid })
    }

    /// Обрабатывает вход от исполнения.
    pub fn on_input(&mut self, input: &ExecInput, now_ms: i64) -> Vec<TrackerEvent> {
        match input {
            ExecInput::Event(ExecEvent::Order(u)) => self.apply_update(u, now_ms),
            ExecInput::Event(ExecEvent::Fill(f)) => self.apply_fill(f, now_ms),
            ExecInput::Event(_) => Vec::new(),
            ExecInput::Result(r) => self.apply_result(r, now_ms),
        }
    }

    fn apply_result(&mut self, r: &CommandResult, now_ms: i64) -> Vec<TrackerEvent> {
        match r {
            CommandResult::Placed(cid, Ok(u)) => {
                let _ = cid;
                self.apply_update(u, now_ms)
            }
            CommandResult::Placed(cid, Err(e)) => {
                let Some(o) = self.orders.get_mut(cid) else { return Vec::new() };
                if o.status != OrderStatus::Sent && o.status != OrderStatus::Unknown {
                    return Vec::new();
                }
                match e {
                    ExecError::Timeout => {
                        o.status = OrderStatus::Unknown;
                        o.updated_ms = now_ms;
                        Vec::new()
                    }
                    // запрос точно не исполнен
                    other => {
                        o.status = OrderStatus::Rejected;
                        o.reason = Some(other.to_string());
                        o.updated_ms = now_ms;
                        vec![TrackerEvent::Closed { job: o.job, cid: *cid, status: OrderStatus::Rejected }]
                    }
                }
            }
            CommandResult::Cancelled(cid, res) => {
                if let Some(o) = self.orders.get_mut(cid) {
                    match res {
                        Ok(()) => {}
                        Err(ExecError::Timeout) => {
                            // исход отмены неизвестен: уточним запросом
                            o.cancel_requested = false;
                            if o.is_live() {
                                o.status = OrderStatus::Unknown;
                            }
                        }
                        Err(_) => o.cancel_requested = false,
                    }
                }
                Vec::new()
            }
            CommandResult::CancelledAll(_, _) => Vec::new(),
            CommandResult::Queried(cid, Ok(Some(u))) => {
                let mut ev = Vec::new();
                if let Some(o) = self.orders.get_mut(cid) {
                    o.last_query_ms = Some(now_ms);
                    // из Unknown переходим в подтверждённое площадкой состояние
                    if o.status == OrderStatus::Unknown {
                        o.status = OrderStatus::Sent;
                    }
                }
                ev.extend(self.apply_update(u, now_ms));
                ev
            }
            CommandResult::Queried(cid, Ok(None)) => {
                let grace = self.not_found_grace_ms;
                let Some(o) = self.orders.get_mut(cid) else { return Vec::new() };
                o.last_query_ms = Some(now_ms);
                if o.status == OrderStatus::Unknown && now_ms - o.sent_ms >= grace {
                    // площадка ордер не знает и после паузы: он не дошёл
                    o.status = OrderStatus::Rejected;
                    o.reason = Some("not found at venue".into());
                    return vec![TrackerEvent::Closed { job: o.job, cid: *cid, status: OrderStatus::Rejected }];
                }
                Vec::new()
            }
            CommandResult::Queried(cid, Err(_)) => {
                if let Some(o) = self.orders.get_mut(cid) {
                    o.last_query_ms = Some(now_ms);
                }
                Vec::new()
            }
        }
    }

    fn apply_update(&mut self, u: &OrderUpdate, now_ms: i64) -> Vec<TrackerEvent> {
        let Some(o) = self.orders.get_mut(&u.cid) else {
            return vec![TrackerEvent::Ignored { cid: u.cid, why: "unknown client id".into() }];
        };
        if !allowed(o.status, u.status) {
            return vec![TrackerEvent::Ignored {
                cid: u.cid,
                why: format!("transition {:?} -> {:?}", o.status, u.status),
            }];
        }
        // количество исполненного считается по заполнениям, статус — по площадке:
        // расхождение (потерянные сделки) ловит сверка позиций
        let was_live = o.is_live();
        o.status = u.status;
        if u.reason.is_some() {
            o.reason.clone_from(&u.reason);
        }
        o.updated_ms = now_ms;
        if was_live && !o.is_live() {
            return vec![TrackerEvent::Closed { job: o.job, cid: u.cid, status: o.status }];
        }
        Vec::new()
    }

    fn apply_fill(&mut self, f: &Fill, now_ms: i64) -> Vec<TrackerEvent> {
        if !self.seen_trades.insert(format!("{}|{}", f.venue, f.trade_id)) {
            return Vec::new();
        }
        let Some(cid) = f.cid else { return vec![TrackerEvent::ForeignFill(f.clone())] };
        let Some(o) = self.orders.get_mut(&cid) else { return vec![TrackerEvent::ForeignFill(f.clone())] };
        let mut out = Vec::new();
        o.filled = Qty(o.filled.0 + f.qty.0);
        o.notional += f.qty.0 * f.px.0;
        o.fees = Usd(o.fees.0 + f.fee.0);
        o.updated_ms = now_ms;
        out.push(TrackerEvent::Filled { job: o.job, fill: f.clone() });
        let was_live = o.is_live();
        if o.filled.0 >= o.req.qty.0 {
            o.status = OrderStatus::Filled;
        } else if matches!(o.status, OrderStatus::Sent | OrderStatus::Acked | OrderStatus::Unknown) {
            o.status = OrderStatus::PartiallyFilled;
        }
        if was_live && !o.is_live() {
            out.push(TrackerEvent::Closed { job: o.job, cid, status: o.status });
        }
        out
    }

    /// Тайм-ауты: `Sent` без ответа → `Unknown`; `Unknown` → запрос по клиентскому ID.
    pub fn tick(&mut self, now_ms: i64) -> Vec<ExecCommand> {
        let mut cmds = Vec::new();
        for o in self.orders.values_mut() {
            if o.status == OrderStatus::Sent && now_ms - o.sent_ms >= self.ack_timeout_ms {
                o.status = OrderStatus::Unknown;
                o.updated_ms = now_ms;
            }
            if o.status == OrderStatus::Unknown && o.last_query_ms.is_none_or(|t| now_ms - t >= self.query_every_ms) {
                o.last_query_ms = Some(now_ms);
                cmds.push(ExecCommand::Query {
                    venue: o.req.venue.clone(),
                    symbol: o.req.symbol.clone(),
                    cid: o.req.cid,
                });
            }
        }
        cmds
    }

    /// После перезапуска: живые ордера переводятся в `Unknown` и уточняются запросом.
    pub fn mark_live_unknown(&mut self) {
        for o in self.orders.values_mut() {
            if o.is_live() {
                o.status = OrderStatus::Unknown;
                o.last_query_ms = None;
                o.cancel_requested = false;
            }
        }
    }

    /// Удаляет закрытые ордера старше `keep_ms` (память ограничена).
    pub fn prune(&mut self, now_ms: i64, keep_ms: i64) {
        self.orders.retain(|_, o| o.is_live() || now_ms - o.updated_ms < keep_ms);
        if self.seen_trades.len() > 100_000 {
            self.seen_trades.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn transition_table() {
        use OrderStatus::*;
        assert!(allowed(Sent, Acked));
        assert!(allowed(Unknown, Filled));
        assert!(!allowed(Filled, Cancelled));
        assert!(!allowed(Cancelled, Acked));
        assert!(!allowed(PartiallyFilled, Acked));
        assert!(!allowed(Acked, Unknown));
    }

    proptest! {
        /// Из конечного состояния ничто не выводит.
        #[test]
        fn terminal_is_absorbing(to in 0usize..9) {
            use OrderStatus::*;
            let all = [New, Sent, Acked, PartiallyFilled, Filled, Cancelled, Rejected, Expired, Unknown];
            for from in [Filled, Cancelled, Rejected, Expired] {
                prop_assert!(from == all[to] || !allowed(from, all[to]));
            }
        }
    }
}
