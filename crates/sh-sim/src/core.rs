//! Синхронное ядро симулятора.

use std::collections::{BTreeMap, VecDeque};

use rust_decimal::Decimal;
use sh_core::{Collateral, Exchange, Px, Qty, Side, Usd, VenueKey};
use sh_md::{InstrumentKey, MarketState};
use sh_signals::FeesConfig;
use sh_venue_api::{
    ClientId, CommandResult, ExecCommand, ExecError, ExecEvent, ExecInput, Fill, FundingPayment, MdEvent, MdEventKind,
    OrderRequest, OrderStatus, OrderUpdate, Tif, VenueBalance, VenuePosition,
};

/// Задержки площадки в симуляторе, мс.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimVenueParams {
    /// Сеть (RTT) до площадки.
    pub rtt_ms: i64,
    /// Протокольная задержка тейкера (Lighter Standard: 300 мс).
    pub taker_delay_ms: i64,
    /// Протокольная задержка мейкера (post-only).
    pub maker_delay_ms: i64,
    /// Протокольная задержка отмены.
    pub cancel_delay_ms: i64,
}

impl SimVenueParams {
    /// Значения по умолчанию для семейства площадок (SPEC.md, docs/venues).
    pub fn default_for(ex: Exchange, lighter_premium: bool) -> Self {
        match ex {
            Exchange::Hyperliquid => Self { rtt_ms: 10, taker_delay_ms: 0, maker_delay_ms: 0, cancel_delay_ms: 0 },
            Exchange::Lighter if lighter_premium => {
                Self { rtt_ms: 10, taker_delay_ms: 140, maker_delay_ms: 0, cancel_delay_ms: 0 }
            }
            Exchange::Lighter => Self { rtt_ms: 10, taker_delay_ms: 300, maker_delay_ms: 0, cancel_delay_ms: 300 },
            Exchange::Aster => Self { rtt_ms: 10, taker_delay_ms: 0, maker_delay_ms: 0, cancel_delay_ms: 0 },
        }
    }
}

/// Режим отказа площадки (учения: отказ площадки, неизвестный исход).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailMode {
    /// Команды не доходят: ошибка транспорта, ничего не исполнено.
    Down,
    /// Команды исполняются, но ответ теряется (тайм-аут): исход неизвестен.
    LostReplies,
    /// Как `LostReplies`, и приватный поток тоже молчит: состояние узнаётся только запросом.
    Blackout,
}

/// Параметры симулятора.
#[derive(Debug, Clone)]
pub struct SimParams {
    /// Задержки по семействам.
    pub venues: BTreeMap<Exchange, SimVenueParams>,
    /// Комиссии.
    pub fees: FeesConfig,
    /// Начальный капитал на каждой площадке (ключ — `VenueKey`).
    pub initial_cash: BTreeMap<VenueKey, Usd>,
    /// Плечо для расчёта доступной маржи.
    pub leverage: Decimal,
}

impl Default for SimParams {
    fn default() -> Self {
        Self {
            venues: Exchange::ALL.iter().map(|e| (*e, SimVenueParams::default_for(*e, false))).collect(),
            fees: FeesConfig::default(),
            initial_cash: BTreeMap::new(),
            leverage: Decimal::from(3),
        }
    }
}

#[derive(Debug, Clone)]
struct SimOrder {
    req: OrderRequest,
    status: OrderStatus,
    filled: Decimal,
    notional: Decimal,
    /// Объём перед ордером в очереди (мейкер).
    queue_ahead: Decimal,
    /// Объём сделок через цену ордера после постановки.
    traded_through: Decimal,
    venue_order_id: u64,
    ts_ms: i64,
}

impl SimOrder {
    fn update(&self, ts_ms: i64, reason: Option<String>) -> OrderUpdate {
        OrderUpdate {
            cid: self.req.cid,
            venue: self.req.venue.clone(),
            symbol: self.req.symbol.clone(),
            status: self.status,
            filled_qty: Qty(self.filled),
            avg_px: (self.filled > Decimal::ZERO).then(|| Px(self.notional / self.filled)),
            venue_order_id: Some(self.venue_order_id.to_string()),
            reason,
            ts_ms,
        }
    }

    fn remaining(&self) -> Decimal {
        self.req.qty.0 - self.filled
    }
}

#[derive(Debug, Clone, Default)]
struct SimPosition {
    qty: Decimal,
    entry_px: Decimal,
}

