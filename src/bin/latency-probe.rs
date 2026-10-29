//! `latency-probe`: замер задержек до площадок с кандидата-хоста.
//! По каждой площадке: DNS, TCP connect, TLS handshake, RTT пинга WebSocket и
//! время до первого байта REST. Печатает p50/p90/p99 и пишет JSON в
//! `data/latency/<host>-<date>.json`. Замер подтверждения ордера (`--allow-test-orders`)
//! в M0 отсутствует: кода ордеров в M0 нет.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use sh_venue_api::{ProxyConfig, connect_ws};
use spread_hunter::config::AppConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
#[command(name = "latency-probe", version, about = "Measure network latency to venues (no orders in M0)")]
struct Args {
    /// Конфиг с URL площадок.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
    /// Площадки через запятую или `all`.
    #[arg(long, default_value = "all")]
    venues: String,
    /// Замеров на каждый этап.
    #[arg(long, default_value_t = 20)]
    samples: usize,
    /// Пауза между замерами, мс.
    #[arg(long, default_value_t = 250)]
    pause_ms: u64,
    /// Каталог результатов.
    #[arg(long, default_value = "data/latency")]
    out_dir: PathBuf,
    /// Имя хоста в имени файла (по умолчанию — системное).
    #[arg(long)]
    host: Option<String>,
}

/// Перцентили выборки, мс. `f64` допустим: это аналитика, не денежный путь.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
struct Stats {
    n: usize,
    min: f64,
    p50: f64,
    p90: f64,
    p99: f64,
    max: f64,
    mean: f64,
}

/// Перцентиль методом ближайшего ранга.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn stats(samples: &[f64]) -> Stats {
    if samples.is_empty() {
        return Stats::default();
    }
    let mut s = samples.to_vec();
    s.sort_by(f64::total_cmp);
    Stats {
        n: s.len(),
        min: s[0],
        p50: percentile(&s, 50.0),
        p90: percentile(&s, 90.0),
        p99: percentile(&s, 99.0),
        max: s[s.len() - 1],
        mean: s.iter().sum::<f64>() / s.len() as f64,
    }
}

#[derive(Debug, Clone, Copy)]
enum PingStyle {
    /// Текстовый пинг и ожидаемый фрагмент ответа.
    Text(&'static str, &'static str),
    /// Кадр Ping → кадр Pong.
    Frame,
}

struct Target {
    name: &'static str,
    rest_base: String,
    ws_url: String,
    rest_probe: RestProbe,
    ping: PingStyle,
}

enum RestProbe {
    Get(&'static str),
    PostJson(&'static str, &'static str),
}

#[derive(Debug, Default, Serialize)]
struct VenueReport {
    rest_host: String,
    ws_host: String,
    dns_ms: Stats,
    tcp_connect_ms: Stats,
    tls_handshake_ms: Stats,
    ws_ping_rtt_ms: Stats,
    rest_ttfb_ms: Stats,
    errors: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Report {
    host: String,
    date: String,
    started_at: String,
    via_proxy: bool,
    note: &'static str,
    samples: usize,
    venues: std::collections::BTreeMap<String, VenueReport>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn tls_connector() -> anyhow::Result<tokio_rustls::TlsConnector> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let cfg = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(Arc::new(cfg)))
}

/// TCP до хоста: напрямую или туннелем через прокси (тогда это время туннеля).
async fn tcp(host: &str, proxy: Option<&ProxyConfig>) -> anyhow::Result<(TcpStream, Duration)> {
    let t = Instant::now();
    let stream = match proxy.filter(|p| p.applies_to(host)) {
        Some(p) => {
            let mut s = TcpStream::connect((p.host.as_str(), p.port)).await?;
            s.write_all(format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").as_bytes()).await?;
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if s.read(&mut b).await? == 0 {
                    anyhow::bail!("proxy closed");
                }
                head.push(b[0]);
            }
            if !String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200") {
                anyhow::bail!("proxy CONNECT failed");
            }
            s
        }
        None => TcpStream::connect((host, 443)).await?,
    };
    stream.set_nodelay(true)?;
    Ok((stream, t.elapsed()))
}

