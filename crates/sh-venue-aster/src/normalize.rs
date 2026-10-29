//! Преобразование сообщений Aster в нормализованные события и метаданные.

use rust_decimal::Decimal;
use sh_core::{Collateral, Exchange, Level, Qty, RecvTs, Side, Usd, VenueKey};
use sh_md::{BookDelta, DeltaSeq};
use sh_venue_api::{
    FeeInfo, FeeSource, FundingUpdate, InstrumentMeta, InstrumentStatus, MdEvent, MdEventKind, PriceRule, TradeEvent,
    VenueError,
};

use crate::wire::{
    AggTrade, BookTicker, Combined, DepthSnapshot, DepthUpdate, Filter, MarkPriceUpdate, PxQty, SymbolInfo,
};

/// Разобранное сообщение комбинированного потока.
#[derive(Debug)]
pub enum AsMsg {
    /// Дельта стакана.
    Depth(DepthUpdate),
    /// Сделка.
    AggTrade(AggTrade),
    /// Цены маркировки и фандинг всех символов.
    MarkPrices(Vec<MarkPriceUpdate>),
    /// BBO.
    BookTicker(BookTicker),
    /// Прочее (ответы на подписки и т. п.).
    Other(String),
}

/// Разбирает сообщение комбинированного потока.
pub fn parse_ws(text: &str) -> Result<AsMsg, VenueError> {
    let Ok(c) = serde_json::from_str::<Combined<'_>>(text) else {
        return Ok(AsMsg::Other(text.chars().take(80).collect()));
    };
    let data = c.data.get();
    let stream = c.stream;
    Ok(if stream.contains("@depth") {
        AsMsg::Depth(serde_json::from_str(data)?)
    } else if stream.ends_with("@aggTrade") {
        AsMsg::AggTrade(serde_json::from_str(data)?)
    } else if stream.starts_with("!markPrice@arr") {
        AsMsg::MarkPrices(serde_json::from_str(data)?)
    } else if stream.starts_with("!bookTicker") || stream.ends_with("@bookTicker") {
        AsMsg::BookTicker(serde_json::from_str(data)?)
    } else {
        AsMsg::Other(stream.to_owned())
    })
}

/// Площадка Aster.
pub fn venue() -> VenueKey {
    VenueKey::new(Exchange::Aster)
}

fn levels(v: &[PxQty]) -> Vec<Level> {
    v.iter().map(|(p, q)| Level::new(*p, *q)).collect()
}

/// Дельта для `SequencedBook`: `first = U`, `last = u`, `prev = pu`.
pub fn book_delta(d: &DepthUpdate) -> BookDelta {
    BookDelta {
        seq: DeltaSeq { first: d.first_id, last: d.last_id, prev: Some(d.prev_id) },
        bids: levels(&d.bids),
        asks: levels(&d.asks),
    }
}

/// Событие дельты стакана.
pub fn delta_event(d: &DepthUpdate, recv: RecvTs) -> MdEvent {
    MdEvent {
        venue: venue(),
        symbol: d.symbol.clone(),
        recv,
        venue_ts_ms: d.tx_ms.or(Some(d.event_ms)),
        kind: MdEventKind::BookDelta {
            bids: levels(&d.bids),
            asks: levels(&d.asks),
            first_seq: d.first_id,
            last_seq: d.last_id,
            prev_seq: Some(d.prev_id),
        },
    }
}

/// Уровни REST-снимка.
pub fn snapshot_levels(s: &DepthSnapshot) -> (Vec<Level>, Vec<Level>) {
    (levels(&s.bids), levels(&s.asks))
}

/// Событие снимка стакана.
pub fn snapshot_event(symbol: &str, s: &DepthSnapshot, recv: RecvTs) -> MdEvent {
    let (bids, asks) = snapshot_levels(s);
    MdEvent {
        venue: venue(),
        symbol: symbol.to_owned(),
        recv,
        venue_ts_ms: s.tx_ms,
        kind: MdEventKind::BookSnapshot { bids, asks, seq: Some(s.last_update_id) },
    }
}