#[derive(Debug, Clone)]
struct Pending {
    due_ms: i64,
    seq: u64,
    cmd: ExecCommand,
    lost_reply: bool,
    blackout: bool,
}

/// Синхронный симулятор всех площадок.
#[derive(Debug)]
pub struct SimCore {
    params: SimParams,
    market: MarketState,
    orders: BTreeMap<ClientId, SimOrder>,
    pending: VecDeque<Pending>,
    positions: BTreeMap<(VenueKey, String), SimPosition>,
    cash: BTreeMap<VenueKey, Decimal>,
    fail: BTreeMap<Exchange, FailMode>,
    now_ms: i64,
    seq: u64,
    next_oid: u64,
    trade_seq: u64,
    out: Vec<ExecInput>,
}

fn sign(side: Side) -> Decimal {
    match side {
        Side::Buy => Decimal::ONE,
        Side::Sell => -Decimal::ONE,
    }
}

impl SimCore {
    /// Новый симулятор.
    pub fn new(params: SimParams) -> Self {
        let cash = params.initial_cash.iter().map(|(k, v)| (k.clone(), v.0)).collect();
        Self {
            params,
            market: MarketState::new(),
            orders: BTreeMap::new(),
            pending: VecDeque::new(),
            positions: BTreeMap::new(),
            cash,
            fail: BTreeMap::new(),
            now_ms: 0,
            seq: 0,
            next_oid: 1,
            trade_seq: 0,
            out: Vec::new(),
        }
    }

    /// Текущее время симулятора, мс.
    pub fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// Состояние рынка симулятора.
    pub fn market(&self) -> &MarketState {
        &self.market
    }

    /// Включает или снимает режим отказа площадки.
    pub fn set_fail(&mut self, ex: Exchange, mode: Option<FailMode>) {
        match mode {
            Some(m) => {
                self.fail.insert(ex, m);
            }
            None => {
                self.fail.remove(&ex);
            }
        }
    }

    /// Задержка исполнения команды.
    fn latency(&self, cmd: &ExecCommand) -> i64 {
        let ex = match cmd {
            ExecCommand::Place(r) => r.venue.exchange,
            ExecCommand::Cancel { venue, .. } | ExecCommand::Query { venue, .. } => venue.exchange,
            ExecCommand::CancelAll(e) => *e,
        };
        let p = self.params.venues.get(&ex).cloned().unwrap_or_else(|| SimVenueParams::default_for(ex, false));
        let protocol = match cmd {
            ExecCommand::Place(r) if r.tif == Tif::PostOnly => p.maker_delay_ms,
            ExecCommand::Place(_) => p.taker_delay_ms,
            ExecCommand::Cancel { .. } | ExecCommand::CancelAll(_) => p.cancel_delay_ms,
            ExecCommand::Query { .. } => 0,
        };
        p.rtt_ms / 2 + protocol
    }

    /// Принимает команду в момент `now_ms`; исполнится через задержку площадки.
    pub fn submit(&mut self, cmd: ExecCommand, now_ms: i64) {
        let ex = match &cmd {
            ExecCommand::Place(r) => r.venue.exchange,
            ExecCommand::Cancel { venue, .. } | ExecCommand::Query { venue, .. } => venue.exchange,
            ExecCommand::CancelAll(e) => *e,
        };
        match self.fail.get(&ex) {
            Some(FailMode::Down) => {
                let err = ExecError::Transport("simulated venue down".into());
                self.out.push(ExecInput::Result(failed(&cmd, err)));
                return;
            }
            Some(FailMode::LostReplies | FailMode::Blackout) => {
                let due_ms = now_ms + self.latency(&cmd);
                self.seq += 1;
                let blackout = self.fail.get(&ex) == Some(&FailMode::Blackout);
                self.pending.push_back(Pending { due_ms, seq: self.seq, cmd, lost_reply: true, blackout });
                return;
            }
            None => {}
        }
        let due_ms = now_ms + self.latency(&cmd);
        self.seq += 1;
        // очередь упорядочена по (время, номер): вставка с сохранением порядка
        let pos =
            self.pending.iter().position(|p| (p.due_ms, p.seq) > (due_ms, self.seq)).unwrap_or(self.pending.len());
        self.pending.insert(pos, Pending { due_ms, seq: self.seq, cmd, lost_reply: false, blackout: false });
    }

