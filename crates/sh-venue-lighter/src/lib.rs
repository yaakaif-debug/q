//! Адаптер Lighter: публичные рыночные данные (M0) и исполнение (M2) через FFI
//! к официальному подписанту `lighter-go`. `unsafe` есть только в модуле `signer`.

pub mod config;
pub mod exec;
pub mod feed;
pub mod normalize;
pub mod replay;
pub mod signer;
pub mod subs;
pub mod wire;

pub use config::LighterConfig;
pub use feed::LighterVenue;
pub use replay::LtReplay;
