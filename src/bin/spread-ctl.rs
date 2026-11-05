//! `spread-ctl`: управление демоном через Unix-сокет.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use sh_core::Exchange;
use spread_hunter::ctl::{CtlRequest, request};
use spread_hunter::engine::OperatorCommand;

#[derive(Parser)]
#[command(name = "spread-ctl", version, about = "Control the spread-hunter daemon")]
struct Cli {
    /// Сокет демона.
    #[arg(long, default_value = "run/spread-hunter.sock")]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Состояние: пары, задачи, риск (JSON).
    Status,
    /// Kill switch: отменить все ордера и запретить новые.
    Kill {
        /// Также закрыть все позиции reduce-only срезами.
        #[arg(long)]
        flatten: bool,
    },
    /// Снять kill switch.
    ReleaseKill,
    /// Снять Halted после разбора расхождения.
    Resume,
    /// Приостановить новые входы.
    Pause,
    /// Возобновить новые входы.
    Unpause,
    /// Выйти из пары.
    Exit {
        /// Идентификатор пары (`UNDERLYING:a~b`).
        pair: String,
    },
    /// Принять позиции площадки как истину (после ручного разбора расхождения).
    Adopt {
        /// hyperliquid, lighter или aster.
        exchange: String,
    },
    /// Перебалансировка обеспечения.
    Rebalance {
        #[command(subcommand)]
        cmd: RebalanceCmd,
    },
    /// Учётная выгрузка за год (CSV) прямо из базы; демон не нужен.
    Export {
        /// Год (UTC).
        #[arg(long)]
        year: i32,
        /// База состояния (paper: data/state/state.sqlite, live: data/state/live.sqlite).
        #[arg(long, default_value = "data/state/live.sqlite")]
        database: PathBuf,
        /// Валюта отчёта (например, EUR); нужен `--rates`.
        #[arg(long)]
        currency: Option<String>,
        /// CSV дневных курсов `YYYY-MM-DD,rate` (единиц валюты за 1 USD).
        #[arg(long)]
        rates: Option<PathBuf>,
        /// Файл результата; по умолчанию — stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum RebalanceCmd {
    /// План переводов с неподписанными данными (ничего не подписывается и не отправляется).
    Plan {
        /// Уже в пути, $.
        #[arg(long, default_value = "0")]
        in_transit_usd: rust_decimal::Decimal,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let req = match cli.cmd {
        Cmd::Export { year, database, currency, rates, out } => {
            let ccy = match (currency, rates) {
                (Some(c), Some(r)) => {
                    let text = std::fs::read_to_string(&r)?;
                    Some((c, spread_hunter::export::parse_rates(&text).map_err(anyhow::Error::msg)?))
                }
                (None, None) => None,
                _ => anyhow::bail!("--currency and --rates go together"),
            };
            let csv = rt.block_on(spread_hunter::export::export_year(&database, year, ccy))?;
            match out {
                Some(p) => std::fs::write(&p, csv)?,
                None => print!("{csv}"),
            }
            return Ok(());
        }
        Cmd::Rebalance { cmd: RebalanceCmd::Plan { in_transit_usd } } => CtlRequest::RebalancePlan { in_transit_usd },
        Cmd::Status => CtlRequest::Status,
        Cmd::Kill { flatten } => CtlRequest::Command(OperatorCommand::Kill { flatten, source: "spread-ctl".into() }),
        Cmd::ReleaseKill => CtlRequest::Command(OperatorCommand::ReleaseKill),
        Cmd::Resume => CtlRequest::Command(OperatorCommand::Resume),
        Cmd::Pause => CtlRequest::Command(OperatorCommand::PauseEntries { paused: true }),
        Cmd::Unpause => CtlRequest::Command(OperatorCommand::PauseEntries { paused: false }),
        Cmd::Exit { pair } => CtlRequest::Command(OperatorCommand::ExitPair { pair_id: pair }),
        Cmd::Adopt { exchange } => {
            let exchange: Exchange = exchange.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
            CtlRequest::Command(OperatorCommand::AdoptPositions { exchange })
        }
    };
    let resp = rt.block_on(request(&cli.socket, &req))?;
    match resp.body {
        serde_json::Value::String(s) => println!("{s}"),
        v => println!("{}", serde_json::to_string_pretty(&v)?),
    }
    if !resp.ok {
        std::process::exit(1);
    }
    Ok(())
}
