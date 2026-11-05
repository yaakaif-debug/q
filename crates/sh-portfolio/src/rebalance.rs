//! Планировщик перебалансировки (M4): по балансам площадок и марже, нужной открытым
//! ногам, предлагает переводы обеспечения. Ничего не подписывает и не отправляет: на
//! выходе — неподписанные данные для оператора (SPEC.md: сервер держит только ключи,
//! которые торгуют, но не выводят средства).
//!
//! Правила:
//! - переводы внутри Hyperliquid между дексами (ядро и HIP-3) — дешёвый маршрут, данные
//!   действия `agentSendAsset` (его может подписать и агент; оператор решает, кто подписывает);
//! - межплатформенные — вывод с источника и депозит на адрес получателя;
//! - в пути одновременно не больше `max_in_transit_pct` капитала (учитывая уже идущие);
//! - вывод с Lighter не используется для срочной маржи (выводы Lighter задерживались на часы);
//! - переводы меньше `min_transfer_usd` не предлагаются.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sh_core::{Exchange, Usd, VenueKey};

/// Площадка для планировщика.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueFunds {
    /// Площадка (декс для Hyperliquid).
    pub venue: VenueKey,
    /// Капитал.
    pub equity: Usd,
    /// Нужная марже открытых ног с резервом.
    pub required: Usd,
    /// Срочно: маржа ниже порога перебалансировки.
    pub urgent: bool,
}

/// Параметры.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanParams {
    /// Капитал всей книги.
    pub total_equity: Usd,
    /// Уже в пути.
    pub in_transit: Usd,
    /// Лимит в пути, % капитала.
    pub max_in_transit_pct: Decimal,
    /// Минимальный перевод, $.
    pub min_transfer_usd: Decimal,
    /// Запас сверх нужной маржи на площадке-источнике, доля.
    pub keep_buffer_frac: Decimal,
    /// Адреса депозита по площадкам (не секрет, задаёт оператор).
    pub deposit_addresses: BTreeMap<String, String>,
    /// Адрес основного аккаунта Hyperliquid (получатель `agentSendAsset`).
    pub hl_account: String,
    /// Токен USDC Hyperliquid (`USDC:0x…`).
    pub hl_usdc_token: String,
}

/// Маршрут перевода.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Между дексами Hyperliquid.
    HyperliquidInternal,
    /// Вывод и депозит между платформами.
    CrossPlatform,
}

/// Предлагаемый перевод.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferPlan {
    /// Откуда.
    pub from: String,
    /// Куда.
    pub to: String,
    /// Сумма, $.
    pub amount: Usd,
    /// Маршрут.
    pub route: Route,
    /// Срочно.
    pub urgent: bool,
    /// Неподписанные данные для оператора.
    pub unsigned: serde_json::Value,
}

/// Результат планирования.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RebalancePlan {
    /// Переводы.
    pub transfers: Vec<TransferPlan>,
    /// Непокрытые дефициты (лимит в пути, нет источника).
    pub uncovered: Vec<(String, Usd)>,
    /// Пояснения.
    pub notes: Vec<String>,
}

fn amount_str(v: Decimal) -> String {
    v.round_dp(2).normalize().to_string()
}

fn unsigned_payload(from: &VenueKey, to: &VenueKey, amount: Decimal, p: &PlanParams) -> (Route, serde_json::Value) {
    if from.exchange == Exchange::Hyperliquid && to.exchange == Exchange::Hyperliquid {
        return (
            Route::HyperliquidInternal,
            json!({
                "venue": "hyperliquid",
                "action": {
                    "type": "agentSendAsset",
                    "destination": p.hl_account,
                    "sourceDex": from.dex,
                    "destinationDex": to.dex,
                    "token": p.hl_usdc_token,
                    "amount": amount_str(amount),
                    "fromSubAccount": "",
                    "nonce": "<current time in ms at signing>"
                },
                "sign_with": "agent wallet (L1 action) or master wallet",
            }),
        );
    }
    let dest = p.deposit_addresses.get(to.exchange.as_str()).cloned().unwrap_or_else(|| "<deposit address>".into());
    let withdraw = match from.exchange {
        // UNVERIFIED: домен и типы user-signed действия withdraw3 — по SDK (sign_withdraw_from_bridge_action)
        Exchange::Hyperliquid => json!({
            "venue": "hyperliquid",
            "note": if from.dex.is_empty() { "withdraw from the core USDC dex" } else { "first move funds to the core dex with agentSendAsset" },
            "typed_data": {
                "domain": {"name": "HyperliquidSignTransaction", "version": "1", "chainId": 42161, "verifyingContract": "0x0000000000000000000000000000000000000000"},
                "primaryType": "HyperliquidTransaction:Withdraw",
                "types": {"HyperliquidTransaction:Withdraw": [
                    {"name": "hyperliquidChain", "type": "string"},
                    {"name": "destination", "type": "string"},
                    {"name": "amount", "type": "string"},
                    {"name": "time", "type": "uint64"}
                ]},
                "message": {"hyperliquidChain": "Mainnet", "destination": dest, "amount": amount_str(amount), "time": "<current time in ms>"}
            },
            "sign_with": "master wallet (hardware)",
        }),
        Exchange::Lighter => json!({
            "venue": "lighter",
            "action": "withdraw",
            "asset": "USDC",
            "amount": amount_str(amount),
            "to": dest,
            "sign_with": "L1 wallet in the Lighter app (withdrawals can take hours)",
        }),
        Exchange::Aster => json!({
            "venue": "aster",
            "action": "withdraw",
            "amount": amount_str(amount),
            "to": dest,
            "sign_with": "main wallet (API wallet cannot withdraw)",
        }),
    };
    (
        Route::CrossPlatform,
        json!({"withdraw": withdraw, "deposit": {"venue": to.exchange.as_str(), "dex": to.dex, "address": dest}}),
    )
}

