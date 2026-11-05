//! Приватный адаптер Lighter: ордера через подписанта `lighter-go` и `POST /api/v1/sendTx`,
//! nonce по API-ключу (начальное значение из `nextNonce`, дальше локальный счётчик,
//! пересинхронизация при отказе), токен авторизации для приватных запросов и каналов.
//!
//! Форматы — по apidocs.lighter.xyz (sendTx, accountActiveOrders, account, WS
//! `account_all_*`). Расхождения помечены UNVERIFIED в docs/venues/lighter.md.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sh_core::secrets::LighterSecrets;
use sh_core::{Collateral, Exchange, Px, Qty, Side, Usd, VenueKey};
use sh_venue_api::private_ws::{PrivateStream, map_reqwest_error, run_private};
use sh_venue_api::{
    ClientId, ExecError, ExecEvent, ExecFuture, Execution, Fill, FundingPayment, InstrumentMeta, MetaRegistry,
    OrderRequest, OrderStatus, OrderUpdate, PriceRule, ProxyConfig, Tif, VenueBalance, VenuePosition, WeightLimiter,
};
use tokio::sync::{Mutex, mpsc, watch};

use crate::signer::{LighterSigner, OrderParams, tif};

/// Настройки исполнения Lighter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LtExecConfig {
    /// REST.
    pub rest: String,
    /// WebSocket (без `readonly`).
    pub ws: String,
    /// Chain ID: testnet 300, mainnet 304 (UNVERIFIED для mainnet).
    pub chain_id: i32,
    /// Путь к библиотеке подписанта.
    pub signer_lib: PathBuf,
    /// Только подписать, не отправлять.
    pub dry_run: bool,
}

impl LtExecConfig {
    /// Значения по умолчанию для сети.
    pub fn for_network(mainnet: bool, signer_lib: PathBuf) -> Self {
        let host = if mainnet { "mainnet.zklighter.elliot.ai" } else { "testnet.zklighter.elliot.ai" };
        Self {
            rest: format!("https://{host}"),
            ws: format!("wss://{host}/stream"),
            chain_id: if mainnet { 304 } else { 300 },
            signer_lib,
            dry_run: false,
        }
    }
}

/// Число десятичных знаков шага (`0.01` → 2).
pub fn decimals_of(step: Decimal) -> Option<u32> {
    let n = step.normalize();
    // шаг должен быть степенью десяти
    if n <= Decimal::ZERO || n.mantissa().unsigned_abs() != 1 {
        return None;
    }
    Some(n.scale())
}

/// Перевод количества и цены в целые единицы площадки.
pub fn to_units(meta: &InstrumentMeta, qty: Qty, px: Px) -> Result<(i64, i32), ExecError> {
    let PriceRule::Tick { tick } = &meta.price_rule else {
        return Err(ExecError::Invalid("lighter price rule must be a tick".into()));
    };
    let (Some(sd), Some(pd)) = (decimals_of(meta.lot_size.0), decimals_of(tick.0)) else {
        return Err(ExecError::Invalid("lot or tick is not a power of ten".into()));
    };
    let base = qty.0 * Decimal::from(10u64.pow(sd));
    let price = px.0 * Decimal::from(10u64.pow(pd));
    if base.fract() != Decimal::ZERO || price.fract() != Decimal::ZERO {
        return Err(ExecError::Invalid("size or price off grid".into()));
    }
    let base =
        i64::try_from(base.mantissa() / 10i128.pow(base.scale())).map_err(|_| ExecError::Invalid("size".into()))?;
    let price =
        i32::try_from(price.mantissa() / 10i128.pow(price.scale())).map_err(|_| ExecError::Invalid("price".into()))?;
    Ok((base, price))
}

fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

/// Состояние ордера Lighter.
pub fn map_status(s: &str) -> OrderStatus {
    match s {
        "open" | "pending" | "in-progress" => OrderStatus::Acked,
        "filled" => OrderStatus::Filled,
        "canceled-post-only"
        | "canceled-margin-not-allowed"
        | "canceled-position-not-allowed"
        | "canceled-invalid-balance" => OrderStatus::Rejected,
        "canceled-expired" => OrderStatus::Expired,
        s if s.starts_with("canceled") => OrderStatus::Cancelled,
        _ => OrderStatus::Unknown,
    }
}

