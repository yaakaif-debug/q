//! Конфигурация процесса: `config/default.toml` + наложение окружения (`config/live.toml`)
//! + переменные `SH_...`. Неизвестные ключи — ошибка.

use std::path::Path;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use rust_decimal::Decimal;
use rust_decimal::dec;
use serde::Deserialize;
use sh_core::SecretsSource;
use sh_obs::{LogConfig, MetricsConfig};
use sh_signals::{CostParams, FeesConfig, RiskLimits, ScannerParams, StrategyParams};
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
    /// Сканер и теневой портфель.
    pub scanner: ScannerParams,
    /// Стратегии.
    pub strategy: StrategyParams,
    /// Модель издержек.
    pub costs: CostParams,
    /// Комиссии уровня аккаунта.
    pub fees: FeesConfig,
    /// Лимиты риска.
    pub risk: RiskLimits,
    /// Сопоставление символов.
    pub symbols: SymbolsConfig,
    /// База состояния.
    pub database: DatabaseConfig,
    /// Секреты.
    pub secrets: SecretsConfig,
    /// Telegram.
    pub telegram: TelegramConfig,
    /// Симуляция paper и backtest.
    pub paper: PaperConfig,
    /// Управление (сокет spread-ctl, файл kill switch).
    pub control: ControlConfig,
    /// Приватное исполнение (адаптеры, подписанты).
    pub execution: ExecutionConfig,
    /// Ступень капитала live (M3: $5k и 2 пары).
    pub stage: StageConfig,
    /// Планировщик перебалансировки (M4).
    pub rebalance: RebalanceConfig,
}

/// Настройки `[rebalance]`: только публичные адреса, ключей нет.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RebalanceConfig {
    /// Адреса депозита по площадкам (`hyperliquid`, `lighter`, `aster`).
    pub deposit_addresses: std::collections::BTreeMap<String, String>,
    /// Адрес основного аккаунта Hyperliquid (получатель `agentSendAsset`).
    pub hyperliquid_account: String,
    /// Минимальный перевод, $.
    pub min_transfer_usd: Decimal,
    /// Запас сверх нужной маржи на источнике, %.
    pub keep_buffer_pct: Decimal,
}

impl Default for RebalanceConfig {
    fn default() -> Self {
        Self {
            deposit_addresses: std::collections::BTreeMap::new(),
            hyperliquid_account: String::new(),
            min_transfer_usd: dec!(100),
            keep_buffer_pct: dec!(10),
        }
    }
}

/// Лестница капитала live (SPEC.md, M3–M4): $5k → $25k → $50k → $100k. Следующую ступень
/// оператор включает сам после 14 чистых дней (`spread-ctl status` → `clean_days`).
pub const STAGE_LADDER_USD: [i64; 4] = [5000, 25_000, 50_000, 100_000];

/// Настройки `[stage]`: ограничения капитала и числа пар в live.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct StageConfig {
    /// Капитал в парах, $.
    pub max_capital_usd: Decimal,
    /// Число пар.
    pub max_pairs: usize,
}

impl Default for StageConfig {
    fn default() -> Self {
        Self { max_capital_usd: dec!(5000), max_pairs: 2 }
    }
}

/// Настройки `[execution]` приватных адаптеров.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ExecutionConfig {
    /// Библиотека подписанта Lighter (`linux-amd64.so` на сервере, `darwin-arm64.dylib` локально).
    pub lighter_signer_lib: String,
    /// Дексы Hyperliquid для позиций и балансов (`""` — ядро).
    pub hyperliquid_dexes: Vec<String>,
    /// Токен USDC для `agentSendAsset` (пусто — по сети).
    pub hyperliquid_usdc_token: String,
    /// REST Aster v3 для подписанных запросов (пусто — по сети).
    pub aster_rest: String,
    /// WebSocket Aster для пользовательского потока (пусто — по сети).
    pub aster_ws: String,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            lighter_signer_lib: "signers/lighter-signer-linux-amd64.so".into(),
            hyperliquid_dexes: vec![String::new()],
            hyperliquid_usdc_token: String::new(),
            aster_rest: String::new(),
            aster_ws: String::new(),
        }
    }
}

/// Настройки `[paper]`: симулированные площадки для paper и backtest.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PaperConfig {
    /// Начальный капитал по семействам площадок, $ (для Hyperliquid — на ядре; HIP-3
    /// дексы в симуляции берут маржу из общего капитала, переводы не моделируются).
    pub cash_usd: std::collections::BTreeMap<String, Decimal>,
    /// RTT до площадок, мс (из отчёта latency-probe оператора).
    pub rtt_ms: std::collections::BTreeMap<String, i64>,
    /// Период такта движка, мс.
    pub tick_ms: i64,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            cash_usd: std::collections::BTreeMap::from([
                ("hyperliquid".to_owned(), dec!(40000)),
                ("lighter".to_owned(), dec!(30000)),
                ("aster".to_owned(), dec!(30000)),
            ]),
            rtt_ms: std::collections::BTreeMap::from([
                ("hyperliquid".to_owned(), 10),
                ("lighter".to_owned(), 10),
                ("aster".to_owned(), 10),
            ]),
            tick_ms: 1000,
        }
    }
}

/// Настройки `[control]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct ControlConfig {
    /// Unix-сокет `spread-ctl`.
    pub socket: String,
    /// Файл kill switch: его появление включает kill switch (содержимое `flatten` — со сбросом).
    pub kill_file: String,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self { socket: "run/spread-hunter.sock".into(), kill_file: "run/KILL".into() }
    }
}

