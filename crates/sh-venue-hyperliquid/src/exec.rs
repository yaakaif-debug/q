//! Приватный адаптер Hyperliquid (ядро и все HIP-3 дексы): ордера, отмены, запросы,
//! позиции, балансы, фандинг, перевод между дексами и приватный поток.
//!
//! Все дексы идут через одного подписанта (агент-кошелёк) и один менеджер nonce
//! (миллисекунды + счётчик, строго возрастает). Агент не может выводить средства.
//! Форматы — по документации «Exchange endpoint», «Info endpoint», «Websocket»;
//! расхождения с живым API помечены UNVERIFIED в docs/venues/hyperliquid.md.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rust_decimal::Decimal;
use serde_json::{Value, json};
use sh_core::secrets::HyperliquidSecrets;
use sh_core::{Collateral, Exchange, Px, Qty, Side, Usd, VenueKey};
use sh_venue_api::private_ws::{PrivateStream, map_reqwest_error, run_private};
use sh_venue_api::{
    ClientId, ExecError, ExecEvent, ExecFuture, Execution, Fill, FundingPayment, MetaRegistry, OrderRequest,
    OrderStatus, OrderUpdate, ProxyConfig, Tif, VenueBalance, VenuePosition, WeightLimiter, meta_of,
};
use tokio::sync::{mpsc, watch};

use crate::sign::{L1Signer, Mp, decimal_to_wire};

/// Настройки исполнения Hyperliquid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlExecConfig {
    /// Mainnet (`true`) или testnet.
    pub mainnet: bool,
    /// REST.
    pub rest: String,
    /// WebSocket.
    pub ws: String,
    /// Дексы для запросов позиций и балансов (`""` — ядро).
    pub dexes: Vec<String>,
    /// Токен обеспечения для `agentSendAsset` (`USDC:0x…`, UNVERIFIED для testnet).
    pub usdc_token: String,
    /// Только построить и подписать, не отправлять (`live --dry-run`).
    pub dry_run: bool,
}

impl HlExecConfig {
    /// Значения по умолчанию для сети.
    pub fn for_network(mainnet: bool) -> Self {
        if mainnet {
            Self {
                mainnet,
                rest: "https://api.hyperliquid.xyz".into(),
                ws: "wss://api.hyperliquid.xyz/ws".into(),
                dexes: vec![String::new()],
                usdc_token: "USDC:0x6d1e7cde53ba9467b783cb7c530ce054".into(),
                dry_run: false,
            }
        } else {
            Self {
                mainnet,
                rest: "https://api.hyperliquid-testnet.xyz".into(),
                ws: "wss://api.hyperliquid-testnet.xyz/ws".into(),
                dexes: vec![String::new()],
                usdc_token: "USDC:0xeb62eee3685fc4c43992febcd9e75443".into(),
                dry_run: false,
            }
        }
    }
}

/// Менеджер nonce: строго возрастающие миллисекунды, общий для ядра и всех дексов.
#[derive(Debug, Default)]
pub struct NonceManager {
    last: AtomicU64,
}

impl NonceManager {
    /// Следующий nonce не меньше `now_ms`.
    pub fn next(&self, now_ms: u64) -> u64 {
        let mut cur = self.last.load(Ordering::SeqCst);
        loop {
            let n = now_ms.max(cur + 1);
            match self.last.compare_exchange(cur, n, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return n,
                Err(v) => cur = v,
            }
        }
    }
}

/// Ключ площадки и символ по имени монеты Hyperliquid (`BTC` или `xyz:NVDA`).
pub fn venue_of_coin(coin: &str) -> (VenueKey, String) {
    match coin.split_once(':') {
        Some((dex, _)) => (VenueKey::hyperliquid(dex), coin.to_owned()),
        None => (VenueKey::hyperliquid(""), coin.to_owned()),
    }
}

fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

