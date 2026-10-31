//! Преобразование сообщений Hyperliquid в нормализованные события и метаданные.

use rust_decimal::Decimal;
use sh_core::{Collateral, Level, Qty, RecvTs, Side, VenueKey};
use sh_venue_api::{
    FeeInfo, FeeSource, FundingUpdate, InstrumentMeta, InstrumentStatus, MdEvent, MdEventKind, PriceRule, TradeEvent,
    VenueError,
};

use crate::wire::{
    AllDexsAssetCtxs, AssetMeta, Meta, PerpAssetCtx, SpotMeta, WsBbo, WsBook, WsEnvelope, WsLevel, WsTrade,
};

/// Значащих цифр в цене perp (документация «Tick and lot size»).
pub const PERP_MAX_SIG_FIGS: u32 = 5;
/// `MAX_DECIMALS` для perp (документация «Tick and lot size»).
pub const PERP_MAX_DECIMALS: u32 = 6;

/// Разобранное сообщение WebSocket.
#[derive(Debug)]
pub enum HlMsg {
    /// Подтверждение подписки.
    SubscriptionResponse,
    /// Ответ на пинг.
    Pong,
    /// Снимок стакана.
    Book(WsBook),
    /// Сделки.
    Trades(Vec<WsTrade>),
    /// Лучшие цены.
    Bbo(WsBbo),
    /// Контексты активов всех дексов.
    AllDexsAssetCtxs(AllDexsAssetCtxs),
    /// Ошибка площадки.
    Error(String),
    /// Прочие каналы (сохраняются рекордером, но не нормализуются).
    Other(String),
}

/// Разбирает текст сообщения WebSocket.
pub fn parse_ws(text: &str) -> Result<HlMsg, VenueError> {
    let env: WsEnvelope<'_> = serde_json::from_str(text)?;
    let data = || env.data.map(|d| d.get()).ok_or_else(|| VenueError::Parse(format!("{} without data", env.channel)));
    Ok(match env.channel {
        "subscriptionResponse" => HlMsg::SubscriptionResponse,
        "pong" => HlMsg::Pong,
        "l2Book" => HlMsg::Book(serde_json::from_str(data()?)?),
        "trades" => HlMsg::Trades(serde_json::from_str(data()?)?),
        "bbo" => HlMsg::Bbo(serde_json::from_str(data()?)?),
        "allDexsAssetCtxs" => HlMsg::AllDexsAssetCtxs(serde_json::from_str(data()?)?),
        "error" => HlMsg::Error(data().map(str::to_owned).unwrap_or_default()),
        other => HlMsg::Other(other.to_owned()),
    })
}

/// Имя декса по имени монеты: `xyz:NVDA` → `xyz`, `BTC` → `` (ядро).
pub fn dex_of_coin(coin: &str) -> &str {
    coin.split_once(':').map(|(d, _)| d).unwrap_or("")
}

/// Ключ площадки для монеты.
pub fn venue_of_coin(coin: &str) -> VenueKey {
    VenueKey::hyperliquid(dex_of_coin(coin))
}

/// Состояние одного декса (ядро — `name == ""`, `index == 0`).
#[derive(Debug, Clone)]
pub struct DexState {
    /// Имя декса.
    pub name: String,
    /// Позиция в ответе `perpDexs` (для asset id).
    pub index: usize,
    /// Обеспечение декса.
    pub collateral: Collateral,
    /// Активы в порядке индексов `meta`.
    pub universe: Vec<AssetMeta>,
    /// Интервал фандинга, ч (из `predictedFundings` или выведенный по истории).
    pub interval_hours: Option<Decimal>,
}

impl DexState {
    /// Ключ площадки декса.
    pub fn venue(&self) -> VenueKey {
        VenueKey::hyperliquid(self.name.clone())
    }