/// Настройки `[symbols]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SymbolsConfig {
    /// Файл сопоставления.
    pub path: String,
}

impl Default for SymbolsConfig {
    fn default() -> Self {
        Self { path: "config/symbols.toml".into() }
    }
}

/// Настройки `[database]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct DatabaseConfig {
    /// Файл SQLite.
    pub path: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self { path: "data/state/state.sqlite".into() }
    }
}

/// Настройки `[secrets]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SecretsConfig {
    /// `none`, `credential:<name>` (systemd LoadCredentialEncrypted), `command:<cmd>` (например,
    /// `age -d -i /root/key.txt /opt/spread-hunter/secrets.toml.age`) или `file:<path>` (права 600, только тест).
    pub source: String,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self { source: "none".into() }
    }
}

impl SecretsConfig {
    /// Разобранный источник.
    pub fn source(&self) -> Result<SecretsSource, String> {
        SecretsSource::parse(&self.source).map_err(|e| e.to_string())
    }
}

/// Настройки `[telegram]`; токен и чат — в секретах.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TelegramConfig {
    /// Включить оповещения.
    pub enabled: bool,
    /// Подавление одинаковых оповещений, с.
    pub dedupe_secs: u64,
    /// Не больше сообщений в минуту (кроме CRIT).
    pub per_minute: usize,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self { enabled: false, dedupe_secs: 300, per_minute: 20 }
    }
}

/// Настройки `[engine]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct EngineConfig {
    /// Режим: `record`, `scan` (с M1), `paper` (с M2, по умолчанию), `live` (с M3).
    pub mode: String,
    /// Резерв капитала вне площадок, %.
    pub equity_reserve_pct: Decimal,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { mode: "paper".into(), equity_reserve_pct: dec!(15) }
    }
}

/// Режимы, доступные в текущем этапе.
pub const AVAILABLE_MODES: &[&str] = &["record", "scan", "paper", "live"];

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
        if !AVAILABLE_MODES.contains(&self.engine.mode.as_str()) {
            return Err(format!(
                "engine.mode '{}' is not available in this build (available: {})",
                self.engine.mode,
                AVAILABLE_MODES.join(", ")
            ));
        }
        if self.engine.mode == "record" && !self.recorder.enabled {
            return Err("engine.mode = \"record\" requires recorder.enabled = true".into());
        }
        if self.engine.equity_reserve_pct < Decimal::ZERO || self.engine.equity_reserve_pct >= dec!(100) {
            return Err("engine.equity_reserve_pct must be in [0, 100)".into());
        }
        self.risk.validate()?;
        self.fees.validate()?;
        self.secrets.source()?;
        self.venues.hyperliquid.validate()?;
        self.venues.lighter.validate()?;
        self.venues.aster.validate()?;
        self.network.proxy()?;
        for k in self.paper.cash_usd.keys().chain(self.paper.rtt_ms.keys()) {
            if k.parse::<sh_core::Exchange>().is_err() {
                return Err(format!("paper: unknown venue '{k}'"));
            }
        }
        if !STAGE_LADDER_USD.iter().any(|v| Decimal::from(*v) == self.stage.max_capital_usd)
            || self.stage.max_pairs == 0
        {
            return Err(format!(
                "stage.max_capital_usd must be one of {STAGE_LADDER_USD:?} and stage.max_pairs positive"
            ));
        }
        if self.paper.tick_ms <= 0 {
            return Err("paper.tick_ms must be positive".into());
        }
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
        assert!(AVAILABLE_MODES.contains(&cfg.engine.mode.as_str()));
        assert_eq!(cfg.venues.hyperliquid.dexes, "auto");
    }

    #[test]
    fn live_overlay_is_valid_and_trading_stays_off() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/default.toml");
        let live = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/live.toml");
        let cfg = AppConfig::load(&base, Some(&live)).unwrap();
        assert_eq!(cfg.engine.mode, "live");
        assert_eq!(cfg.stage.max_pairs, 2);
        assert!(!cfg.venues.hyperliquid.trading_enabled && !cfg.venues.lighter.trading_enabled);
        assert!(!cfg.venues.aster.trading_enabled);
        assert_ne!(cfg.database.path, AppConfig::load(&base, None).unwrap().database.path);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(parse("[engine]\nmode = \"record\"\nfoo = 1\n").is_err());
        assert!(parse("[venues.hyperliquid]\nmax_subs = 5\n").is_err());
        assert!(parse("[strategy.s3_spot_perp]\nmin_net_apr = 12.0\n").is_err());
        assert!(parse("[strategy.s1_funding_carry]\nmin_net_apr = 12.0\n").is_ok());
    }

    #[test]
    fn non_record_modes_and_trading_are_rejected() {
        assert!(parse("[engine]\nmode = \"live\"\n").is_ok());
        assert!(parse("[engine]\nmode = \"margin\"\n").is_err());
        assert!(parse("[engine]\nmode = \"record\"\n[recorder]\nenabled = false\n").is_err());
        assert!(parse("[secrets]\nsource = \"vault:x\"\n").is_err());
        assert!(parse("[venues.aster]\ntrading_enabled = true\n").is_ok());
        assert!(parse("[stage]\nmax_capital_usd = 25000\nmax_pairs = 4\n").is_ok());
        assert!(parse("[stage]\nmax_capital_usd = 30000\n").is_err());
        assert!(parse("[venues.lighter]\ntrading_enabled = true\n").is_ok());
        assert!(parse("[network]\nproxy = \"socks5://x:1\"\n").is_err());
        assert!(parse("").is_ok());
    }
}
