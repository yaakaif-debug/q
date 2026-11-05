//! Адаптер публичных рыночных данных Hyperliquid: ядро и все HIP-3 дексы.
//! Дексы обнаруживаются через `perpDexs` при старте и каждый час; список не хардкодится.
//! Подписи, приватные эндпоинты и ордера в M0 отсутствуют.

pub mod config;
pub mod feed;
pub mod normalize;
pub mod replay;
pub mod subs;
pub mod wire;

pub use config::HyperliquidConfig;
pub use feed::HyperliquidVenue;
pub use replay::HlReplay;
