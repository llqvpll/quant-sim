//! `quant-sim` 命令行入口。
//!
//! 六个子命令的职责边界很重要：只有 `run` 与 `backtest` 需要大模型，因此只有它们
//! 会构造 `LlmClient`。`summary` / `reset` / `dashboard` 在没有 API Key 的机器上
//! 也必须能用——否则用户连自己的持仓和成绩单都看不到。

mod backtest;
mod bot;
mod calendar;
mod config;
mod dashboard;
mod error;
mod http;
mod indicators;
mod learner;
mod llm;
mod market;
mod news;
mod rules;
mod signal;
mod simulator;
mod sizing;
mod snapshot;
mod store;
mod types;
mod util;
mod walk;

use std::rc::Rc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::backtest::{Backtest, BacktestSettings};
use crate::bot::{print_summary, Bot, LockGuard};
use crate::calendar::Calendar;
use crate::error::{AppError, AppResult};
use crate::http::HttpClient;
use crate::llm::{LlmClient, LlmConfig};
use crate::market::MarketData;
use crate::simulator::Simulator;
use crate::store::Paths;
use crate::types::MarketSource;
use crate::util::{log, now_local};

/// 行情短请求超时（秒）。
const FAST_TIMEOUT_SECS: u64 = 10;
/// 大模型请求附加余量（秒）。
const SLOW_TIMEOUT_MARGIN_SECS: u64 = 30;

#[derive(Parser)]
#[command(
    name = "quant-sim",
    version,
    about = "A 股 AI 模拟盘量化交易系统",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 运行机器人（单次扫描或盘中循环）
    Run {
        #[arg(long)]
        once: bool,
        #[arg(long = "loop")]
        loop_: bool,
        #[arg(long)]
        scan: Option<u32>,
        #[arg(long)]
        interval: Option<u64>,
        #[arg(long, allow_hyphen_values = true)]
        capital: Option<f64>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        reset: bool,
    },
    /// 打印账户成绩单
    Summary {
        #[arg(long, allow_hyphen_values = true)]
        capital: Option<f64>,
        #[arg(long)]
        reset: bool,
    },
    /// 重置模拟账户
    Reset {
        #[arg(long, allow_hyphen_values = true)]
        capital: Option<f64>,
    },
    /// 启动本地看板
    Dashboard {
        #[arg(long, default_value_t = 8000)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
    /// 历史数据回测
    Backtest {
        #[arg(long, default_value = "600519")]
        symbol: String,
        #[arg(long, default_value = "")]
        symbols: String,
        /// 回测交易日数；多窗口模式下为参与检验的总天数上限，不传则用满快照可用区间
        #[arg(long)]
        days: Option<u32>,
        #[arg(long)]
        decision_every: Option<u32>,
        #[arg(long, allow_hyphen_values = true)]
        capital: Option<f64>,
        #[arg(long, default_value_t = 5.0)]
        slippage_bps: f64,
        #[arg(long, default_value_t = 0.05)]
        max_volume_ratio: f64,
        /// 基准指数代码；裸 6 位码按上证指数处理，也可显式写 sh000300 / sz399300
        #[arg(long, default_value = "000300")]
        benchmark: String,
        /// 从 data/backtest_signals.jsonl 重放信号，不再调用模型
        #[arg(long)]
        replay: bool,
        /// 使用本地行情快照（快照 id 或 latest），不联网取行情
        #[arg(long)]
        snapshot: Option<String>,
        /// 滚动窗口数，>=2 时切成互不重叠的连续窗口各跑一遍；
        /// 多窗口模式下 --days 为参与检验的总交易日上限，不传 --days 时使用快照全部可用交易日
        #[arg(long)]
        walk: Option<u32>,
    },
    /// 管理本地行情快照
    Snapshot {
        #[command(subcommand)]
        action: SnapshotCmd,
    },
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// 抓取当前全市场列表与指定标的日 K，落盘成带 id 的快照
    Create {
        /// 逗号分隔的标的代码
        #[arg(long, default_value = "")]
        symbols: String,
        /// 回测天数，决定快照多取多少根 K 线
        #[arg(long)]
        days: Option<u32>,
        /// 基准指数代码
        #[arg(long, default_value = "000300")]
        benchmark: String,
    },
    /// 列出已有快照
    List,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            log(&format!("[错误] {err}"));
            std::process::ExitCode::from(1)
        }
    }
}