/// Действие `order` для одного ордера.
pub fn order_action(asset: u64, req: &OrderRequest) -> Mp {
    let tif = match req.tif {
        Tif::PostOnly => "Alo",
        Tif::Ioc => "Ioc",
        Tif::Gtc => "Gtc",
    };
    let o = Mp::Map(vec![
        ("a".into(), Mp::UInt(asset)),
        ("b".into(), Mp::Bool(req.side == Side::Buy)),
        ("p".into(), Mp::s(decimal_to_wire(req.px.0))),
        ("s".into(), Mp::s(decimal_to_wire(req.qty.0))),
        ("r".into(), Mp::Bool(req.reduce_only)),
        ("t".into(), Mp::map([("limit", Mp::map([("tif", Mp::s(tif))]))])),
        ("c".into(), Mp::s(req.cid.hl_cloid())),
    ]);
    Mp::map([("type", Mp::s("order")), ("orders", Mp::Arr(vec![o])), ("grouping", Mp::s("na"))])
}

/// Действие `cancelByCloid`.
pub fn cancel_action(asset: u64, cid: ClientId) -> Mp {
    Mp::map([
        ("type", Mp::s("cancelByCloid")),
        ("cancels", Mp::Arr(vec![Mp::map([("asset", Mp::UInt(asset)), ("cloid", Mp::s(cid.hl_cloid()))])])),
    ])
}

/// Действие `cancel` по биржевым идентификаторам.
pub fn cancel_oids_action(items: &[(u64, u64)]) -> Mp {
    let cancels = items.iter().map(|(a, o)| Mp::map([("a", Mp::UInt(*a)), ("o", Mp::UInt(*o))])).collect();
    Mp::map([("type", Mp::s("cancel")), ("cancels", Mp::Arr(cancels))])
}

/// Действие `agentSendAsset` (перевод между дексами на свой же адрес). Порядок ключей —
/// как в документации (UNVERIFIED: в SDK нет тест-вектора для этого действия).
pub fn agent_send_asset_action(
    destination: &str,
    from_dex: &str,
    to_dex: &str,
    token: &str,
    amount: Decimal,
    nonce: u64,
) -> Mp {
    Mp::map([
        ("type", Mp::s("agentSendAsset")),
        ("destination", Mp::s(destination.to_ascii_lowercase())),
        ("sourceDex", Mp::s(from_dex)),
        ("destinationDex", Mp::s(to_dex)),
        ("token", Mp::s(token)),
        ("amount", Mp::s(decimal_to_wire(amount))),
        ("fromSubAccount", Mp::s("")),
        ("nonce", Mp::UInt(nonce)),
    ])
}

/// Разбор ответа `/exchange` на ордер: первое состояние.
pub fn parse_order_response(body: &str, req: &OrderRequest, now_ms: i64) -> Result<OrderUpdate, ExecError> {
    let v: Value = serde_json::from_str(body).map_err(|_| ExecError::Rejected("bad exchange response".into()))?;
    if v["status"] != "ok" {
        let msg = v["response"].as_str().unwrap_or("error").chars().take(200).collect::<String>();
        return Err(ExecError::Rejected(msg));
    }
    let st = &v["response"]["data"]["statuses"][0];
    let base = |status, filled: Decimal, avg: Option<Px>, oid: Option<String>| OrderUpdate {
        cid: req.cid,
        venue: req.venue.clone(),
        symbol: req.symbol.clone(),
        status,
        filled_qty: Qty(filled),
        avg_px: avg,
        venue_order_id: oid,
        reason: None,
        ts_ms: now_ms,
    };
    if let Some(e) = st["error"].as_str() {
        return Err(ExecError::Rejected(e.chars().take(200).collect()));
    }
    if let Some(r) = st.get("resting") {
        return Ok(base(OrderStatus::Acked, Decimal::ZERO, None, r["oid"].as_u64().map(|o| o.to_string())));
    }
    if let Some(f) = st.get("filled") {
        let total = dec(&f["totalSz"]).unwrap_or_default();
        let status = if total >= req.qty.0 { OrderStatus::Filled } else { OrderStatus::Cancelled };
        return Ok(base(status, total, dec(&f["avgPx"]).map(Px), f["oid"].as_u64().map(|o| o.to_string())));
    }
    Err(ExecError::Rejected("unrecognised order status".into()))
}