/// Ордер из ответа REST или WS.
pub fn parse_order(o: &Value, symbol_of: &dyn Fn(i64) -> Option<String>) -> Option<OrderUpdate> {
    let cid = ClientId(u64::try_from(o["client_order_index"].as_i64()?).ok()?);
    let symbol = symbol_of(o["market_index"].as_i64()?)?;
    Some(OrderUpdate {
        cid,
        venue: VenueKey::new(Exchange::Lighter),
        symbol,
        status: map_status(o["status"].as_str().unwrap_or_default()),
        filled_qty: Qty(dec(&o["filled_base_amount"]).unwrap_or_default()),
        avg_px: {
            let b = dec(&o["filled_base_amount"]).unwrap_or_default();
            let q = dec(&o["filled_quote_amount"]).unwrap_or_default();
            (b > Decimal::ZERO).then(|| Px(q / b))
        },
        venue_order_id: o["order_index"].as_i64().map(|x| x.to_string()),
        reason: o["status"].as_str().map(str::to_owned),
        ts_ms: o["updated_at"].as_i64().or_else(|| o["timestamp"].as_i64()).unwrap_or_default(),
    })
}

/// Сделка канала `account_all_trades` с точки зрения нашего аккаунта.
pub fn parse_trade(t: &Value, account: i64, symbol_of: &dyn Fn(i64) -> Option<String>) -> Option<Fill> {
    let symbol = symbol_of(t["market_id"].as_i64()?)?;
    let we_bid = t["bid_account_id"].as_i64() == Some(account);
    let we_ask = t["ask_account_id"].as_i64() == Some(account);
    if !we_bid && !we_ask {
        return None;
    }
    let side = if we_bid { Side::Buy } else { Side::Sell };
    let cid_v = if we_bid { &t["bid_client_id"] } else { &t["ask_client_id"] };
    let maker_ask = t["is_maker_ask"].as_bool().unwrap_or(false);
    let maker = (maker_ask && we_ask) || (!maker_ask && we_bid);
    let usd = dec(&t["usd_amount"]).unwrap_or_default();
    // комиссия — целое число в миллионных долях номинала (UNVERIFIED)
    let fee_ppm = if maker { &t["maker_fee"] } else { &t["taker_fee"] };
    let fee = dec(fee_ppm).unwrap_or_default() * usd / Decimal::from(1_000_000);
    Some(Fill {
        cid: cid_v.as_u64().filter(|c| *c > 0).map(ClientId),
        venue: VenueKey::new(Exchange::Lighter),
        symbol,
        side,
        qty: Qty(dec(&t["size"]).unwrap_or_default()),
        px: Px(dec(&t["price"]).unwrap_or_default()),
        fee: Usd(fee),
        maker,
        trade_id: t["trade_id"].as_i64().map(|x| x.to_string()).unwrap_or_default(),
        ts_ms: t["timestamp"].as_i64().unwrap_or_default(),
        liquidation: t["type"].as_str().is_some_and(|s| s != "trade"),
    })
}

/// Позиция Lighter (`Position`).
pub fn parse_position(p: &Value, symbol_of: &dyn Fn(i64) -> Option<String>) -> Option<VenuePosition> {
    let symbol = symbol_of(p["market_id"].as_i64()?)?;
    let sign = p["sign"].as_i64().unwrap_or(1);
    let qty = dec(&p["position"])? * Decimal::from(sign);
    Some(VenuePosition {
        venue: VenueKey::new(Exchange::Lighter),
        symbol,
        qty,
        entry_px: dec(&p["avg_entry_price"]).map(Px),
        liq_px: dec(&p["liquidation_price"]).filter(|x| !x.is_zero()).map(Px),
        margin: dec(&p["allocated_margin"]).map(Usd),
    })
}

/// Исполнение Lighter.
pub struct LtExecution {
    cfg: LtExecConfig,
    http: reqwest::Client,
    signer: Arc<LighterSigner>,
    account: i64,
    api_key_index: u8,
    meta: MetaRegistry,
    nonce: Mutex<Option<i64>>,
    limiter: WeightLimiter,
    proxy: Option<ProxyConfig>,
}