    /// Asset id: индекс для ядра, `100000 + dex_index * 10000 + i` для HIP-3.
    pub fn asset_id(&self, i: usize) -> u64 {
        let i = i as u64;
        if self.index == 0 { i } else { 100_000 + self.index as u64 * 10_000 + i }
    }

    /// Нормализованные метаданные всех активов декса.
    pub fn instruments(&self) -> Vec<InstrumentMeta> {
        self.universe.iter().enumerate().map(|(i, a)| self.instrument(i, a)).collect()
    }

    /// Нормализованные метаданные одного актива.
    pub fn instrument(&self, i: usize, a: &AssetMeta) -> InstrumentMeta {
        let lot = Decimal::new(1, a.sz_decimals);
        let margin_mode = a
            .margin_mode
            .clone()
            .or_else(|| a.only_isolated.map(|iso| if iso { "isolated".to_owned() } else { "cross".to_owned() }));
        InstrumentMeta {
            venue: self.venue(),
            symbol: a.name.clone(),
            venue_id: Some(self.asset_id(i)),
            base: a.name.split_once(':').map(|(_, b)| b).unwrap_or(&a.name).to_owned(),
            collateral: self.collateral.clone(),
            price_rule: PriceRule::SigFigs {
                max_sig_figs: PERP_MAX_SIG_FIGS,
                max_decimals: PERP_MAX_DECIMALS.saturating_sub(a.sz_decimals),
            },
            lot_size: Qty(lot),
            min_qty: Some(Qty(lot)),
            min_notional: None,
            max_leverage: Some(a.max_leverage),
            margin_mode: margin_mode.or_else(|| Some("cross".to_owned())),
            funding_interval_hours: self.interval_hours,
            fees: FeeInfo {
                maker: None,
                taker: None,
                source: FeeSource::AccountRequired,
                deployer_fee_scale: a.deployer_fee_scale,
                growth_mode: a.growth_mode.as_deref().map(|g| g == "enabled"),
            },
            status: if a.is_delisted.unwrap_or(false) { InstrumentStatus::Delisted } else { InstrumentStatus::Active },
            listed_at_ms: None,
        }
    }

    /// Активные (не делистингованные) монеты.
    pub fn active_coins(&self) -> impl Iterator<Item = &str> {
        self.universe.iter().filter(|a| !a.is_delisted.unwrap_or(false)).map(|a| a.name.as_str())
    }
}

/// Строит состояние декса из `meta` и `spotMeta`.
pub fn dex_state(name: &str, index: usize, meta: &Meta, spot: &SpotMeta, interval_hours: Option<Decimal>) -> DexState {
    let collateral = meta
        .collateral_token
        .and_then(|idx| spot.tokens.iter().find(|t| t.index == idx))
        .map(|t| Collateral::from_symbol(&t.name))
        .unwrap_or_else(|| Collateral::Other(format!("token#{}", meta.collateral_token.unwrap_or_default())));
    DexState { name: name.to_owned(), index, collateral, universe: meta.universe.clone(), interval_hours }
}

fn level(l: &WsLevel) -> Level {
    Level::new(l.px, l.sz)
}

/// Снимок стакана → событие.
pub fn book_event(b: &WsBook, recv: RecvTs) -> MdEvent {
    let side = |i: usize| b.levels.get(i).map(|v| v.iter().map(level).collect()).unwrap_or_default();
    MdEvent {
        venue: venue_of_coin(&b.coin),
        symbol: b.coin.clone(),
        recv,
        venue_ts_ms: Some(b.time),
        kind: MdEventKind::BookSnapshot { bids: side(0), asks: side(1), seq: None },
    }
}

/// Сторона агрессора: `B` — покупка, `A` — продажа (UNVERIFIED, см. docs/venues/hyperliquid.md).
fn taker_side(s: &str) -> Option<Side> {
    match s {
        "B" => Some(Side::Buy),
        "A" => Some(Side::Sell),
        _ => None,
    }
}

