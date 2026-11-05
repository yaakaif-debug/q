//! `backtest`: событийный повтор записей через движок, стратегии, риск и симулятор.

use std::path::PathBuf;

use clap::Parser;
use spread_hunter::config::AppConfig;
use spread_hunter::replay_source::parse_time;

#[derive(Parser)]
#[command(name = "backtest", version, about = "Event-driven backtest over raw recordings")]
struct Cli {
    /// Базовый конфиг.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
    /// Наложение.
    #[arg(long)]
    overlay: Option<PathBuf>,
    /// Каталог сырых записей.
    #[arg(long, default_value = "data/raw")]
    replay: PathBuf,
    /// Начало: `YYYY-MM-DD` или RFC 3339 (UTC).
    #[arg(long)]
    from: String,
    /// Конец (не включительно).
    #[arg(long)]
    to: String,
    /// Каталог результатов.
    #[arg(long, default_value = "reports/backtest")]
    out: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = AppConfig::load(&cli.config, cli.overlay.as_deref())?;
    let _ = sh_obs::init_logging(&sh_obs::LogConfig { filter: "info".into(), json_stdout: false, dir: None });
    let from = parse_time(&cli.from).map_err(anyhow::Error::msg)?;
    let to = parse_time(&cli.to).map_err(anyhow::Error::msg)?;
    let s = spread_hunter::backtest::run_backtest(&cfg, &cli.replay, from, to, &cli.out)?;
    println!(
        "records {} | orders {} | pairs {} | equity {} -> {} | unknown orders {}",
        s.records,
        s.orders,
        s.pairs,
        s.start_equity.round_dp(2),
        s.end_equity.round_dp(2),
        s.unknown_orders
    );
    for (k, v) in s.by_kind {
        println!("  {k}: {}", v.round_dp(2));
    }
    Ok(())
}