/// Состояние ордера Hyperliquid.
pub fn map_status(s: &str) -> OrderStatus {
    match s {
        "open" | "triggered" => OrderStatus::Acked,
        "filled" => OrderStatus::Filled,
        "rejected" | "marginCanceled" | "perpMarginRejected" | "reduceOnlyRejected" => OrderStatus::Rejected,
        s if s.contains("anceled") || s.contains("Rejected") => OrderStatus::Cancelled,
        _ => OrderStatus::Unknown,
    }
}

/// Разбор `orderStatus` (запрос по cloid).
pub fn parse_order_status(body: &str, now_ms: i64) -> Result<Option<OrderUpdate>, ExecError> {
    let v: Value = serde_json::from_str(body).map_err(|_| ExecError::Rejected("bad info response".into()))?;
    if v["status"] == "unknownOid" {
        return Ok(None);
    }
    let o = &v["order"]["order"];
    let coin = o["coin"].as_str().unwrap_or_default();
    let (venue, symbol) = venue_of_coin(coin);
    let Some(cid) = o["cloid"].as_str().and_then(ClientId::from_hl_cloid) else { return Ok(None) };
    let orig = dec(&o["origSz"]).unwrap_or_default();
    let left = dec(&o["sz"]).unwrap_or_default();
    Ok(Some(OrderUpdate {
        cid,
        venue,
        symbol,
        status: map_status(v["order"]["status"].as_str().unwrap_or_default()),
        filled_qty: Qty(orig - left),
        avg_px: None,
        venue_order_id: o["oid"].as_u64().map(|x| x.to_string()),
        reason: None,
        ts_ms: v["order"]["statusTimestamp"].as_i64().unwrap_or(now_ms),
    }))
}

/// Разбор сообщения приватного WS (`orderUpdates`, `userFills`, `userFundings`).
pub fn parse_ws(text: &str) -> Vec<ExecEvent> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let mut out = Vec::new();
    match v["channel"].as_str() {
        Some("orderUpdates") => {
            for u in v["data"].as_array().into_iter().flatten() {
                let o = &u["order"];
                let Some(cid) = o["cloid"].as_str().and_then(ClientId::from_hl_cloid) else { continue };
                let (venue, symbol) = venue_of_coin(o["coin"].as_str().unwrap_or_default());
                let orig = dec(&o["origSz"]).unwrap_or_default();
                let left = dec(&o["sz"]).unwrap_or_default();
                out.push(ExecEvent::Order(OrderUpdate {
                    cid,
                    venue,
                    symbol,
                    status: map_status(u["status"].as_str().unwrap_or_default()),
                    filled_qty: Qty(orig - left),
                    avg_px: None,
                    venue_order_id: o["oid"].as_u64().map(|x| x.to_string()),
                    reason: u["status"].as_str().map(str::to_owned),
                    ts_ms: u["statusTimestamp"].as_i64().unwrap_or_default(),
                }));
            }
        }
        Some("userFills") => {
            // снимок при подписке содержит историю: её не применяем
            if v["data"]["isSnapshot"].as_bool() == Some(true) {
                return out;
            }
            for f in v["data"]["fills"].as_array().into_iter().flatten() {
                let coin = f["coin"].as_str().unwrap_or_default();
                let (venue, symbol) = venue_of_coin(coin);
                let side = if f["side"] == "B" { Side::Buy } else { Side::Sell };
                out.push(ExecEvent::Fill(Fill {
                    cid: f["cloid"].as_str().and_then(ClientId::from_hl_cloid),
                    venue,
                    symbol,
                    side,
                    qty: Qty(dec(&f["sz"]).unwrap_or_default()),
                    px: Px(dec(&f["px"]).unwrap_or_default()),
                    fee: Usd(dec(&f["fee"]).unwrap_or_default()),
                    maker: f["crossed"].as_bool() == Some(false),
                    trade_id: f["tid"].as_u64().map(|t| t.to_string()).unwrap_or_else(|| f["hash"].to_string()),
                    ts_ms: f["time"].as_i64().unwrap_or_default(),
                    liquidation: f.get("liquidation").is_some_and(|l| !l.is_null()),
                }));
            }
        }
        Some("userFundings") => {
            if v["data"]["isSnapshot"].as_bool() == Some(true) {
                return out;
            }
            for f in v["data"]["fundings"].as_array().into_iter().flatten() {
                let (venue, symbol) = venue_of_coin(f["coin"].as_str().unwrap_or_default());
                out.push(ExecEvent::Funding(FundingPayment {
                    venue,
                    symbol,
                    amount: Usd(dec(&f["usdc"]).unwrap_or_default()),
                    rate: dec(&f["fundingRate"]),
                    ts_ms: f["time"].as_i64().unwrap_or_default(),
                }));
            }
        }
        _ => {}
    }
    out
}

