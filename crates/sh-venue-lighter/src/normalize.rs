//! Преобразование сообщений Lighter в нормализованные события и метаданные.

#![forbid(unsafe_code)]

use rust_decimal::Decimal;
use sh_core::{Collateral, Exchange, Level, Px, Qty, RecvTs, Side, Usd, VenueKey};
use sh_md::{BookDelta, DeltaSeq};
use sh_venue_api::{
    FeeInfo, FeeSource, FundingUpdate, InstrumentMeta, InstrumentStatus, MdEvent, MdEventKind, PriceRule, TradeEvent,
    VenueError,
};

use crate::wire::{MarketStats, PerpDetail, WsEnvelope, WsLevel, WsOrderBook, WsTicker, WsTrade, parse_market_stats};

/// Разобранное сообщение.
#[derive(Debug)]
pub enum LtMsg {
    /// Соединение готово.
    Connected,
    /// Ответ на пинг.
    Pong,
    /// Стакан: снимок (`subscribed/`) или дельта (`update/`).
    OrderBook {
        /// Рынок.
        market: u32,
        /// Снимок или дельта.
        snapshot: bool,
        /// Время кадра, мс.
        ts_ms: Option<i64>,
        /// Тело.
        book: WsOrderBook,
    },
    /// Сделки (включая ликвидации).
    Trades {
        /// Рынок.
        market: u32,
        /// Снимок последних сделок при подписке.
        snapshot: bool,
        /// Сделки.
        trades: Vec<WsTrade>,
    },
    /// Статистика рынков.
    MarketStats(Vec<MarketStats>),
    /// BBO.
    Ticker {
        /// Рынок.
        market: u32,
        /// Время кадра, мс.
        ts_ms: Option<i64>,
        /// Тело.
        ticker: WsTicker,
    },
    /// Ошибка площадки.
    Error(String),
    /// Прочее.
    Other(String),
}

/// Номер рынка из канала `order_book:12`.
fn market_of(channel: Option<&str>) -> Result<u32, VenueError> {
    channel
        .and_then(|c| c.rsplit(':').next())
        .and_then(|m| m.parse().ok())
        .ok_or_else(|| VenueError::Parse(format!("no market id in channel {channel:?}")))
}

/// Разбирает текст сообщения WebSocket.
pub fn parse_ws(text: &str) -> Result<LtMsg, VenueError> {
    let env: WsEnvelope<'_> = serde_json::from_str(text)?;
    if let Some(e) = env.error {
        return Ok(LtMsg::Error(format!("{}: {}", e.code, e.message)));
    }
    let (phase, channel) = env.kind.split_once('/').unwrap_or((env.kind.as_str(), ""));
    let snapshot = phase == "subscribed";
    Ok(match (phase, channel) {
        ("connected", _) => LtMsg::Connected,
        ("pong", _) => LtMsg::Pong,
        ("subscribed" | "update", "order_book") => LtMsg::OrderBook {
            market: market_of(env.channel.as_deref())?,
            snapshot,
            ts_ms: env.timestamp,
            book: env.order_book.ok_or_else(|| VenueError::Parse("order_book missing".into()))?,
        },
        ("subscribed" | "update", "trade") => {
            let mut trades = env.trades.unwrap_or_default();
            trades.extend(env.liquidation_trades.unwrap_or_default());
            LtMsg::Trades { market: market_of(env.channel.as_deref())?, snapshot, trades }
        }
        ("subscribed" | "update", "market_stats") => {
            let (stats, skipped) = env.market_stats.map(parse_market_stats).unwrap_or_default();
            if skipped > 0 {
                metrics::counter!("md_parse_errors_total", "venue" => "lighter").increment(skipped as u64);
            }
            LtMsg::MarketStats(stats)
        }
        ("subscribed" | "update", "ticker") => LtMsg::Ticker {
            market: market_of(env.channel.as_deref())?,
            ts_ms: env.timestamp,
            ticker: env.ticker.ok_or_else(|| VenueError::Parse("ticker missing".into()))?,
        },
        _ => LtMsg::Other(env.kind),
    })
}

/// Площадка Lighter.
pub fn venue() -> VenueKey {
    VenueKey::new(Exchange::Lighter)
}

/// Процент → доля.
pub fn pct(v: Decimal) -> Decimal {
    v / Decimal::ONE_HUNDRED
}

fn levels(v: &[WsLevel]) -> Vec<Level> {
    v.iter().map(|l| Level::new(l.price, l.size)).collect()
}

/// Дельта стакана для `SequencedBook`: `prev = begin_nonce`, `last = nonce`.
pub fn book_delta(b: &WsOrderBook) -> BookDelta {
    BookDelta {
        seq: DeltaSeq { first: b.begin_nonce, last: b.nonce, prev: Some(b.begin_nonce) },
        bids: levels(&b.bids),
        asks: levels(&b.asks),
    }
}

/// Событие стакана (снимок или дельта).
pub fn book_event(symbol: &str, snapshot: bool, ts_ms: Option<i64>, b: &WsOrderBook, recv: RecvTs) -> MdEvent {
    let kind = if snapshot {
        MdEventKind::BookSnapshot { bids: levels(&b.bids), asks: levels(&b.asks), seq: Some(b.nonce) }
    } else {
        MdEventKind::BookDelta {
            bids: levels(&b.bids),
            asks: levels(&b.asks),
            first_seq: b.begin_nonce,
            last_seq: b.nonce,
            prev_seq: Some(b.begin_nonce),
        }
    };
    MdEvent { venue: venue(), symbol: symbol.to_owned(), recv, venue_ts_ms: ts_ms, kind }
}

