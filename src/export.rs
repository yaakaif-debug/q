//! Учётная выгрузка (SPEC.md, «Accounting export»): CSV всех заполнений, платежей
//! фандинга, комиссий и переводов за год в USD, по желанию — в валюте отчёта по дневному
//! курсу из файла оператора. Налоги движок не считает.

use std::collections::BTreeMap;
use std::path::Path;

use rust_decimal::Decimal;
use sh_store::{Db, FillRow, LedgerRow};

/// Дневные курсы USD → валюта отчёта: CSV `YYYY-MM-DD,rate`.
pub fn parse_rates(text: &str) -> Result<BTreeMap<String, Decimal>, String> {
    let mut m = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("date") {
            continue;
        }
        let (d, r) = line.split_once(',').ok_or_else(|| format!("rates line {}: expected date,rate", i + 1))?;
        let rate: Decimal = r.trim().parse().map_err(|_| format!("rates line {}: bad rate", i + 1))?;
        m.insert(d.trim().to_owned(), rate);
    }
    Ok(m)
}

fn date_of(ts_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts_ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// Курс на дату или последний известный раньше неё.
fn rate_on(rates: &BTreeMap<String, Decimal>, date: &str) -> Option<Decimal> {
    rates.range(..=date.to_owned()).next_back().map(|(_, r)| *r)
}

/// Строит CSV. Знак суммы: плюс — получено, минус — уплачено.
pub fn build_csv(fills: &[FillRow], ledger: &[LedgerRow], ccy: Option<(&str, &BTreeMap<String, Decimal>)>) -> String {
    let mut rows: Vec<(i64, String)> = Vec::new();
    let conv = |ts: i64, usd: Decimal| -> (String, String) {
        match ccy {
            Some((_, rates)) => match rate_on(rates, &date_of(ts)) {
                Some(r) => ((usd * r).round_dp(8).to_string(), r.to_string()),
                None => (String::new(), String::new()),
            },
            None => (String::new(), String::new()),
        }
    };
    for f in fills {
        let qty: Decimal = f.qty.parse().unwrap_or_default();
        let px: Decimal = f.px.parse().unwrap_or_default();
        let fee: Decimal = f.fee.parse().unwrap_or_default();
        let notional = qty * px;
        // покупка — отток денег, продажа — приток
        let cash = if f.side == "buy" { -notional } else { notional };
        let (c, r) = conv(f.ts_ms, cash);
        rows.push((
            f.ts_ms,
            format!(
                "{},{},fill,{},{},{},{},{},{},{},{},{},{}",
                date_of(f.ts_ms),
                f.ts_ms,
                f.venue,
                f.symbol,
                f.side,
                qty,
                px,
                cash.round_dp(8),
                c,
                r,
                f.trade_id,
                f.pair_id.clone().unwrap_or_default()
            ),
        ));
        if !fee.is_zero() {
            let (c, r) = conv(f.ts_ms, -fee);
            rows.push((
                f.ts_ms,
                format!(
                    "{},{},fee,{},{},,,,{},{},{},{},{}",
                    date_of(f.ts_ms),
                    f.ts_ms,
                    f.venue,
                    f.symbol,
                    -fee,
                    c,
                    r,
                    f.trade_id,
                    f.pair_id.clone().unwrap_or_default()
                ),
            ));
        }
    }
    for l in ledger {
        // в базе есть и теневой портфель сканера (pair_id "shadow:…"), и атрибуция — их не выгружаем
        if l.pair_id.starts_with("shadow:") || !matches!(l.kind.as_str(), "funding" | "transfer") {
            continue;
        }
        let amount: Decimal = l.amount.parse().unwrap_or_default();
        let (venue, symbol) = l
            .leg
            .as_deref()
            .and_then(|x| x.split_once('/'))
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .unwrap_or_default();
        let (c, r) = conv(l.ts_ms, amount);
        rows.push((
            l.ts_ms,
            format!(
                "{},{},{},{},{},,,,{},{},{},,{}",
                date_of(l.ts_ms),
                l.ts_ms,
                l.kind,
                venue,
                symbol,
                amount,
                c,
                r,
                l.pair_id
            ),
        ));
    }
    rows.sort_by_key(|r| r.0);
    let code = ccy.map(|(c, _)| c.to_ascii_lowercase()).unwrap_or_else(|| "ccy".into());
    let mut s = format!(
        "date,ts_ms,type,venue,symbol,side,qty,px,amount_usd,amount_{code},rate_{code}_per_usd,trade_id,pair\n"
    );
    for (_, r) in rows {
        s.push_str(&r);
        s.push('\n');
    }
    s
}

/// Выгрузка за год из базы.
pub async fn export_year(
    db_path: &Path,
    year: i32,
    ccy: Option<(String, BTreeMap<String, Decimal>)>,
) -> anyhow::Result<String> {
    let db = Db::open(db_path).await?;
    let from = chrono::NaiveDate::from_ymd_opt(year, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().timestamp_millis())
        .ok_or_else(|| anyhow::anyhow!("bad year"))?;
    let to = chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().timestamp_millis())
        .ok_or_else(|| anyhow::anyhow!("bad year"))?;
    let fills = db.fills_between(from, to).await?;
    let ledger = db.ledger_between(from, to).await?;
    Ok(build_csv(&fills, &ledger, ccy.as_ref().map(|(c, r)| (c.as_str(), r))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn fill(ts: i64, side: &str, fee: &str) -> FillRow {
        FillRow {
            ts_ms: ts,
            venue: "lighter".into(),
            symbol: "ETH".into(),
            side: side.into(),
            qty: "2".into(),
            px: "100".into(),
            fee: fee.into(),
            maker: true,
            trade_id: format!("t{ts}"),
            cid: None,
            liquidation: false,
            pair_id: Some("ETH:p".into()),
        }
    }

    #[test]
    fn csv_with_conversion() {
        let day = 1_767_225_600_000; // 2026-01-01
        let fills = vec![fill(day + 1000, "buy", "0.5"), fill(day + 2000, "sell", "0")];
        let ledger = vec![
            LedgerRow {
                ts_ms: day + 3000,
                pair_id: "ETH:p".into(),
                leg: Some("lighter/ETH".into()),
                kind: "funding".into(),
                amount: "1.25".into(),
            },
            LedgerRow {
                ts_ms: day + 4000,
                pair_id: "shadow:X".into(),
                leg: None,
                kind: "funding".into(),
                amount: "9".into(),
            },
            LedgerRow {
                ts_ms: day + 5000,
                pair_id: "ETH:p".into(),
                leg: None,
                kind: "slippage".into(),
                amount: "-1".into(),
            },
        ];
        let rates = parse_rates("date,rate\n2025-12-31,0.9\n").unwrap();
        let csv = build_csv(&fills, &ledger, Some(("EUR", &rates)));
        let lines: Vec<&str> = csv.lines().collect();
        assert!(lines[0].contains("amount_eur"));
        // покупка, комиссия, продажа, фандинг; теневой и атрибуция исключены
        assert_eq!(lines.len(), 5, "{csv}");
        assert!(lines[1].contains(",fill,lighter,ETH,buy,2,100,-200,-180.0,0.9,"));
        assert!(lines[2].contains(",fee,lighter,ETH,,,,-0.5,"));
        assert!(lines[4].contains(",funding,lighter,ETH,,,,1.25,1.125,0.9,"));
        assert_eq!(rate_on(&rates, "2026-02-01"), Some(dec!(0.9)));
        assert!(parse_rates("x").is_err());
    }
}
