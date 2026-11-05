//! Приватный адаптер Aster (фьючерсы v3): подпись API-кошелька по EIP-712
//! (`Message { msg }`, домен `AsterSignTransaction`, версия 1, chainId 1666), где `msg` —
//! строка параметров запроса в порядке отправки, включая `nonce` (микросекунды),
//! `user` и `signer`. Подписант не может выводить средства без подписи основного кошелька.
//!
//! Эндпоинты совместимы с Binance USDⓈ-M; пути v3 — по документации Aster v3.
//! Расхождения помечены UNVERIFIED в docs/venues/aster.md.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use alloy::primitives::Address;
use rust_decimal::Decimal;
use serde_json::Value;
use sh_core::secrets::AsterSecrets;
use sh_core::{Collateral, Exchange, Px, Qty, Side, Usd, VenueKey};
use sh_venue_api::eip712::{EvmSigner, Field, digest, domain_separator, hash_struct};
use sh_venue_api::private_ws::{PrivateStream, map_reqwest_error, run_private};
use sh_venue_api::{
    ClientId, ExecError, ExecEvent, ExecFuture, Execution, Fill, FundingPayment, OrderRequest, OrderStatus,
    OrderUpdate, ProxyConfig, Tif, VenueBalance, VenuePosition, WeightLimiter, form_urlencode,
};
use tokio::sync::{mpsc, watch};

/// Настройки исполнения Aster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsExecConfig {
    /// REST (v3).
    pub rest: String,
    /// WebSocket.
    pub ws: String,
    /// Только подписать, не отправлять.
    pub dry_run: bool,
}

impl AsExecConfig {
    /// Значения по умолчанию. Testnet Aster — UNVERIFIED (адреса задаёт оператор).
    pub fn for_network(mainnet: bool) -> Self {
        if mainnet {
            Self { rest: "https://fapi3.asterdex.com".into(), ws: "wss://fstream.asterdex.com".into(), dry_run: false }
        } else {
            Self {
                rest: "https://fapi3.asterdex-testnet.com".into(),
                ws: "wss://fstream.asterdex-testnet.com".into(),
                dry_run: false,
            }
        }
    }
}

/// Подписант запросов Aster.
#[derive(Debug)]
pub struct AsterSigner {
    key: EvmSigner,
    user: String,
    signer: String,
    nonce: AtomicU64,
}

impl AsterSigner {
    /// Из секретов.
    pub fn new(s: &AsterSecrets) -> Result<Self, ExecError> {
        let key = EvmSigner::new(&s.signer_private_key).map_err(|e| ExecError::Auth(e.to_string()))?;
        let declared: Address = s.signer.parse().map_err(|_| ExecError::Auth("invalid signer address".into()))?;
        if declared != key.address() {
            return Err(ExecError::Auth("signer key does not match signer address".into()));
        }
        Ok(Self { key, user: s.user.clone(), signer: s.signer.clone(), nonce: AtomicU64::new(0) })
    }

    /// Строго возрастающий nonce в микросекундах.
    pub fn next_nonce(&self, now_us: u64) -> u64 {
        let mut cur = self.nonce.load(Ordering::SeqCst);
        loop {
            let n = now_us.max(cur + 1);
            match self.nonce.compare_exchange(cur, n, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return n,
                Err(v) => cur = v,
            }
        }
    }

    /// Подписывает параметры: добавляет `nonce`, `user`, `signer`, возвращает строку
    /// запроса с `signature` в конце.
    pub fn sign(&self, params: &[(&str, String)], nonce_us: u64) -> Result<String, ExecError> {
        let mut p: Vec<(&str, String)> = params.to_vec();
        p.push(("nonce", nonce_us.to_string()));
        p.push(("user", self.user.clone()));
        p.push(("signer", self.signer.clone()));
        let msg = form_urlencode(&p);
        let sig = self.key.sign_digest(message_digest(&msg)).map_err(|e| ExecError::Auth(e.to_string()))?;
        Ok(format!("{msg}&signature={}", sig.to_hex65()))
    }
}

