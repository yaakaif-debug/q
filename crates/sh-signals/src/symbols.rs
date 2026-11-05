//! Сопоставление инструментов каноническим базовым активам (`config/symbols.toml`)
//! и автоматические предложения, которые оператор одобряет вручную.
//!
//! Формат:
//! ```toml
//! [[underlying]]
//! id = "NVDA"
//! members = ["hyperliquid:xyz/xyz:NVDA", "lighter/NVDA", "aster/NVDAUSDT"]
//! session = "us_equity"          # необязательно: календарь для охраны S2
//! allow_mark_divergence = false  # разрешить пары с разными маркировками
//! allow_new_listing = false      # пропустить фильтр возраста листинга
//!
//! [calendars]
//! us_equity = ["MON 13:30", "MON 20:00"]
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use figment::Figment;
use figment::providers::{Format, Toml};
use serde::Deserialize;
use sh_core::UnderlyingId;
use sh_md::{InstrumentKey, MarketState};
use thiserror::Error;

use crate::sessions::{Calendar, CalendarError};

/// Ошибка загрузки сопоставления.
#[derive(Debug, Error)]
pub enum SymbolsError {
    /// Файл не читается или не разбирается.
    #[error("symbols file: {0}")]
    Parse(String),
    /// Неверный ключ инструмента.
    #[error("bad member '{0}': expected '<venue>/<symbol>'")]
    BadMember(String),
    /// Инструмент в двух базовых активах.
    #[error("member '{0}' is mapped to both '{1}' and '{2}'")]
    Duplicate(String, String, String),
    /// Неизвестный календарь.
    #[error("underlying '{0}' refers to unknown calendar '{1}'")]
    UnknownCalendar(String, String),
    /// Ошибка календаря.
    #[error(transparent)]
    Calendar(#[from] CalendarError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnderlyingCfg {
    id: String,
    members: Vec<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    allow_mark_divergence: bool,
    #[serde(default)]
    allow_new_listing: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SymbolsFile {
    #[serde(default)]
    underlying: Vec<UnderlyingCfg>,
    #[serde(default)]
    calendars: BTreeMap<String, Vec<String>>,
}

/// Базовый актив и его инструменты.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Underlying {
    /// Идентификатор.
    pub id: UnderlyingId,
    /// Инструменты.
    pub members: Vec<InstrumentKey>,
    /// Календарь сессий.
    pub session: Option<String>,
    /// Разрешены разные маркировки (одобрено оператором).
    pub allow_mark_divergence: bool,
    /// Разрешён молодой листинг (одобрено оператором).
    pub allow_new_listing: bool,
}

/// Сопоставление.
#[derive(Debug, Clone, Default)]
pub struct SymbolMap {
    underlyings: Vec<Underlying>,
    by_key: HashMap<InstrumentKey, usize>,
    calendars: BTreeMap<String, Calendar>,
}

impl SymbolMap {
    /// Загружает файл; отсутствующий файл — пустое сопоставление.
    pub fn load(path: &Path) -> Result<Self, SymbolsError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let file: SymbolsFile =
            Figment::from(Toml::file_exact(path)).extract().map_err(|e| SymbolsError::Parse(e.to_string()))?;
        Self::build(file)
    }

    /// Разбирает строку TOML.
    pub fn from_toml_str(s: &str) -> Result<Self, SymbolsError> {
        let file: SymbolsFile =
            Figment::from(Toml::string(s)).extract().map_err(|e| SymbolsError::Parse(e.to_string()))?;
        Self::build(file)
    }