    /// Продвигает время до `now_ms`, исполняя наступившие команды; возвращает накопленный выход.
    pub fn advance(&mut self, now_ms: i64) -> Vec<ExecInput> {
        while let Some(p) = self.pending.front() {
            if p.due_ms > now_ms {
                break;
            }
            let Some(p) = self.pending.pop_front() else { break };
            self.now_ms = self.now_ms.max(p.due_ms);
            let before = self.out.len();
            self.execute(p.cmd.clone());
            if p.lost_reply {
                // ответ потерян: результат команды заменяется тайм-аутом, события потока остаются
                for o in &mut self.out[before..] {
                    if let ExecInput::Result(r) = o {
                        *r = failed(&p.cmd, ExecError::Timeout);
                    }
                }
                if p.blackout {
                    let mut i = before;
                    while i < self.out.len() {
                        if matches!(self.out[i], ExecInput::Event(_)) {
                            self.out.remove(i);
                        } else {
                            i += 1;
                        }
                    }
                }
            }
        }
        self.now_ms = self.now_ms.max(now_ms);
        std::mem::take(&mut self.out)
    }

    /// Событие рынка: время продвигается до времени получения, затем обновляются стакан,
    /// исполнение мейкер-ордеров по сделкам и фандинг.
    pub fn on_md(&mut self, ev: &MdEvent) -> Vec<ExecInput> {
        let ts = ev.recv.wall_ns.div_euclid(1_000_000);
        let mut out = self.advance(ts);
        if let Some(ns) = self.market.apply(ev) {
            self.apply_funding(&ns.key, ns.settlement.rate, ns.settlement.ts_ms);
        }
        if let MdEventKind::Trades(trades) = &ev.kind {
            for t in trades {
                self.match_trade(&ev.venue, &ev.symbol, t.px.0, t.qty.0, t.taker_side);
            }
        }
        out.append(&mut self.out);
        out
    }

    fn apply_funding(&mut self, key: &InstrumentKey, rate: Decimal, ts_ms: i64) {
        let k = (key.venue.clone(), key.symbol.clone());
        let Some(pos) = self.positions.get(&k) else { return };
        if pos.qty.is_zero() {
            return;
        }
        let Some(px) = self.market.get(key).and_then(|s| s.funding_price(key.venue.exchange).or_else(|| s.mid()))
        else {
            return;
        };
        // лонг платит при положительной ставке
        let amount = -pos.qty * rate * px.0;
        *self.cash.entry(key.venue.clone()).or_default() += amount;
        self.out.push(ExecInput::Event(ExecEvent::Funding(FundingPayment {
            venue: key.venue.clone(),
            symbol: key.symbol.clone(),
            amount: Usd(amount),
            rate: Some(rate),
            ts_ms,
        })));
    }

    fn fee_rates(&self, venue: &VenueKey, symbol: &str) -> (Decimal, Decimal) {
        let key = InstrumentKey::new(venue.clone(), symbol);
        match self.market.get(&key).and_then(|s| s.meta.as_ref()) {
            Some(m) => {
                let r = self.params.fees.resolve(m);
                (r.maker, r.taker)
            }
            None => (Decimal::ZERO, Decimal::ZERO),
        }
    }

    fn execute(&mut self, cmd: ExecCommand) {
        match cmd {
            ExecCommand::Place(req) => self.place(req),
            ExecCommand::Cancel { cid, .. } => {
                let res = match self.orders.get_mut(&cid) {
                    Some(o) if !o.status.is_terminal() => {
                        o.status = OrderStatus::Cancelled;
                        let u = o.update(self.now_ms, Some("cancelled".into()));
                        self.out.push(ExecInput::Event(ExecEvent::Order(u)));
                        Ok(())
                    }
                    Some(_) => Err(ExecError::Rejected("order already closed".into())),
                    None => Err(ExecError::Rejected("unknown order".into())),
                };
                self.out.push(ExecInput::Result(CommandResult::Cancelled(cid, res)));
            }
            ExecCommand::CancelAll(ex) => {
                let mut n = 0u32;
                let now = self.now_ms;
                let mut ups = Vec::new();
                for o in self.orders.values_mut() {
                    if o.req.venue.exchange == ex && !o.status.is_terminal() {
                        o.status = OrderStatus::Cancelled;
                        ups.push(o.update(now, Some("cancel all".into())));
                        n += 1;
                    }
                }
                self.out.extend(ups.into_iter().map(|u| ExecInput::Event(ExecEvent::Order(u))));
                self.out.push(ExecInput::Result(CommandResult::CancelledAll(ex, Ok(n))));
            }
            ExecCommand::Query { cid, .. } => {
                let u = self.orders.get(&cid).map(|o| o.update(self.now_ms, None));
                self.out.push(ExecInput::Result(CommandResult::Queried(cid, Ok(u))));
            }
        }
    }

