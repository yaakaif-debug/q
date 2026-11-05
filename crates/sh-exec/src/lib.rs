//! Исполнение (SPEC.md, «Entry sequence», «Order handling rules»): автомат состояний
//! ордера с применением только подтверждённых переходов и задача пары S5
//! (мейкер-нога post-only, хедж IOC на каждое заполнение, тайм-ауты, повторы,
//! разворот reduce-only). Код синхронный: команды уходят в оболочку, результаты и
//! события площадок возвращаются обратно.

#![forbid(unsafe_code)]

pub mod job;
pub mod tracker;

pub use job::{JobAction, JobId, JobKind, JobLeg, JobParams, JobState, PairJob, common_lot};
pub use tracker::{OrderTracker, TrackedOrder, TrackerEvent};
