//! `spread-hunter`: демон спред-движка. В M0 доступен только режим записи
//! рыночных данных (`run` с `engine.mode = "record"`), выгрузка метаданных
//! и проверка записей на разрывы.

mod monitor;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use sh_core::{Clock, Exchange, SystemClock};
use sh_store::{FORMAT, FileHeader, Recorder};
use sh_venue_api::{MarketDataVenue, ProxyConfig, Sinks, VenueError};
use sh_venue_aster::AsterVenue;
use sh_venue_hyperliquid::HyperliquidVenue;
use sh_venue_lighter::LighterVenue;
use tokio::sync::{mpsc, watch};

use spread_hunter::config::AppConfig;

#[derive(Parser)]
#[command(name = "spread-hunter", version, about = "Delta-neutral spread engine (M0: market-data recorder)")]
struct Cli {
    /// Базовый конфиг.
    #[arg(long, default_value = "config/default.toml", global = true)]
    config: PathBuf,
    /// Наложение окружения (например, config/live.toml).
    #[arg(long, global = true)]
    overlay: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Запуск в режиме из конфига (в M0 — только `record`).
    Run,
    /// Загрузить и вывести нормализованные метаданные площадок (JSON).
    Metadata {
        /// Площадка: hyperliquid, lighter, aster или all.
        #[arg(long, default_value = "all")]
        venue: String,
    },
    /// Отчёт о разрывах по файлам рекордера (файлы или каталоги).
    Inspect {
        /// Файлы `.jsonl.zst` или каталоги с ними.
        paths: Vec<PathBuf>,
        /// Вывести JSON вместо таблицы.
        #[arg(long)]
        json: bool,
    },
    /// Распаковать файл рекордера в stdout (строки JSON как есть).
    Dump {
        /// Файл `.jsonl.zst`.
        path: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Inspect { paths, json } => inspect(&paths, json),
        Cmd::Dump { path } => dump(&path),
        cmd => {
            let cfg = AppConfig::load(&cli.config, cli.overlay.as_deref())?;
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(async move {
                match cmd {
                    Cmd::Run => run(cfg).await,
                    Cmd::Metadata { venue } => metadata(cfg, &venue).await,
                    Cmd::Inspect { .. } | Cmd::Dump { .. } => Ok(()),
                }
            })
        }
    }
}

fn install_crypto() {
    // один провайдер rustls на процесс; повторная установка безвредна
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_owned()))
        .unwrap_or_else(|| "unknown".into())
}