/// Сделка → событие. `m = true`: покупатель — мейкер, агрессор продавал.
pub fn trade_event(t: &AggTrade, recv: RecvTs) -> MdEvent {
    MdEvent {
        venue: venue(),
        symbol: t.symbol.clone(),
        recv,
        venue_ts_ms: Some(t.trade_ms),
        kind: MdEventKind::Trades(vec![TradeEvent {
            px: t.px,
            qty: t.qty,
            taker_side: Some(if t.buyer_is_maker { Side::Sell } else { Side::Buy }),
            trade_id: Some(t.id.to_string()),
            ts_ms: Some(t.trade_ms),
        }]),
    }
}

/// Цена маркировки → события фандинга и цен.
pub fn mark_events(m: &MarkPriceUpdate, interval_hours: Option<Decimal>, recv: RecvTs) -> Vec<MdEvent> {
    let ev = |kind| MdEvent { venue: venue(), symbol: m.symbol.clone(), recv, venue_ts_ms: Some(m.event_ms), kind };
    vec![
        ev(MdEventKind::Funding(FundingUpdate {
            predicted_rate: m.funding_rate,
            next_funding_ms: m.next_funding_ms,
            interval_hours,
            ..FundingUpdate::default()
        })),
        ev(MdEventKind::Mark { mark: Some(m.mark), index: m.index, oracle: None, mid: None }),
    ]
}

/// BBO → событие.
pub fn bbo_event(b: &BookTicker, recv: RecvTs) -> MdEvent {
    MdEvent {
        venue: venue(),
        symbol: b.symbol.clone(),
        recv,
        venue_ts_ms: Some(b.event_ms),
        kind: MdEventKind::Bbo { bid: Some(Level::new(b.bid, b.bid_qty)), ask: Some(Level::new(b.ask, b.ask_qty)) },
    }
}

/// Символ подходит для записи: бессрочный и торгуется.
pub fn is_recordable(s: &SymbolInfo) -> bool {
    s.contract_type == "PERPETUAL" && s.status == "TRADING"
}

/// Метаданные символа. Комиссии в публичных данных отсутствуют (только `commissionRate`, подписанный запрос).
pub fn instrument(s: &SymbolInfo, interval_hours: Option<Decimal>) -> Option<InstrumentMeta> {
    let mut tick = None;
    let mut lot = None;
    let mut min_qty = None;
    let mut min_notional = None;
    for f in &s.filters {
        match f {
            Filter::Price { tick_size } => tick = Some(*tick_size),
            Filter::Lot { step_size, min_qty: m } => {
                lot = Some(*step_size);
                min_qty = Some(*m);
            }
            Filter::MinNotional { notional } => min_notional = Some(Usd(*notional)),
            Filter::Other => {}
        }
    }
    Some(InstrumentMeta {
        venue: venue(),
        symbol: s.symbol.clone(),
        venue_id: None,
        base: s.base_asset.clone(),
        collateral: Collateral::from_symbol(&s.margin_asset),
        price_rule: PriceRule::Tick { tick: tick? },
        lot_size: lot.unwrap_or(Qty::ZERO),
        min_qty,
        min_notional,
        max_leverage: None,
        margin_mode: None,
        funding_interval_hours: interval_hours,
        fees: FeeInfo {
            maker: None,
            taker: None,
            source: FeeSource::AccountRequired,
            deployer_fee_scale: None,
            growth_mode: None,
        },
        status: if is_recordable(s) { InstrumentStatus::Active } else { InstrumentStatus::Inactive },
        listed_at_ms: s.onboard_date,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_combined_messages_are_other() {
        assert!(matches!(parse_ws(r#"{"result":null,"id":1}"#).unwrap(), AsMsg::Other(_)));
        assert!(matches!(parse_ws(r#"{"stream":"x@kline_1m","data":{}}"#).unwrap(), AsMsg::Other(_)));
        assert!(parse_ws(r#"{"stream":"btcusdt@depth@100ms","data":{"e":"depthUpdate"}}"#).is_err());
    }
}