impl LtExecution {
    /// Загружает подписанта и создаёт адаптер.
    pub fn new(
        cfg: LtExecConfig,
        secrets: &LighterSecrets,
        meta: MetaRegistry,
        proxy: Option<ProxyConfig>,
    ) -> Result<Self, ExecError> {
        let signer = LighterSigner::load(
            &cfg.signer_lib,
            &cfg.rest,
            &secrets.api_private_key,
            cfg.chain_id,
            secrets.api_key_index,
            secrets.account_index,
        )
        .map_err(|e| ExecError::Auth(e.to_string()))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ExecError::Transport(e.to_string()))?;
        Ok(Self {
            cfg,
            http,
            signer: Arc::new(signer),
            account: secrets.account_index,
            api_key_index: secrets.api_key_index,
            meta,
            nonce: Mutex::new(None),
            // стандартный аккаунт: лимиты по весу запросов (docs/venues/lighter.md)
            limiter: WeightLimiter::per_minute(60),
            proxy,
        })
    }

    fn symbols(&self) -> BTreeMap<i64, String> {
        self.meta
            .read()
            .map(|m| {
                m.values()
                    .filter(|x| x.venue.exchange == Exchange::Lighter)
                    .filter_map(|x| Some((i64::try_from(x.venue_id?).ok()?, x.symbol.clone())))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn meta_of(&self, symbol: &str) -> Result<InstrumentMeta, ExecError> {
        sh_venue_api::meta_of(&self.meta, &VenueKey::new(Exchange::Lighter), symbol)
            .ok_or_else(|| ExecError::Invalid(format!("unknown lighter symbol {symbol}")))
    }

    fn auth(&self) -> Result<SecretString, ExecError> {
        let deadline = chrono::Utc::now().timestamp() + 600;
        self.signer.auth_token(deadline).map_err(|e| ExecError::Auth(e.to_string()))
    }

    async fn get(&self, path: &str, auth: bool) -> Result<Value, ExecError> {
        self.limiter.acquire(1).await;
        let mut r = self.http.get(format!("{}{path}", self.cfg.rest));
        if auth {
            r = r.header("authorization", self.auth()?.expose_secret());
        }
        let resp = r.send().await.map_err(|e| map_reqwest_error(&e))?;
        if resp.status().as_u16() == 429 {
            return Err(ExecError::RateLimited);
        }
        let text = resp.text().await.map_err(|e| map_reqwest_error(&e))?;
        serde_json::from_str(&text).map_err(|_| ExecError::Rejected("bad lighter response".into()))
    }

    async fn next_nonce(&self) -> Result<i64, ExecError> {
        let mut g = self.nonce.lock().await;
        let n = match *g {
            Some(n) => n,
            None => {
                let v = self
                    .get(
                        &format!(
                            "/api/v1/nextNonce?account_index={}&api_key_index={}",
                            self.account, self.api_key_index
                        ),
                        false,
                    )
                    .await?;
                v["nonce"].as_i64().ok_or_else(|| ExecError::Rejected("nextNonce missing".into()))?
            }
        };
        *g = Some(n + 1);
        Ok(n)
    }

    async fn reset_nonce(&self) {
        *self.nonce.lock().await = None;
    }

    async fn send_tx(&self, tx: crate::signer::SignedTx) -> Result<Value, ExecError> {
        if self.cfg.dry_run {
            tracing::info!(tx_type = tx.tx_type, tx_hash = %tx.tx_hash, "dry run: signed lighter tx not sent");
            return Err(ExecError::NotSupported("dry run".into()));
        }
        self.limiter.acquire(1).await;
        let form =
            [("tx_type", tx.tx_type.to_string()), ("tx_info", tx.tx_info), ("price_protection", "true".to_owned())];
        let resp = self
            .http
            .post(format!("{}/api/v1/sendTx", self.cfg.rest))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(sh_venue_api::form_urlencode(&form))
            .send()
            .await
            .map_err(|e| map_reqwest_error(&e))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| map_reqwest_error(&e))?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.as_u16() == 429 {
            return Err(ExecError::RateLimited);
        }
        if !status.is_success() || v["code"].as_i64().is_some_and(|c| c != 200) {
            let msg: String = v["message"].as_str().unwrap_or("sendTx rejected").chars().take(200).collect();
            // неверный nonce: пересинхронизация при следующей транзакции
            if msg.to_ascii_lowercase().contains("nonce") {
                self.reset_nonce().await;
            }
            return Err(ExecError::Rejected(msg));
        }
        Ok(v)
    }

    async fn active_orders(&self) -> Result<Vec<Value>, ExecError> {
        let v = self.get(&format!("/api/v1/accountActiveOrders?account_index={}", self.account), true).await?;
        Ok(v["orders"].as_array().cloned().unwrap_or_default())
    }
}

impl Execution for LtExecution {
    fn exchange(&self) -> Exchange {
        Exchange::Lighter
    }

    fn place(&self, req: OrderRequest) -> ExecFuture<'_, OrderUpdate> {
        Box::pin(async move {
            let m = self.meta_of(&req.symbol)?;
            let market =
                m.venue_id.and_then(|x| i64::try_from(x).ok()).ok_or_else(|| ExecError::Invalid("market id".into()))?;
            let (base, price) = to_units(&m, req.qty, req.px)?;
            let (t, expiry) = match req.tif {
                Tif::Ioc => (tif::IOC, 0),
                Tif::PostOnly => (tif::POST_ONLY, -1),
                Tif::Gtc => (tif::GTT, -1),
            };
            let nonce = self.next_nonce().await?;
            let p = OrderParams {
                market_index: market,
                client_order_index: i64::try_from(req.cid.0).map_err(|_| ExecError::Invalid("client id".into()))?,
                base_amount: base,
                price,
                is_ask: req.side == Side::Sell,
                time_in_force: t,
                reduce_only: req.reduce_only,
                order_expiry: expiry,
                nonce,
            };
            let tx = self.signer.sign_create_order(&p).map_err(|e| ExecError::Auth(e.to_string()))?;
            self.send_tx(tx).await?;
            // sendTx подтверждает приём транзакции; исполнение придёт в потоке
            Ok(OrderUpdate {
                cid: req.cid,
                venue: req.venue.clone(),
                symbol: req.symbol.clone(),
                status: OrderStatus::Acked,
                filled_qty: Qty(Decimal::ZERO),
                avg_px: None,
                venue_order_id: None,
                reason: None,
                ts_ms: chrono::Utc::now().timestamp_millis(),
            })
        })
    }

    fn cancel(&self, _venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            // отмена по индексу ордера: находим его среди активных по клиентскому индексу
            let orders = self.active_orders().await?;
            let o = orders
                .iter()
                .find(|o| o["client_order_index"].as_u64() == Some(cid.0))
                .ok_or_else(|| ExecError::Rejected("order not active".into()))?;
            let market = o["market_index"].as_i64().ok_or_else(|| ExecError::Rejected("market".into()))?;
            let index = o["order_index"].as_i64().ok_or_else(|| ExecError::Rejected("order index".into()))?;
            let _ = symbol;
            let nonce = self.next_nonce().await?;
            let tx = self.signer.sign_cancel_order(market, index, nonce).map_err(|e| ExecError::Auth(e.to_string()))?;
            self.send_tx(tx).await.map(|_| ())
        })
    }

    fn cancel_all(&self) -> ExecFuture<'_, u32> {
        Box::pin(async move {
            let n = u32::try_from(self.active_orders().await?.len()).unwrap_or(u32::MAX);
            let nonce = self.next_nonce().await?;
            let tx = self.signer.sign_cancel_all(nonce).map_err(|e| ExecError::Auth(e.to_string()))?;
            self.send_tx(tx).await?;
            Ok(n)
        })
    }

    fn query_order(&self, _venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, Option<OrderUpdate>> {
        Box::pin(async move {
            let syms = self.symbols();
            let f = |id: i64| syms.get(&id).cloned();
            if let Some(o) =
                self.active_orders().await?.iter().find(|o| o["client_order_index"].as_u64() == Some(cid.0))
            {
                return Ok(parse_order(o, &f));
            }
            // закрытые ордера (UNVERIFIED: путь и параметры accountInactiveOrders)
            let m = self.meta_of(&symbol)?;
            let market = m.venue_id.unwrap_or(255);
            let v = self
                .get(
                    &format!(
                        "/api/v1/accountInactiveOrders?account_index={}&market_id={market}&limit=100",
                        self.account
                    ),
                    true,
                )
                .await?;
            Ok(v["orders"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["client_order_index"].as_u64() == Some(cid.0))
                .and_then(|o| parse_order(o, &f)))
        })
    }

    fn positions(&self) -> ExecFuture<'_, Vec<VenuePosition>> {
        Box::pin(async move {
            let v = self.get(&format!("/api/v1/account?by=index&value={}", self.account), false).await?;
            let syms = self.symbols();
            let f = |id: i64| syms.get(&id).cloned();
            Ok(v["accounts"][0]["positions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| parse_position(p, &f))
                .filter(|p| !p.qty.is_zero())
                .collect())
        })
    }

    fn balances(&self) -> ExecFuture<'_, Vec<VenueBalance>> {
        Box::pin(async move {
            let v = self.get(&format!("/api/v1/account?by=index&value={}", self.account), false).await?;
            let a = &v["accounts"][0];
            // equity = обеспечение + нереализованный результат (поле total_asset_value, UNVERIFIED)
            let equity = dec(&a["total_asset_value"]).or_else(|| dec(&a["collateral"])).unwrap_or_default();
            Ok(vec![VenueBalance {
                venue: VenueKey::new(Exchange::Lighter),
                collateral: Collateral::Usdc,
                equity: Usd(equity),
                available: Usd(dec(&a["available_balance"]).unwrap_or_default()),
                maintenance_margin: None,
            }])
        })
    }

    fn funding_payments(&self, since_ms: i64) -> ExecFuture<'_, Vec<FundingPayment>> {
        Box::pin(async move {
            // UNVERIFIED: positionFunding — поля и единицы времени
            let v =
                self.get(&format!("/api/v1/positionFunding?account_index={}&limit=100", self.account), true).await?;
            let syms = self.symbols();
            Ok(v["position_fundings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| {
                    let ts = f["timestamp"].as_i64()?;
                    let ts_ms = if ts < 10_000_000_000 { ts * 1000 } else { ts };
                    (ts_ms >= since_ms).then_some(())?;
                    Some(FundingPayment {
                        venue: VenueKey::new(Exchange::Lighter),
                        symbol: syms.get(&f["market_id"].as_i64()?)?.clone(),
                        amount: Usd(dec(&f["change"])?),
                        rate: dec(&f["rate"]),
                        ts_ms,
                    })
                })
                .collect())
        })
    }

    fn run_stream(&self, tx: mpsc::Sender<ExecEvent>, shutdown: watch::Receiver<bool>) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            let s = LtStream {
                url: self.cfg.ws.clone(),
                account: self.account,
                signer: self.signer.clone(),
                symbols: self.symbols(),
            };
            run_private(Exchange::Lighter, s, self.proxy.clone(), tx, shutdown).await;
            Ok(())
        })
    }
}