async fn run(cfg: AppConfig) -> anyhow::Result<()> {
    sh_obs::init_logging(&cfg.log).context("logging")?;
    install_crypto();
    sh_obs::init_metrics(&cfg.metrics).map_err(anyhow::Error::msg).context("metrics")?;
    let proxy = cfg.network.proxy().map_err(anyhow::Error::msg)?;
    if let Some(p) = &proxy {
        tracing::warn!(proxy = %format!("{}:{}", p.host, p.port), "websocket traffic goes through an HTTP proxy");
    }

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let start = clock.wall_ns();
    let header = FileHeader {
        format: FORMAT.into(),
        session: format!("{start:x}-{}", std::process::id()),
        host: host_name(),
        session_start_wall_ns: start,
        version: env!("CARGO_PKG_VERSION").into(),
    };
    tracing::info!(session = %header.session, dir = %cfg.recorder.dir.display(), "recorder starting");
    let recorder = Recorder::start(cfg.recorder.clone(), header).context("recorder")?;

    let (md_tx, md_rx) = mpsc::channel(65_536);
    let sinks = Sinks::new(Some(recorder.sender()), Some(md_tx), clock.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let monitor = tokio::spawn(monitor::run(md_rx, clock.clone()));

    let mut venues = tokio::task::JoinSet::new();
    let v = &cfg.venues;
    if v.hyperliquid.enabled {
        spawn_venue(&mut venues, HyperliquidVenue::new(v.hyperliquid.clone(), proxy.clone()), &sinks, &shutdown_rx);
    }
    if v.lighter.enabled {
        spawn_venue(&mut venues, LighterVenue::new(v.lighter.clone(), proxy.clone()), &sinks, &shutdown_rx);
    }
    if v.aster.enabled {
        spawn_venue(&mut venues, AsterVenue::new(v.aster.clone(), proxy.clone()), &sinks, &shutdown_rx);
    }
    drop(sinks);
    if venues.is_empty() {
        anyhow::bail!("no venues enabled");
    }

    let mut fatal: Option<anyhow::Error> = None;
    tokio::select! {
        _ = shutdown_signal() => tracing::info!("shutdown signal received"),
        Some(res) = venues.join_next() => {
            fatal = Some(match res {
                Ok((ex, Ok(()))) => anyhow::anyhow!("venue {ex} stopped unexpectedly"),
                Ok((ex, Err(e))) => anyhow::anyhow!("venue {ex} failed: {e}"),
                Err(e) => anyhow::anyhow!("venue task panicked: {e}"),
            });
        }
    }
    let _ = shutdown_tx.send(true);
    // адаптеры закрывают соединения и пишут ConnDown; ждём ограниченное время
    let drained = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(res) = venues.join_next().await {
            if let Ok((ex, Err(e))) = res {
                tracing::warn!(venue = %ex, error = %e, "venue stopped with error");
            }
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("venues did not stop within 15s, aborting");
        venues.abort_all();
        while venues.join_next().await.is_some() {}
    }
    let _ = monitor.await;
    let stats = tokio::task::spawn_blocking(move || recorder.finish()).await?;
    tracing::info!(records = ?stats.records, raw_bytes = stats.raw_bytes, compressed_bytes = stats.compressed_bytes, write_errors = stats.write_errors, "recorder stopped");
    match fatal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn spawn_venue<V: MarketDataVenue>(
    set: &mut tokio::task::JoinSet<(Exchange, Result<(), VenueError>)>,
    venue: V,
    sinks: &Sinks,
    shutdown: &watch::Receiver<bool>,
) {
    let (sinks, shutdown) = (sinks.clone(), shutdown.clone());
    set.spawn(async move {
        let ex = venue.exchange();
        tracing::info!(venue = %ex, "venue starting");
        (ex, venue.run(sinks, shutdown).await)
    });
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

async fn metadata(cfg: AppConfig, venue: &str) -> anyhow::Result<()> {
    install_crypto();
    let proxy: Option<ProxyConfig> = cfg.network.proxy().map_err(anyhow::Error::msg)?;
    // без рекордера и без канала событий: только ответы REST
    let sinks = Sinks::new(None, None, Arc::new(SystemClock::new()));
    let v = &cfg.venues;
    let mut all = Vec::new();
    if matches!(venue, "all" | "hyperliquid") {
        all.extend(HyperliquidVenue::new(v.hyperliquid.clone(), proxy.clone()).fetch_metadata(&sinks).await?);
    }
    if matches!(venue, "all" | "lighter") {
        all.extend(LighterVenue::new(v.lighter.clone(), proxy.clone()).fetch_metadata(&sinks).await?);
    }
    if matches!(venue, "all" | "aster") {
        all.extend(AsterVenue::new(v.aster.clone(), proxy).fetch_metadata(&sinks).await?);
    }
    if all.is_empty() && !matches!(venue, "all" | "hyperliquid" | "lighter" | "aster") {
        anyhow::bail!("unknown venue '{venue}'");
    }
    println!("{}", serde_json::to_string_pretty(&all)?);
    Ok(())
}

fn dump(path: &Path) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(zstd::stream::read::Decoder::new(file)?);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for line in reader.lines() {
        match line {
            Ok(l) => {
                if let Err(e) = writeln!(out, "{l}") {
                    // `dump | head`: читатель закрыл канал — это не ошибка
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok(());
                    }
                    return Err(e.into());
                }
            }
            Err(e) => {
                // обрезанный хвост после аварии: всё прочитанное уже выведено
                eprintln!("read stopped: {e}");
                break;
            }
        }
    }
    match out.flush() {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

fn collect_files(paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    fn walk(p: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        if p.is_dir() {
            for e in std::fs::read_dir(p)? {
                walk(&e?.path(), out)?;
            }
        } else if p.to_string_lossy().ends_with(".jsonl.zst") {
            out.push(p.to_path_buf());
        }
        Ok(())
    }
    let mut out = Vec::new();
    for p in paths {
        walk(p, &mut out)?;
    }
    Ok(out)
}

fn inspect(paths: &[PathBuf], json: bool) -> anyhow::Result<()> {
    let files = collect_files(paths)?;
    if files.is_empty() {
        anyhow::bail!("no .jsonl.zst files found");
    }
    let rep = sh_store::inspect_files(&files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rep)?);
        return Ok(());
    }
    println!(
        "{:<12} {:>10} {:>10} {:>6} {:>12} {:>6} {:>8} {:>12} {:>12} {:>8}",
        "venue", "span_h", "records", "conns", "conn_gap_%", "books", "gaps", "book_gap_%", "max_quiet_s", "read_err"
    );
    for (venue, r) in &rep.venues {
        let span_h = (r.last_wall_ns - r.first_wall_ns) as f64 / 3.6e12;
        let records: u64 = r.records_by_source.values().sum();
        println!(
            "{:<12} {:>10.2} {:>10} {:>6} {:>12.4} {:>6} {:>8} {:>12.4} {:>12.1} {:>8}",
            venue,
            span_h,
            records,
            r.connections,
            r.conn_gap_fraction * 100.0,
            r.books,
            r.book_gaps,
            r.book_gap_fraction * 100.0,
            r.max_silence_secs,
            r.read_errors
        );
    }
    Ok(())
}
