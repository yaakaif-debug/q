//! `spread-hunter selftest`: проверка приватных адаптеров на малом объёме перед live.
//! Запускает оператор (testnet или mainnet); Claude Code этот код только пишет и
//! проверяет офлайн против симулятора (SPEC.md, «Testing responsibility»).
//!
//! По каждой площадке: метаданные, балансы, post-only + отмена, IOC-заполнение,
//! заполнение и позиция в приватном потоке, запрос по клиентскому ID, закрытие
//! reduce-only, чтение платежей фандинга, перевод между HIP-3 дексами (Hyperliquid),
//! kill switch (отмена всех) и проверка, что аккаунт остался без позиции.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use rust_decimal::Decimal;
use sh_core::{Px, Qty, Side, Usd, VenueKey};
use sh_venue_api::{
    ClientId, ClientIdGen, ExecError, ExecEvent, Execution, InstrumentMeta, InstrumentStatus, OrderRequest,
    OrderStatus, Rounding, Tif,
};
use tokio::sync::{mpsc, watch};

/// Итог шага.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Пройден.
    Pass,
    /// Не пройден.
    Fail,
    /// Пропущен.
    Skip,
}

/// Результат шага.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    /// Площадка.
    pub venue: String,
    /// Шаг.
    pub step: &'static str,
    /// Итог.
    pub outcome: Outcome,
    /// Подробности.
    pub detail: String,
}

/// Источник лучших цен `(bid, ask)`.
pub type QuoteFn = Arc<dyn Fn() -> BoxFuture<'static, Option<(Px, Px)>> + Send + Sync>;

/// Контекст проверки площадки.
pub struct VenueCheck {
    /// Исполнение.
    pub exec: Arc<dyn Execution>,
    /// Площадка инструмента.
    pub venue: VenueKey,
    /// Символ.
    pub symbol: String,
    /// Метаданные (если найдены).
    pub meta: Option<InstrumentMeta>,
    /// Лучшие цены.
    pub quote: QuoteFn,
    /// Максимальный номинал ордера, $.
    pub max_notional: Decimal,
    /// Перевод между дексами (из, в) — только Hyperliquid.
    pub transfer: Option<(String, String)>,
    /// Ожидание событий, мс.
    pub wait_ms: u64,
}

fn bps(v: i64) -> Decimal {
    Decimal::from(v) / Decimal::from(10_000)
}

struct Run {
    venue: String,
    out: Vec<StepResult>,
}

impl Run {
    fn rec(&mut self, step: &'static str, outcome: Outcome, detail: impl Into<String>) {
        let detail = detail.into();
        match outcome {
            Outcome::Fail => tracing::error!(venue = %self.venue, step, %detail, "selftest step failed"),
            _ => tracing::info!(venue = %self.venue, step, ?outcome, %detail, "selftest step"),
        }
        self.out.push(StepResult { venue: self.venue.clone(), step, outcome, detail });
    }

    fn res<T>(&mut self, step: &'static str, r: &Result<T, ExecError>, ok: impl Into<String>) -> bool {
        match r {
            Ok(_) => {
                self.rec(step, Outcome::Pass, ok);
                true
            }
            Err(e) => {
                self.rec(step, Outcome::Fail, e.to_string());
                false
            }
        }
    }
}

async fn wait_status(c: &VenueCheck, cid: ClientId, want: &[OrderStatus]) -> Option<OrderStatus> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(c.wait_ms);
    let mut last = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(u)) = c.exec.query_order(c.venue.clone(), c.symbol.clone(), cid).await {
            last = Some(u.status);
            if want.contains(&u.status) {
                return last;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    last
}

async fn position(c: &VenueCheck) -> Result<Decimal, ExecError> {
    Ok(c.exec.positions().await?.iter().filter(|p| p.venue == c.venue && p.symbol == c.symbol).map(|p| p.qty).sum())
}

async fn wait_event(rx: &mut mpsc::Receiver<ExecEvent>, ms: u64, f: impl Fn(&ExecEvent) -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(ev)) if f(&ev) => return true,
            Ok(Some(_)) => {}
            _ => return false,
        }
    }
}

