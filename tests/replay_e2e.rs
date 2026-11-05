//! Сквозной тест повтора: запись настоящим рекордером из фикстур площадок,
//! повтор сканера через полночь UTC, дневной отчёт и таблица возможностей.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use sh_core::{Exchange, RawRecord, RawSource, RecvTs};
use sh_store::recorder::{HourlyWriter, RecorderConfig};
use sh_store::{FORMAT, FileHeader};
use spread_hunter::config::AppConfig;
use spread_hunter::replay_source::parse_time;

fn fx(crate_dir: &str, name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates").join(crate_dir).join("tests/fixtures").join(name);
    std::fs::read_to_string(p).unwrap()
}

fn rec(ex: Exchange, source: RawSource, ms: i64, payload: String) -> RawRecord {
    RawRecord { exchange: ex, source, conn: "c0".into(), recv: RecvTs { wall_ns: ms * 1_000_000, mono_ns: 0 }, payload }
}

#[test]
fn replay_writes_daily_report() {
    let dir = std::env::temp_dir().join(format!("sh-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let raw = dir.join("raw");
    let t0 = parse_time("2026-10-29T23:50:00Z").unwrap();
    let hdr = FileHeader {
        format: FORMAT.into(),
        session: "s".into(),
        host: "t".into(),
        session_start_wall_ns: 0,
        version: "0".into(),
    };
    let mut w = HourlyWriter::new(RecorderConfig { dir: raw.clone(), ..RecorderConfig::default() }, hdr);
    let hl = |body: &str| RawSource::Rest { ep: format!("POST /info {body}") };
    let records = vec![
        rec(Exchange::Hyperliquid, hl(r#"{"type":"perpDexs"}"#), t0, fx("sh-venue-hyperliquid", "rest_perp_dexs.json")),
        rec(
            Exchange::Hyperliquid,
            hl(r#"{"type":"spotMeta"}"#),
            t0 + 1,
            fx("sh-venue-hyperliquid", "rest_spot_meta.json"),
        ),
        rec(
            Exchange::Hyperliquid,
            hl(r#"{"type":"metaAndAssetCtxs"}"#),
            t0 + 2,
            fx("sh-venue-hyperliquid", "rest_meta_and_asset_ctxs_core.json"),
        ),
        rec(Exchange::Hyperliquid, RawSource::Ws, t0 + 3, fx("sh-venue-hyperliquid", "ws_l2book_btc.json")),
        rec(
            Exchange::Aster,
            RawSource::Rest { ep: "GET /fapi/v3/fundingInfo".into() },
            t0 + 4,
            fx("sh-venue-aster", "rest_funding_info.json"),
        ),
        rec(
            Exchange::Aster,
            RawSource::Rest { ep: "GET /fapi/v3/exchangeInfo".into() },
            t0 + 5,
            fx("sh-venue-aster", "rest_exchange_info.json"),
        ),
        rec(Exchange::Aster, RawSource::Ws, t0 + 6, fx("sh-venue-aster", "ws_mark_price_arr.json")),
        // после полуночи — ещё одно сообщение, чтобы повтор прошёл границу суток
        rec(Exchange::Hyperliquid, RawSource::Ws, t0 + 15 * 60_000, fx("sh-venue-hyperliquid", "ws_l2book_btc.json")),
    ];
    for r in &records {
        w.push(r).unwrap();
    }
    w.close_all().unwrap();

    let symbols = dir.join("symbols.toml");
    std::fs::write(&symbols, "[[underlying]]\nid = \"BTC\"\nmembers = [\"hyperliquid/BTC\", \"aster/BTCUSDT\"]\n")
        .unwrap();
    let mut cfg = AppConfig::default();
    cfg.symbols.path = symbols.display().to_string();
    let out = dir.join("reports");
    let s = spread_hunter::runner::run_replay(&cfg, &raw, t0 - 60_000, t0 + 20 * 60_000, &out).unwrap();
    assert_eq!(s.records, records.len() as u64);
    assert_eq!(s.reports, 1);
    let md = std::fs::read_to_string(out.join("2026-10-29.md")).unwrap();
    assert!(md.contains("Теневой портфель — 2026-10-29"));
    let csv = std::fs::read_to_string(out.join("scan-latest.csv")).unwrap();
    // пара BTC оценена: метаданные обеих ног восстановлены из записей
    assert!(csv.lines().any(|l| l.starts_with("BTC:hyperliquid/BTC~aster/BTCUSDT")), "{csv}");
    assert_eq!(s.opened, 0);

    // бэктест по тем же записям: дважды, результат побайтно одинаков
    let (b1, b2) = (dir.join("bt1"), dir.join("bt2"));
    let r1 = spread_hunter::backtest::run_backtest(&cfg, &raw, t0 - 60_000, t0 + 20 * 60_000, &b1).unwrap();
    let r2 = spread_hunter::backtest::run_backtest(&cfg, &raw, t0 - 60_000, t0 + 20 * 60_000, &b2).unwrap();
    assert_eq!(r1, r2);
    assert_eq!(r1.records, records.len() as u64);
    for f in ["ledger.csv", "final-status.json", "alerts.log"] {
        assert_eq!(std::fs::read(b1.join(f)).unwrap(), std::fs::read(b2.join(f)).unwrap(), "{f}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
