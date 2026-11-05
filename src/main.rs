//! `spread-hunter`: демон спред-движка. Режимы из конфигурации (`run`), повтор
//! сканера по записям, производные Parquet, выгрузка метаданных и проверка записей.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use sh_core::SystemClock;
use sh_venue_api::{MarketDataVenue, ProxyConfig, Sinks};
use sh_venue_aster::AsterVenue;
use sh_venue_hyperliquid::HyperliquidVenue;
use sh_venue_lighter::LighterVenue;

use spread_hunter::config::AppConfig;
use spread_hunter::replay_source::parse_time;
use spread_hunter::runner::{self, install_crypto};

#[derive(Parser)]
#[command(name = "spread-hunter", version, about = "Delta-neutral spread engine")]
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
    /// Запуск в режиме из конфига (`record`, `scan`, `paper`, `live`).
    Run {
        /// Разрешить live (вместе с `engine.mode = "live"`).
        #[arg(long)]
        live: bool,
        /// Подтверждение: live торгует реальными деньгами.
        #[arg(long)]
        i_understand_real_money: bool,
        /// live без отправки ордеров: подпись и журнал, исполнение на симуляторе.
        #[arg(long)]
        dry_run: bool,
    },
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
    /// Сканер и теневой портфель по записям (без сети, детерминированно).
    Scan {
        /// Каталог сырых записей.
        #[arg(long)]
        replay: PathBuf,
        /// Начало: `YYYY-MM-DD` или RFC 3339 (UTC).
        #[arg(long)]
        from: String,
        /// Конец (не включительно).
        #[arg(long)]
        to: String,
        /// Каталог отчётов.
        #[arg(long, default_value = "reports/replay")]
        out: PathBuf,
    },
    /// Проверка приватных адаптеров малыми ордерами (запускает оператор; ставит ордера).
    Selftest {
        /// `testnet` или `mainnet`.
        #[arg(long)]
        network: String,
        /// Максимальный номинал одного ордера, $ (не больше 1000).
        #[arg(long)]
        max_notional_usd: rust_decimal::Decimal,
        /// `all`, `hyperliquid`, `lighter` или `aster`.
        #[arg(long, default_value = "all")]
        venue: String,
        /// Символ Hyperliquid (ядро `BTC` или HIP-3 `xyz:NVDA`).
        #[arg(long, default_value = "BTC")]
        symbol_hyperliquid: String,
        /// Символ Lighter.
        #[arg(long, default_value = "ETH")]
        symbol_lighter: String,
        /// Символ Aster.
        #[arg(long, default_value = "BTCUSDT")]
        symbol_aster: String,
        /// HIP-3 декс для проверки перевода $1 туда и обратно.
        #[arg(long)]
        transfer_dex: Option<String>,
        /// Обязательно для mainnet: ордера реальные.
        #[arg(long)]
        i_understand_real_money: bool,
    },
    /// Производные Parquet за сутки UTC (ночное задание).
    Derive {
        /// Каталог сырых записей.
        #[arg(long, default_value = "data/raw")]
        raw: PathBuf,
        /// Каталог Parquet.
        #[arg(long, default_value = "data/parquet")]
        out: PathBuf,
        /// Дата `YYYY-MM-DD`; по умолчанию — вчера (UTC).
        #[arg(long)]
        date: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Inspect { paths, json } => inspect(&paths, json),
        Cmd::Dump { path } => dump(&path),
        Cmd::Scan { replay, from, to, out } => {
            let cfg = AppConfig::load(&cli.config, cli.overlay.as_deref())?;
            init_cli_logging();
            let (from, to) =
                (parse_time(&from).map_err(anyhow::Error::msg)?, parse_time(&to).map_err(anyhow::Error::msg)?);
            let s = runner::run_replay(&cfg, &replay, from, to, &out)?;
            println!(
                "records {} | daily reports {} | positions opened {} | final equity {} | max drawdown {}%",
                s.records,
                s.reports,
                s.opened,
                s.final_equity.round_dp(2),
                (s.max_drawdown * rust_decimal::Decimal::ONE_HUNDRED).round_dp(2)
            );
            Ok(())
        }
        Cmd::Derive { raw, out, date } => {
            init_cli_logging();
            let day = match date {
                Some(d) => parse_time(&d).map_err(anyhow::Error::msg)?,
                None => {
                    let now = chrono::Utc::now().timestamp_millis();
                    now - now.rem_euclid(86_400_000) - 86_400_000
                }
            };
            let s = spread_hunter::derive::derive_day(&raw, &out, day)?;
            println!(
                "{}: records {} | bbo {} | funding {} | book levels {} | oi {}",
                s.out_dir.display(),
                s.records,
                s.bbo_rows,
                s.funding_rows,
                s.book_rows,
                s.oi_rows
            );
            Ok(())
        }
        cmd => {
            let cfg = AppConfig::load(&cli.config, cli.overlay.as_deref())?;
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(async move {
                match cmd {
                    Cmd::Run { live, i_understand_real_money, dry_run } => {
                        let flags =
                            spread_hunter::live::LiveFlags { live, real_money: i_understand_real_money, dry_run };
                        runner::run_live(cfg, flags).await
                    }
                    Cmd::Metadata { venue } => metadata(cfg, &venue).await,
                    Cmd::Selftest {
                        network,
                        max_notional_usd,
                        venue,
                        symbol_hyperliquid,
                        symbol_lighter,
                        symbol_aster,
                        transfer_dex,
                        i_understand_real_money,
                    } => {
                        init_cli_logging();
                        let symbols = std::collections::BTreeMap::from([
                            (sh_core::Exchange::Hyperliquid, symbol_hyperliquid),
                            (sh_core::Exchange::Lighter, symbol_lighter),
                            (sh_core::Exchange::Aster, symbol_aster),
                        ]);
                        let args = spread_hunter::selftest::CliArgs {
                            network,
                            max_notional_usd,
                            venue,
                            symbols,
                            transfer_dex,
                            real_money: i_understand_real_money,
                        };
                        if spread_hunter::selftest::run_cli(cfg, args).await? {
                            Ok(())
                        } else {
                            anyhow::bail!("selftest: some steps failed")
                        }
                    }
                    _ => Ok(()),
                }
            })
        }
    }
}

/// Логи в stderr для разовых команд.
fn init_cli_logging() {
    let _ = sh_obs::init_logging(&sh_obs::LogConfig { filter: "warn".into(), json_stdout: false, dir: None });
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
