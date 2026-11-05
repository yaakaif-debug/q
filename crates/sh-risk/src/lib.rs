//! Риск (SPEC.md, «Risk management»): проверки перед входом и перед каждым ордером,
//! непрерывные мониторы и kill switch. Крейт ничего не отправляет на площадки:
//! он возвращает вето и действия, которые исполняет движок.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sh_core::{Collateral, Exchange, Px, Usd, VenueKey};
use sh_signals::{PairKind, RiskLimits, pct};
use sh_venue_api::OrderRequest;

/// Уровень оповещения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertLevel {
    /// Информация.
    Info,
    /// Предупреждение.
    Warn,
    /// Критично.
    Crit,
}

/// Причина вето.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum Veto {
    /// Kill switch включён.
    #[error("kill switch engaged")]
    KillSwitch,
    /// Движок остановлен (Halted).
    #[error("engine halted: {0}")]
    Halted(String),
    /// Торговля на площадке выключена конфигурацией.
    #[error("trading disabled on {0}")]
    TradingDisabled(Exchange),
    /// Площадка деградировала (устаревшие данные, сбои REST).
    #[error("venue degraded: {0}")]
    VenueDegraded(String),
    /// Превышена доля платформы.
    #[error("platform share exceeded on {0}")]
    PlatformShare(Exchange),
    /// Превышена доля S2.
    #[error("S2 share exceeded")]
    S2Share,
    /// Превышена доля пары.
    #[error("pair share exceeded")]
    PairShare,
    /// Плечо ноги выше предела.
    #[error("leverage above cap on {0}")]
    Leverage(Exchange),
    /// Дневной лимит убытка.
    #[error("daily loss limit: entries blocked")]
    DailyLoss,
    /// Просадка от пика.
    #[error("max drawdown reached")]
    Drawdown,
    /// Стейблкоин отклонился от USDC.
    #[error("collateral depeg: {0:?}")]
    Depeg(Collateral),
    /// Превышен непохеджированный номинал.
    #[error("unhedged notional above cap")]
    Unhedged,
    /// Сущность уже в другой паре.
    #[error("instrument already used by another pair")]
    InstrumentBusy,
    /// Достигнут лимит числа пар (live-ступень).
    #[error("max pairs reached")]
    MaxPairs,
    /// Превышен капитал live-ступени.
    #[error("capital cap reached")]
    CapitalCap,
}

/// Нога позиции для мониторов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegExposure {
    /// Площадка.
    pub venue: VenueKey,
    /// Символ.
    pub symbol: String,
    /// Количество со знаком.
    pub qty: Decimal,
    /// Маркировка.
    pub mark: Option<Px>,
    /// Цена ликвидации (от площадки).
    pub liq_px: Option<Px>,
    /// Средний дневной диапазон за 30 дней, доля (если известен).
    pub adr_frac: Option<Decimal>,
}

impl LegExposure {
    /// Номинал по маркировке.
    pub fn notional(&self) -> Decimal {
        self.mark.map(|m| self.qty.abs() * m.0).unwrap_or_default()
    }

    /// Дельта в $ со знаком.
    pub fn delta_usd(&self) -> Decimal {
        self.mark.map(|m| self.qty * m.0).unwrap_or_default()
    }
}

/// Пара в книге.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairExposure {
    /// Идентификатор пары.
    pub pair_id: String,
    /// Вид.
    pub kind: PairKind,
    /// Капитал пары (маржа обеих ног с резервом).
    pub capital: Usd,
    /// Ноги.
    pub legs: Vec<LegExposure>,
}

/// Снимок книги для риска.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookView {
    /// Капитал всех площадок.
    pub equity: Usd,
    /// Пик капитала.
    pub peak_equity: Usd,
    /// Капитал на начало суток UTC.
    pub day_start_equity: Usd,
    /// Маржа (капитал пар) на платформе.
    pub platform_capital: BTreeMap<Exchange, Usd>,
    /// Пары.
    pub pairs: Vec<PairExposure>,
    /// Непохеджированный номинал в задачах исполнения.
    pub unhedged_usd: Usd,
    /// Ноги вне пар (остатки, ADL) — учитываются в чистой дельте.
    pub orphan_legs: Vec<LegExposure>,
}

