//! Конфигурация процесса: `config/default.toml` + наложение окружения (`config/live.toml`)
//! + переменные `SH_...`. Неизвестные ключи — ошибка.

use std::path::Path;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;
use sh_obs::{LogConfig, MetricsConfig};
use sh_store::RecorderConfig;
use sh_venue_api::ProxyConfig;
use sh_venue_aster::AsterConfig;
use sh_venue_hyperliquid::HyperliquidConfig;
use sh_venue_lighter::LighterConfig;

/// Корневая конфигурация.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AppConfig {
    /// Движок.
    pub engine: EngineConfig,
    /// Рекордер.
    pub recorder: RecorderConfig,
    /// Логи.
    pub log: LogConfig,
    /// Метрики.
    pub metrics: MetricsConfig,
    /// Сеть.
    pub network: NetworkConfig,
    /// Площадки.
    pub venues: VenuesConfig,
}

/// Настройки `[engine]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct EngineConfig {
    /// Режим: в M0 доступен только `record`.
    pub mode: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { mode: "record".into() }
    }
}

/// Настройки `[network]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkConfig {
    /// Прокси для WebSocket: `env` (из `HTTPS_PROXY`), `none` или `http://host:port`.
    pub proxy: String,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { proxy: "env".into() }
    }
}

impl NetworkConfig {
    /// Разрешает настройку прокси.
    pub fn proxy(&self) -> Result<Option<ProxyConfig>, String> {
        match self.proxy.as_str() {
            "env" => Ok(ProxyConfig::from_env()),
            "none" => Ok(None),
            url => {
                let no_proxy = std::env::var("NO_PROXY").unwrap_or_default();
                ProxyConfig::parse(url, &no_proxy).map(Some).map_err(|e| e.to_string())
            }
        }
    }
}

/// Настройки `[venues.*]`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct VenuesConfig {
    /// Hyperliquid (ядро и HIP-3).
    pub hyperliquid: HyperliquidConfig,
    /// Lighter.
    pub lighter: LighterConfig,
    /// Aster.
    pub aster: AsterConfig,
}

impl AppConfig {
    /// Загружает и проверяет конфигурацию.
    pub fn load(base: &Path, overlay: Option<&Path>) -> anyhow::Result<Self> {
        if !base.exists() {
            anyhow::bail!("config file {} not found", base.display());
        }
        let mut fig = Figment::new().merge(Toml::file_exact(base));
        if let Some(o) = overlay {
            if !o.exists() {
                anyhow::bail!("overlay config {} not found", o.display());
            }
            fig = fig.merge(Toml::file_exact(o));
        }
        let cfg: AppConfig = fig.merge(Env::prefixed("SH_").split("__")).extract()?;
        cfg.validate().map_err(anyhow::Error::msg)?;
        Ok(cfg)
    }

    /// Проверка ограничений M0.
    pub fn validate(&self) -> Result<(), String> {
        if self.engine.mode != "record" {
            return Err(format!("engine.mode '{}' is not available in M0 (only 'record')", self.engine.mode));
        }
        self.venues.hyperliquid.validate()?;
        self.venues.lighter.validate()?;
        self.venues.aster.validate()?;
        self.network.proxy()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<AppConfig, String> {
        let cfg: AppConfig = Figment::new().merge(Toml::string(toml)).extract().map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn default_config_file_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/default.toml");
        let cfg = AppConfig::load(&path, None).unwrap();
        assert_eq!(cfg.engine.mode, "record");
        assert_eq!(cfg.venues.hyperliquid.dexes, "auto");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(parse("[engine]\nmode = \"record\"\nfoo = 1\n").is_err());
        assert!(parse("[venues.hyperliquid]\nmax_subs = 5\n").is_err());
        assert!(parse("[strategy.s1_funding_carry]\nmin_net_apr = 12.0\n").is_err());
    }

    #[test]
    fn non_record_modes_and_trading_are_rejected() {
        assert!(parse("[engine]\nmode = \"live\"\n").is_err());
        assert!(parse("[venues.aster]\ntrading_enabled = true\n").is_err());
        assert!(parse("[network]\nproxy = \"socks5://x:1\"\n").is_err());
        assert!(parse("").is_ok());
    }
}