    fn build(file: SymbolsFile) -> Result<Self, SymbolsError> {
        let mut calendars = BTreeMap::new();
        for (name, specs) in &file.calendars {
            calendars.insert(name.clone(), Calendar::parse(specs)?);
        }
        let mut map = Self { calendars, ..Self::default() };
        for u in file.underlying {
            if let Some(s) = &u.session
                && !map.calendars.contains_key(s)
            {
                return Err(SymbolsError::UnknownCalendar(u.id.clone(), s.clone()));
            }
            let mut members = Vec::with_capacity(u.members.len());
            for m in &u.members {
                let key = InstrumentKey::parse(m).ok_or_else(|| SymbolsError::BadMember(m.clone()))?;
                if let Some(&i) = map.by_key.get(&key) {
                    return Err(SymbolsError::Duplicate(m.clone(), map.underlyings[i].id.0.clone(), u.id.clone()));
                }
                map.by_key.insert(key.clone(), map.underlyings.len());
                members.push(key);
            }
            map.underlyings.push(Underlying {
                id: UnderlyingId(u.id),
                members,
                session: u.session,
                allow_mark_divergence: u.allow_mark_divergence,
                allow_new_listing: u.allow_new_listing,
            });
        }
        Ok(map)
    }

    /// Базовые активы.
    pub fn underlyings(&self) -> &[Underlying] {
        &self.underlyings
    }

    /// Базовый актив инструмента.
    pub fn underlying_of(&self, key: &InstrumentKey) -> Option<&Underlying> {
        self.by_key.get(key).map(|&i| &self.underlyings[i])
    }

    /// Календарь по имени.
    pub fn calendar(&self, name: &str) -> Option<&Calendar> {
        self.calendars.get(name)
    }
}

/// Нормализованный базовый актив инструмента для автосопоставления:
/// у HIP-3 убирается префикс декса; у Aster берётся `baseAsset` из метаданных.
fn normalized_base(key: &InstrumentKey, market: &MarketState) -> Option<String> {
    let base = market.get(key)?.meta.as_ref()?.base.clone();
    let base = base.split_once(':').map(|(_, b)| b.to_owned()).unwrap_or(base);
    Some(base.to_ascii_uppercase())
}

/// Предложения сопоставления: инструменты с одинаковым нормализованным базовым активом
/// на двух и более площадках, ещё не сопоставленные. Масштабированные контракты
/// (`1000PEPE`) не совпадут с `PEPE` — это намеренно: размер контракта разный.
pub fn suggest_mappings(market: &MarketState, map: &SymbolMap) -> Vec<(String, Vec<InstrumentKey>)> {
    let mut groups: BTreeMap<String, Vec<InstrumentKey>> = BTreeMap::new();
    for (key, st) in market.iter() {
        let active = st.meta.as_ref().is_some_and(|m| m.status == sh_venue_api::InstrumentStatus::Active);
        if !active || map.underlying_of(key).is_some() {
            continue;
        }
        if let Some(b) = normalized_base(key, market) {
            groups.entry(b).or_default().push(key.clone());
        }
    }
    groups
        .into_iter()
        .filter(|(_, v)| v.iter().map(|k| k.venue.clone()).collect::<BTreeSet<_>>().len() >= 2)
        .collect()
}