impl BookView {
    /// Чистая дельта книги в $.
    pub fn net_delta_usd(&self) -> Decimal {
        self.pairs.iter().flat_map(|p| p.legs.iter()).chain(self.orphan_legs.iter()).map(LegExposure::delta_usd).sum()
    }

    /// Капитал в S2.
    pub fn s2_capital(&self) -> Decimal {
        self.pairs.iter().filter(|p| p.kind == PairKind::S2).map(|p| p.capital.0).sum()
    }

    /// Капитал во всех парах.
    pub fn used_capital(&self) -> Decimal {
        self.pairs.iter().map(|p| p.capital.0).sum()
    }
}

/// Намерение войти в пару.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryIntent {
    /// Пара.
    pub pair_id: String,
    /// Вид.
    pub kind: PairKind,
    /// Капитал пары.
    pub capital: Usd,
    /// Ноги: площадка, символ, плечо, маржа ноги, обеспечение.
    pub legs: [(VenueKey, String, Decimal, Usd, Collateral); 2],
}

/// Ограничения live-ступени (M3/M4): капитал и число пар.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageCaps {
    /// Максимальный капитал в парах, $.
    pub max_capital_usd: Option<Decimal>,
    /// Максимальное число пар.
    pub max_pairs: Option<usize>,
}

/// Действие монитора.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskAction {
    /// Оповещение.
    Alert {
        /// Уровень.
        level: AlertLevel,
        /// Ключ дедупликации.
        key: String,
        /// Текст.
        text: String,
    },
    /// Отменить все ордера.
    CancelAll,
    /// Все позиции — в выход.
    ExitAll(String),
    /// Сократить пару (расстояние до ликвидации).
    ReducePair {
        /// Пара.
        pair_id: String,
        /// Причина.
        reason: String,
    },
    /// Срочно выровнять чистую дельту.
    HedgeDelta(Decimal),
}

/// Состояние kill switch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSwitch {
    /// Включён.
    pub engaged: bool,
    /// Источник и причина.
    pub reason: Option<String>,
    /// Время включения, мс.
    pub at_ms: Option<i64>,
}

/// Состояние риска.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskState {
    /// Лимиты (из конфигурации, не сохраняются).
    #[serde(skip)]
    pub limits: RiskLimits,
    /// Kill switch.
    pub kill: KillSwitch,
    /// Остановка (Halted) с причиной; снимается только оператором.
    pub halted: Option<String>,
    /// Входы запрещены до, мс.
    pub entries_blocked_until_ms: Option<i64>,
    /// Деградировавшие площадки.
    pub degraded: BTreeMap<String, String>,
    /// Отвязавшиеся стейблкоины.
    pub depegged: BTreeSet<String>,
    /// Торговля разрешена на семействе (`trading_enabled`).
    pub trading_enabled: BTreeMap<Exchange, bool>,
    /// Все позиции в выходе по просадке.
    pub exiting_all: bool,
    /// Ограничения ступени.
    pub stage: StageCaps,
}

fn collateral_name(c: &Collateral) -> String {
    match c {
        Collateral::Usdc => "USDC".into(),
        Collateral::Usdt => "USDT".into(),
        Collateral::Usdh => "USDH".into(),
        Collateral::Usde => "USDE".into(),
        Collateral::Other(s) => s.to_ascii_uppercase(),
    }
}

