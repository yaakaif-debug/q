//! Рыночные данные: локальные стаканы, обнаружение разрывов последовательности,
//! пересинхронизация по снимку, BBO, проход по глубине и устаревание.

pub mod book;
pub mod sequenced;
pub mod state;

pub use book::{OrderBook, WalkResult};
pub use sequenced::{ApplyOutcome, BookDelta, DeltaSeq, SequencedBook, SyncState};
pub use state::{FundingState, InstrumentKey, InstrumentState, MarketState, NewSettlement, Settlement};