/// Текст TOML с предложениями для ручного переноса в `config/symbols.toml`.
pub fn suggestions_toml(sugs: &[(String, Vec<InstrumentKey>)]) -> String {
    let mut out = String::from(
        "# Предложения автосопоставления. Проверьте, что инструменты действительно\n\
         # на один и тот же базовый актив (одинаковый размер контракта и индекс),\n\
         # и перенесите одобренные записи в config/symbols.toml.\n\n",
    );
    for (id, members) in sugs {
        let list: Vec<String> = members.iter().map(|m| format!("\"{}\"", m.id())).collect();
        out.push_str(&format!("[[underlying]]\nid = \"{id}\"\nmembers = [{}]\n\n", list.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sh_core::{Collateral, Qty, RecvTs, VenueKey};
    use sh_venue_api::{FeeInfo, FeeSource, InstrumentMeta, InstrumentStatus, MdEvent, MdEventKind, PriceRule};

    const TOML: &str = r#"
[[underlying]]
id = "NVDA"
members = ["hyperliquid:xyz/xyz:NVDA", "lighter/NVDA"]
session = "us_equity"

[[underlying]]
id = "BTC"
members = ["hyperliquid/BTC", "aster/BTCUSDT"]

[calendars]
us_equity = ["MON 13:30", "FRI 20:00"]
"#;

    #[test]
    fn loads_and_indexes() {
        let m = SymbolMap::from_toml_str(TOML).unwrap();
        assert_eq!(m.underlyings().len(), 2);
        let k = InstrumentKey::parse("lighter/NVDA").unwrap();
        assert_eq!(m.underlying_of(&k).unwrap().id.0, "NVDA");
        assert!(m.calendar("us_equity").is_some());
    }

    #[test]
    fn rejects_duplicates_and_unknown_calendars() {
        let dup = "[[underlying]]\nid='A'\nmembers=['lighter/X']\n[[underlying]]\nid='B'\nmembers=['lighter/X']\n";
        assert!(matches!(SymbolMap::from_toml_str(dup), Err(SymbolsError::Duplicate(..))));
        let cal = "[[underlying]]\nid='A'\nmembers=['lighter/X']\nsession='nope'\n";
        assert!(matches!(SymbolMap::from_toml_str(cal), Err(SymbolsError::UnknownCalendar(..))));
        assert!(matches!(SymbolMap::from_toml_str("[[underlying]]\nid='A'\nmembers=['x']\n"), Err(SymbolsError::BadMember(_))));
        assert!(SymbolMap::load(Path::new("/nonexistent/symbols.toml")).unwrap().underlyings().is_empty());
    }

    fn meta(venue: VenueKey, symbol: &str, base: &str) -> MdEvent {
        let m = InstrumentMeta {
            venue: venue.clone(),
            symbol: symbol.into(),
            venue_id: None,
            base: base.into(),
            collateral: Collateral::Usdc,
            price_rule: PriceRule::SigFigs { max_sig_figs: 5, max_decimals: 6 },
            lot_size: Qty(rust_decimal::Decimal::ONE),
            min_qty: None,
            min_notional: None,
            max_leverage: None,
            margin_mode: None,
            funding_interval_hours: None,
            fees: FeeInfo { maker: None, taker: None, source: FeeSource::AccountRequired, deployer_fee_scale: None, growth_mode: None },
            status: InstrumentStatus::Active,
            listed_at_ms: None,
        };
        MdEvent { venue, symbol: symbol.into(), recv: RecvTs { wall_ns: 0, mono_ns: 0 }, venue_ts_ms: None, kind: MdEventKind::Metadata(Box::new(m)) }
    }

    #[test]
    fn suggestions_group_by_base_across_venues() {
        let mut market = MarketState::new();
        market.apply(&meta(VenueKey::hyperliquid(""), "ETH", "ETH"));
        market.apply(&meta(VenueKey::new(sh_core::Exchange::Aster), "ETHUSDT", "ETH"));
        market.apply(&meta(VenueKey::new(sh_core::Exchange::Aster), "ETHUSD1", "ETH"));
        market.apply(&meta(VenueKey::new(sh_core::Exchange::Aster), "SOLUSDT", "SOL"));
        market.apply(&meta(VenueKey::hyperliquid("xyz"), "xyz:NVDA", "NVDA"));
        market.apply(&meta(VenueKey::new(sh_core::Exchange::Lighter), "NVDA", "NVDA"));
        let map = SymbolMap::from_toml_str(TOML).unwrap();
        let sugs = suggest_mappings(&market, &map);
        // NVDA уже сопоставлен, SOL только на одной площадке
        assert_eq!(sugs.len(), 1);
        assert_eq!(sugs[0].0, "ETH");
        assert_eq!(sugs[0].1.len(), 3);
        let text = suggestions_toml(&sugs);
        assert!(text.contains("\"hyperliquid/ETH\""));
        assert!(SymbolMap::from_toml_str(&text).is_ok());
    }
}
