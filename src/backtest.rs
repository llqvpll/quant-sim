//! 组合回测：收盘决策、次日开盘成交。
//!
//! 关键的诚实性约束：决策只用当日及以前的数据（指标走 `as_of` 截断），成交价一律取
//! 次日开盘价并加滑点。当日收盘决策按当日收盘成交会让收益虚高，这条不变量不可放松。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::rc::Rc;

use serde::Serialize;

use crate::calendar::Calendar;
use crate::config::Config;
use crate::error::AppResult;
use crate::indicators;
use crate::rules;
use crate::signal::{AnalyzeRequest, SignalAction};
use crate::simulator::{BuyRequest, SellRequest, Simulator, Snapshot};
use crate::store::{self, NetValue, Order, Paths};
use crate::types::{AiSource, Bar, MarketSource};
use crate::util;

/// 每个标的至少要有的 K 线根数，太短算不出指标。
const MIN_ROWS: usize = 40;
/// 合并后至少要有这么多交易日，否则回测区间没有意义。
const MIN_DATES: usize = 45;
/// 起点要求每个标的都已累计这么多根 K 线（与指标最小样本对齐）。
const WARMUP_BARS: usize = 30;
/// 回测区间至少这么多个交易日。
const MIN_RANGE_DAYS: usize = 10;

pub struct PendingOrder {
    pub code: String,
    pub name: String,
    pub action: String,
    pub ratio: f64,
    pub reason: String,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BacktestTrade {
    pub date: String,
    pub action: String,
    pub code: String,
    pub name: String,
    pub price: f64,
    pub shares: i64,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BacktestResult {
    pub symbols: Vec<String>,
    pub start: String,
    pub end: String,
    pub trading_days: usize,
    pub initial: f64,
    /// 落盘键名固定为 `final`（外部契约），Rust 侧字段用 `final_assets` 表达语义。
    #[serde(rename = "final")]
    pub final_assets: f64,
    pub total_return_pct: f64,
    pub annualized_pct: f64,
    pub benchmark_return_pct: f64,
    pub excess_return_pct: f64,
    pub max_drawdown_pct: f64,
    pub sharpe: f64,
    pub ai_calls: u64,
    pub ai_errors: u64,
    pub slippage_bps: f64,
    pub max_volume_ratio: f64,
    pub trades: Vec<BacktestTrade>,
    pub net_values: Vec<NetValue>,
    pub closed_trades: usize,
    pub win_rate: f64,
    pub avg_trade_pct: f64,
    pub summary: Snapshot,
    // 结果文件写盘时这两项还没有值，跳过序列化：落盘结构里不该出现恒为 null 的键
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct Backtest<'a, M: MarketSource, A: AiSource> {
    symbols: Vec<String>,
    days: usize,
    decision_every: usize,
    capital: Option<f64>,
    slippage_bps: f64,
    max_volume_ratio: f64,
    bars: HashMap<String, HashMap<String, Bar>>,
    prev_close: HashMap<String, HashMap<String, f64>>,
    dates: Vec<String>,
    pending: Vec<PendingOrder>,
    trades: Vec<BacktestTrade>,
    ai_calls: u64,
    ai_errors: u64,
    md: &'a mut M,
    ai: &'a A,
    cal: Rc<Calendar>,
    paths: &'a Paths,
}

impl<'a, M: MarketSource, A: AiSource> Backtest<'a, M, A> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        md: &'a mut M,
        ai: &'a A,
        paths: &'a Paths,
        cal: Rc<Calendar>,
        symbols: Vec<String>,
        days: usize,
        decision_every: usize,
        capital: Option<f64>,
        slippage_bps: f64,
        max_volume_ratio: f64,
    ) -> Self {
        let symbols: Vec<String> = symbols
            .into_iter()
            .map(|symbol| symbol.trim().to_string())
            .filter(|symbol| !symbol.is_empty())
            .collect();
        Backtest {
            symbols,
            days: days.max(MIN_RANGE_DAYS),
            decision_every: decision_every.max(1),
            capital,
            slippage_bps: slippage_bps.max(0.0),
            max_volume_ratio: max_volume_ratio.clamp(0.0, 1.0),
            bars: HashMap::new(),
            prev_close: HashMap::new(),
            dates: Vec::new(),
            pending: Vec::new(),
            trades: Vec::new(),
            ai_calls: 0,
            ai_errors: 0,
            md,
            ai,
            cal,
            paths,
        }
    }