impl RiskState {
    /// Новое состояние; торговля выключена на всех площадках, пока не разрешена явно.
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            kill: KillSwitch::default(),
            halted: None,
            entries_blocked_until_ms: None,
            degraded: BTreeMap::new(),
            depegged: BTreeSet::new(),
            trading_enabled: Exchange::ALL.iter().map(|e| (*e, false)).collect(),
            exiting_all: false,
            stage: StageCaps::default(),
        }
    }

    /// Включает kill switch.
    pub fn engage_kill(&mut self, reason: impl Into<String>, now_ms: i64) -> Vec<RiskAction> {
        let reason = reason.into();
        let first = !self.kill.engaged;
        self.kill = KillSwitch { engaged: true, reason: Some(reason.clone()), at_ms: Some(now_ms) };
        let mut out = vec![RiskAction::CancelAll];
        if first {
            out.push(RiskAction::Alert {
                level: AlertLevel::Crit,
                key: "kill".into(),
                text: format!("KILL SWITCH engaged: {reason}"),
            });
        }
        out
    }

    /// Снимает kill switch (только оператор).
    pub fn release_kill(&mut self) {
        self.kill = KillSwitch::default();
    }

    /// Останавливает движок (Halted).
    pub fn halt(&mut self, reason: impl Into<String>) -> Vec<RiskAction> {
        let reason = reason.into();
        let first = self.halted.is_none();
        self.halted = Some(reason.clone());
        let mut out = vec![RiskAction::CancelAll];
        if first {
            out.push(RiskAction::Alert {
                level: AlertLevel::Crit,
                key: "halted".into(),
                text: format!("HALTED: {reason}"),
            });
        }
        out
    }

    /// Снимает Halted (только оператор).
    pub fn resume(&mut self) {
        self.halted = None;
    }

    fn venue_tag(v: &VenueKey) -> String {
        v.to_string()
    }

    fn common_gate(&self) -> Result<(), Veto> {
        if self.kill.engaged {
            return Err(Veto::KillSwitch);
        }
        if let Some(h) = &self.halted {
            return Err(Veto::Halted(h.clone()));
        }
        Ok(())
    }

    /// Проверка нового входа (SPEC.md: доли платформ, S2, пары; плечо; дневной убыток;
    /// просадка; деградация; отвязка обеспечения; ступень капитала).
    pub fn check_entry(&self, intent: &EntryIntent, book: &BookView, now_ms: i64) -> Result<(), Veto> {
        self.common_gate()?;
        if self.exiting_all {
            return Err(Veto::Drawdown);
        }
        if self.entries_blocked_until_ms.is_some_and(|t| now_ms < t) {
            return Err(Veto::DailyLoss);
        }
        let equity = book.equity.0;
        for (venue, symbol, lev, _, coll) in &intent.legs {
            if !self.trading_enabled.get(&venue.exchange).copied().unwrap_or(false) {
                return Err(Veto::TradingDisabled(venue.exchange));
            }
            if let Some(d) = self.degraded.get(&Self::venue_tag(venue)) {
                return Err(Veto::VenueDegraded(d.clone()));
            }
            if *lev > self.limits.leverage_cap(venue.exchange) {
                return Err(Veto::Leverage(venue.exchange));
            }
            if self.depegged.contains(&collateral_name(coll)) {
                return Err(Veto::Depeg(coll.clone()));
            }
            let busy = book.pairs.iter().any(|p| p.legs.iter().any(|l| l.venue == *venue && l.symbol == *symbol));
            if busy {
                return Err(Veto::InstrumentBusy);
            }
        }
        if intent.capital.0 > equity * pct(self.limits.max_pair_share_pct) {
            return Err(Veto::PairShare);
        }
        if intent.kind == PairKind::S2
            && book.s2_capital() + intent.capital.0 > equity * pct(self.limits.max_s2_share_pct)
        {
            return Err(Veto::S2Share);
        }
        let mut add: BTreeMap<Exchange, Decimal> = BTreeMap::new();
        for (venue, _, _, margin, _) in &intent.legs {
            *add.entry(venue.exchange).or_default() += margin.0;
        }
        for (ex, m) in add {
            let used = book.platform_capital.get(&ex).map(|u| u.0).unwrap_or_default();
            if used + m > equity * self.limits.platform_share(ex) {
                return Err(Veto::PlatformShare(ex));
            }
        }
        if let Some(n) = self.stage.max_pairs
            && book.pairs.len() >= n
        {
            return Err(Veto::MaxPairs);
        }
        if let Some(c) = self.stage.max_capital_usd
            && book.used_capital() + intent.capital.0 > c
        {
            return Err(Veto::CapitalCap);
        }
        Ok(())
    }

    /// Проверка отдельного ордера. Ордера, сокращающие позицию, и хеджи проходят и при
    /// kill switch выключенной торговле только если `reduce_only`; новые дочерние — только
    /// при непохеджированном номинале ниже предела.
    pub fn check_order(&self, req: &OrderRequest, is_new_child: bool, book: &BookView) -> Result<(), Veto> {
        if !self.trading_enabled.get(&req.venue.exchange).copied().unwrap_or(false) {
            return Err(Veto::TradingDisabled(req.venue.exchange));
        }
        if req.reduce_only {
            // сокращение позиции разрешено всегда: это путь к безопасности
            return Ok(());
        }
        self.common_gate()?;
        if let Some(d) = self.degraded.get(&Self::venue_tag(&req.venue)) {
            return Err(Veto::VenueDegraded(d.clone()));
        }
        if is_new_child && book.unhedged_usd.0 >= self.limits.max_unhedged_usd {
            return Err(Veto::Unhedged);
        }
        Ok(())
    }

    /// Можно ли ставить новые дочерние ордера.
    pub fn allow_new_child(&self, book: &BookView) -> bool {
        self.common_gate().is_ok() && book.unhedged_usd.0 < self.limits.max_unhedged_usd
    }

    /// Обновляет признак деградации площадки (устаревшие данные или сбои REST).
    pub fn set_degraded(&mut self, venue: &VenueKey, reason: Option<String>, has_positions: bool) -> Vec<RiskAction> {
        let tag = Self::venue_tag(venue);
        match reason {
            Some(r) => {
                if self.degraded.insert(tag.clone(), r.clone()).is_none() {
                    let level = if has_positions { AlertLevel::Crit } else { AlertLevel::Warn };
                    return vec![RiskAction::Alert {
                        level,
                        key: format!("degraded:{tag}"),
                        text: format!("venue {tag} degraded: {r}"),
                    }];
                }
            }
            None => {
                if self.degraded.remove(&tag).is_some() {
                    return vec![RiskAction::Alert {
                        level: AlertLevel::Info,
                        key: format!("recovered:{tag}"),
                        text: format!("venue {tag} recovered"),
                    }];
                }
            }
        }
        Vec::new()
    }

    /// Курс обеспечения к USDC: отвязка больше `max_stable_depeg_pct` запрещает входы.
    pub fn on_collateral_rate(&mut self, c: &Collateral, rate_to_usdc: Decimal) -> Vec<RiskAction> {
        let name = collateral_name(c);
        let dev = (rate_to_usdc - Decimal::ONE).abs();
        if dev > pct(self.limits.max_stable_depeg_pct) {
            if self.depegged.insert(name.clone()) {
                return vec![RiskAction::Alert {
                    level: AlertLevel::Warn,
                    key: format!("depeg:{name}"),
                    text: format!("{name} deviates from USDC: rate {rate_to_usdc}"),
                }];
            }
        } else {
            self.depegged.remove(&name);
        }
        Vec::new()
    }

    /// Непрерывные мониторы: непохеджированный номинал, чистая дельта, дневной убыток,
    /// просадка, расстояние до ликвидации.
    pub fn monitor(&mut self, book: &BookView, now_ms: i64) -> Vec<RiskAction> {
        let mut out = Vec::new();
        let l = &self.limits;
        if book.unhedged_usd.0 >= l.max_unhedged_usd {
            out.push(RiskAction::Alert {
                level: AlertLevel::Warn,
                key: "unhedged".into(),
                text: format!("unhedged notional {} above cap {}", book.unhedged_usd.0.round_dp(0), l.max_unhedged_usd),
            });
        }
        let delta = book.net_delta_usd();
        // непохеджированное в задачах само уйдёт в хедж; дельта сверх него — избыточна
        if delta.abs() > l.max_net_delta_usd + book.unhedged_usd.0 {
            out.push(RiskAction::HedgeDelta(delta));
            out.push(RiskAction::Alert {
                level: AlertLevel::Crit,
                key: "net_delta".into(),
                text: format!("net delta {} above cap {}", delta.round_dp(0), l.max_net_delta_usd),
            });
        }
        let equity = book.equity.0;
        if book.day_start_equity.0 > Decimal::ZERO
            && book.day_start_equity.0 - equity > book.day_start_equity.0 * pct(l.daily_loss_limit_pct)
            && self.entries_blocked_until_ms.is_none_or(|t| now_ms >= t)
        {
            self.entries_blocked_until_ms = Some(now_ms + 86_400_000);
            out.push(RiskAction::Alert {
                level: AlertLevel::Crit,
                key: "daily_loss".into(),
                text: "daily loss limit hit: no new entries for 24h".into(),
            });
        }
        if book.peak_equity.0 > Decimal::ZERO
            && book.peak_equity.0 - equity > book.peak_equity.0 * pct(l.max_drawdown_pct)
            && !self.exiting_all
        {
            self.exiting_all = true;
            out.push(RiskAction::ExitAll("max drawdown".into()));
            out.push(RiskAction::Alert {
                level: AlertLevel::Crit,
                key: "drawdown".into(),
                text: "max drawdown reached: all positions exiting".into(),
            });
        }
        for p in &book.pairs {
            for leg in &p.legs {
                let (Some(mark), Some(liq)) = (leg.mark, leg.liq_px) else { continue };
                if mark.0 <= Decimal::ZERO {
                    continue;
                }
                let dist = (mark.0 - liq.0).abs() / mark.0;
                let need = pct(l.min_liq_distance_pct).max(leg.adr_frac.unwrap_or_default() * l.liq_distance_adr_mult);
                if dist < need {
                    out.push(RiskAction::Alert {
                        level: AlertLevel::Crit,
                        key: format!("liq:{}:{}", p.pair_id, leg.venue),
                        text: format!(
                            "{} leg {} {} liquidation distance {}% below {}%",
                            p.pair_id,
                            leg.venue,
                            leg.symbol,
                            (dist * Decimal::from(100)).round_dp(1),
                            (need * Decimal::from(100)).round_dp(1)
                        ),
                    });
                    // ниже половины порога — сокращаем пару
                    if dist < need / Decimal::from(2) {
                        out.push(RiskAction::ReducePair {
                            pair_id: p.pair_id.clone(),
                            reason: "liquidation distance".into(),
                        });
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn venue(e: Exchange) -> VenueKey {
        VenueKey::new(e)
    }

    fn intent(capital: Decimal, kind: PairKind) -> EntryIntent {
        EntryIntent {
            pair_id: "ETH:x".into(),
            kind,
            capital: Usd(capital),
            legs: [
                (venue(Exchange::Hyperliquid), "ETH".into(), dec!(3), Usd(capital / dec!(2)), Collateral::Usdc),
                (venue(Exchange::Lighter), "ETH".into(), dec!(2), Usd(capital / dec!(2)), Collateral::Usdc),
            ],
        }
    }

    fn enabled() -> RiskState {
        let mut r = RiskState::new(RiskLimits::default());
        for e in Exchange::ALL {
            r.trading_enabled.insert(e, true);
        }
        r
    }

    fn book(equity: Decimal) -> BookView {
        BookView { equity: Usd(equity), peak_equity: Usd(equity), day_start_equity: Usd(equity), ..BookView::default() }
    }

    #[test]
    fn trading_disabled_by_default() {
        let r = RiskState::new(RiskLimits::default());
        assert_eq!(
            r.check_entry(&intent(dec!(1000), PairKind::S1), &book(dec!(100000)), 0),
            Err(Veto::TradingDisabled(Exchange::Hyperliquid))
        );
    }

    #[test]
    fn shares_and_leverage() {
        let r = enabled();
        let b = book(dec!(100000));
        assert!(r.check_entry(&intent(dec!(20000), PairKind::S1), &b, 0).is_ok());
        assert_eq!(r.check_entry(&intent(dec!(30000), PairKind::S1), &b, 0), Err(Veto::PairShare));
        let mut i = intent(dec!(10000), PairKind::S1);
        i.legs[1].2 = dec!(2.5);
        assert_eq!(r.check_entry(&i, &b, 0), Err(Veto::Leverage(Exchange::Lighter)));
        let mut b2 = b.clone();
        b2.platform_capital.insert(Exchange::Lighter, Usd(dec!(36000)));
        assert_eq!(
            r.check_entry(&intent(dec!(10000), PairKind::S1), &b2, 0),
            Err(Veto::PlatformShare(Exchange::Lighter))
        );
    }

    #[test]
    fn kill_blocks_new_but_not_reduce_only() {
        let mut r = enabled();
        let acts = r.engage_kill("test", 1);
        assert!(acts.contains(&RiskAction::CancelAll));
        let b = book(dec!(100000));
        let mut req = OrderRequest {
            cid: sh_venue_api::ClientId(1),
            venue: venue(Exchange::Lighter),
            symbol: "ETH".into(),
            side: sh_core::Side::Buy,
            qty: sh_core::Qty(dec!(1)),
            px: Px(dec!(100)),
            tif: sh_venue_api::Tif::Ioc,
            reduce_only: false,
        };
        assert_eq!(r.check_order(&req, false, &b), Err(Veto::KillSwitch));
        req.reduce_only = true;
        assert!(r.check_order(&req, false, &b).is_ok());
    }

    #[test]
    fn daily_loss_and_drawdown() {
        let mut r = enabled();
        let mut b = book(dec!(98900));
        b.day_start_equity = Usd(dec!(100000));
        b.peak_equity = Usd(dec!(100000));
        let acts = r.monitor(&b, 0);
        assert!(acts.iter().any(|a| matches!(a, RiskAction::Alert { key, .. } if key == "daily_loss")));
        assert_eq!(r.check_entry(&intent(dec!(1000), PairKind::S1), &b, 10), Err(Veto::DailyLoss));
        assert!(r.check_entry(&intent(dec!(1000), PairKind::S1), &b, 86_400_001).is_ok());
        b.equity = Usd(dec!(96900));
        assert!(r.monitor(&b, 1).iter().any(|a| matches!(a, RiskAction::ExitAll(_))));
        // повторно не срабатывает
        assert!(!r.monitor(&b, 2).iter().any(|a| matches!(a, RiskAction::ExitAll(_))));
    }

    #[test]
    fn liquidation_distance_and_delta() {
        let mut r = enabled();
        let mut b = book(dec!(100000));
        b.pairs.push(PairExposure {
            pair_id: "ETH:x".into(),
            kind: PairKind::S1,
            capital: Usd(dec!(10000)),
            legs: vec![
                LegExposure {
                    venue: venue(Exchange::Hyperliquid),
                    symbol: "ETH".into(),
                    qty: dec!(-10),
                    mark: Some(Px(dec!(100))),
                    liq_px: Some(Px(dec!(110))),
                    adr_frac: None,
                },
                LegExposure {
                    venue: venue(Exchange::Lighter),
                    symbol: "ETH".into(),
                    qty: dec!(30),
                    mark: Some(Px(dec!(100))),
                    liq_px: None,
                    adr_frac: None,
                },
            ],
        });
        let acts = r.monitor(&b, 0);
        assert!(acts.iter().any(|a| matches!(a, RiskAction::ReducePair { .. })));
        assert!(acts.contains(&RiskAction::HedgeDelta(dec!(2000))));
    }

    #[test]
    fn depeg_blocks_collateral() {
        let mut r = enabled();
        assert_eq!(r.on_collateral_rate(&Collateral::Usdt, dec!(0.995)).len(), 1);
        let mut i = intent(dec!(1000), PairKind::S1);
        i.legs[0].4 = Collateral::Usdt;
        assert_eq!(r.check_entry(&i, &book(dec!(100000)), 0), Err(Veto::Depeg(Collateral::Usdt)));
        r.on_collateral_rate(&Collateral::Usdt, dec!(0.999));
        assert!(r.check_entry(&i, &book(dec!(100000)), 0).is_ok());
    }
}