fn dispatch(cli: Cli) -> AppResult<()> {
    match cli.cmd {
        Cmd::Run {
            once,
            loop_,
            scan,
            interval,
            capital,
            force,
            reset,
        } => cmd_run(once, loop_, scan, interval, capital, force, reset),
        Cmd::Summary { capital, reset } => cmd_summary(capital, reset),
        Cmd::Reset { capital } => cmd_reset(capital),
        Cmd::Dashboard { port, host } => cmd_dashboard(&host, port),
        Cmd::Backtest {
            symbol,
            symbols,
            days,
            decision_every,
            capital,
            slippage_bps,
            max_volume_ratio,
            benchmark,
            replay,
            snapshot,
            walk,
        } => {
            let args = BacktestCliArgs {
                symbol,
                symbols,
                days,
                decision_every,
                capital,
                slippage_bps,
                max_volume_ratio,
                benchmark,
                replay,
                snapshot,
                walk,
            };
            cmd_backtest(&args)
        }
        Cmd::Snapshot { action } => match action {
            SnapshotCmd::Create {
                symbols,
                days,
                benchmark,
            } => cmd_snapshot_create(&symbols, days, &benchmark),
            SnapshotCmd::List => cmd_snapshot_list(),
        },
    }
}

/// 回测子命令的原始输入，与 clap 字段一一对应。
struct BacktestCliArgs {
    symbol: String,
    symbols: String,
    days: Option<u32>,
    decision_every: Option<u32>,
    capital: Option<f64>,
    slippage_bps: f64,
    max_volume_ratio: f64,
    benchmark: String,
    replay: bool,
    snapshot: Option<String>,
    walk: Option<u32>,
}

fn cmd_run(
    once: bool,
    loop_: bool,
    scan: Option<u32>,
    interval: Option<u64>,
    capital: Option<f64>,
    force: bool,
    reset: bool,
) -> AppResult<()> {
    bot::validate_run_args(capital, scan, interval)?;
    let paths = Paths::resolve();
    let cal = Rc::new(Calendar::load(&paths));
    // 锁在函数结束（含 `?` 提前返回）时由 Drop 释放。
    let _lock = LockGuard::acquire(&paths)
        .ok_or_else(|| AppError::Invalid("单实例锁被占用（见上一条日志）".to_string()))?;

    if reset {
        Simulator::reset(&paths, capital, false, Rc::clone(&cal))?;
    }

    // 未指定 --once/--loop 时先判交易时段：非交易时段直接退出，不因为缺 Key 报错，
    // 这样"跑一下看看"的用户看到的是业务提示而不是配置错误。
    let implicit_loop = !once && !loop_;
    if implicit_loop && !force {
        // 探测只用行情通道，慢通道给个宽松值即可，不会真的发请求。
        let probe_http = HttpClient::new(
            Duration::from_secs(FAST_TIMEOUT_SECS),
            Duration::from_secs(FAST_TIMEOUT_SECS * 3),
        )?;
        let mut probe = MarketData::new(probe_http, Rc::clone(&cal));
        if !probe.market_is_open(now_local()) {
            log("当前非交易时段；单次扫描已禁用，如需强制请加 --force");
            return Ok(());
        }
        log("当前为交易时段，进入盘中循环模式");
    }

    // 缺 Key 时这里会返回可读错误，直接以退出码 1 结束，不打堆栈。
    let mut bot = Bot::boot(paths, Rc::clone(&cal), capital)?;
    bot.force = force;
    let top_n = scan.map(|value| value as usize);

    if once {
        bot.scan_once(top_n, Some(force))?;
        return Ok(());
    }
    bot.run_loop(interval, top_n)
}

fn cmd_summary(capital: Option<f64>, reset: bool) -> AppResult<()> {
    if let Some(value) = capital {
        if value <= 0.0 {
            return Err(AppError::Invalid("--capital 必须为正数".to_string()));
        }
    }
    let paths = Paths::resolve();
    let cal = Rc::new(Calendar::load(&paths));
    if reset {
        Simulator::reset(&paths, capital, false, Rc::clone(&cal))?;
    }
    // 成绩单只读账户，不需要大模型与网络。
    let sim = Simulator::open(&paths, capital, cal)?;
    print_summary(&sim.summary());
    Ok(())
}

fn cmd_reset(capital: Option<f64>) -> AppResult<()> {
    if let Some(value) = capital {
        if value <= 0.0 {
            return Err(AppError::Invalid("--capital 必须为正数".to_string()));
        }
    }
    let paths = Paths::resolve();
    let cal = Rc::new(Calendar::load(&paths));
    Simulator::reset(&paths, capital, false, cal)?;
    Ok(())
}

fn cmd_dashboard(host: &str, port: u16) -> AppResult<()> {
    let paths = Paths::resolve();
    dashboard::serve(&paths, host, port)
}

