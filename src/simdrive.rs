//! Связка движка с симулятором площадок: paper (живые данные), backtest (повтор
//! записей) и офлайн-учения. Команды движка исполняются симулятором с задержками
//! площадок; ответы, заполнения, фандинг и снимки позиций возвращаются в движок.

use rust_decimal::Decimal;
use sh_core::{Exchange, Px, Usd, VenueKey};
use sh_sim::SimCore;
use sh_venue_api::{ExecEvent, ExecInput, MdEvent};

use crate::engine::{Engine, EngineOutput, OperatorCommand};

/// Движок и симулятор.
pub struct SimDriver {
    /// Движок.
    pub engine: Engine,
    /// Симулятор.
    pub sim: SimCore,
    /// Период такта движка, мс.
    pub tick_every_ms: i64,
    next_tick_ms: Option<i64>,
    pending: EngineOutput,
}

fn merge(into: &mut EngineOutput, from: EngineOutput) {
    into.commands.extend(from.commands);
    into.alerts.extend(from.alerts);
    into.ledger.extend(from.ledger);
    into.scanned |= from.scanned;
    if from.report.is_some() {
        into.report = from.report;
    }
    into.first_seen.extend(from.first_seen);
    into.want_snapshots |= from.want_snapshots;
}

impl SimDriver {
    /// Новый драйвер.
    pub fn new(engine: Engine, sim: SimCore, tick_every_ms: i64) -> Self {
        Self { engine, sim, tick_every_ms: tick_every_ms.max(1), next_tick_ms: None, pending: EngineOutput::default() }
    }

    /// Восстанавливает позиции симулятора из книги движка (перезапуск paper).
    pub fn seed_from_book(&mut self) {
        let positions = self.engine.book().positions();
        let balances: Vec<(VenueKey, Usd)> =
            self.engine.book().balances.values().map(|b| (b.venue.clone(), b.equity)).collect();
        let book = self.engine.book();
        let seeded: Vec<(VenueKey, String, Decimal, Option<Px>)> =
            positions.iter().map(|(k, q)| (k.venue.clone(), k.symbol.clone(), *q, book.entry_mid(k))).collect();
        self.sim.seed(&seeded, &balances);
    }

    fn feed(&mut self, out: Vec<ExecInput>, now_ms: i64) {
        for o in out {
            self.engine.on_exec(&o, now_ms);
        }
    }

    /// Событие рынка: симулятор (исполнение по стакану и сделкам), затем движок; такт по расписанию.
    pub fn on_md(&mut self, ev: &MdEvent) {
        let now = ev.recv.wall_ns.div_euclid(1_000_000);
        let out = self.sim.on_md(ev);
        self.feed(out, now);
        self.engine.on_md(ev);
        if self.next_tick_ms.is_none_or(|t| now >= t) {
            let o = self.tick(now);
            merge(&mut self.pending, o);
        }
    }

    /// Накопленный выход тактов, вызванных событиями рынка.
    pub fn take_output(&mut self) -> EngineOutput {
        std::mem::take(&mut self.pending)
    }

    /// Такт: движок, исполнение команд, снимки для сверки.
    pub fn tick(&mut self, now_ms: i64) -> EngineOutput {
        self.next_tick_ms = Some(now_ms - now_ms.rem_euclid(self.tick_every_ms) + self.tick_every_ms);
        let out = self.sim.advance(now_ms);
        self.feed(out, now_ms);
        let o = self.engine.tick(now_ms);
        // команды остаются в выходе: live --dry-run подписывает их адаптерами без отправки
        for c in &o.commands {
            self.sim.submit(c.clone(), now_ms);
        }
        if o.want_snapshots {
            self.snapshots(now_ms);
        }
        let out = self.sim.advance(now_ms);
        self.feed(out, now_ms);
        o
    }

    /// Снимки позиций и балансов всех семейств.
    pub fn snapshots(&mut self, now_ms: i64) {
        self.engine.on_exec(&ExecInput::Event(ExecEvent::Balances(self.sim.balances(None))), now_ms);
        for ex in Exchange::ALL {
            let positions = self.sim.positions(Some(ex));
            self.engine.on_exec(&ExecInput::Event(ExecEvent::Positions { exchange: ex, positions }), now_ms);
        }
    }

    /// Команда оператора; её команды площадкам исполняются сразу.
    pub fn command(&mut self, cmd: &OperatorCommand, now_ms: i64) -> String {
        let reply = self.engine.on_command(cmd, now_ms);
        let o = self.tick(now_ms);
        merge(&mut self.pending, o);
        reply
    }
}
