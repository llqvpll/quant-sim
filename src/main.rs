//! `quant-sim` 命令行入口。
//!
//! 五个子命令的职责边界很重要：只有 `run` 与 `backtest` 需要大模型，因此只有它们
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
mod store;
mod types;
mod util;

use std::rc::Rc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::backtest::Backtest;
use crate::bot::{print_summary, Bot, LockGuard};
use crate::calendar::Calendar;
use crate::error::{AppError, AppResult};
use crate::http::HttpClient;
use crate::llm::{LlmClient, LlmConfig};
use crate::market::MarketData;
use crate::simulator::Simulator;
use crate::store::Paths;
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
    },
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
        } => cmd_backtest(
            &symbol,
            &symbols,
            days,
            decision_every,
            capital,
            slippage_bps,
            max_volume_ratio,
        ),
    }
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

fn cmd_backtest(
    symbol: &str,
    symbols: &str,
    days: Option<u32>,
    decision_every: Option<u32>,
    capital: Option<f64>,
    slippage_bps: f64,
    max_volume_ratio: f64,
) -> AppResult<()> {
    if let Some(value) = capital {
        if value <= 0.0 {
            return Err(AppError::Invalid("--capital 必须为正数".to_string()));
        }
    }
    let paths = Paths::resolve();
    let cal = Rc::new(Calendar::load(&paths));
    // 回测天数与决策频率的默认值来自账户配置，所以先读一次 state。
    let cfg = {
        let sim = Simulator::open(&paths, capital, Rc::clone(&cal))?;
        sim.cfg().clone()
    };

    let symbol_list = parse_symbols(symbol, symbols);
    if symbol_list.is_empty() {
        return Err(AppError::Invalid("--symbols 至少要给一个代码".to_string()));
    }

    let llm_cfg = LlmConfig::from_env(&cfg)?;
    let fast = Duration::from_secs(FAST_TIMEOUT_SECS);
    let slow = Duration::from_secs(cfg.ai_timeout_seconds as u64 + SLOW_TIMEOUT_MARGIN_SECS);
    let ai = LlmClient::new(HttpClient::new(fast, slow)?, llm_cfg);
    let mut md = MarketData::new(HttpClient::new(fast, slow)?, Rc::clone(&cal));

    let mut engine = Backtest::new(
        &mut md,
        &ai,
        &paths,
        Rc::clone(&cal),
        symbol_list,
        days.unwrap_or(cfg.backtest_days) as usize,
        decision_every.unwrap_or(cfg.backtest_decision_every) as usize,
        capital,
        slippage_bps,
        max_volume_ratio,
    );
    // 回测的头部与结果行（含结果文件路径）由 Backtest::run 自己打印，这里不重复。
    let result = engine.run()?;
    match result.error {
        Some(error) => Err(AppError::Business(error)),
        None => Ok(()),
    }
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
            } => {
                assert_eq!(symbol, "600519");
                assert_eq!(symbols, "");
                assert_eq!(days, Some(60));
                assert_eq!(decision_every, None);
                assert_eq!(capital, None);
                assert_eq!(slippage_bps, 5.0);
                assert_eq!(max_volume_ratio, 0.05);
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