/// Разбор `clearinghouseState` декса: позиции и баланс.
pub fn parse_clearinghouse(body: &str, dex: &str) -> Result<(Vec<VenuePosition>, VenueBalance), ExecError> {
    let v: Value = serde_json::from_str(body).map_err(|_| ExecError::Rejected("bad clearinghouseState".into()))?;
    let venue = VenueKey::hyperliquid(dex);
    let mut pos = Vec::new();
    for p in v["assetPositions"].as_array().into_iter().flatten() {
        let p = &p["position"];
        let qty = dec(&p["szi"]).unwrap_or_default();
        if qty.is_zero() {
            continue;
        }
        pos.push(VenuePosition {
            venue: venue.clone(),
            symbol: p["coin"].as_str().unwrap_or_default().to_owned(),
            qty,
            entry_px: dec(&p["entryPx"]).map(Px),
            liq_px: dec(&p["liquidationPx"]).map(Px),
            margin: dec(&p["marginUsed"]).map(Usd),
        });
    }
    let ms = &v["marginSummary"];
    let equity = dec(&ms["accountValue"]).unwrap_or_default();
    let bal = VenueBalance {
        venue,
        collateral: Collateral::Usdc,
        equity: Usd(equity),
        available: Usd(dec(&v["withdrawable"]).unwrap_or_default()),
        maintenance_margin: dec(&v["crossMaintenanceMarginUsed"]).map(Usd),
    };
    Ok((pos, bal))
}

/// Исполнение Hyperliquid.
pub struct HlExecution {
    cfg: HlExecConfig,
    http: reqwest::Client,
    signer: Arc<L1Signer>,
    account: String,
    nonce: Arc<NonceManager>,
    meta: MetaRegistry,
    limiter: WeightLimiter,
    proxy: Option<ProxyConfig>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl HlExecution {
    /// Адаптер из секретов. Ключ агента остаётся в подписанте и нигде не выводится.
    pub fn new(
        cfg: HlExecConfig,
        secrets: &HyperliquidSecrets,
        meta: MetaRegistry,
        proxy: Option<ProxyConfig>,
    ) -> Result<Self, ExecError> {
        let signer =
            L1Signer::new(&secrets.agent_private_key, cfg.mainnet).map_err(|e| ExecError::Auth(e.to_string()))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ExecError::Transport(e.to_string()))?;
        Ok(Self {
            cfg,
            http,
            signer: Arc::new(signer),
            account: secrets.account_address.to_ascii_lowercase(),
            nonce: Arc::new(NonceManager::default()),
            meta,
            // адрес и IP делят лимит: 1200 веса в минуту на все дексы (docs/venues/hyperliquid.md)
            limiter: WeightLimiter::per_minute(1200),
            proxy,
        })
    }