fn cmd_backtest(args: &BacktestCliArgs) -> AppResult<()> {
    if let Some(value) = args.capital {
        if value <= 0.0 {
            return Err(AppError::Invalid("--capital 必须为正数".to_string()));
        }
    }
    let paths = Paths::resolve();
    let cal = Rc::new(Calendar::load(&paths));
    // 回测天数与决策频率的默认值来自账户配置，所以先读一次 state。
    let cfg = {
        let sim = Simulator::open(&paths, args.capital, Rc::clone(&cal))?;
        sim.cfg().clone()
    };

    let symbol_list = parse_symbols(&args.symbol, &args.symbols);
    if symbol_list.is_empty() {
        return Err(AppError::Invalid("--symbols 至少要给一个代码".to_string()));
    }

    // 重放时模型不会被调用，但仍需一个 AiSource 实例；用占位 Key 构造，避免无 Key 机器上无法复现。
    let llm_cfg = if args.replay {
        LlmConfig::resolve(
            &|name| {
                if name.ends_with("API_KEY") {
                    Some("replay-offline".to_string())
                } else {
                    None
                }
            },
            &cfg,
        )
    } else {
        LlmConfig::from_env(&cfg)
    }?;
    let ai_model = llm_cfg.model.clone();
    let fast = Duration::from_secs(FAST_TIMEOUT_SECS);
    let slow = Duration::from_secs(cfg.ai_timeout_seconds as u64 + SLOW_TIMEOUT_MARGIN_SECS);
    let llm_http = HttpClient::new(fast, slow)?;
    let ai = if args.replay {
        LlmClient::new_quiet(llm_http, llm_cfg)
    } else {
        LlmClient::new(llm_http, llm_cfg)
    };
    // 快照模式完全不构造 MarketData：离线源结构体里没有任何 HTTP 客户端。
    let mut source: Box<dyn MarketSource>;
    let snapshot_id: Option<String>;
    if let Some(requested) = &args.snapshot {
        let resolved = snapshot::resolve_id(&paths, requested)?;
        snapshot_id = Some(resolved.clone());
        source = Box::new(snapshot::SnapshotSource::open(&paths, &resolved)?);
    } else {
        snapshot_id = None;
        source = Box::new(MarketData::new(
            HttpClient::new(fast, slow)?,
            Rc::clone(&cal),
        ));
    }

    let mut settings = BacktestSettings::new(
        symbol_list,
        args.days.unwrap_or(cfg.backtest_days) as usize,
        args.decision_every.unwrap_or(cfg.backtest_decision_every) as usize,
        args.capital,
        args.slippage_bps,
        args.max_volume_ratio,
    );
    settings.benchmark = args.benchmark.clone();
    settings.replay = args.replay;
    settings.ai_model = ai_model;
    settings.snapshot_id = snapshot_id;
    settings.walk_windows = walk::validate_walk_arg(args.walk).map_err(AppError::Invalid)?;
    settings.walk_days_limit = args.days.map(|value| value as usize);
    let mut engine = Backtest::new(&mut source, &ai, &paths, Rc::clone(&cal), settings);
    if engine_walks(args.walk) {
        // 多窗口只写 backtest_walk.json，不覆盖单段结果文件。
        let walk = engine.run_walk()?;
        if walk.results.len() != walk.report.windows {
            return Err(AppError::Invalid("滚动窗口段数与报告不一致".to_string()));
        }
        return Ok(());
    }
    // 回测的头部与结果行（含结果文件路径）由 Backtest::run 自己打印，这里不重复。
    let result = engine.run()?;
    match result.error {
        Some(error) => Err(AppError::Business(error)),
        None => Ok(()),
    }
}

fn engine_walks(walk: Option<u32>) -> bool {
    walk.unwrap_or(0) >= 2
}

fn cmd_snapshot_create(symbols: &str, days: Option<u32>, benchmark: &str) -> AppResult<()> {
    let list = parse_symbols("", symbols);
    if list.is_empty() {
        return Err(AppError::Invalid("--symbols 至少要给一个代码".to_string()));
    }
    let paths = Paths::resolve();
    let cal = Rc::new(Calendar::load(&paths));
    let fast = Duration::from_secs(FAST_TIMEOUT_SECS);
    let slow = Duration::from_secs(
        config::Config::default().ai_timeout_seconds as u64 + SLOW_TIMEOUT_MARGIN_SECS,
    );
    let mut md = MarketData::new(HttpClient::new(fast, slow)?, Rc::clone(&cal));
    let (meta, files) = snapshot::create(
        &paths,
        &mut md,
        &list,
        benchmark,
        days.unwrap_or(config::Config::default().backtest_days) as usize,
    )?;
    log(&format!(
        "[快照] 已创建 {}：全市场 {} 只 / K线 {} 份 / 基准 {} / 文件 {} 个",
        meta.id,
        meta.symbols.len(),
        meta.kline_symbols.len(),
        meta.benchmark_source,
        files
    ));
    log(&format!("[快照] {}", meta.note));
    Ok(())
}