    pub fn run(&mut self) -> AppResult<BacktestResult> {
        if !self.load() {
            return Ok(self.failure("K线数据不足或标的为空"));
        }
        let Some(start) = self.start_index() else {
            return Ok(self.failure("无法找到所有标的同时具备 30 根 K 线的起点"));
        };
        let end = (start + self.days).min(self.dates.len() - 1);
        let days: Vec<String> = self.dates[start..=end].to_vec();
        if days.len() < MIN_RANGE_DAYS {
            return Ok(self.failure("回测区间太短"));
        }

        // 每次回测都用全新账户，旧的专用状态文件先删掉再建
        let slug = util::hash8(&self.symbols.join(","));
        let state_path = self.paths.backtest_state(&slug);
        if state_path.exists() {
            let _ = std::fs::remove_file(&state_path);
        }
        let state = store::new_state(self.capital)?;
        let mut sim = Simulator::at_state(state, state_path, Rc::clone(&self.cal));

        util::log(&format!(
            "[回测] {} | {} ~ {} | {} 个交易日 | 每 {} 日决策 | 初始 {:.0}",
            self.symbols.join(", "),
            days[0],
            days[days.len() - 1],
            days.len(),
            self.decision_every,
            sim.state.initial_capital
        ));

        for (index, day) in days.iter().enumerate() {
            self.execute_pending(day, &mut sim);
            self.mark(day, &mut sim);
            if index % self.decision_every == 0 {
                self.decide(day, &sim);
            }
            sim.snapshot(Some(day))?;
        }

        let last_day = days[days.len() - 1].clone();
        self.liquidate(&last_day, &mut sim);
        sim.snapshot(Some(&last_day))?;
        self.collect(&days, &last_day, &sim)
    }

    // ---------- 数据 ----------
    fn load(&mut self) -> bool {
        let limit = self.days + 120;
        let symbols = self.symbols.clone();
        let mut all_dates: BTreeSet<String> = BTreeSet::new();
        for symbol in symbols {
            let rows = self.md.get_kline(&symbol, "1d", limit, None);
            if rows.len() < MIN_ROWS {
                util::log(&format!("[回测] {symbol} K线不足({}根)，跳过", rows.len()));
                continue;
            }
            let mut mapping: HashMap<String, Bar> = HashMap::new();
            let mut previous: HashMap<String, f64> = HashMap::new();
            let mut last_close: Option<f64> = None;
            for row in rows {
                if let Some(prev) = last_close {
                    previous.insert(row.date.clone(), prev);
                }
                last_close = Some(row.close);
                all_dates.insert(row.date.clone());
                mapping.insert(row.date.clone(), row);
            }
            self.bars.insert(symbol.clone(), mapping);
            self.prev_close.insert(symbol, previous);
        }
        self.dates = all_dates.into_iter().collect();
        self.dates.len() >= MIN_DATES && !self.bars.is_empty()
    }

    /// 所有标的同时累计满 30 根 K 线的那一天。
    fn start_index(&self) -> Option<usize> {
        let mut counts: BTreeMap<&str, usize> =
            self.bars.keys().map(|key| (key.as_str(), 0)).collect();
        for (index, day) in self.dates.iter().enumerate() {
            for (symbol, mapping) in self.bars.iter() {
                if mapping.contains_key(day) {
                    if let Some(slot) = counts.get_mut(symbol.as_str()) {
                        *slot += 1;
                    }
                }
            }
            if counts.values().all(|count| *count >= WARMUP_BARS) {
                return Some(index);
            }
        }
        None
    }

    // ---------- 交易 ----------
    fn queue(&mut self, order: PendingOrder) {
        if self.pending.iter().any(|item| item.code == order.code) {
            return;
        }
        self.pending.push(order);
    }

    fn execute_pending(&mut self, day: &str, sim: &mut Simulator) {
        // 取走挂单即视为本轮结束：失败也不留到明天补单
        for order in std::mem::take(&mut self.pending) {
            let code = order.code.clone();
            let Some(bar) = self.bars.get(&code).and_then(|mapping| mapping.get(day)) else {
                // 当天停牌/无数据：撤单，绝不拿过期价格凑成交
                continue;
            };
            let open = bar.open;
            let volume = bar.volume;
            let previous = self
                .prev_close
                .get(&code)
                .and_then(|mapping| mapping.get(day))
                .copied();
            // A 股最小价位 0.01，成交价必须落在 tick 上；不做取整会产出 9.834915
            // 这种不存在的价格。
            // 取整会让小额滑点被吸收：0.01 的最小变动意味着 5bp 滑点要推动一个 tick
            // 需价格 ≥10 元，低价股的成交价会等于开盘价。这是预期行为，不是滑点未生效；
            // 验证滑点链路请用 50bp 或 10 元以上的标的。
            let price = if order.action == "BUY" {
                rules::round_price(open * (1.0 + self.slippage_bps / 10_000.0))
            } else {
                rules::round_price(open * (1.0 - self.slippage_bps / 10_000.0))
            };
            let outcome = if order.action == "BUY" {
                let budget = sim.total_assets() * order.ratio;
                let mut shares = if price > 0.0 {
                    rules::normalize_buy_shares(&code, (budget / price) as i64)
                } else {
                    0
                };
                // 单笔不超过当日成交量的 max_volume_ratio（K 线成交量单位为手）
                if self.max_volume_ratio > 0.0 && volume > 0.0 {
                    let cap = (volume * 100.0 * self.max_volume_ratio) as i64;
                    shares = shares.min(cap / 100 * 100);
                }
                if shares <= 0 || shares as f64 * price < sim.cfg().min_trade_amount {
                    continue;
                }
                sim.buy(&BuyRequest {
                    code: &code,
                    name: &order.name,
                    price,
                    shares,
                    reason: &order.reason,
                    trade_date: util::parse_date(day),
                    prev_close: previous,
                    stop_loss: order.stop_loss,
                    take_profit: order.take_profit,
                })
            } else {
                sim.sell(&SellRequest {
                    code: &code,
                    price,
                    shares: -1,
                    reason: &order.reason,
                    trade_date: util::parse_date(day),
                    prev_close: previous,
                })
            };
            match outcome {
                Ok(outcome) if outcome.success => {
                    let shares = outcome.order.as_ref().map(|item| item.shares).unwrap_or(0);
                    self.trades.push(BacktestTrade {
                        date: day.to_string(),
                        action: order.action,
                        code,
                        name: order.name,
                        price,
                        shares,
                        reason: order.reason,
                    });
                }
                Ok(outcome) => util::log(&format!(
                    "  [回测] {day} {} {} 未成交: {}",
                    order.action, code, outcome.msg
                )),
                Err(err) => util::log(&format!(
                    "  [回测] {day} {} {} 落单失败: {err}",
                    order.action, code
                )),
            }
        }
    }