/// Закрывает позицию reduce-only IOC (уборка после ошибок).
async fn close_position(c: &VenueCheck, ids: &ClientIdGen, meta: &InstrumentMeta) -> Result<(), ExecError> {
    let q = position(c).await?;
    if q.is_zero() {
        return Ok(());
    }
    let (bid, ask) = (c.quote)().await.ok_or_else(|| ExecError::Invalid("no quote".into()))?;
    let (side, raw, mode) = if q > Decimal::ZERO {
        (Side::Sell, bid.0 * (Decimal::ONE - bps(100)), Rounding::Up)
    } else {
        (Side::Buy, ask.0 * (Decimal::ONE + bps(100)), Rounding::Down)
    };
    let px = meta.price_rule.round(Px(raw), mode).ok_or_else(|| ExecError::Invalid("price".into()))?;
    let req = OrderRequest {
        cid: ids.next(chrono::Utc::now().timestamp_millis()),
        venue: c.venue.clone(),
        symbol: c.symbol.clone(),
        side,
        qty: Qty(q.abs()),
        px,
        tif: Tif::Ioc,
        reduce_only: true,
    };
    c.exec.place(req).await.map(|_| ())
}

/// Проверки одной площадки. Всегда пытается оставить аккаунт без ордеров и позиции.
pub async fn check_venue(c: &VenueCheck) -> Vec<StepResult> {
    let mut r = Run { venue: format!("{}/{}", c.venue, c.symbol), out: Vec::new() };
    let ids = ClientIdGen::new(chrono::Utc::now().timestamp_millis());
    let now = || chrono::Utc::now().timestamp_millis();

    // 1. метаданные
    let meta = match &c.meta {
        Some(m) if m.status == InstrumentStatus::Active => {
            r.rec(
                "metadata",
                Outcome::Pass,
                format!("lot {} min notional {:?}", m.lot_size.0, m.min_notional.map(|u| u.0)),
            );
            m.clone()
        }
        Some(m) => {
            r.rec("metadata", Outcome::Fail, format!("instrument status {:?}", m.status));
            return r.out;
        }
        None => {
            r.rec("metadata", Outcome::Fail, "instrument not found in venue metadata");
            return r.out;
        }
    };

    // приватный поток
    let (tx, mut rx) = mpsc::channel(1024);
    let (stop_tx, stop_rx) = watch::channel(false);
    let exec = c.exec.clone();
    let stream = tokio::spawn(async move { exec.run_stream(tx, stop_rx).await });
    let up = wait_event(&mut rx, c.wait_ms, |e| matches!(e, ExecEvent::StreamUp(_))).await;
    if !up {
        r.rec("private_stream", Outcome::Fail, "stream did not connect");
    }

    // 2. балансы
    match c.exec.balances().await {
        Ok(b) => {
            let avail: Decimal = b.iter().map(|x| x.available.0).sum();
            let ok = avail >= c.max_notional * Decimal::TWO;
            r.rec(
                "balances",
                if ok { Outcome::Pass } else { Outcome::Fail },
                format!("available {} across {} balances", avail.round_dp(2), b.len()),
            );
        }
        Err(e) => r.rec("balances", Outcome::Fail, e.to_string()),
    }

    let Some((bid, ask)) = (c.quote)().await else {
        r.rec("quote", Outcome::Fail, "no best bid/ask");
        let _ = stop_tx.send(true);
        let _ = stream.await;
        return r.out;
    };
    let lot = meta.lot_size;
    let qty = lot.0 * (c.max_notional / ask.0 / lot.0).floor();
    let min_ok = qty > Decimal::ZERO
        && meta.min_qty.is_none_or(|m| qty >= m.0)
        && meta.min_notional.is_none_or(|m| qty * ask.0 >= m.0);
    if !min_ok {
        r.rec("sizing", Outcome::Fail, format!("max notional {} is below the venue minimum", c.max_notional));
        let _ = stop_tx.send(true);
        let _ = stream.await;
        return r.out;
    }

    // 3. post-only далеко от рынка + отмена
    let po_px = meta.price_rule.round(Px(bid.0 * (Decimal::ONE - bps(300))), Rounding::Down);
    if let Some(px) = po_px {
        let cid = ids.next(now());
        let req = OrderRequest {
            cid,
            venue: c.venue.clone(),
            symbol: c.symbol.clone(),
            side: Side::Buy,
            qty: Qty(qty),
            px,
            tif: Tif::PostOnly,
            reduce_only: false,
        };
        let placed = c.exec.place(req).await;
        if r.res("post_only_place", &placed, format!("{qty} @ {}", px.0)) {
            let st = wait_status(c, cid, &[OrderStatus::Acked]).await;
            r.rec(
                "post_only_query",
                if st == Some(OrderStatus::Acked) { Outcome::Pass } else { Outcome::Fail },
                format!("{st:?}"),
            );
            let cancel = c.exec.cancel(c.venue.clone(), c.symbol.clone(), cid).await;
            r.res("cancel", &cancel, "cancelled");
            let st = wait_status(c, cid, &[OrderStatus::Cancelled]).await;
            r.rec(
                "cancel_confirmed",
                if st == Some(OrderStatus::Cancelled) { Outcome::Pass } else { Outcome::Fail },
                format!("{st:?}"),
            );
        }
    } else {
        r.rec("post_only_place", Outcome::Fail, "price rounding failed");
    }

    // 4–7. IOC, поток, запрос по клиентскому ID, позиция
    let ioc_cid = ids.next(now());
    let ioc_px = meta.price_rule.round(Px(ask.0 * (Decimal::ONE + bps(50))), Rounding::Down);
    let mut opened = false;
    if let Some(px) = ioc_px {
        let req = OrderRequest {
            cid: ioc_cid,
            venue: c.venue.clone(),
            symbol: c.symbol.clone(),
            side: Side::Buy,
            qty: Qty(qty),
            px,
            tif: Tif::Ioc,
            reduce_only: false,
        };
        let placed = c.exec.place(req).await;
        opened = r.res("ioc_place", &placed, format!("{qty} @ ≤{}", px.0));
        if opened {
            let got =
                wait_event(&mut rx, c.wait_ms, |e| matches!(e, ExecEvent::Fill(f) if f.cid == Some(ioc_cid))).await;
            r.rec(
                "stream_fill",
                if got { Outcome::Pass } else { Outcome::Fail },
                if got { "fill event received" } else { "no fill event" },
            );
            let st = wait_status(c, ioc_cid, &[OrderStatus::Filled]).await;
            r.rec(
                "query_by_client_id",
                if st == Some(OrderStatus::Filled) { Outcome::Pass } else { Outcome::Fail },
                format!("{st:?}"),
            );
            let mut p = Decimal::ZERO;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(c.wait_ms);
            while tokio::time::Instant::now() < deadline {
                p = position(c).await.unwrap_or_default();
                if p > Decimal::ZERO {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            r.rec("position", if p > Decimal::ZERO { Outcome::Pass } else { Outcome::Fail }, format!("position {p}"));
        }
    }

    // 8. закрытие reduce-only
    if opened {
        let closed = close_position(c, &ids, &meta).await;
        r.res("reduce_only_close", &closed, "reduce-only IOC sent");
        tokio::time::sleep(Duration::from_millis(c.wait_ms.min(3000))).await;
    } else {
        r.rec("reduce_only_close", Outcome::Skip, "no position opened");
    }

    // 9. фандинг
    let fp = c.exec.funding_payments(now() - 7 * 86_400_000).await;
    match &fp {
        Ok(v) => r.rec("funding_read", Outcome::Pass, format!("{} payments in 7 days", v.len())),
        Err(e) => r.rec("funding_read", Outcome::Fail, e.to_string()),
    }

    // 10. перевод между дексами HIP-3
    match &c.transfer {
        Some((from, to)) => {
            let amount = Usd(Decimal::ONE);
            let there = c.exec.transfer_between_dexes(from.clone(), to.clone(), amount).await;
            if r.res("transfer_out", &there, format!("$1 {from:?} -> {to:?}")) {
                let back = c.exec.transfer_between_dexes(to.clone(), from.clone(), amount).await;
                r.res("transfer_back", &back, format!("$1 {to:?} -> {from:?}"));
            }
        }
        None => r.rec("transfer", Outcome::Skip, "not requested (only Hyperliquid with --transfer-dex)"),
    }

    // 11. kill switch: отмена всех
    if let Some(px) = po_px {
        let cid = ids.next(now());
        let req = OrderRequest {
            cid,
            venue: c.venue.clone(),
            symbol: c.symbol.clone(),
            side: Side::Buy,
            qty: Qty(qty),
            px,
            tif: Tif::PostOnly,
            reduce_only: false,
        };
        if c.exec.place(req).await.is_ok() {
            let _ = wait_status(c, cid, &[OrderStatus::Acked]).await;
            let all = c.exec.cancel_all().await;
            r.res("kill_cancel_all", &all, format!("{:?} orders cancelled", all.as_ref().ok()));
            let st = wait_status(c, cid, &[OrderStatus::Cancelled]).await;
            r.rec(
                "kill_confirmed",
                if st == Some(OrderStatus::Cancelled) { Outcome::Pass } else { Outcome::Fail },
                format!("{st:?}"),
            );
        } else {
            r.rec("kill_cancel_all", Outcome::Fail, "could not place the test order");
        }
    }

    // 12. аккаунт без позиции и ордеров
    let _ = c.exec.cancel_all().await;
    if position(c).await.map(|p| !p.is_zero()).unwrap_or(true) {
        let _ = close_position(c, &ids, &meta).await;
        tokio::time::sleep(Duration::from_millis(c.wait_ms.min(3000))).await;
    }
    match position(c).await {
        Ok(p) if p.is_zero() => r.rec("flat", Outcome::Pass, "no position left"),
        Ok(p) => r.rec("flat", Outcome::Fail, format!("position {p} left: close it manually")),
        Err(e) => r.rec("flat", Outcome::Fail, e.to_string()),
    }
    let _ = stop_tx.send(true);
    let _ = stream.await;
    r.out
}

/// Таблица результатов.
pub fn table(results: &[StepResult]) -> String {
    let mut s = String::from("| venue | step | result | detail |\n| --- | --- | --- | --- |\n");
    for x in results {
        let o = match x.outcome {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Skip => "SKIP",
        };
        s.push_str(&format!("| {} | {} | {o} | {} |\n", x.venue, x.step, x.detail.replace('|', "/")));
    }
    let fails = results.iter().filter(|x| x.outcome == Outcome::Fail).count();
    s.push_str(&format!("\n{} steps, {fails} failed\n", results.len()));
    s
}

/// Параметры командной строки `selftest`.
#[derive(Debug, Clone)]
pub struct CliArgs {
    /// `testnet` или `mainnet`.
    pub network: String,
    /// Максимальный номинал ордера, $.
    pub max_notional_usd: Decimal,
    /// `all`, `hyperliquid`, `lighter` или `aster`.
    pub venue: String,
    /// Символы по площадкам.
    pub symbols: std::collections::BTreeMap<sh_core::Exchange, String>,
    /// Декс HIP-3 для проверки перевода (`--transfer-dex`).
    pub transfer_dex: Option<String>,
    /// Подтверждение для mainnet.
    pub real_money: bool,
}

/// Предельный номинал ордера selftest, $.
pub const MAX_SELFTEST_NOTIONAL: i64 = 1000;

fn d(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

/// Лучшие цены из публичного REST площадки (UNVERIFIED для Lighter и Aster v3).
fn quote_fn(ex: sh_core::Exchange, rest: String, symbol: String, market_id: Option<u64>) -> QuoteFn {
    let http = reqwest::Client::new();
    Arc::new(move || {
        let (http, rest, symbol) = (http.clone(), rest.clone(), symbol.clone());
        Box::pin(async move {
            let v: serde_json::Value = match ex {
                sh_core::Exchange::Hyperliquid => http
                    .post(format!("{rest}/info"))
                    .json(&serde_json::json!({"type": "l2Book", "coin": symbol}))
                    .send()
                    .await
                    .ok()?
                    .json()
                    .await
                    .ok()?,
                sh_core::Exchange::Lighter => http
                    .get(format!("{rest}/api/v1/orderBookOrders?market_id={}&limit=1", market_id?))
                    .send()
                    .await
                    .ok()?
                    .json()
                    .await
                    .ok()?,
                sh_core::Exchange::Aster => http
                    .get(format!("{rest}/fapi/v3/ticker/bookTicker?symbol={symbol}"))
                    .send()
                    .await
                    .ok()?
                    .json()
                    .await
                    .ok()?,
            };
            let (b, a) = match ex {
                sh_core::Exchange::Hyperliquid => (d(&v["levels"][0][0]["px"])?, d(&v["levels"][1][0]["px"])?),
                sh_core::Exchange::Lighter => (d(&v["bids"][0]["price"])?, d(&v["asks"][0]["price"])?),
                sh_core::Exchange::Aster => (d(&v["bidPrice"])?, d(&v["askPrice"])?),
            };
            Some((Px(b), Px(a)))
        })
    })
}

/// Запуск `selftest` оператором. Возвращает `true`, если ни один шаг не провален.
pub async fn run_cli(mut cfg: crate::config::AppConfig, a: CliArgs) -> anyhow::Result<bool> {
    use anyhow::Context;
    use sh_core::{Exchange, Secrets, SystemClock};
    use sh_venue_api::{MarketDataVenue, MetaRegistry, ProxyConfig, Sinks};

    let mainnet = match a.network.as_str() {
        "mainnet" => true,
        "testnet" => false,
        other => anyhow::bail!("--network must be testnet or mainnet, got '{other}'"),
    };
    if mainnet && !a.real_money {
        anyhow::bail!("selftest on mainnet places real orders: add --i-understand-real-money");
    }
    if a.max_notional_usd <= Decimal::ZERO || a.max_notional_usd > Decimal::from(MAX_SELFTEST_NOTIONAL) {
        anyhow::bail!("--max-notional-usd must be in (0, {MAX_SELFTEST_NOTIONAL}]");
    }
    crate::runner::install_crypto();
    let proxy: Option<ProxyConfig> = cfg.network.proxy().map_err(anyhow::Error::msg)?;
    let mut secrets = Secrets::load(&cfg.secrets.source().map_err(anyhow::Error::msg)?).context("secrets")?;
    if !mainnet {
        cfg.venues.hyperliquid.rest = "https://api.hyperliquid-testnet.xyz".into();
        cfg.venues.hyperliquid.ws = "wss://api.hyperliquid-testnet.xyz/ws".into();
        cfg.venues.lighter.rest = "https://testnet.zklighter.elliot.ai".into();
        cfg.venues.lighter.ws = "wss://testnet.zklighter.elliot.ai/stream?readonly=true".into();
        let aster = sh_venue_aster::exec::AsExecConfig::for_network(false);
        cfg.venues.aster.rest =
            if cfg.execution.aster_rest.is_empty() { aster.rest } else { cfg.execution.aster_rest.clone() };
    }
    let sinks = Sinks::new(None, None, Arc::new(SystemClock::new()));
    let want = |e: Exchange| a.venue == "all" || a.venue == e.as_str();
    let mut results = Vec::new();
    let wait_ms = 10_000;

    if want(Exchange::Hyperliquid) {
        let symbol = a.symbols.get(&Exchange::Hyperliquid).cloned().unwrap_or_else(|| "BTC".into());
        let metas = sh_venue_hyperliquid::HyperliquidVenue::new(cfg.venues.hyperliquid.clone(), proxy.clone())
            .fetch_metadata(&sinks)
            .await?;
        let reg: MetaRegistry = Arc::new(std::sync::RwLock::new(
            metas.iter().map(|m| ((m.venue.clone(), m.symbol.clone()), m.clone())).collect(),
        ));
        let (venue, _) = sh_venue_hyperliquid::exec::venue_of_coin(&symbol);
        let meta = metas.iter().find(|m| m.symbol == symbol).cloned();
        match secrets.hyperliquid.take() {
            Some(s) => {
                let mut ec = sh_venue_hyperliquid::exec::HlExecConfig::for_network(mainnet);
                ec.dexes.clone_from(&cfg.execution.hyperliquid_dexes);
                if let Some(dex) = &a.transfer_dex
                    && !ec.dexes.contains(dex)
                {
                    ec.dexes.push(dex.clone());
                }
                if !cfg.execution.hyperliquid_usdc_token.is_empty() {
                    ec.usdc_token.clone_from(&cfg.execution.hyperliquid_usdc_token);
                }
                let rest = ec.rest.clone();
                let exec = sh_venue_hyperliquid::exec::HlExecution::new(ec, &s, reg, proxy.clone())?;
                let check = VenueCheck {
                    exec: Arc::new(exec),
                    venue,
                    symbol: symbol.clone(),
                    meta,
                    quote: quote_fn(Exchange::Hyperliquid, rest, symbol, None),
                    max_notional: a.max_notional_usd,
                    transfer: a.transfer_dex.clone().map(|d| (String::new(), d)),
                    wait_ms,
                };
                results.extend(check_venue(&check).await);
            }
            None => results.push(StepResult {
                venue: "hyperliquid".into(),
                step: "secrets",
                outcome: Outcome::Fail,
                detail: "[hyperliquid] missing in secrets".into(),
            }),
        }
    }
    if want(Exchange::Lighter) {
        let symbol = a.symbols.get(&Exchange::Lighter).cloned().unwrap_or_else(|| "ETH".into());
        let metas = sh_venue_lighter::LighterVenue::new(cfg.venues.lighter.clone(), proxy.clone())
            .fetch_metadata(&sinks)
            .await?;
        let reg: MetaRegistry = Arc::new(std::sync::RwLock::new(
            metas.iter().map(|m| ((m.venue.clone(), m.symbol.clone()), m.clone())).collect(),
        ));
        let meta = metas.iter().find(|m| m.symbol == symbol).cloned();
        match secrets.lighter.take() {
            Some(s) => {
                let ec = sh_venue_lighter::exec::LtExecConfig::for_network(
                    mainnet,
                    cfg.execution.lighter_signer_lib.clone().into(),
                );
                let rest = ec.rest.clone();
                let exec = sh_venue_lighter::exec::LtExecution::new(ec, &s, reg, proxy.clone())?;
                let check = VenueCheck {
                    exec: Arc::new(exec),
                    venue: VenueKey::new(Exchange::Lighter),
                    symbol: symbol.clone(),
                    quote: quote_fn(Exchange::Lighter, rest, symbol, meta.as_ref().and_then(|m| m.venue_id)),
                    meta,
                    max_notional: a.max_notional_usd,
                    transfer: None,
                    wait_ms,
                };
                results.extend(check_venue(&check).await);
            }
            None => results.push(StepResult {
                venue: "lighter".into(),
                step: "secrets",
                outcome: Outcome::Fail,
                detail: "[lighter] missing in secrets".into(),
            }),
        }
    }
    if want(Exchange::Aster) {
        let symbol = a.symbols.get(&Exchange::Aster).cloned().unwrap_or_else(|| "BTCUSDT".into());
        let metas =
            sh_venue_aster::AsterVenue::new(cfg.venues.aster.clone(), proxy.clone()).fetch_metadata(&sinks).await?;
        let meta = metas.iter().find(|m| m.symbol == symbol).cloned();
        match secrets.aster.take() {
            Some(s) => {
                let mut ec = sh_venue_aster::exec::AsExecConfig::for_network(mainnet);
                if !cfg.execution.aster_rest.is_empty() {
                    ec.rest.clone_from(&cfg.execution.aster_rest);
                }
                if !cfg.execution.aster_ws.is_empty() {
                    ec.ws.clone_from(&cfg.execution.aster_ws);
                }
                let rest = ec.rest.clone();
                let exec = sh_venue_aster::exec::AsExecution::new(ec, &s, proxy.clone())?;
                let check = VenueCheck {
                    exec: Arc::new(exec),
                    venue: VenueKey::new(Exchange::Aster),
                    symbol: symbol.clone(),
                    meta,
                    quote: quote_fn(Exchange::Aster, rest, symbol, None),
                    max_notional: a.max_notional_usd,
                    transfer: None,
                    wait_ms,
                };
                results.extend(check_venue(&check).await);
            }
            None => results.push(StepResult {
                venue: "aster".into(),
                step: "secrets",
                outcome: Outcome::Fail,
                detail: "[aster] missing in secrets".into(),
            }),
        }
    }
    let t = table(&results);
    println!("{t}");
    let path = std::path::Path::new(&cfg.scanner.report_dir).join(format!(
        "selftest-{}-{}.md",
        a.network,
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    crate::runner::write_atomic(&path, &t)?;
    println!("report: {}", path.display());
    Ok(results.iter().all(|r| r.outcome != Outcome::Fail))
}