fn cmd_snapshot_list() -> AppResult<()> {
    let paths = Paths::resolve();
    let entries = snapshot::list(&paths)?;
    if entries.is_empty() {
        log("[快照] 暂无快照，可用 snapshot create 创建");
        return Ok(());
    }
    for entry in entries {
        log(&format!(
            "[快照] {} | {} | K线 {} 只",
            entry.id, entry.created_at, entry.kline_count
        ));
    }
    Ok(())
}

/// `--symbols "600519,000001"` 优先；没给就用 `--symbol`。
fn parse_symbols(symbol: &str, symbols: &str) -> Vec<String> {
    let raw = if symbols.trim().is_empty() {
        symbol
    } else {
        symbols
    };
    raw.split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_accepts_all_documented_flags() {
        let cli = Cli::try_parse_from([
            "quant-sim",
            "run",
            "--once",
            "--scan",
            "5",
            "--capital",
            "50000",
            "--force",
            "--reset",
        ])
        .expect("run 参数应被接受");
        match cli.cmd {
            Cmd::Run {
                once,
                loop_,
                scan,
                capital,
                force,
                reset,
                ..
            } => {
                assert!(once);
                assert!(!loop_);
                assert_eq!(scan, Some(5));
                assert_eq!(capital, Some(50_000.0));
                assert!(force);
                assert!(reset);
            }
            _ => panic!("应解析为 run 子命令"),
        }
    }

    #[test]
    fn run_accepts_loop_flag_name() {
        let cli = Cli::try_parse_from(["quant-sim", "run", "--loop", "--interval", "300"]).unwrap();
        match cli.cmd {
            Cmd::Run {
                loop_, interval, ..
            } => {
                assert!(loop_);
                assert_eq!(interval, Some(300));
            }
            _ => panic!("应解析为 run 子命令"),
        }
    }

    #[test]
    fn dashboard_has_documented_defaults() {
        let cli = Cli::try_parse_from(["quant-sim", "dashboard"]).unwrap();
        match cli.cmd {
            Cmd::Dashboard { port, host } => {
                assert_eq!(port, 8000);
                assert_eq!(host, "127.0.0.1");
            }
            _ => panic!("应解析为 dashboard 子命令"),
        }
    }

    #[test]
    fn backtest_defaults_match_contract() {
        let cli = Cli::try_parse_from([
            "quant-sim",
            "backtest",
            "--symbol",
            "600519",
            "--days",
            "60",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Backtest {
                symbol,
                symbols,
                days,
                decision_every,
                capital,
                slippage_bps,
                max_volume_ratio,
                benchmark,
                replay,
                snapshot,
                walk,
            } => {
                assert_eq!(symbol, "600519");
                assert_eq!(symbols, "");
                assert_eq!(days, Some(60));
                assert_eq!(decision_every, None);
                assert_eq!(capital, None);
                assert_eq!(slippage_bps, 5.0);
                assert_eq!(max_volume_ratio, 0.05);
                assert_eq!(benchmark, "000300");
                assert!(!replay);
                assert!(snapshot.is_none());
                assert!(walk.is_none());
            }
            _ => panic!("应解析为 backtest 子命令"),
        }
    }

    #[test]
    fn backtest_accepts_benchmark_and_replay() {
        let cli = Cli::try_parse_from([
            "quant-sim",
            "backtest",
            "--benchmark",
            "sz399300",
            "--replay",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Backtest {
                benchmark, replay, ..
            } => {
                assert_eq!(benchmark, "sz399300");
                assert!(replay);
            }
            _ => panic!("应解析为 backtest 子命令"),
        }
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["quant-sim", "unknown"]).is_err());
    }

    #[test]
    fn summary_and_reset_share_capital_flag() {
        let cli = Cli::try_parse_from(["quant-sim", "summary", "--capital", "100000", "--reset"])
            .unwrap();
        match cli.cmd {
            Cmd::Summary { capital, reset } => {
                assert_eq!(capital, Some(100_000.0));
                assert!(reset);
            }
            _ => panic!("应解析为 summary 子命令"),
        }
        let cli = Cli::try_parse_from(["quant-sim", "reset", "--capital", "20000"]).unwrap();
        match cli.cmd {
            Cmd::Reset { capital } => assert_eq!(capital, Some(20_000.0)),
            _ => panic!("应解析为 reset 子命令"),
        }
    }

    #[test]
    fn symbols_flag_overrides_single_symbol() {
        assert_eq!(
            parse_symbols("600519", "600519, 000001 ,"),
            vec!["600519".to_string(), "000001".to_string()]
        );
        assert_eq!(parse_symbols("600519", "   "), vec!["600519".to_string()]);
        assert!(parse_symbols("", " , ").is_empty());
    }

    #[test]
    fn invalid_capital_is_rejected_by_summary() {
        assert!(cmd_summary(Some(0.0), false).is_err());
        assert!(cmd_summary(Some(-5.0), false).is_err());
    }
}