/// Дайджест EIP-712 `Message { msg }` в домене Aster.
pub fn message_digest(msg: &str) -> alloy::primitives::B256 {
    let dom = domain_separator("AsterSignTransaction", "1", 1666, Address::ZERO);
    digest(dom, hash_struct("Message(string msg)", &[Field::Str(msg)]))
}

fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

/// Состояние ордера Aster (Binance-совместимое).
pub fn map_status(s: &str) -> OrderStatus {
    match s {
        "NEW" => OrderStatus::Acked,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "FILLED" => OrderStatus::Filled,
        "CANCELED" => OrderStatus::Cancelled,
        "REJECTED" => OrderStatus::Rejected,
        "EXPIRED" => OrderStatus::Expired,
        _ => OrderStatus::Unknown,
    }
}

/// Ордер из REST-ответа.
pub fn parse_order(v: &Value, now_ms: i64) -> Option<OrderUpdate> {
    let cid = ClientId::from_aster_id(v["clientOrderId"].as_str()?)?;
    let filled = dec(&v["executedQty"]).unwrap_or_default();
    Some(OrderUpdate {
        cid,
        venue: VenueKey::new(Exchange::Aster),
        symbol: v["symbol"].as_str()?.to_owned(),
        status: map_status(v["status"].as_str().unwrap_or_default()),
        filled_qty: Qty(filled),
        avg_px: dec(&v["avgPrice"]).filter(|p| !p.is_zero()).map(Px),
        venue_order_id: v["orderId"].as_u64().map(|x| x.to_string()),
        reason: None,
        ts_ms: v["updateTime"].as_i64().unwrap_or(now_ms),
    })
}

/// Разбор события пользовательского потока.
pub fn parse_ws(text: &str) -> Vec<ExecEvent> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let mut out = Vec::new();
    if v["e"] == "ORDER_TRADE_UPDATE" {
        let o = &v["o"];
        let Some(symbol) = o["s"].as_str() else { return out };
        let cid = o["c"].as_str().and_then(ClientId::from_aster_id);
        if o["x"] == "TRADE" {
            out.push(ExecEvent::Fill(Fill {
                cid,
                venue: VenueKey::new(Exchange::Aster),
                symbol: symbol.to_owned(),
                side: if o["S"] == "BUY" { Side::Buy } else { Side::Sell },
                qty: Qty(dec(&o["l"]).unwrap_or_default()),
                px: Px(dec(&o["L"]).unwrap_or_default()),
                fee: Usd(dec(&o["n"]).unwrap_or_default()),
                maker: o["m"].as_bool().unwrap_or(false),
                trade_id: o["t"].as_i64().map(|t| t.to_string()).unwrap_or_default(),
                ts_ms: o["T"].as_i64().unwrap_or_default(),
                // ордера ликвидации у Binance-совместимых площадок имеют префикс autoclose/adl
                liquidation: o["c"].as_str().is_some_and(|c| c.starts_with("autoclose") || c.starts_with("adl")),
            }));
        }
        if let Some(cid) = cid {
            out.push(ExecEvent::Order(OrderUpdate {
                cid,
                venue: VenueKey::new(Exchange::Aster),
                symbol: symbol.to_owned(),
                status: map_status(o["X"].as_str().unwrap_or_default()),
                filled_qty: Qty(dec(&o["z"]).unwrap_or_default()),
                avg_px: dec(&o["ap"]).filter(|p| !p.is_zero()).map(Px),
                venue_order_id: o["i"].as_u64().map(|x| x.to_string()),
                reason: o["x"].as_str().map(str::to_owned),
                ts_ms: v["E"].as_i64().unwrap_or_default(),
            }));
        }
    }
    out
}