    fn mark(&mut self, day: &str, sim: &mut Simulator) {
        let mut prices: HashMap<String, f64> = HashMap::new();
        for code in sim.state.positions.keys() {
            if let Some(bar) = self.bars.get(code).and_then(|mapping| mapping.get(day)) {
                prices.insert(code.clone(), bar.close);
            }
        }
        if prices.is_empty() {
            return;
        }
        let _ = sim.update_prices(&prices);
        for signal in sim.check_stop_loss_take_profit(&prices) {
            self.queue(PendingOrder {
                code: signal.code.clone(),
                name: signal.code,
                action: "SELL".to_string(),
                ratio: 0.0,
                reason: format!("回测{}", signal.reason),
                stop_loss: None,
                take_profit: None,
            });
        }
    }

    fn decide(&mut self, day: &str, sim: &Simulator) {
        let symbols: Vec<String> = self.bars.keys().cloned().collect();
        for symbol in symbols {
            let Some(bar) = self
                .bars
                .get(&symbol)
                .and_then(|mapping| mapping.get(day))
                .cloned()
            else {
                continue;
            };
            if self.pending.iter().any(|item| item.code == symbol) {
                continue;
            }
            let held = sim.state.positions.contains_key(&symbol);
            let question = if held {
                "我已持有该股票，请分析是继续持有还是卖出？"
            } else {
                "现在适合买入吗？请给出明确的操作建议（买入/持有/观望/卖出）。"
            };
            // as_of 必须传决策日，否则指标会把未来数据算进来
            let technical = indicators::get_indicators(
                &mut *self.md,
                &self.cal,
                &symbol,
                120,
                Some(day),
                util::now_local(),
            )
            .map(|ind| ind.signal_txt)
            .unwrap_or_default();
            let context = account_context(sim);
            let sig = self.ai.analyze(&AnalyzeRequest {
                symbol: symbol.as_str(),
                question,
                technical: Some(technical.as_str()),
                news: None,
                experience: None,
                extra: Some(context.as_str()),
                as_of: Some(day),
                verify_price: Some(bar.close),
                expected_name: None,
            });
            self.ai_calls += 1;
            if matches!(sig.signal, SignalAction::Error) {
                self.ai_errors += 1;
                continue;
            }
            let confidence = sig.confidence;
            if matches!(sig.signal, SignalAction::Buy) && !held {
                if confidence < sim.cfg().confidence_buy_threshold {
                    continue;
                }
                let mut ratio = confidence_ratio(confidence, sim.cfg());
                if let Some(pct) = sig.position_pct {
                    if pct == 0 {
                        continue;
                    }
                    ratio = ratio.min(pct as f64 / 100.0);
                }
                let ratio = ratio.min(sim.cfg().confidence_max_ratio);
                self.queue(PendingOrder {
                    code: symbol.clone(),
                    name: symbol.clone(),
                    action: "BUY".to_string(),
                    ratio,
                    reason: format!("AI买入(置信{:.0}%)", confidence * 100.0),
                    stop_loss: sig.stop_loss,
                    take_profit: sig.take_profit,
                });
            } else if matches!(sig.signal, SignalAction::Sell) && held {
                self.queue(PendingOrder {
                    code: symbol.clone(),
                    name: symbol.clone(),
                    action: "SELL".to_string(),
                    ratio: 0.0,
                    reason: format!("AI卖出(置信{:.0}%)", confidence * 100.0),
                    stop_loss: None,
                    take_profit: None,
                });
            }
        }
    }

    /// 期末清仓：用最后一天收盘价卖出；T+1 不允许卖的保留市值，不强卖。
    fn liquidate(&mut self, last_day: &str, sim: &mut Simulator) {
        let codes: Vec<String> = sim.state.positions.keys().cloned().collect();
        for code in codes {
            let Some(bar) = self
                .bars
                .get(&code)
                .and_then(|mapping| mapping.get(last_day))
                .cloned()
            else {
                continue;
            };
            let previous = self
                .prev_close
                .get(&code)
                .and_then(|mapping| mapping.get(last_day))
                .copied();
            let outcome = sim.sell(&SellRequest {
                code: &code,
                price: bar.close,
                shares: -1,
                reason: "回测结束清仓",
                trade_date: util::parse_date(last_day),
                prev_close: previous,
            });
            if let Ok(outcome) = outcome {
                if outcome.success {
                    let shares = outcome.order.as_ref().map(|item| item.shares).unwrap_or(0);
                    self.trades.push(BacktestTrade {
                        date: last_day.to_string(),
                        action: "SELL".to_string(),
                        code: code.clone(),
                        name: code,
                        price: bar.close,
                        shares,
                        reason: "期末清仓".to_string(),
                    });
                }
            }
        }
    }