    fn asset(&self, venue: &VenueKey, symbol: &str) -> Result<u64, ExecError> {
        meta_of(&self.meta, venue, symbol)
            .and_then(|m| m.venue_id)
            .ok_or_else(|| ExecError::Invalid(format!("unknown instrument {venue}/{symbol}")))
    }

    async fn info(&self, body: Value, weight: u32) -> Result<String, ExecError> {
        self.limiter.acquire(weight).await;
        let r = self
            .http
            .post(format!("{}/info", self.cfg.rest))
            .json(&body)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        if r.status().as_u16() == 429 {
            return Err(ExecError::RateLimited);
        }
        if !r.status().is_success() {
            return Err(ExecError::Rejected(format!("info status {}", r.status())));
        }
        r.text().await.map_err(|e| map_reqwest_error(&e))
    }

    async fn exchange(&self, action: Mp, nonce: u64) -> Result<String, ExecError> {
        let sig = self.signer.sign_l1(&action, None, nonce, None).map_err(|e| ExecError::Auth(e.to_string()))?;
        let body = json!({ "action": action.to_json(), "nonce": nonce, "signature": sig.to_json() });
        if self.cfg.dry_run {
            tracing::info!(action = %action.to_json(), nonce, "dry run: signed action not sent");
            return Err(ExecError::NotSupported("dry run".into()));
        }
        self.limiter.acquire(1).await;
        let r = self
            .http
            .post(format!("{}/exchange", self.cfg.rest))
            .json(&body)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        if r.status().as_u16() == 429 {
            return Err(ExecError::RateLimited);
        }
        r.text().await.map_err(|e| map_reqwest_error(&e))
    }

    fn next_nonce(&self) -> u64 {
        self.nonce.next(u64::try_from(now_ms()).unwrap_or(0))
    }
}