async fn probe_venue(
    t: &Target,
    args: &Args,
    proxy: Option<&ProxyConfig>,
    tls: &tokio_rustls::TlsConnector,
) -> VenueReport {
    let rest_host = sh_venue_api::ws::host_port(&t.rest_base).map(|(h, _)| h).unwrap_or_default();
    let ws_host = sh_venue_api::ws::host_port(&t.ws_url).map(|(h, _)| h).unwrap_or_default();
    let mut rep = VenueReport { rest_host: rest_host.clone(), ws_host, ..VenueReport::default() };
    let pause = Duration::from_millis(args.pause_ms);
    let (mut dns, mut tcp_v, mut tls_v, mut ws_v, mut rest_v) = (vec![], vec![], vec![], vec![], vec![]);

    for _ in 0..args.samples {
        let s = Instant::now();
        match tokio::net::lookup_host((rest_host.as_str(), 443)).await {
            Ok(_) => dns.push(ms(s.elapsed())),
            Err(e) => {
                rep.errors.push(format!("dns: {e}"));
                break;
            }
        }
        tokio::time::sleep(pause).await;
    }

    for _ in 0..args.samples {
        match tcp(&rest_host, proxy).await {
            Ok((stream, d)) => {
                tcp_v.push(ms(d));
                let name = match rustls::pki_types::ServerName::try_from(rest_host.clone()) {
                    Ok(n) => n,
                    Err(e) => {
                        rep.errors.push(format!("tls name: {e}"));
                        break;
                    }
                };
                let s = Instant::now();
                match tokio::time::timeout(Duration::from_secs(10), tls.connect(name, stream)).await {
                    Ok(Ok(_)) => tls_v.push(ms(s.elapsed())),
                    Ok(Err(e)) => rep.errors.push(format!("tls: {e}")),
                    Err(_) => rep.errors.push("tls: timeout".into()),
                }
            }
            Err(e) => {
                rep.errors.push(format!("tcp: {e}"));
                break;
            }
        }
        tokio::time::sleep(pause).await;
    }

    match connect_ws(&t.ws_url, proxy, Duration::from_secs(15)).await {
        Ok(mut ws) => {
            for _ in 0..args.samples {
                let s = Instant::now();
                let sent = match t.ping {
                    PingStyle::Text(msg, _) => ws.send(Message::text(msg)).await,
                    PingStyle::Frame => ws.send(Message::Ping(b"probe".to_vec().into())).await,
                };
                if let Err(e) = sent {
                    rep.errors.push(format!("ws send: {e}"));
                    break;
                }
                let got = tokio::time::timeout(Duration::from_secs(10), async {
                    while let Some(m) = ws.next().await {
                        match (m, t.ping) {
                            (Ok(Message::Text(txt)), PingStyle::Text(_, want)) if txt.as_str().contains(want) => {
                                return true;
                            }
                            (Ok(Message::Pong(_)), PingStyle::Frame) => return true,
                            (Err(_), _) => return false,
                            _ => {}
                        }
                    }
                    false
                })
                .await;
                match got {
                    Ok(true) => ws_v.push(ms(s.elapsed())),
                    _ => {
                        rep.errors.push("ws ping: no pong".into());
                        break;
                    }
                }
                tokio::time::sleep(pause).await;
            }
            let _ = ws.close(None).await;
        }
        Err(e) => rep.errors.push(format!("ws connect: {e}")),
    }

    // keep-alive: первый запрос прогревает соединение и в выборку не входит
    match reqwest::Client::builder().timeout(Duration::from_secs(10)).build() {
        Ok(http) => {
            for i in 0..=args.samples {
                let req = match &t.rest_probe {
                    RestProbe::Get(path) => http.get(format!("{}{}", t.rest_base, path)),
                    RestProbe::PostJson(path, body) => http
                        .post(format!("{}{}", t.rest_base, path))
                        .header("content-type", "application/json")
                        .body(*body),
                };
                let s = Instant::now();
                match req.send().await {
                    Ok(resp) => {
                        // send() завершается на заголовках ответа: это и есть время до первого байта
                        let d = s.elapsed();
                        let ok = resp.status().is_success();
                        let _ = resp.bytes().await;
                        if !ok {
                            rep.errors.push("rest: non-2xx".into());
                        } else if i > 0 {
                            rest_v.push(ms(d));
                        }
                    }
                    Err(e) => {
                        rep.errors.push(format!("rest: {e}"));
                        break;
                    }
                }
                tokio::time::sleep(pause).await;
            }
        }
        Err(e) => rep.errors.push(format!("http client: {e}")),
    }

    rep.dns_ms = stats(&dns);
    rep.tcp_connect_ms = stats(&tcp_v);
    rep.tls_handshake_ms = stats(&tls_v);
    rep.ws_ping_rtt_ms = stats(&ws_v);
    rep.rest_ttfb_ms = stats(&rest_v);
    rep.errors.dedup();
    rep
}