    // ---------- 指标 ----------
    fn benchmark(&self, start_day: &str, end_day: &str) -> f64 {
        let mut returns: Vec<f64> = Vec::new();
        for mapping in self.bars.values() {
            let start = match mapping.get(start_day) {
                Some(bar) => Some(bar),
                None => self
                    .dates
                    .iter()
                    .filter(|day| day.as_str() >= start_day)
                    .find_map(|day| mapping.get(day)),
            };
            let Some(start) = start else {
                continue;
            };
            let end = match mapping.get(end_day) {
                Some(bar) => Some(bar),
                None => self
                    .dates
                    .iter()
                    .filter(|day| day.as_str() <= end_day)
                    .filter_map(|day| mapping.get(day))
                    .next_back(),
            };
            let Some(end) = end else {
                continue;
            };
            if start.close > 0.0 {
                returns.push(end.close / start.close - 1.0);
            }
        }
        if returns.is_empty() {
            return 0.0;
        }
        returns.iter().sum::<f64>() / returns.len() as f64 * 100.0
    }

    fn collect(
        &mut self,
        days: &[String],
        last_day: &str,
        sim: &Simulator,
    ) -> AppResult<BacktestResult> {
        let net_values: Vec<f64> = sim
            .state
            .daily_net_values
            .iter()
            .map(|item| item.total_assets)
            .collect();
        let initial = sim.state.initial_capital;
        let final_assets = net_values
            .last()
            .copied()
            .unwrap_or_else(|| sim.total_assets());
        let total_return = if initial != 0.0 {
            (final_assets - initial) / initial * 100.0
        } else {
            0.0
        };
        let benchmark = self.benchmark(&days[0], last_day);
        // 净值序列首日即起点，收益期数按“净值点数 - 1”算，至少 1 以避免除零
        let trading_days = (net_values.len().max(2) - 1) as f64;
        let annualized = if initial > 0.0 && final_assets > 0.0 {
            ((final_assets / initial).powf(252.0 / trading_days) - 1.0) * 100.0
        } else {
            0.0
        };
        let (closed_trades, win_rate, avg_trade_pct) = closed_stats(&sim.state.orders);

        let mut result = BacktestResult {
            symbols: self.symbols.clone(),
            start: days[0].clone(),
            end: last_day.to_string(),
            trading_days: days.len(),
            initial: util::round_to(initial, 2),
            final_assets: util::round_to(final_assets, 2),
            total_return_pct: util::round_to(total_return, 2),
            annualized_pct: util::round_to(annualized, 2),
            benchmark_return_pct: util::round_to(benchmark, 2),
            excess_return_pct: util::round_to(total_return - benchmark, 2),
            max_drawdown_pct: util::round_to(max_drawdown(&net_values), 2),
            sharpe: util::round_to(sharpe(&net_values), 2),
            ai_calls: self.ai_calls,
            ai_errors: self.ai_errors,
            slippage_bps: self.slippage_bps,
            max_volume_ratio: self.max_volume_ratio,
            trades: std::mem::take(&mut self.trades),
            net_values: sim.state.daily_net_values.clone(),
            closed_trades,
            win_rate,
            avg_trade_pct,
            summary: sim.summary(),
            output: None,
            error: None,
        };

        let path = self.paths.backtest_result();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::to_string_pretty(&result)?;
        std::fs::write(&path, format!("{text}\n"))?;
        result.output = Some(path.display().to_string());
        print_report(&result);
        Ok(result)
    }

    fn failure(&self, message: &str) -> BacktestResult {
        util::log(&format!("[回测] 失败: {message}"));
        BacktestResult {
            symbols: self.symbols.clone(),
            error: Some(message.to_string()),
            ..BacktestResult::default()
        }
    }
}

pub fn max_drawdown(values: &[f64]) -> f64 {
    let mut peak = values.first().copied().unwrap_or(0.0);
    let mut worst = 0.0f64;
    for value in values {
        if *value > peak {
            peak = *value;
        }
        if peak > 0.0 {
            worst = worst.max((peak - value) / peak * 100.0);
        }
    }
    worst
}

/// 夏普比率：总体标准差（不是样本标准差）× √252。
pub fn sharpe(values: &[f64]) -> f64 {
    if values.len() < 3 {
        return 0.0;
    }
    let returns: Vec<f64> = values
        .windows(2)
        .filter(|pair| pair[0] > 0.0)
        .map(|pair| pair[1] / pair[0] - 1.0)
        .collect();
    if returns.len() < 2 {
        return 0.0;
    }
    let count = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / count;
    let variance = returns
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f64>()
        / count;
    let std = variance.sqrt();
    if std == 0.0 {
        return 0.0;
    }
    mean / std * 252f64.sqrt()
}

