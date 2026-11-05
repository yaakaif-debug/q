//! Адаптер публичных рыночных данных Aster (futures v3). Без API-кошелька,
//! без EIP-712 и подписанных запросов.

pub mod config;
pub mod exec;
pub mod feed;
pub mod normalize;
pub mod replay;
pub mod wire;

pub use config::AsterConfig;
pub use feed::AsterVenue;
pub use replay::AsReplay;