/// Сторона агрессора: мейкер на ask → агрессор покупал (UNVERIFIED, вывод из имени поля).
pub fn taker_side(is_maker_ask: bool) -> Side {
    if is_maker_ask { Side::Buy } else { Side::Sell }
}

/// Сделки → событие.
pub fn trades_event(symbol: &str, trades: &[WsTrade], recv: RecvTs) -> Option<MdEvent> {
    let evs: Vec<TradeEvent> = trades
        .iter()
        .map(|t| TradeEvent {
            px: t.price,
            qty: t.size,
            taker_side: Some(taker_side(t.is_maker_ask)),
            trade_id: Some(t.trade_id.to_string()),
            ts_ms: Some(t.timestamp),
        })
        .collect();
    let ts = trades.iter().map(|t| t.timestamp).max();
    (!evs.is_empty()).then(|| MdEvent {
        venue: venue(),
        symbol: symbol.to_owned(),
        recv,
        venue_ts_ms: ts,
        kind: MdEventKind::Trades(evs),
    })
}

/// Статистика рынка → события фандинга, цен и открытого интереса.
pub fn stats_events(s: &MarketStats, interval_hours: Option<Decimal>, recv: RecvTs) -> Vec<MdEvent> {
    let ev = |kind| MdEvent { venue: venue(), symbol: s.symbol.clone(), recv, venue_ts_ms: None, kind };
    let settled_at = s.funding_timestamp.filter(|t| *t > 0);
    let next = match (settled_at, interval_hours) {
        (Some(t), Some(h)) => (h * Decimal::from(3_600_000)).trunc().try_into().ok().map(|ms: i64| t + ms),
        _ => None,
    };
    vec![
        ev(MdEventKind::Funding(FundingUpdate {
            predicted_rate: s.current_funding_rate.map(pct),
            settled_rate: s.funding_rate.map(pct),
            settled_at_ms: settled_at,
            next_funding_ms: next,
            interval_hours,
        })),
        ev(MdEventKind::Mark { mark: s.mark_price, index: s.index_price, oracle: None, mid: s.mid_price }),
        ev(MdEventKind::OpenInterest { base: None, usd: s.open_interest.map(Usd) }),
    ]
}

/// BBO → событие.
pub fn ticker_event(symbol: &str, ts_ms: Option<i64>, t: &WsTicker, recv: RecvTs) -> MdEvent {
    let lvl = |l: &Option<WsLevel>| l.as_ref().map(|l| Level::new(l.price, l.size));
    MdEvent {
        venue: venue(),
        symbol: symbol.to_owned(),
        recv,
        venue_ts_ms: ts_ms,
        kind: MdEventKind::Bbo { bid: lvl(&t.b), ask: lvl(&t.a) },
    }
}

/// Метаданные рынка.
pub fn instrument(d: &PerpDetail, interval_hours: Option<Decimal>) -> InstrumentMeta {
    let lot = Decimal::new(1, d.size_decimals);
    let tick = Decimal::new(1, d.price_decimals);
    // доли маржи в 1/10000: 200 → 2% → плечо 50 (UNVERIFIED единицы, см. docs/venues/lighter.md)
    let max_lev = d.min_initial_margin_fraction.filter(|f| *f > 0).map(|f| Decimal::from(10_000) / Decimal::from(f));
    let cfg = d.market_config.clone().unwrap_or_default();
    let status = match (d.status.as_str(), cfg.force_reduce_only) {
        ("active", Some(true)) => InstrumentStatus::Inactive,
        ("active", _) => InstrumentStatus::Active,
        _ => InstrumentStatus::Inactive,
    };
    InstrumentMeta {
        venue: venue(),
        symbol: d.symbol.clone(),
        venue_id: Some(u64::from(d.market_id)),
        base: d.symbol.clone(),
        collateral: Collateral::Usdc,
        price_rule: PriceRule::Tick { tick: Px(tick) },
        lot_size: Qty(lot),
        min_qty: Some(d.min_base_amount),
        min_notional: Some(Usd(d.min_quote_amount)),
        max_leverage: max_lev,
        margin_mode: cfg.market_margin_mode.map(|m| format!("mode{m}")),
        funding_interval_hours: interval_hours,
        fees: FeeInfo {
            maker: Some(pct(d.maker_fee)),
            taker: Some(pct(d.taker_fee)),
            source: FeeSource::Metadata,
            deployer_fee_scale: None,
            growth_mode: None,
        },
        status,
        listed_at_ms: d.created_at.as_deref().and_then(|c| c.parse().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_from_channel() {
        assert_eq!(market_of(Some("order_book:12")).unwrap(), 12);
        assert!(market_of(Some("market_stats:all")).is_err());
        assert!(market_of(None).is_err());
    }

    #[test]
    fn error_without_type() {
        let m = parse_ws(r#"{"error":{"code":30003,"message":"Invalid Channel"}}"#).unwrap();
        assert!(matches!(m, LtMsg::Error(e) if e.contains("30003")));
    }

    #[test]
    fn percent_to_fraction() {
        assert_eq!(pct(Decimal::new(12, 4)), Decimal::new(12, 6));
    }

    #[test]
    fn taker_side_from_maker_flag() {
        assert_eq!(taker_side(true), Side::Buy);
        assert_eq!(taker_side(false), Side::Sell);
    }
}