/// FIFO 配对统计已平仓交易：(笔数, 胜率%, 平均收益%)。
pub fn closed_stats(orders: &[Order]) -> (usize, f64, f64) {
    let mut lots: HashMap<&str, VecDeque<(i64, f64)>> = HashMap::new();
    let mut pnls: Vec<f64> = Vec::new();
    for order in orders {
        if order.code.is_empty() || order.shares <= 0 {
            continue;
        }
        let shares = order.shares as f64;
        if order.action == "BUY" {
            let fee = order.commission + order.transfer_fee;
            lots.entry(order.code.as_str())
                .or_default()
                .push_back((order.shares, (order.amount + fee) / shares));
        } else if order.action == "SELL" {
            let fee = order.commission + order.stamp_tax + order.transfer_fee;
            let net = (order.amount - fee) / shares;
            let queue = lots.entry(order.code.as_str()).or_default();
            let mut remaining = order.shares;
            while remaining > 0 {
                let Some(front) = queue.front_mut() else {
                    break;
                };
                let take = front.0.min(remaining);
                if front.1 != 0.0 {
                    pnls.push((net - front.1) / front.1 * 100.0);
                }
                front.0 -= take;
                remaining -= take;
                if front.0 <= 0 {
                    queue.pop_front();
                }
            }
        }
    }
    if pnls.is_empty() {
        return (0, 0.0, 0.0);
    }
    let wins = pnls.iter().filter(|value| **value > 0.0).count();
    let win_rate = util::round_to(wins as f64 / pnls.len() as f64 * 100.0, 1);
    let mean = pnls.iter().sum::<f64>() / pnls.len() as f64;
    (pnls.len(), win_rate, util::round_to(mean, 2))
}

/// 置信度映射到仓位比例，超过阈值后线性放大到上限。
fn confidence_ratio(confidence: f64, cfg: &Config) -> f64 {
    let threshold = cfg.confidence_buy_threshold;
    if confidence < threshold {
        return 0.0;
    }
    let base = cfg.confidence_base_ratio;
    let maximum = cfg.confidence_max_ratio;
    let ratio = base + (confidence - threshold) * (maximum - base) / (1.0 - threshold).max(1e-9);
    ratio.clamp(0.0, maximum)
}

fn account_context(sim: &Simulator) -> String {
    let cfg = sim.cfg();
    format!(
        "回测账户总资产 {:.0} 元，持仓上限 {} 只，单票上限 {:.0}%，佣金{:.2}‱（最低{:.0}元），印花税{:.2}%，T+1，涨跌停不可成交。",
        sim.total_assets(),
        cfg.max_positions,
        cfg.max_single_position_ratio * 100.0,
        cfg.commission_rate * 10000.0,
        cfg.min_commission,
        cfg.stamp_tax_rate * 100.0
    )
}

fn print_report(result: &BacktestResult) {
    println!();
    util::log("===== 回测结果 =====");
    util::log(&format!(
        "区间: {} ~ {} | 交易日 {} | AI 调用 {} 次(失败 {})",
        result.start, result.end, result.trading_days, result.ai_calls, result.ai_errors
    ));
    util::log(&format!(
        "收益: {}% | 年化 {}% | 基准 {}% | 超额 {}%",
        py_float(result.total_return_pct),
        py_float(result.annualized_pct),
        py_float(result.benchmark_return_pct),
        py_float(result.excess_return_pct)
    ));
    util::log(&format!(
        "最大回撤: {}% | 夏普 {} | 平仓 {} 笔 | 胜率 {}%",
        py_float(result.max_drawdown_pct),
        py_float(result.sharpe),
        result.closed_trades,
        py_float(result.win_rate)
    ));
    util::log(&format!(
        "期末资产: {:.2} | 结果文件: {}",
        result.final_assets,
        result.output.clone().unwrap_or_default()
    ));
}