    fn reject(&mut self, req: &OrderRequest, reason: &str) {
        let o = SimOrder {
            req: req.clone(),
            status: OrderStatus::Rejected,
            filled: Decimal::ZERO,
            notional: Decimal::ZERO,
            queue_ahead: Decimal::ZERO,
            traded_through: Decimal::ZERO,
            venue_order_id: 0,
            ts_ms: self.now_ms,
        };
        let u = o.update(self.now_ms, Some(reason.into()));
        self.orders.insert(req.cid, o);
        self.out.push(ExecInput::Result(CommandResult::Placed(req.cid, Err(ExecError::Rejected(reason.into())))));
        self.out.push(ExecInput::Event(ExecEvent::Order(u)));
    }

    fn place(&mut self, mut req: OrderRequest) {
        if self.orders.contains_key(&req.cid) {
            // повтор клиентского идентификатора: площадки отклоняют дубликаты
            self.out.push(ExecInput::Result(CommandResult::Placed(
                req.cid,
                Err(ExecError::Rejected("duplicate client id".into())),
            )));
            return;
        }
        let key = InstrumentKey::new(req.venue.clone(), req.symbol.clone());
        if let Err(reason) = self.validate(&key, &mut req) {
            return self.reject(&req, reason);
        }
        let Some(state) = self.market.get(&key) else {
            return self.reject(&req, "unknown instrument");
        };
        let (best_bid, best_ask) = state.best();
        let crosses = match req.side {
            Side::Buy => best_ask.is_some_and(|a| req.px.0 >= a.px.0),
            Side::Sell => best_bid.is_some_and(|b| req.px.0 <= b.px.0),
        };
        if req.tif == Tif::PostOnly && crosses {
            return self.reject(&req, "post only would cross");
        }
        let oid = self.next_oid;
        self.next_oid += 1;
        let queue_ahead =
            state.book.top(req.side, 50).iter().find(|l| l.px == req.px).map(|l| l.qty.0).unwrap_or_default();
        let mut o = SimOrder {
            req: req.clone(),
            status: OrderStatus::Acked,
            filled: Decimal::ZERO,
            notional: Decimal::ZERO,
            queue_ahead,
            traded_through: Decimal::ZERO,
            venue_order_id: oid,
            ts_ms: self.now_ms,
        };
        let mut fills = Vec::new();
        if crosses {
            // тейкер: проход по противоположной стороне до предельной цены
            let opp = match req.side {
                Side::Buy => Side::Sell,
                Side::Sell => Side::Buy,
            };
            let mut left = req.qty.0;
            for l in state.book.top(opp, 1000) {
                let ok = match req.side {
                    Side::Buy => l.px.0 <= req.px.0,
                    Side::Sell => l.px.0 >= req.px.0,
                };
                if !ok || left.is_zero() {
                    break;
                }
                let q = left.min(l.qty.0);
                left -= q;
                fills.push((l.px.0, q));
            }
            o.queue_ahead = Decimal::ZERO;
        }
        let (_, taker) = self.fee_rates(&req.venue, &req.symbol);
        let cid = req.cid;
        let ack = o.update(self.now_ms, None);
        self.orders.insert(cid, o);
        self.out.push(ExecInput::Result(CommandResult::Placed(cid, Ok(ack.clone()))));
        self.out.push(ExecInput::Event(ExecEvent::Order(ack)));
        for (px, q) in fills {
            self.fill(cid, px, q, false, taker);
        }
        let now = self.now_ms;
        if let Some(o) = self.orders.get_mut(&cid)
            && !o.status.is_terminal()
            && o.req.tif == Tif::Ioc
        {
            // остаток IOC отменяется
            o.status = OrderStatus::Cancelled;
            let u = o.update(now, Some("ioc remainder cancelled".into()));
            self.out.push(ExecInput::Event(ExecEvent::Order(u)));
        }
    }