/// Исполнение Aster.
pub struct AsExecution {
    cfg: AsExecConfig,
    http: reqwest::Client,
    signer: Arc<AsterSigner>,
    limiter: WeightLimiter,
    proxy: Option<ProxyConfig>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn now_us() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_micros()).unwrap_or(0)
}

impl AsExecution {
    /// Адаптер из секретов.
    pub fn new(cfg: AsExecConfig, secrets: &AsterSecrets, proxy: Option<ProxyConfig>) -> Result<Self, ExecError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ExecError::Transport(e.to_string()))?;
        Ok(Self {
            cfg,
            http,
            signer: Arc::new(AsterSigner::new(secrets)?),
            // 2400 веса в минуту на IP (docs/venues/aster.md)
            limiter: WeightLimiter::per_minute(2400),
            proxy,
        })
    }

    async fn signed(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &[(&str, String)],
        weight: u32,
    ) -> Result<Value, ExecError> {
        let q = self.signer.sign(params, self.signer.next_nonce(now_us()))?;
        if self.cfg.dry_run && method != reqwest::Method::GET {
            tracing::info!(%path, "dry run: signed aster request not sent");
            return Err(ExecError::NotSupported("dry run".into()));
        }
        self.limiter.acquire(weight).await;
        let url = format!("{}{path}", self.cfg.rest);
        let req = if method == reqwest::Method::GET {
            self.http.get(format!("{url}?{q}"))
        } else {
            self.http.request(method, url).header("content-type", "application/x-www-form-urlencoded").body(q)
        };
        let resp = req.send().await.map_err(|e| map_reqwest_error(&e))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| map_reqwest_error(&e))?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.as_u16() == 429 || status.as_u16() == 418 {
            return Err(ExecError::RateLimited);
        }
        if !status.is_success() {
            let code = v["code"].as_i64().unwrap_or_default();
            let msg: String = v["msg"].as_str().unwrap_or("request failed").chars().take(200).collect();
            return Err(ExecError::Rejected(format!("{code}: {msg}")));
        }
        Ok(v)
    }
}

impl Execution for AsExecution {
    fn exchange(&self) -> Exchange {
        Exchange::Aster
    }

