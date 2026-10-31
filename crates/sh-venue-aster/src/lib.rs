//! Адаптер публичных рыночных данных Aster (futures v3). Без API-кошелька,
//! без EIP-712 и подписанных запросов.

pub mod config;
pub mod feed;
pub mod normalize;
pub mod wire;

pub use config::AsterConfig;
pub use feed::AsterVenue;