impl Execution for HlExecution {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    fn place(&self, req: OrderRequest) -> ExecFuture<'_, OrderUpdate> {
        Box::pin(async move {
            let asset = self.asset(&req.venue, &req.symbol)?;
            let body = self.exchange(order_action(asset, &req), self.next_nonce()).await?;
            parse_order_response(&body, &req, now_ms())
        })
    }

    fn cancel(&self, venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            let asset = self.asset(&venue, &symbol)?;
            let body = self.exchange(cancel_action(asset, cid), self.next_nonce()).await?;
            let v: Value = serde_json::from_str(&body).map_err(|_| ExecError::Rejected("bad response".into()))?;
            if v["status"] == "ok" && v["response"]["data"]["statuses"][0] == "success" {
                Ok(())
            } else {
                Err(ExecError::Rejected(
                    v["response"]["data"]["statuses"][0]["error"].as_str().unwrap_or("cancel failed").into(),
                ))
            }
        })
    }

    fn cancel_all(&self) -> ExecFuture<'_, u32> {
        Box::pin(async move {
            let mut items = Vec::new();
            for dex in &self.cfg.dexes {
                let body = self.info(json!({"type": "openOrders", "user": self.account, "dex": dex}), 20).await?;
                let v: Value = serde_json::from_str(&body).map_err(|_| ExecError::Rejected("bad openOrders".into()))?;
                for o in v.as_array().into_iter().flatten() {
                    let (venue, symbol) = venue_of_coin(o["coin"].as_str().unwrap_or_default());
                    if let (Ok(a), Some(oid)) = (self.asset(&venue, &symbol), o["oid"].as_u64()) {
                        items.push((a, oid));
                    }
                }
            }
            if items.is_empty() {
                return Ok(0);
            }
            let n = u32::try_from(items.len()).unwrap_or(u32::MAX);
            self.exchange(cancel_oids_action(&items), self.next_nonce()).await?;
            Ok(n)
        })
    }

    fn query_order(&self, _venue: VenueKey, _symbol: String, cid: ClientId) -> ExecFuture<'_, Option<OrderUpdate>> {
        Box::pin(async move {
            let body =
                self.info(json!({"type": "orderStatus", "user": self.account, "oid": cid.hl_cloid()}), 2).await?;
            parse_order_status(&body, now_ms())
        })
    }

    fn positions(&self) -> ExecFuture<'_, Vec<VenuePosition>> {
        Box::pin(async move {
            let mut out = Vec::new();
            for dex in &self.cfg.dexes {
                let body =
                    self.info(json!({"type": "clearinghouseState", "user": self.account, "dex": dex}), 2).await?;
                out.extend(parse_clearinghouse(&body, dex)?.0);
            }
            Ok(out)
        })
    }

    fn balances(&self) -> ExecFuture<'_, Vec<VenueBalance>> {
        Box::pin(async move {
            let mut out = Vec::new();
            for dex in &self.cfg.dexes {
                let body =
                    self.info(json!({"type": "clearinghouseState", "user": self.account, "dex": dex}), 2).await?;
                out.push(parse_clearinghouse(&body, dex)?.1);
            }
            Ok(out)
        })
    }

    fn funding_payments(&self, since_ms: i64) -> ExecFuture<'_, Vec<FundingPayment>> {
        Box::pin(async move {
            let body =
                self.info(json!({"type": "userFunding", "user": self.account, "startTime": since_ms}), 20).await?;
            let v: Value = serde_json::from_str(&body).map_err(|_| ExecError::Rejected("bad userFunding".into()))?;
            Ok(v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| {
                    let d = &e["delta"];
                    let (venue, symbol) = venue_of_coin(d["coin"].as_str()?);
                    Some(FundingPayment {
                        venue,
                        symbol,
                        amount: Usd(dec(&d["usdc"])?),
                        rate: dec(&d["fundingRate"]),
                        ts_ms: e["time"].as_i64()?,
                    })
                })
                .collect())
        })
    }

    fn transfer_between_dexes(&self, from_dex: String, to_dex: String, amount: Usd) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            let nonce = self.next_nonce();
            let a = agent_send_asset_action(&self.account, &from_dex, &to_dex, &self.cfg.usdc_token, amount.0, nonce);
            let body = self.exchange(a, nonce).await?;
            let v: Value = serde_json::from_str(&body).map_err(|_| ExecError::Rejected("bad response".into()))?;
            if v["status"] == "ok" {
                Ok(())
            } else {
                Err(ExecError::Rejected(
                    v["response"].as_str().unwrap_or("transfer failed").chars().take(200).collect(),
                ))
            }
        })
    }

    fn run_stream(&self, tx: mpsc::Sender<ExecEvent>, shutdown: watch::Receiver<bool>) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            let s = HlStream { url: self.cfg.ws.clone(), user: self.account.clone() };
            run_private(Exchange::Hyperliquid, s, self.proxy.clone(), tx, shutdown).await;
            Ok(())
        })
    }
}

struct HlStream {
    url: String,
    user: String,
}

impl PrivateStream for HlStream {
    async fn url(&mut self) -> Result<String, String> {
        Ok(self.url.clone())
    }

    async fn subscribe(&mut self) -> Result<Vec<String>, String> {
        Ok(["orderUpdates", "userFills", "userFundings"]
            .iter()
            .map(|t| json!({"method": "subscribe", "subscription": {"type": t, "user": self.user}}).to_string())
            .collect())
    }

    fn parse(&mut self, text: &str) -> Vec<ExecEvent> {
        parse_ws(text)
    }