/// 整数值的浮点打印带上 `.0`（如 1.0 而非 1），终端结果更易读。
fn py_float(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::AiSignal;
    use crate::store::{AiUsage, Position, State};
    use chrono::{Datelike, NaiveDate};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!(
            "quant-sim-bt-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("data")).expect("创建临时目录");
        Paths { base_dir: base }
    }

    /// 造一段确定性行情：开盘 = 昨收 + 0.1，收盘 = 开盘 + 0.1，逐日上涨。
    fn make_rows(count: usize, start: f64) -> Vec<Bar> {
        let mut rows = Vec::with_capacity(count);
        let mut day = NaiveDate::from_ymd_opt(2026, 1, 5).expect("固定日期");
        let mut price = start;
        for index in 0..count {
            while day.weekday().num_days_from_monday() >= 5 {
                day = day.succ_opt().expect("日期递增");
            }
            let open = price + 0.1;
            let close = open + 0.1;
            rows.push(Bar {
                date: util::ymd(day),
                open,
                close,
                high: close + 0.2,
                low: open - 0.2,
                volume: 1000.0 + index as f64,
            });
            price = close;
            day = day.succ_opt().expect("日期递增");
        }
        rows
    }

    struct FakeMarket {
        rows: Vec<Bar>,
    }

    impl MarketSource for FakeMarket {
        fn market_is_open(&mut self, _now: chrono::NaiveDateTime) -> bool {
            false
        }

        fn get_all_a_stocks(&mut self, _force: bool) -> Vec<crate::types::StockInfo> {
            Vec::new()
        }

        fn get_realtime(&mut self, _symbol: &str) -> Option<crate::types::Quote> {
            None
        }

        fn get_realtime_batch(
            &mut self,
            _symbols: &[String],
        ) -> HashMap<String, crate::types::Quote> {
            HashMap::new()
        }

        fn get_kline(
            &mut self,
            _symbol: &str,
            _period: &str,
            limit: usize,
            end_date: Option<&str>,
        ) -> Vec<Bar> {
            let filtered: Vec<Bar> = self
                .rows
                .iter()
                .filter(|bar| match end_date {
                    Some(cut) => bar.date.as_str() <= cut,
                    None => true,
                })
                .map(|bar| Bar {
                    date: bar.date.clone(),
                    open: bar.open,
                    close: bar.close,
                    high: bar.high,
                    low: bar.low,
                    volume: bar.volume,
                })
                .collect();
            let start = filtered.len().saturating_sub(limit);
            filtered.into_iter().skip(start).collect()
        }
    }

    /// 永远看多、置信 0.9、目标仓位 10%。
    struct FakeAi;

    impl AiSource for FakeAi {
        fn analyze(&self, _req: &AnalyzeRequest<'_>) -> AiSignal {
            AiSignal {
                signal: SignalAction::Buy,
                confidence: 0.9,
                position_pct: Some(10),
                reason: "test".to_string(),
                ..AiSignal::default()
            }
        }
    }

    fn order(action: &str, price: f64, shares: i64) -> Order {
        let amount = price * shares as f64;
        let (commission, stamp_tax) = if action == "BUY" {
            (5.0, 0.0)
        } else {
            (5.0, amount * 0.0005)
        };
        Order {
            id: 1,
            time: "2026-01-05T10:00:00".to_string(),
            action: action.to_string(),
            code: "600000".to_string(),
            name: "浦发银行".to_string(),
            price,
            shares,
            amount,
            commission,
            transfer_fee: amount * 0.00001,
            stamp_tax,
            reason: String::new(),
        }
    }

    #[test]
    fn max_drawdown_zero_when_monotonic_up() {
        assert_eq!(max_drawdown(&[1.0, 2.0, 3.0, 4.0]), 0.0);
        assert!((max_drawdown(&[100.0, 80.0, 90.0]) - 20.0).abs() < 1e-9);
        assert!((max_drawdown(&[100.0, 90.0, 50.0]) - 50.0).abs() < 1e-9);
        assert_eq!(max_drawdown(&[]), 0.0);
    }

    #[test]
    fn sharpe_zero_for_flat_and_short_series() {
        assert_eq!(sharpe(&[100.0, 100.0, 100.0, 100.0]), 0.0);
        assert_eq!(sharpe(&[100.0, 110.0]), 0.0);
        assert_eq!(sharpe(&[]), 0.0);
        // 稳定上涨时为正
        assert!(sharpe(&[100.0, 101.0, 102.0, 103.0]) > 0.0);
    }

    #[test]
    fn closed_stats_counts_fifo_partial_sells() {
        let orders = vec![
            order("BUY", 10.0, 200),
            order("SELL", 11.0, 100),
            order("SELL", 12.0, 100),
        ];
        let (count, win_rate, avg) = closed_stats(&orders);
        assert_eq!(count, 2, "部分卖出应按批次拆成 2 笔");
        assert_eq!(win_rate, 100.0);
        assert!(avg > 0.0);
        // 没有成交时全零
        assert_eq!(closed_stats(&[]), (0, 0.0, 0.0));
    }

    #[test]
    fn benchmark_is_equal_weighted() {
        let mut market = FakeMarket {
            rows: make_rows(60, 10.0),
        };
        let ai = FakeAi;
        let paths = temp_paths("bench");
        let mut bt = Backtest::new(
            &mut market,
            &ai,
            &paths,
            Rc::new(Calendar::empty()),
            vec!["600000".to_string()],
            20,
            5,
            Some(50_000.0),
            0.0,
            0.05,
        );
        assert!(bt.load());
        let start = bt.dates[0].clone();
        let end = bt.dates[bt.dates.len() - 1].clone();
        let benchmark = bt.benchmark(&start, &end);
        // 单标的时基准就等于该标的的区间涨幅
        let mapping = bt.bars.get("600000").expect("有行情");
        let expected = mapping[&end].close / mapping[&start].close - 1.0;
        assert!((benchmark - expected * 100.0).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn start_index_waits_for_all_symbols() {
        let mut market = FakeMarket {
            rows: make_rows(60, 10.0),
        };
        let ai = FakeAi;
        let paths = temp_paths("start");
        let mut bt = Backtest::new(
            &mut market,
            &ai,
            &paths,
            Rc::new(Calendar::empty()),
            vec!["600000".to_string()],
            20,
            5,
            Some(50_000.0),
            0.0,
            0.05,
        );
        assert!(bt.load());
        // 单标的下起点就是第 30 根 K 线所在位置
        assert_eq!(bt.start_index(), Some(WARMUP_BARS - 1));
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    /// 最需要守住的不变量：成交价是执行日开盘价，不是决策日收盘价。
    #[test]
    fn executes_next_open_not_signal_close() {
        let rows = make_rows(90, 10.0);
        let row_map: HashMap<String, Bar> = rows
            .iter()
            .map(|bar| {
                (
                    bar.date.clone(),
                    Bar {
                        date: bar.date.clone(),
                        open: bar.open,
                        close: bar.close,
                        high: bar.high,
                        low: bar.low,
                        volume: bar.volume,
                    },
                )
            })
            .collect();
        let mut market = FakeMarket { rows };
        let ai = FakeAi;
        let paths = temp_paths("next-open");
        let mut bt = Backtest::new(
            &mut market,
            &ai,
            &paths,
            Rc::new(Calendar::empty()),
            vec!["600000".to_string(), "000001".to_string()],
            20,
            5,
            Some(50_000.0),
            0.0,
            0.05,
        );
        let result = bt.run().expect("回测可运行");
        assert!(result.error.is_none(), "错误: {:?}", result.error);
        let first_buy = result
            .trades
            .iter()
            .find(|trade| trade.action == "BUY")
            .expect("应该有买入");
        assert!(
            first_buy.date > result.start,
            "成交日必须晚于回测起点：{} vs {}",
            first_buy.date,
            result.start
        );
        let bar = row_map.get(&first_buy.date).expect("成交日有行情");
        assert!(
            (first_buy.price - rules::round_price(bar.open)).abs() < 1e-9,
            "无滑点时成交价必须等于该日开盘价（取整到分）：{} vs {}",
            first_buy.price,
            bar.open
        );
        // 成交价必须落在 0.01 的整数倍上，不能出现 9.834915 这种价格
        let ticks = first_buy.price * 100.0;
        assert!(
            (ticks - ticks.round()).abs() < 1e-6,
            "成交价必须落在 tick 上：{}",
            first_buy.price
        );
        assert!(first_buy.price < bar.close, "开盘价必须低于当日收盘价");
        assert!(result.ai_calls > 0);
        assert!(result.net_values.len() >= 2);
        assert!(paths.backtest_result().exists(), "结果文件应落盘");
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn slippage_moves_price_in_right_direction() {
        let rows = make_rows(90, 10.0);
        let row_map: HashMap<String, Bar> = rows
            .iter()
            .map(|bar| {
                (
                    bar.date.clone(),
                    Bar {
                        date: bar.date.clone(),
                        open: bar.open,
                        close: bar.close,
                        high: bar.high,
                        low: bar.low,
                        volume: bar.volume,
                    },
                )
            })
            .collect();
        let mut market = FakeMarket { rows };
        let ai = FakeAi;
        let paths = temp_paths("slip");
        let mut bt = Backtest::new(
            &mut market,
            &ai,
            &paths,
            Rc::new(Calendar::empty()),
            vec!["600000".to_string()],
            20,
            5,
            Some(50_000.0),
            50.0,
            0.05,
        );
        let result = bt.run().expect("回测可运行");
        let first_buy = result
            .trades
            .iter()
            .find(|trade| trade.action == "BUY")
            .expect("应该有买入");
        let open = row_map.get(&first_buy.date).expect("成交日有行情").open;
        // 期望值用与实现完全相同的算式，避免 `1.005` 字面量与 `1 + 50/10000` 的末位差异
        // 在 x.xx5 边界上偶发失败；同时仍能卡住"用收盘价成交/滑点方向反/漏取整"三种退化。
        let expected = rules::round_price(open * (1.0 + 50.0 / 10_000.0));
        assert!(
            (first_buy.price - expected).abs() < 1e-9,
            "成交价 {} 应为开盘价 {} 加 50bp 滑点后取整的 {}",
            first_buy.price,
            open,
            expected
        );
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn insufficient_data_returns_error_field() {
        let mut market = FakeMarket {
            rows: make_rows(20, 10.0),
        };
        let ai = FakeAi;
        let paths = temp_paths("short");
        let mut bt = Backtest::new(
            &mut market,
            &ai,
            &paths,
            Rc::new(Calendar::empty()),
            vec!["600000".to_string()],
            60,
            5,
            Some(50_000.0),
            5.0,
            0.05,
        );
        let result = bt.run().expect("不 panic");
        assert_eq!(result.error.as_deref(), Some("K线数据不足或标的为空"));
        assert!(result.trades.is_empty());
        assert!(!paths.backtest_result().exists(), "失败时不应写结果文件");
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    /// 真实行情冒烟：不调 AI（无 Key 也能跑），只验证数据加载、次日开盘成交、落盘与指标。
    #[test]
    #[ignore = "需要联网取真实 K 线，手动用 --ignored 运行"]
    fn live_backtest_with_real_kline() {
        use crate::http::HttpClient;
        use crate::market::MarketData;
        use std::time::Duration;

        let paths = temp_paths("live");
        let cal = Rc::new(Calendar::load(&paths));
        let http = HttpClient::new(Duration::from_secs(10), Duration::from_secs(30)).expect("HTTP");
        let mut md = MarketData::new(http, Rc::clone(&cal));
        let ai = FakeAi;

        // 茅台一手要 14 万，5 万本金按单票 25% 上限根本买不起：这属于真实约束，不是 bug
        let mut bt = Backtest::new(
            &mut md,
            &ai,
            &paths,
            Rc::clone(&cal),
            vec!["600519".to_string()],
            60,
            5,
            Some(50_000.0),
            5.0,
            0.05,
        );
        let result = bt.run().expect("回测可运行");
        assert!(result.error.is_none(), "错误: {:?}", result.error);
        assert!(result.trading_days >= 10);
        assert!(!result.net_values.is_empty());
        println!(
            "600519 | 收益 {}% | 基准 {}% | 平仓 {} 笔 | 成交 {} 笔",
            result.total_return_pct,
            result.benchmark_return_pct,
            result.closed_trades,
            result.trades.len()
        );

        // 低价股才能真成交，用它验证“次日开盘价成交”在真实数据上成立
        let mut bt = Backtest::new(
            &mut md,
            &ai,
            &paths,
            Rc::clone(&cal),
            vec!["600000".to_string()],
            60,
            5,
            Some(50_000.0),
            5.0,
            0.05,
        );
        let result = bt.run().expect("回测可运行");
        assert!(result.error.is_none(), "错误: {:?}", result.error);
        let first_buy = result
            .trades
            .iter()
            .find(|trade| trade.action == "BUY")
            .expect("低价股应能成交");
        assert!(first_buy.date > result.start);
        assert!(first_buy.price > 0.0);
        assert!(paths.backtest_result().exists());
        println!(
            "600000 | 收益 {}% | 基准 {}% | 首笔买入 {} @ {}",
            result.total_return_pct, result.benchmark_return_pct, first_buy.date, first_buy.price
        );
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    /// `backtest_result.json` 外层英文 snake_case、内嵌 summary 中文键，两层风格不同，
    /// 这里一次性固化，避免后人「统一键名」时把看板/QA 的字段约定改坏。
    #[test]
    fn result_file_keeps_two_key_styles() {
        let mut market = FakeMarket {
            rows: make_rows(90, 10.0),
        };
        let ai = FakeAi;
        let paths = temp_paths("result-keys");
        let mut bt = Backtest::new(
            &mut market,
            &ai,
            &paths,
            Rc::new(Calendar::empty()),
            vec!["600000".to_string()],
            20,
            5,
            Some(50_000.0),
            0.0,
            0.05,
        );
        let result = bt.run().expect("回测可运行");
        assert!(result.error.is_none(), "错误: {:?}", result.error);
        let text = std::fs::read_to_string(paths.backtest_result()).expect("读结果文件");
        let value: serde_json::Value = serde_json::from_str(&text).expect("结果文件是合法 JSON");

        for key in [
            "symbols",
            "start",
            "end",
            "trading_days",
            "initial",
            "total_return_pct",
            "annualized_pct",
            "benchmark_return_pct",
            "excess_return_pct",
            "max_drawdown_pct",
            "sharpe",
            "ai_calls",
            "ai_errors",
            "slippage_bps",
            "max_volume_ratio",
            "trades",
            "net_values",
            "closed_trades",
            "win_rate",
            "avg_trade_pct",
        ] {
            assert!(value.get(key).is_some(), "结果文件缺少键 {key}");
        }
        // 期末资产对外键名是 final，`final_assets` 只存在于 Rust 侧
        assert!(value.get("final").is_some());
        assert!(value.get("final_assets").is_none());
        // 写盘时这两项还没值，落盘结构里也不该出现恒为 null 的键
        assert!(value.get("output").is_none());
        assert!(value.get("error").is_none());
        // 内嵌成绩单仍是中文键
        assert!(value["summary"]["初始资金"].is_number());
        assert!(value["summary"]["浮动盈亏"].is_number());
        assert!(value["summary"]["持仓列表"].is_array());
        assert!(value["summary"]["熔断状态"].is_string());
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn confidence_ratio_and_context_are_stable() {
        let cfg = Config::default();
        assert_eq!(confidence_ratio(0.5, &cfg), 0.0);
        assert!((confidence_ratio(0.65, &cfg) - 0.15).abs() < 1e-9);
        assert!((confidence_ratio(1.0, &cfg) - 0.25).abs() < 1e-9);
        assert!(confidence_ratio(0.9, &cfg) <= cfg.confidence_max_ratio);

        let state = State {
            version: 2,
            created_at: String::new(),
            initial_capital: 50_000.0,
            cash: 50_000.0,
            positions: std::collections::BTreeMap::<String, Position>::new(),
            orders: Vec::new(),
            daily_net_values: Vec::new(),
            signal_log: Vec::new(),
            peak_assets: 50_000.0,
            halted: false,
            halt_reason: String::new(),
            halted_at: None,
            ai_usage: AiUsage::default(),
            last_scan: serde_json::Value::Null,
            next_order_id: 1,
            config: cfg,
        };
        let sim = Simulator::at_state(
            state,
            std::env::temp_dir().join("quant-sim-bt-context.json"),
            Rc::new(Calendar::empty()),
        );
        let context = account_context(&sim);
        assert!(context.contains("总资产 50000 元"));
        assert!(context.contains("持仓上限 3 只"));
        assert!(context.contains("T+1"));
    }
}