    fn validate(&self, key: &InstrumentKey, req: &mut OrderRequest) -> Result<(), &'static str> {
        let state = self.market.get(key).ok_or("unknown instrument")?;
        if let Some(m) = &state.meta {
            if !m.price_rule.is_valid(req.px) {
                return Err("invalid price");
            }
            if req.qty.floor_to_lot(m.lot_size).ok() != Some(req.qty) {
                return Err("invalid size");
            }
            if let Some(mn) = m.min_notional
                && req.px.notional(req.qty).0 < mn.0
                && !req.reduce_only
            {
                return Err("below min notional");
            }
        }
        if req.qty.0 <= Decimal::ZERO {
            return Err("invalid size");
        }
        if req.reduce_only {
            let pos = self.positions.get(&(req.venue.clone(), req.symbol.clone())).map(|p| p.qty).unwrap_or_default();
            let reducible = if sign(req.side) * pos < Decimal::ZERO { pos.abs() } else { Decimal::ZERO };
            if reducible.is_zero() {
                return Err("reduce only would increase position");
            }
            req.qty = Qty(req.qty.0.min(reducible));
        }
        Ok(())
    }

    fn fill(&mut self, cid: ClientId, px: Decimal, q: Decimal, maker: bool, fee_rate: Decimal) {
        let now = self.now_ms;
        let Some(o) = self.orders.get_mut(&cid) else { return };
        let q = q.min(o.remaining());
        if q <= Decimal::ZERO {
            return;
        }
        o.filled += q;
        o.notional += q * px;
        o.status = if o.remaining().is_zero() { OrderStatus::Filled } else { OrderStatus::PartiallyFilled };
        let side = o.req.side;
        let venue = o.req.venue.clone();
        let symbol = o.req.symbol.clone();
        let update = o.update(now, None);
        let fee = q * px * fee_rate;
        self.trade_seq += 1;
        let realized = self.apply_position(&venue, &symbol, sign(side) * q, px);
        *self.cash.entry(venue.clone()).or_default() += realized - fee;
        self.out.push(ExecInput::Event(ExecEvent::Fill(Fill {
            cid: Some(cid),
            venue,
            symbol,
            side,
            qty: Qty(q),
            px: Px(px),
            fee: Usd(fee),
            maker,
            trade_id: format!("sim-{}", self.trade_seq),
            ts_ms: now,
            liquidation: false,
        })));
        self.out.push(ExecInput::Event(ExecEvent::Order(update)));
    }

    /// Обновляет позицию; возвращает реализованный результат.
    fn apply_position(&mut self, venue: &VenueKey, symbol: &str, dq: Decimal, px: Decimal) -> Decimal {
        let p = self.positions.entry((venue.clone(), symbol.to_owned())).or_default();
        let ch = sh_core::apply_fill(p.qty, p.entry_px, dq, px);
        p.qty = ch.qty;
        p.entry_px = ch.entry_px;
        ch.realized
    }

    fn match_trade(&mut self, venue: &VenueKey, symbol: &str, px: Decimal, qty: Decimal, taker: Option<Side>) {
        let mut hits = Vec::new();
        for (cid, o) in &mut self.orders {
            if o.status.is_terminal() || o.req.venue != *venue || o.req.symbol != symbol || o.ts_ms > self.now_ms {
                continue;
            }
            // сделка должна быть со стороны, которая бьёт по нашему ордеру
            let side_ok = match (o.req.side, taker) {
                (Side::Buy, Some(Side::Buy)) | (Side::Sell, Some(Side::Sell)) => false,
                _ => true,
            };
            let through = match o.req.side {
                Side::Buy => px <= o.req.px.0,
                Side::Sell => px >= o.req.px.0,
            };
            if !side_ok || !through {
                continue;
            }
            let before = (o.traded_through - o.queue_ahead).max(Decimal::ZERO);
            o.traded_through += qty;
            let avail = (o.traded_through - o.queue_ahead).max(Decimal::ZERO) - before;
            if avail > Decimal::ZERO {
                hits.push((*cid, o.req.px.0, avail));
            }
        }
        for (cid, px, q) in hits {
            let (maker, _) = self.fee_rates(venue, symbol);
            self.fill(cid, px, q, true, maker);
        }
    }

    /// Принудительное изменение позиции не нашими ордерами (учения ADL / ликвидации).
    pub fn force_position_change(&mut self, venue: &VenueKey, symbol: &str, delta: Decimal, px: Px) -> Vec<ExecInput> {
        let realized = self.apply_position(venue, symbol, delta, px.0);
        *self.cash.entry(venue.clone()).or_default() += realized;
        self.trade_seq += 1;
        self.out.push(ExecInput::Event(ExecEvent::Fill(Fill {
            cid: None,
            venue: venue.clone(),
            symbol: symbol.to_owned(),
            side: if delta > Decimal::ZERO { Side::Buy } else { Side::Sell },
            qty: Qty(delta.abs()),
            px,
            fee: Usd(Decimal::ZERO),
            maker: false,
            trade_id: format!("sim-adl-{}", self.trade_seq),
            ts_ms: self.now_ms,
            liquidation: true,
        })));
        std::mem::take(&mut self.out)
    }

    /// Перевод обеспечения между дексами Hyperliquid.
    pub fn transfer(&mut self, from: &VenueKey, to: &VenueKey, amount: Usd) -> Result<(), ExecError> {
        let avail = self.balance_of(from).available.0;
        if amount.0 <= Decimal::ZERO || amount.0 > avail {
            return Err(ExecError::Rejected("insufficient available balance".into()));
        }
        *self.cash.entry(from.clone()).or_default() -= amount.0;
        *self.cash.entry(to.clone()).or_default() += amount.0;
        Ok(())
    }

    /// Состояние ордера.
    pub fn order(&self, cid: ClientId) -> Option<OrderUpdate> {
        self.orders.get(&cid).map(|o| o.update(self.now_ms, None))
    }

    /// Открытые ордера.
    pub fn open_orders(&self) -> Vec<OrderUpdate> {
        self.orders.values().filter(|o| !o.status.is_terminal()).map(|o| o.update(self.now_ms, None)).collect()
    }

    /// Позиции (ненулевые) семейства или всех площадок.
    pub fn positions(&self, ex: Option<Exchange>) -> Vec<VenuePosition> {
        self.positions
            .iter()
            .filter(|((v, _), p)| !p.qty.is_zero() && ex.is_none_or(|e| v.exchange == e))
            .map(|((v, s), p)| VenuePosition {
                venue: v.clone(),
                symbol: s.clone(),
                qty: p.qty,
                entry_px: Some(Px(p.entry_px)),
                liq_px: None,
                margin: self.mark(v, s).map(|m| Usd(p.qty.abs() * m / self.params.leverage)),
            })
            .collect()
    }

    fn mark(&self, venue: &VenueKey, symbol: &str) -> Option<Decimal> {
        let st = self.market.get(&InstrumentKey::new(venue.clone(), symbol))?;
        st.mark.or_else(|| st.mid()).map(|p| p.0)
    }

    fn balance_of(&self, venue: &VenueKey) -> VenueBalance {
        let cash = self.cash.get(venue).copied().unwrap_or_default();
        let mut upnl = Decimal::ZERO;
        let mut margin = Decimal::ZERO;
        for ((v, s), p) in &self.positions {
            if v != venue || p.qty.is_zero() {
                continue;
            }
            if let Some(m) = self.mark(v, s) {
                upnl += p.qty * (m - p.entry_px);
                margin += p.qty.abs() * m / self.params.leverage;
            }
        }
        let equity = cash + upnl;
        VenueBalance {
            venue: venue.clone(),
            collateral: Collateral::Usdc,
            equity: Usd(equity),
            available: Usd(equity - margin),
            maintenance_margin: Some(Usd(margin / Decimal::from(2))),
        }
    }

    /// Балансы семейства или всех площадок.
    pub fn balances(&self, ex: Option<Exchange>) -> Vec<VenueBalance> {
        let mut keys: Vec<VenueKey> = self.cash.keys().cloned().collect();
        for (v, _) in self.positions.keys() {
            if !keys.contains(v) {
                keys.push(v.clone());
            }
        }
        keys.sort();
        keys.into_iter().filter(|k| ex.is_none_or(|e| k.exchange == e)).map(|k| self.balance_of(&k)).collect()
    }

    /// Капитал всех площадок.
    pub fn total_equity(&self) -> Usd {
        Usd(self.balances(None).iter().map(|b| b.equity.0).sum())
    }
}

fn failed(cmd: &ExecCommand, err: ExecError) -> CommandResult {
    match cmd {
        ExecCommand::Place(r) => CommandResult::Placed(r.cid, Err(err)),
        ExecCommand::Cancel { cid, .. } => CommandResult::Cancelled(*cid, Err(err)),
        ExecCommand::CancelAll(e) => CommandResult::CancelledAll(*e, Err(err)),
        ExecCommand::Query { cid, .. } => CommandResult::Queried(*cid, Err(err)),
    }
}