    fn place(&self, req: OrderRequest) -> ExecFuture<'_, OrderUpdate> {
        Box::pin(async move {
            let tif = match req.tif {
                Tif::PostOnly => "GTX",
                Tif::Ioc => "IOC",
                Tif::Gtc => "GTC",
            };
            let params = [
                ("symbol", req.symbol.clone()),
                ("side", if req.side == Side::Buy { "BUY" } else { "SELL" }.to_owned()),
                ("type", "LIMIT".to_owned()),
                ("timeInForce", tif.to_owned()),
                ("quantity", req.qty.0.normalize().to_string()),
                ("price", req.px.0.normalize().to_string()),
                ("reduceOnly", req.reduce_only.to_string()),
                ("newClientOrderId", req.cid.aster_id()),
                ("newOrderRespType", "RESULT".to_owned()),
            ];
            let v = self.signed(reqwest::Method::POST, "/fapi/v3/order", &params, 1).await?;
            parse_order(&v, now_ms()).ok_or_else(|| ExecError::Rejected("bad order response".into()))
        })
    }

    fn cancel(&self, _venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            let params = [("symbol", symbol), ("origClientOrderId", cid.aster_id())];
            self.signed(reqwest::Method::DELETE, "/fapi/v3/order", &params, 1).await.map(|_| ())
        })
    }

    fn cancel_all(&self) -> ExecFuture<'_, u32> {
        Box::pin(async move {
            let open = self.signed(reqwest::Method::GET, "/fapi/v3/openOrders", &[], 40).await?;
            let symbols: BTreeSet<String> =
                open.as_array().into_iter().flatten().filter_map(|o| o["symbol"].as_str().map(str::to_owned)).collect();
            let n = u32::try_from(open.as_array().map_or(0, Vec::len)).unwrap_or(u32::MAX);
            for s in symbols {
                self.signed(reqwest::Method::DELETE, "/fapi/v3/allOpenOrders", &[("symbol", s)], 1).await?;
            }
            Ok(n)
        })
    }

    fn query_order(&self, _venue: VenueKey, symbol: String, cid: ClientId) -> ExecFuture<'_, Option<OrderUpdate>> {
        Box::pin(async move {
            let params = [("symbol", symbol), ("origClientOrderId", cid.aster_id())];
            match self.signed(reqwest::Method::GET, "/fapi/v3/order", &params, 1).await {
                Ok(v) => Ok(parse_order(&v, now_ms())),
                // -2013: ордер не существует
                Err(ExecError::Rejected(m)) if m.starts_with("-2013") => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    fn positions(&self) -> ExecFuture<'_, Vec<VenuePosition>> {
        Box::pin(async move {
            let v = self.signed(reqwest::Method::GET, "/fapi/v3/positionRisk", &[], 5).await?;
            Ok(v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| {
                    let qty = dec(&p["positionAmt"])?;
                    (!qty.is_zero()).then(|| VenuePosition {
                        venue: VenueKey::new(Exchange::Aster),
                        symbol: p["symbol"].as_str().unwrap_or_default().to_owned(),
                        qty,
                        entry_px: dec(&p["entryPrice"]).map(Px),
                        liq_px: dec(&p["liquidationPrice"]).filter(|x| !x.is_zero()).map(Px),
                        margin: dec(&p["isolatedMargin"]).map(Usd),
                    })
                })
                .collect())
        })
    }

    fn balances(&self) -> ExecFuture<'_, Vec<VenueBalance>> {
        Box::pin(async move {
            let v = self.signed(reqwest::Method::GET, "/fapi/v3/balance", &[], 5).await?;
            Ok(v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| {
                    let bal = dec(&b["balance"])?;
                    let upnl = dec(&b["crossUnPnl"]).unwrap_or_default();
                    (!bal.is_zero()).then(|| VenueBalance {
                        venue: VenueKey::new(Exchange::Aster),
                        collateral: Collateral::from_symbol(b["asset"].as_str().unwrap_or_default()),
                        equity: Usd(bal + upnl),
                        available: Usd(dec(&b["availableBalance"]).unwrap_or_default()),
                        maintenance_margin: None,
                    })
                })
                .collect())
        })
    }

    fn funding_payments(&self, since_ms: i64) -> ExecFuture<'_, Vec<FundingPayment>> {
        Box::pin(async move {
            let params = [
                ("incomeType", "FUNDING_FEE".to_owned()),
                ("startTime", since_ms.to_string()),
                ("limit", "1000".to_owned()),
            ];
            let v = self.signed(reqwest::Method::GET, "/fapi/v3/income", &params, 30).await?;
            Ok(v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| {
                    Some(FundingPayment {
                        venue: VenueKey::new(Exchange::Aster),
                        symbol: i["symbol"].as_str()?.to_owned(),
                        amount: Usd(dec(&i["income"])?),
                        rate: None,
                        ts_ms: i["time"].as_i64()?,
                    })
                })
                .collect())
        })
    }

    fn run_stream(&self, tx: mpsc::Sender<ExecEvent>, shutdown: watch::Receiver<bool>) -> ExecFuture<'_, ()> {
        Box::pin(async move {
            let s = AsStream { exec: self };
            run_private(Exchange::Aster, s, self.proxy.clone(), tx, shutdown).await;
            Ok(())
        })
    }
}

struct AsStream<'a> {
    exec: &'a AsExecution,
}

