//! Базовые типы проекта: денежные новые типы, ключ площадки, часы,
//! сырые записи рекордера и общие ошибки. Крейт не делает I/O.

pub mod clock;
pub mod error;
pub mod num;
pub mod raw;
pub mod secrets;
pub mod serde_util;
pub mod venue;

pub use clock::{Clock, ManualClock, RecvTs, SystemClock};
pub use error::CoreError;
pub use num::{Bps, PositionChange, Px, Qty, Usd, apply_fill};
pub use raw::{RawRecord, RawSource, SysEvent};
pub use rust_decimal::Decimal;
pub use secrets::{Secrets, SecretsError, SecretsSource};
pub use venue::{Collateral, Exchange, Level, Side, UnderlyingId, VenueKey};