    fn ping(&self) -> Option<String> {
        Some(json!({"method": "ping"}).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn req(tif: Tif) -> OrderRequest {
        OrderRequest {
            cid: ClientId(1),
            venue: VenueKey::hyperliquid("xyz"),
            symbol: "xyz:NVDA".into(),
            side: Side::Buy,
            qty: Qty(dec!(2.50)),
            px: Px(dec!(180.10)),
            tif,
            reduce_only: false,
        }
    }

    #[test]
    fn nonces_strictly_increase() {
        let n = NonceManager::default();
        let a = n.next(1000);
        let b = n.next(1000);
        let c = n.next(999);
        assert!(a < b && b < c);
        assert_eq!(n.next(5000), 5000);
    }

    #[test]
    fn order_action_wire_format() {
        let a = order_action(110_003, &req(Tif::PostOnly)).to_json();
        assert_eq!(a["orders"][0]["t"]["limit"]["tif"], "Alo");
        assert_eq!(a["orders"][0]["p"], "180.1");
        assert_eq!(a["orders"][0]["s"], "2.5");
        assert_eq!(a["orders"][0]["c"], "0x00000000000000000000000000000001");
        assert_eq!(a["grouping"], "na");
    }

    #[test]
    fn order_responses_from_docs() {
        let r = req(Tif::Gtc);
        let resting =
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":77738308}}]}}}"#;
        assert_eq!(parse_order_response(resting, &r, 0).unwrap().status, OrderStatus::Acked);
        let filled = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"totalSz":"2.5","avgPx":"180.1","oid":77747314}}]}}}"#;
        let u = parse_order_response(filled, &r, 0).unwrap();
        assert_eq!((u.status, u.filled_qty.0), (OrderStatus::Filled, dec!(2.5)));
        let err = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order must have minimum value of $10."}]}}}"#;
        assert!(matches!(parse_order_response(err, &r, 0), Err(ExecError::Rejected(_))));
    }

    #[test]
    fn ws_fills_and_order_updates() {
        let fills = r#"{"channel":"userFills","data":{"isSnapshot":false,"user":"0x1","fills":[{"coin":"xyz:NVDA","px":"180.1","sz":"1.0","side":"B","time":1700000000000,"startPosition":"0","dir":"Open Long","closedPnl":"0","hash":"0xabc","oid":1,"crossed":true,"fee":"0.05","tid":42,"cloid":"0x00000000000000000000000000000001","feeToken":"USDC"}]}}"#;
        let ev = parse_ws(fills);
        let ExecEvent::Fill(f) = &ev[0] else { panic!("fill expected") };
        assert_eq!(f.venue, VenueKey::hyperliquid("xyz"));
        assert_eq!(f.cid, Some(ClientId(1)));
        assert!(!f.maker);
        let snap = r#"{"channel":"userFills","data":{"isSnapshot":true,"user":"0x1","fills":[{"coin":"BTC"}]}}"#;
        assert!(parse_ws(snap).is_empty());
        let ou = r#"{"channel":"orderUpdates","data":[{"order":{"coin":"BTC","side":"A","limitPx":"60000","sz":"0","oid":5,"timestamp":1,"origSz":"0.1","cloid":"0x00000000000000000000000000000002"},"status":"filled","statusTimestamp":2}]}"#;
        let ev = parse_ws(ou);
        let ExecEvent::Order(u) = &ev[0] else { panic!("order expected") };
        assert_eq!((u.status, u.filled_qty.0), (OrderStatus::Filled, dec!(0.1)));
    }

    #[test]
    fn clearinghouse_state() {
        let body = r#"{"assetPositions":[{"position":{"coin":"ETH","szi":"-1.5","entryPx":"3000","liquidationPx":"4000","marginUsed":"1500"},"type":"oneWay"}],"marginSummary":{"accountValue":"10000","totalMarginUsed":"1500"},"crossMaintenanceMarginUsed":"300","withdrawable":"8500"}"#;
        let (p, b) = parse_clearinghouse(body, "").unwrap();
        assert_eq!(p[0].qty, dec!(-1.5));
        assert_eq!(p[0].liq_px, Some(Px(dec!(4000))));
        assert_eq!(b.equity.0, dec!(10000));
        assert_eq!(b.available.0, dec!(8500));
    }

    #[test]
    fn statuses() {
        assert_eq!(map_status("canceled"), OrderStatus::Cancelled);
        assert_eq!(map_status("reduceOnlyCanceled"), OrderStatus::Cancelled);
        assert_eq!(map_status("open"), OrderStatus::Acked);
        assert_eq!(map_status("rejected"), OrderStatus::Rejected);
    }
}