/// Строит план.
pub fn plan(venues: &[VenueFunds], p: &PlanParams) -> RebalancePlan {
    let mut out = RebalancePlan::default();
    let mut deficits: Vec<(VenueKey, Decimal, bool)> = venues
        .iter()
        .filter(|v| v.equity.0 < v.required.0)
        .map(|v| (v.venue.clone(), v.required.0 - v.equity.0, v.urgent))
        .collect();
    // срочные — первыми, затем по размеру
    deficits.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
    let mut surplus: BTreeMap<VenueKey, Decimal> = venues
        .iter()
        .map(|v| (v.venue.clone(), v.equity.0 - v.required.0 * (Decimal::ONE + p.keep_buffer_frac)))
        .filter(|(_, s)| *s > Decimal::ZERO)
        .collect();
    let cap = p.total_equity.0 * p.max_in_transit_pct / Decimal::from(100);
    let mut transit = p.in_transit.0;
    for (to, mut need, urgent) in deficits {
        // внутри Hyperliquid — сначала дексы той же платформы, Lighter — последним
        let mut sources: Vec<(VenueKey, Decimal)> = surplus.iter().map(|(k, v)| (k.clone(), *v)).collect();
        sources.sort_by_key(|(k, v)| {
            let same = k.exchange == to.exchange;
            let lighter = k.exchange == Exchange::Lighter;
            (!same, lighter, std::cmp::Reverse(*v))
        });
        for (from, avail) in sources {
            if need < p.min_transfer_usd {
                break;
            }
            if from == to {
                continue;
            }
            if urgent && from.exchange == Exchange::Lighter && to.exchange != Exchange::Lighter {
                out.notes.push(format!("not using a Lighter withdrawal for urgent margin on {to}"));
                continue;
            }
            let internal = from.exchange == Exchange::Hyperliquid && to.exchange == Exchange::Hyperliquid;
            let room = if internal { need } else { (cap - transit).max(Decimal::ZERO) };
            let amount = need.min(avail).min(room).round_dp(2);
            if amount < p.min_transfer_usd {
                if !internal && room < p.min_transfer_usd {
                    out.notes.push(format!("in-transit cap reached ({} of {})", transit.round_dp(0), cap.round_dp(0)));
                }
                continue;
            }
            let (route, unsigned) = unsigned_payload(&from, &to, amount, p);
            if !internal {
                transit += amount;
            }
            if let Some(s) = surplus.get_mut(&from) {
                *s -= amount;
            }
            need -= amount;
            out.transfers.push(TransferPlan {
                from: from.to_string(),
                to: to.to_string(),
                amount: Usd(amount),
                route,
                urgent,
                unsigned,
            });
        }
        if need >= p.min_transfer_usd {
            out.uncovered.push((to.to_string(), Usd(need.round_dp(2))));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn v(venue: VenueKey, equity: Decimal, required: Decimal, urgent: bool) -> VenueFunds {
        VenueFunds { venue, equity: Usd(equity), required: Usd(required), urgent }
    }

    fn params() -> PlanParams {
        PlanParams {
            total_equity: Usd(dec!(100000)),
            in_transit: Usd(Decimal::ZERO),
            max_in_transit_pct: dec!(25),
            min_transfer_usd: dec!(100),
            keep_buffer_frac: dec!(0.1),
            deposit_addresses: BTreeMap::from([("aster".to_owned(), "0xaster".to_owned())]),
            hl_account: "0xme".into(),
            hl_usdc_token: "USDC:0x6d1e7cde53ba9467b783cb7c530ce054".into(),
        }
    }

    #[test]
    fn hip3_deficit_uses_internal_transfer_first() {
        let venues = [
            v(VenueKey::hyperliquid(""), dec!(20000), dec!(5000), false),
            v(VenueKey::hyperliquid("xyz"), dec!(1000), dec!(4000), true),
            v(VenueKey::new(Exchange::Lighter), dec!(30000), dec!(1000), false),
        ];
        let pl = plan(&venues, &params());
        assert_eq!(pl.transfers.len(), 1);
        let t = &pl.transfers[0];
        assert_eq!((t.route, t.amount.0), (Route::HyperliquidInternal, dec!(3000)));
        assert_eq!(t.unsigned["action"]["type"], "agentSendAsset");
        assert_eq!(t.unsigned["action"]["destinationDex"], "xyz");
    }

    #[test]
    fn urgent_margin_never_relies_on_lighter_and_respects_transit_cap() {
        let venues = [
            v(VenueKey::new(Exchange::Aster), dec!(1000), dec!(40000), true),
            v(VenueKey::new(Exchange::Lighter), dec!(60000), dec!(1000), false),
            v(VenueKey::hyperliquid(""), dec!(30000), dec!(5000), false),
        ];
        let pl = plan(&venues, &params());
        assert!(pl.transfers.iter().all(|t| !t.from.starts_with("lighter")));
        let total: Decimal = pl.transfers.iter().map(|t| t.amount.0).sum();
        // в пути не больше 25% капитала
        assert!(total <= dec!(25000));
        assert!(!pl.uncovered.is_empty());
        assert_eq!(pl.transfers[0].unsigned["deposit"]["address"], "0xaster");
        assert!(pl.notes.iter().any(|n| n.contains("Lighter")));
    }

    #[test]
    fn nothing_to_do_when_balanced() {
        let venues = [v(VenueKey::new(Exchange::Aster), dec!(5000), dec!(4000), false)];
        let pl = plan(&venues, &params());
        assert!(pl.transfers.is_empty() && pl.uncovered.is_empty());
    }
}
