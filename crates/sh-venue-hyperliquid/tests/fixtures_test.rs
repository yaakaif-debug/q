//! Тесты разбора на записанных ответах Hyperliquid (tests/fixtures).

// в интеграционных тестах паника на неожиданных данных — ожидаемое поведение
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rust_decimal::dec;
use sh_core::{Collateral, Px, Qty, RecvTs, Side};
use sh_venue_api::{InstrumentStatus, MdEventKind, PriceRule};
use sh_venue_hyperliquid::normalize::{self, HlMsg, parse_ws};
use sh_venue_hyperliquid::wire::{FundingHistoryItem, MetaAndAssetCtxs, PerpDexInfo, PredictedFundings, SpotMeta};

fn fx(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

const RECV: RecvTs = RecvTs { wall_ns: 1, mono_ns: 2 };

#[test]
fn l2book_snapshot() {
    let HlMsg::Book(b) = parse_ws(&fx("ws_l2book_btc.json")).unwrap() else { panic!("not a book") };
    let ev = normalize::book_event(&b, RECV);
    assert_eq!(ev.venue.to_string(), "hyperliquid");
    assert_eq!(ev.venue_ts_ms, Some(1_793_255_321_278));
    let MdEventKind::BookSnapshot { bids, asks, seq } = ev.kind else { panic!() };
    assert_eq!(seq, None);
    assert_eq!(bids[0].px, Px(dec!(80058.0)));
    assert_eq!(bids[0].qty, Qty(dec!(0.43886)));
    // bids по убыванию, asks по возрастанию, стакан не перекрещен
    assert!(bids.windows(2).all(|w| w[0].px > w[1].px));
    assert!(asks.windows(2).all(|w| w[0].px < w[1].px));
    assert!(bids[0].px < asks[0].px);
}

#[test]
fn trades_grouped_by_coin() {
    let HlMsg::Trades(t) = parse_ws(&fx("ws_trades_btc.json")).unwrap() else { panic!() };
    let evs = normalize::trade_events(&t, RECV);
    assert_eq!(evs.len(), 1);
    let MdEventKind::Trades(trades) = &evs[0].kind else { panic!() };
    assert_eq!(trades.len(), t.len());
    assert_eq!(trades[0].taker_side, Some(Side::Sell));
    assert_eq!(trades[0].px, Px(dec!(80072.0)));
}

#[test]
fn bbo_and_control_messages() {
    let HlMsg::Bbo(b) = parse_ws(&fx("ws_bbo_eth.json")).unwrap() else { panic!() };
    let MdEventKind::Bbo { bid, ask } = normalize::bbo_event(&b, RECV).kind else { panic!() };
    assert_eq!(bid.unwrap().px, Px(dec!(2315.2)));
    assert_eq!(ask.unwrap().qty, Qty(dec!(0.25)));
    assert!(matches!(parse_ws(&fx("ws_pong.json")).unwrap(), HlMsg::Pong));
    assert!(matches!(parse_ws(&fx("ws_sub_ack_l2book.json")).unwrap(), HlMsg::SubscriptionResponse));
}

#[test]
fn meta_spot_and_collateral() {
    let spot: SpotMeta = serde_json::from_str(&fx("rest_spot_meta.json")).unwrap();
    let MetaAndAssetCtxs(meta, ctxs) = serde_json::from_str(&fx("rest_meta_and_asset_ctxs_xyz.json")).unwrap();
    let dex = normalize::dex_state("xyz", 1, &meta, &spot, Some(dec!(1)));
    assert_eq!(dex.collateral, Collateral::Usdc);
    let inst = dex.instruments();
    assert_eq!(inst.len(), 3);
    let first = &inst[0];
    assert_eq!(first.symbol, "xyz:XYZ100");
    assert_eq!(first.base, "XYZ100");
    assert_eq!(first.venue.to_string(), "hyperliquid:xyz");
    assert_eq!(first.venue_id, Some(110_000));
    assert_eq!(first.lot_size, Qty(dec!(0.0001)));
    assert_eq!(first.price_rule, PriceRule::SigFigs { max_sig_figs: 5, max_decimals: 2 });
    assert_eq!(first.fees.deployer_fee_scale, Some(dec!(1.0)));
    assert_eq!(first.fees.growth_mode, Some(true));
    // в живом ответе marginMode отсутствует → кросс-маржа разрешена
    assert_eq!(first.margin_mode.as_deref(), Some("cross"));
    assert_eq!(first.funding_interval_hours, Some(dec!(1)));
    assert_eq!(first.status, InstrumentStatus::Active);
    // контексты: фандинг, цены, ОИ
    let evs = normalize::ctx_events(&dex, &first.symbol, &ctxs[0], RECV, None);
    assert!(
        evs.iter().any(|e| matches!(&e.kind, MdEventKind::Funding(f) if f.predicted_rate == Some(dec!(0.00000625))))
    );
    assert!(evs.iter().any(|e| matches!(&e.kind, MdEventKind::OpenInterest { base: Some(_), .. })));
}

#[test]
fn core_meta_has_usdc_and_asset_index() {
    let spot: SpotMeta = serde_json::from_str(&fx("rest_spot_meta.json")).unwrap();
    let MetaAndAssetCtxs(meta, ctxs) = serde_json::from_str(&fx("rest_meta_and_asset_ctxs_core.json")).unwrap();
    assert_eq!(meta.universe.len(), ctxs.len());
    let dex = normalize::dex_state("", 0, &meta, &spot, None);
    let inst = dex.instruments();
    assert_eq!(inst[0].symbol, "BTC");
    assert_eq!(inst[0].venue_id, Some(0));
    assert_eq!(inst[0].collateral, Collateral::Usdc);
    assert_eq!(inst[0].funding_interval_hours, None);
}

#[test]
fn spot_tokens_resolve_other_stablecoins() {
    let spot: SpotMeta = serde_json::from_str(&fx("rest_spot_meta.json")).unwrap();
    let names: Vec<(u32, Collateral)> =
        spot.tokens.iter().map(|t| (t.index, Collateral::from_symbol(&t.name))).collect();
    assert!(names.contains(&(360, Collateral::Usdh)));
    assert!(names.contains(&(235, Collateral::Usde)));
}

#[test]
fn all_dexs_asset_ctxs_maps_by_index() {
    let HlMsg::AllDexsAssetCtxs(all) = parse_ws(&fx("ws_all_dexs_asset_ctxs.json")).unwrap() else { panic!() };
    let names: Vec<&str> = all.ctxs.iter().map(|(d, _)| d.as_str()).collect();
    assert_eq!(names, vec!["", "xyz"]);
    assert_eq!(all.ctxs[0].1.len(), 3);
    assert_eq!(all.ctxs[0].1[0].funding, Some(dec!(0.0000125)));
}

#[test]
fn perp_dexs_first_is_core() {
    let dexes: Vec<Option<PerpDexInfo>> = serde_json::from_str(&fx("rest_perp_dexs.json")).unwrap();
    assert!(dexes[0].is_none());
    assert_eq!(dexes[1].as_ref().unwrap().name, "xyz");
}

#[test]
fn funding_history_infers_hourly_interval() {
    let items: Vec<FundingHistoryItem> = serde_json::from_str(&fx("rest_funding_history_xyz_nvda.json")).unwrap();
    let ts: Vec<i64> = items.iter().map(|i| i.time).collect();
    assert_eq!(sh_venue_api::infer_interval_ms(&ts), Some(3_600_000));
    let items: Vec<FundingHistoryItem> = serde_json::from_str(&fx("rest_funding_history_btc.json")).unwrap();
    assert!(!items.is_empty());
}

#[test]
fn predicted_fundings_carry_interval() {
    let p: PredictedFundings = serde_json::from_str(&fx("rest_predicted_fundings.json")).unwrap();
    let hl = p[0].1.iter().find(|(v, _)| v == "HlPerp").and_then(|(_, f)| f.as_ref()).unwrap();
    assert_eq!(hl.funding_interval_hours, Some(dec!(1)));
}

#[test]
fn documented_perp_dexs_example_parses() {
    // пример из документации «Retrieve all perpetual dexs»
    let doc = r#"[null,{"name":"test","fullName":"test dex","deployer":"0x5e89b26d8d66da9888c835c9bfcc2aa51813e152","oracleUpdater":null,"feeRecipient":null,"assetToStreamingOiCap":[["COIN1","100000.0"]],"assetToFundingMultiplier":[["COIN1","1.0"]]}]"#;
    let dexes: Vec<Option<PerpDexInfo>> = serde_json::from_str(doc).unwrap();
    assert_eq!(dexes[1].as_ref().unwrap().full_name.as_deref(), Some("test dex"));
}
