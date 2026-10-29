//! Получатели событий адаптера: сырые записи для рекордера и нормализованные
//! события для движка. Сырые записи не теряются никогда (ждём место в канале),
//! нормализованные события стакана можно отбросить при переполнении.

use std::sync::Arc;

use sh_core::{Clock, Exchange, RawRecord, RawSource, RecvTs, SysEvent};
use tokio::sync::mpsc;

use crate::error::VenueError;
use crate::events::{MdEvent, MdEventKind};

/// Набор получателей, клонируется в каждую задачу адаптера.
#[derive(Clone)]
pub struct Sinks {
    raw: Option<mpsc::Sender<RawRecord>>,
    md: Option<mpsc::Sender<MdEvent>>,
    clock: Arc<dyn Clock>,
}

impl Sinks {
    /// Создаёт набор получателей.
    pub fn new(raw: Option<mpsc::Sender<RawRecord>>, md: Option<mpsc::Sender<MdEvent>>, clock: Arc<dyn Clock>) -> Self {
        Self { raw, md, clock }
    }

    /// Часы процесса.
    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    /// Метки времени «сейчас».
    pub fn now(&self) -> RecvTs {
        self.clock.recv_ts()
    }

    /// Записывает сырое сообщение; ждёт место в канале рекордера.
    pub async fn raw(
        &self,
        exchange: Exchange,
        source: RawSource,
        conn: &str,
        recv: RecvTs,
        payload: String,
    ) -> Result<(), VenueError> {
        let Some(tx) = &self.raw else { return Ok(()) };
        let rec = RawRecord { exchange, source, conn: conn.to_owned(), recv, payload };
        tx.send(rec).await.map_err(|_| VenueError::SinkClosed)
    }

    /// Записывает системное событие с текущим временем.
    pub async fn sys(&self, exchange: Exchange, conn: &str, ev: &SysEvent) -> Result<(), VenueError> {
        self.raw(exchange, RawSource::Sys, conn, self.now(), ev.to_json()).await
    }

    /// Отправляет нормализованное событие. События стакана при переполнении
    /// отбрасываются (возвращается `false`), остальные ждут места.
    pub async fn md(&self, ev: MdEvent) -> Result<bool, VenueError> {
        let Some(tx) = &self.md else { return Ok(true) };
        let droppable = matches!(
            ev.kind,
            MdEventKind::BookSnapshot { .. } | MdEventKind::BookDelta { .. } | MdEventKind::Bbo { .. }
        );
        if droppable {
            match tx.try_send(ev) {
                Ok(()) => Ok(true),
                Err(mpsc::error::TrySendError::Full(ev)) => {
                    metrics::counter!("md_events_dropped_total", "venue" => ev.venue.exchange.as_str()).increment(1);
                    Ok(false)
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(VenueError::SinkClosed),
            }
        } else {
            tx.send(ev).await.map_err(|_| VenueError::SinkClosed)?;
            Ok(true)
        }
    }
}