/// Сделки → события (по одному на монету).
pub fn trade_events(trades: &[WsTrade], recv: RecvTs) -> Vec<MdEvent> {
    let mut out: Vec<MdEvent> = Vec::new();
    for t in trades {
        let ev = TradeEvent {
            px: t.px,
            qty: t.sz,
            taker_side: taker_side(&t.side),
            trade_id: Some(t.tid.to_string()),
            ts_ms: Some(t.time),
        };
        match out.iter_mut().find(|e| e.symbol == t.coin) {
            Some(MdEvent { kind: MdEventKind::Trades(v), venue_ts_ms, .. }) => {
                v.push(ev);
                *venue_ts_ms = (*venue_ts_ms).max(Some(t.time));
            }
            _ => out.push(MdEvent {
                venue: venue_of_coin(&t.coin),
                symbol: t.coin.clone(),
                recv,
                venue_ts_ms: Some(t.time),
                kind: MdEventKind::Trades(vec![ev]),
            }),
        }
    }
    out
}

/// BBO → событие.
pub fn bbo_event(b: &WsBbo, recv: RecvTs) -> MdEvent {
    let get = |i: usize| b.bbo.get(i).and_then(|l| l.as_ref()).map(level);
    MdEvent {
        venue: venue_of_coin(&b.coin),
        symbol: b.coin.clone(),
        recv,
        venue_ts_ms: Some(b.time),
        kind: MdEventKind::Bbo { bid: get(0), ask: get(1) },
    }
}

/// Контекст актива → события фандинга, цен и открытого интереса.
pub fn ctx_events(
    dex: &DexState,
    coin: &str,
    ctx: &PerpAssetCtx,
    recv: RecvTs,
    next_funding_ms: Option<i64>,
) -> Vec<MdEvent> {
    let ev = |kind| MdEvent { venue: dex.venue(), symbol: coin.to_owned(), recv, venue_ts_ms: None, kind };
    let mut out = Vec::with_capacity(3);
    if ctx.funding.is_some() {
        out.push(ev(MdEventKind::Funding(FundingUpdate {
            predicted_rate: ctx.funding,
            interval_hours: dex.interval_hours,
            next_funding_ms,
            ..FundingUpdate::default()
        })));
    }
    out.push(ev(MdEventKind::Mark { mark: ctx.mark_px, index: None, oracle: ctx.oracle_px, mid: ctx.mid_px }));
    if let Some(oi) = ctx.open_interest {
        out.push(ev(MdEventKind::OpenInterest { base: Some(oi), usd: None }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dex_of_coin_works() {
        assert_eq!(dex_of_coin("BTC"), "");
        assert_eq!(dex_of_coin("xyz:NVDA"), "xyz");
        assert_eq!(venue_of_coin("km:US500").to_string(), "hyperliquid:km");
    }

    #[test]
    fn asset_ids_follow_docs() {
        // документация: test:ABC на тестнете — perp_dex_index = 1, index_in_meta = 0 → 110000
        let d = DexState {
            name: "test".into(),
            index: 1,
            collateral: Collateral::Usdc,
            universe: vec![],
            interval_hours: None,
        };
        assert_eq!(d.asset_id(0), 110_000);
        let core = DexState { name: String::new(), index: 0, ..d };
        assert_eq!(core.asset_id(5), 5);
    }

    #[test]
    fn taker_side_mapping() {
        assert_eq!(taker_side("B"), Some(Side::Buy));
        assert_eq!(taker_side("A"), Some(Side::Sell));
        assert_eq!(taker_side("?"), None);
    }

    #[test]
    fn parse_error_channel_and_unknown() {
        assert!(matches!(parse_ws(r#"{"channel":"error","data":"bad sub"}"#).unwrap(), HlMsg::Error(_)));
        assert!(matches!(parse_ws(r#"{"channel":"post","data":{}}"#).unwrap(), HlMsg::Other(_)));
        assert!(parse_ws(r#"{"channel":"l2Book"}"#).is_err());
        assert!(parse_ws("not json").is_err());
    }
}