struct LtStream {
    url: String,
    account: i64,
    signer: Arc<LighterSigner>,
    symbols: BTreeMap<i64, String>,
}

impl PrivateStream for LtStream {
    async fn url(&mut self) -> Result<String, String> {
        Ok(self.url.clone())
    }

    async fn subscribe(&mut self) -> Result<Vec<String>, String> {
        let deadline = chrono::Utc::now().timestamp() + 7 * 3600;
        let auth = self.signer.auth_token(deadline).map_err(|e| e.to_string())?;
        let a = self.account;
        let tok = auth.expose_secret();
        Ok(vec![
            json!({"type": "subscribe", "channel": format!("account_all_orders/{a}"), "auth": tok}).to_string(),
            json!({"type": "subscribe", "channel": format!("account_all_trades/{a}"), "auth": tok}).to_string(),
            json!({"type": "subscribe", "channel": format!("account_all_positions/{a}"), "auth": tok}).to_string(),
        ])
    }

    fn parse(&mut self, text: &str) -> Vec<ExecEvent> {
        parse_ws(text, self.account, &self.symbols)
    }
}

/// Разбор приватных каналов Lighter.
pub fn parse_ws(text: &str, account: i64, symbols: &BTreeMap<i64, String>) -> Vec<ExecEvent> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let f = |id: i64| symbols.get(&id).cloned();
    let ty = v["type"].as_str().unwrap_or_default();
    let mut out = Vec::new();
    if ty.ends_with("account_all_orders") {
        for list in v["orders"].as_object().into_iter().flat_map(|m| m.values()) {
            for o in list.as_array().into_iter().flatten() {
                if let Some(u) = parse_order(o, &f) {
                    out.push(ExecEvent::Order(u));
                }
            }
        }
    } else if ty == "update/account_all_trades" {
        for list in v["trades"].as_object().into_iter().flat_map(|m| m.values()) {
            for t in list.as_array().into_iter().flatten() {
                if let Some(fl) = parse_trade(t, account, &f) {
                    out.push(ExecEvent::Fill(fl));
                }
            }
        }
    } else if ty.ends_with("account_all_positions") {
        let positions: Vec<VenuePosition> = v["positions"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
            .filter_map(|p| parse_position(p, &f))
            .filter(|p| !p.qty.is_zero())
            .collect();
        out.push(ExecEvent::Positions { exchange: Exchange::Lighter, positions });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use sh_venue_api::{FeeInfo, FeeSource, InstrumentStatus};

    fn meta() -> InstrumentMeta {
        InstrumentMeta {
            venue: VenueKey::new(Exchange::Lighter),
            symbol: "ETH".into(),
            venue_id: Some(0),
            base: "ETH".into(),
            collateral: Collateral::Usdc,
            price_rule: PriceRule::Tick { tick: Px(dec!(0.01)) },
            lot_size: Qty(dec!(0.0001)),
            min_qty: None,
            min_notional: None,
            max_leverage: None,
            margin_mode: None,
            funding_interval_hours: None,
            fees: FeeInfo {
                maker: None,
                taker: None,
                source: FeeSource::Metadata,
                deployer_fee_scale: None,
                growth_mode: None,
            },
            status: InstrumentStatus::Active,
            listed_at_ms: None,
        }
    }

    #[test]
    fn integer_units() {
        assert_eq!(decimals_of(dec!(0.0001)), Some(4));
        assert_eq!(decimals_of(dec!(1)), Some(0));
        assert_eq!(decimals_of(dec!(0.05)), None);
        assert_eq!(to_units(&meta(), Qty(dec!(1.2345)), Px(dec!(3012.34))).unwrap(), (12345, 301234));
        assert!(to_units(&meta(), Qty(dec!(1.23456)), Px(dec!(3012.34))).is_err());
    }

    #[test]
    fn trades_from_our_side() {
        let syms = BTreeMap::from([(0i64, "ETH".to_owned())]);
        let msg = r#"{"channel":"account_all_trades:7","trades":{"0":[{"trade_id":1,"type":"trade","market_id":0,"size":"0.5","price":"3000.00","usd_amount":"1500","ask_id":1,"bid_id":2,"ask_client_id":11,"bid_client_id":22,"ask_account_id":7,"bid_account_id":9,"is_maker_ask":true,"block_height":1,"timestamp":1700000000000,"maker_fee":0}]},"type":"update/account_all_trades"}"#;
        let ev = parse_ws(msg, 7, &syms);
        let ExecEvent::Fill(f) = &ev[0] else { panic!("fill expected") };
        assert_eq!((f.side, f.maker, f.cid), (Side::Sell, true, Some(ClientId(11))));
        assert_eq!(f.fee.0, Decimal::ZERO);
        // чужая сделка не наша
        assert!(parse_ws(msg, 5, &syms).is_empty());
    }

    #[test]
    fn positions_and_orders() {
        let syms = BTreeMap::from([(0i64, "ETH".to_owned())]);
        let pos = r#"{"channel":"account_all_positions:7","positions":{"0":{"market_id":0,"symbol":"ETH","sign":-1,"position":"0.5","avg_entry_price":"3000","liquidation_price":"4000","allocated_margin":"0"}},"type":"update/account_all_positions"}"#;
        let ev = parse_ws(pos, 7, &syms);
        let ExecEvent::Positions { positions, .. } = &ev[0] else { panic!("positions expected") };
        assert_eq!(positions[0].qty, dec!(-0.5));
        let ord = r#"{"channel":"account_all_orders:7","orders":{"0":[{"order_index":5,"client_order_index":33,"market_index":0,"status":"canceled-post-only","filled_base_amount":"0","filled_quote_amount":"0","timestamp":1}]},"type":"update/account_all_orders"}"#;
        let ev = parse_ws(ord, 7, &syms);
        let ExecEvent::Order(u) = &ev[0] else { panic!("order expected") };
        assert_eq!((u.cid, u.status), (ClientId(33), OrderStatus::Rejected));
    }
}
