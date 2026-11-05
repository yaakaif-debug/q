//! Асинхронная обёртка симулятора, реализующая `Execution`. Нужна для режима paper
//! с общим кодом live-оболочки и для офлайн-проверки `selftest` против симулятора.
//! Время — часы `Clock`; команда исполняется сразу с учётом задержки площадки.

use std::sync::{Arc, Mutex};

use sh_core::{Clock, Exchange, Usd, VenueKey};
use sh_venue_api::{
    ClientId, CommandResult, ExecCommand, ExecError, ExecEvent, ExecFuture, ExecInput, Execution, FundingPayment,
    OrderRequest, OrderUpdate, VenueBalance, VenuePosition,
};
use tokio::sync::{mpsc, watch};

use crate::core::SimCore;

/// Исполнение одного семейства площадок через общий симулятор.
pub struct SimExecution {
    exchange: Exchange,
    core: Arc<Mutex<SimCore>>,
    clock: Arc<dyn Clock>,
    events: Mutex<Option<mpsc::Sender<ExecEvent>>>,
    funding: Mutex<Vec<FundingPayment>>,
}

impl SimExecution {
    /// Обёртка над общим ядром.
    pub fn new(exchange: Exchange, core: Arc<Mutex<SimCore>>, clock: Arc<dyn Clock>) -> Self {
        Self { exchange, core, clock, events: Mutex::new(None), funding: Mutex::new(Vec::new()) }
    }

    fn now_ms(&self) -> i64 {
        self.clock.wall_ms()
    }

    /// Исполняет команду до получения её результата; события потока рассылаются подписчику.
    fn run(&self, cmd: ExecCommand) -> Result<CommandResult, ExecError> {
        let now = self.now_ms();
        let out = {
            let mut core = self.core.lock().map_err(|_| ExecError::Transport("sim lock poisoned".into()))?;
            core.submit(cmd, now);
            // задержки площадок не больше нескольких секунд
            core.advance(now + 10_000)
        };
        let mut result = None;
        let tx = self.events.lock().ok().and_then(|g| g.clone());
        for o in out {
            match o {
                ExecInput::Result(r) => result = Some(r),
                ExecInput::Event(ev) => {
                    if let ExecEvent::Funding(f) = &ev
                        && let Ok(mut g) = self.funding.lock()
                    {
                        g.push(f.clone());
                    }
                    if let Some(tx) = &tx {
                        let _ = tx.try_send(ev);
                    }
                }
            }
        }
        result.ok_or(ExecError::Timeout)
    }

    fn core(&self) -> Result<std::sync::MutexGuard<'_, SimCore>, ExecError> {
        self.core.lock().map_err(|_| ExecError::Transport("sim lock poisoned".into()))
    }
}

impl Execution for SimExecution {
    fn exchange(&self) -> Exchange {
        self.exchange
    }

    fn place(&self, req: OrderRequest) -> ExecFuture<'_, OrderUpdate> {
        Box::pin(async move {
            match self.run(ExecCommand::Place(req))? {
                CommandResult::Placed(_, r) => r,
                _ => Err(ExecError::Timeout),
            }
        })
    }

    fn cancel(&self, venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            match self.run(ExecCommand::Cancel { venue, symbol, cid })? {
                CommandResult::Cancelled(_, r) => r,
                _ => Err(ExecError::Timeout),
            }
        })
    }

    fn cancel_all(&self) -> ExecFuture<'_, u32> {
        Box::pin(async move {
            match self.run(ExecCommand::CancelAll(self.exchange))? {
                CommandResult::CancelledAll(_, r) => r,
                _ => Err(ExecError::Timeout),
            }
        })
    }

    fn query_order(&self, venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, Option<OrderUpdate>> {
        Box::pin(async move {
            match self.run(ExecCommand::Query { venue, symbol, cid })? {
                CommandResult::Queried(_, r) => r,
                _ => Err(ExecError::Timeout),
            }
        })
    }

    fn positions(&self) -> ExecFuture<'_, Vec<VenuePosition>> {
        Box::pin(async move { Ok(self.core()?.positions(Some(self.exchange))) })
    }

    fn balances(&self) -> ExecFuture<'_, Vec<VenueBalance>> {
        Box::pin(async move { Ok(self.core()?.balances(Some(self.exchange))) })
    }

    fn funding_payments(&self, since_ms: i64) -> ExecFuture<'_, Vec<FundingPayment>> {
        Box::pin(async move {
            let g = self.funding.lock().map_err(|_| ExecError::Transport("sim lock poisoned".into()))?;
            Ok(g.iter().filter(|f| f.ts_ms >= since_ms && f.venue.exchange == self.exchange).cloned().collect())
        })
    }

    fn transfer_between_dexes(&self, from_dex: String, to_dex: String, amount: Usd) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            if self.exchange != Exchange::Hyperliquid {
                return Err(ExecError::NotSupported("transfer between dexes".into()));
            }
            self.core()?.transfer(&VenueKey::hyperliquid(from_dex), &VenueKey::hyperliquid(to_dex), amount)
        })
    }

    fn run_stream(&self, tx: mpsc::Sender<ExecEvent>, mut shutdown: watch::Receiver<bool>) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            if let Ok(mut g) = self.events.lock() {
                *g = Some(tx.clone());
            }
            let _ = tx.send(ExecEvent::StreamUp(self.exchange)).await;
            while !*shutdown.borrow() {
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
            if let Ok(mut g) = self.events.lock() {
                *g = None;
            }
            Ok(())
        })
    }
}