impl PrivateStream for AsStream<'_> {
    async fn url(&mut self) -> Result<String, String> {
        // новый listenKey при каждом подключении; продление (PUT) не делаем — поток
        // переподключается сам (UNVERIFIED: срок жизни ключа 60 минут как у Binance)
        let v =
            self.exec.signed(reqwest::Method::POST, "/fapi/v3/listenKey", &[], 1).await.map_err(|e| e.to_string())?;
        let key = v["listenKey"].as_str().ok_or("listenKey missing")?;
        Ok(format!("{}/ws/{key}", self.exec.cfg.ws))
    }

    async fn subscribe(&mut self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    fn parse(&mut self, text: &str) -> Vec<ExecEvent> {
        parse_ws(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use secrecy::SecretString;

    const KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

    fn secrets(signer: &str) -> AsterSecrets {
        AsterSecrets {
            user: "0x63DD5aCC6b1aa0f563956C0e534DD30B6dcF7C4e".into(),
            signer: signer.into(),
            signer_private_key: SecretString::from(KEY),
        }
    }

    fn addr() -> String {
        EvmSigner::new(&SecretString::from(KEY)).unwrap().address().to_string()
    }

    #[test]
    fn signer_address_must_match_key() {
        assert!(AsterSigner::new(&secrets("0x21cF8Ae13Bb72632562c6Fff438652Ba1a151bb0")).is_err());
        assert!(AsterSigner::new(&secrets(&addr())).is_ok());
    }

    #[test]
    fn signed_query_layout_and_recovery() {
        let s = AsterSigner::new(&secrets(&addr())).unwrap();
        let q = s.sign(&[("symbol", "BTCUSDT".into()), ("side", "BUY".into())], 1_700_000_000_000_000).unwrap();
        let (msg, sig) = q.rsplit_once("&signature=").unwrap();
        assert!(msg.starts_with("symbol=BTCUSDT&side=BUY&nonce=1700000000000000&user=0x63DD5aCC"));
        assert!(msg.ends_with(&format!("&signer={}", addr())));
        assert_eq!(sig.len(), 132);
        // подпись восстанавливается в адрес API-кошелька
        let bytes: Vec<u8> = (0..65).map(|i| u8::from_str_radix(&sig[2 + 2 * i..4 + 2 * i], 16).unwrap()).collect();
        let parsed = alloy::primitives::Signature::from_raw(&bytes).unwrap();
        let rec = parsed.recover_address_from_prehash(&message_digest(msg)).unwrap();
        assert_eq!(rec.to_string(), addr());
    }

    #[test]
    fn nonce_monotonic_microseconds() {
        let s = AsterSigner::new(&secrets(&addr())).unwrap();
        let a = s.next_nonce(10);
        assert!(s.next_nonce(10) > a);
    }

    #[test]
    fn user_stream_trade() {
        let m = r#"{"e":"ORDER_TRADE_UPDATE","E":1568879465651,"T":1568879465650,"o":{"s":"BTCUSDT","c":"sh-42","S":"SELL","o":"LIMIT","f":"GTX","q":"0.002","p":"60000","ap":"60000","x":"TRADE","X":"PARTIALLY_FILLED","i":8886774,"l":"0.001","z":"0.001","L":"60000","N":"USDT","n":"0","T":1568879465651,"t":12,"m":true,"R":false}}"#;
        let ev = parse_ws(m);
        let ExecEvent::Fill(f) = &ev[0] else { panic!("fill expected") };
        assert_eq!((f.cid, f.qty.0, f.maker), (Some(ClientId(42)), dec!(0.001), true));
        let ExecEvent::Order(u) = &ev[1] else { panic!("order expected") };
        assert_eq!(u.status, OrderStatus::PartiallyFilled);
    }

    #[test]
    fn rest_order() {
        let v: Value = serde_json::from_str(r#"{"clientOrderId":"sh-7","symbol":"ETHUSDT","status":"EXPIRED","executedQty":"0","avgPrice":"0.00","orderId":1,"updateTime":5}"#).unwrap();
        let u = parse_order(&v, 0).unwrap();
        assert_eq!((u.cid, u.status), (ClientId(7), OrderStatus::Expired));
    }
}