/// Путь результата; не перезаписывает существующий файл за тот же день.
fn out_path(dir: &Path, host: &str, date: &str) -> PathBuf {
    let base = dir.join(format!("{host}-{date}.json"));
    if !base.exists() {
        return base;
    }
    (2..).map(|i| dir.join(format!("{host}-{date}-{i}.json"))).find(|p| !p.exists()).unwrap_or(base)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cfg = AppConfig::load(&args.config, None)?;
    let proxy = cfg.network.proxy().map_err(anyhow::Error::msg)?;
    let v = &cfg.venues;
    let targets = [
        Target {
            name: "hyperliquid",
            rest_base: v.hyperliquid.rest.clone(),
            ws_url: v.hyperliquid.ws.clone(),
            rest_probe: RestProbe::PostJson("/info", r#"{"type":"exchangeStatus"}"#),
            ping: PingStyle::Text(r#"{"method":"ping"}"#, r#""pong""#),
        },
        Target {
            name: "lighter",
            rest_base: v.lighter.rest.clone(),
            ws_url: v.lighter.ws.clone(),
            rest_probe: RestProbe::Get("/"),
            ping: PingStyle::Text(r#"{"type":"ping"}"#, r#""pong""#),
        },
        Target {
            name: "aster",
            rest_base: v.aster.rest.clone(),
            ws_url: format!("{}/ws/btcusdt@bookTicker", v.aster.ws.trim_end_matches('/')),
            rest_probe: RestProbe::Get("/fapi/v3/ping"),
            ping: PingStyle::Frame,
        },
    ];
    let wanted: Vec<&str> = args.venues.split(',').map(str::trim).collect();
    let tls = tls_connector()?;
    let host = args.host.clone().unwrap_or_else(|| {
        std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_owned()).unwrap_or_else(|_| "unknown".into())
    });
    let now = chrono::Utc::now();
    let via_proxy = proxy.is_some();
    if via_proxy {
        eprintln!(
            "WARNING: HTTPS proxy in use; TCP/TLS/RTT numbers include the proxy and do not reflect the host's direct path"
        );
    }
    let mut report = Report {
        host: host.clone(),
        date: now.format("%Y-%m-%d").to_string(),
        started_at: now.to_rfc3339(),
        via_proxy,
        note: "order-ack RTT (--allow-test-orders) is not available in M0",
        samples: args.samples,
        venues: Default::default(),
    };
    for t in targets.iter().filter(|t| wanted.contains(&"all") || wanted.contains(&t.name)) {
        eprintln!("probing {} ...", t.name);
        report.venues.insert(t.name.to_owned(), probe_venue(t, &args, proxy.as_ref(), &tls).await);
    }

    println!("{:<12} {:<16} {:>4} {:>9} {:>9} {:>9}", "venue", "metric", "n", "p50_ms", "p90_ms", "p99_ms");
    for (name, r) in &report.venues {
        for (m, s) in [
            ("dns", &r.dns_ms),
            ("tcp_connect", &r.tcp_connect_ms),
            ("tls_handshake", &r.tls_handshake_ms),
            ("ws_ping_rtt", &r.ws_ping_rtt_ms),
            ("rest_ttfb", &r.rest_ttfb_ms),
        ] {
            println!("{:<12} {:<16} {:>4} {:>9.2} {:>9.2} {:>9.2}", name, m, s.n, s.p50, s.p90, s.p99);
        }
        for e in &r.errors {
            println!("{:<12} error: {e}", name);
        }
    }
    std::fs::create_dir_all(&args.out_dir)?;
    let path = out_path(&args.out_dir, &host, &report.date);
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?).with_context(|| path.display().to_string())?;
    eprintln!("written {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = stats(&v);
        assert_eq!((s.p50, s.p90, s.p99, s.min, s.max), (50.0, 90.0, 99.0, 1.0, 100.0));
        assert_eq!(stats(&[5.0]).p99, 5.0);
        assert_eq!(stats(&[]).n, 0);
        assert!((stats(&[1.0, 2.0, 3.0]).mean - 2.0).abs() < 1e-12);
    }

    #[test]
    fn out_path_does_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("lp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = out_path(&dir, "h", "2026-10-29");
        assert!(p1.ends_with("h-2026-10-29.json"));
        std::fs::write(&p1, "{}").unwrap();
        assert!(out_path(&dir, "h", "2026-10-29").ends_with("h-2026-10-29-2.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
