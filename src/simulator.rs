//! 模拟撮合引擎：下单校验、费用、T+1、熔断与账户统计。
//!
//! 只认 state 里的那一份配置，杜绝“看板显示一套、撮合用另一套”。

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::calendar::Calendar;
use crate::config::Config;
use crate::error::AppResult;
use crate::rules;
use crate::store::{self, NetValue, Order, Paths, Position, SignalLogEntry, State, MAX_SIGNAL_LOG};
use crate::types::RiskSignal;
use crate::util;

pub struct TradeOutcome {
    pub success: bool,
    pub msg: String,
    pub order: Option<Order>,
}

impl TradeOutcome {
    fn reject(msg: impl Into<String>) -> TradeOutcome {
        TradeOutcome {
            success: false,
            msg: msg.into(),
            order: None,
        }
    }
}

pub struct BuyRequest<'a> {
    pub code: &'a str,
    pub name: &'a str,
    pub price: f64,
    pub shares: i64,
    pub reason: &'a str,
    pub trade_date: Option<NaiveDate>,
    pub prev_close: Option<f64>,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
}

pub struct SellRequest<'a> {
    pub code: &'a str,
    pub price: f64,
    pub shares: i64,
    pub reason: &'a str,
    pub trade_date: Option<NaiveDate>,
    pub prev_close: Option<f64>,
}

pub struct SignalLogInput<'a> {
    pub code: &'a str,
    pub name: &'a str,
    pub signal: &'a str,
    pub confidence: f64,
    pub reason: &'a str,
    pub executed: bool,
    pub note: &'a str,
    pub skip_reason: &'a str,
    pub price: Option<f64>,
    pub hallucination_risk: bool,
}

/// 成绩单；键名是中文，看板与 CLI 直接展示，不可改。
///
/// `Default` 只给回测的失败结果占位用（成绩单全零），正常流程永远走 `summary()`。
#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    #[serde(rename = "初始资金")]
    pub initial_capital: f64,
    #[serde(rename = "当前总资产")]
    pub total_assets: f64,
    #[serde(rename = "总收益率%")]
    pub total_return_pct: f64,
    #[serde(rename = "现金")]
    pub cash: f64,
    #[serde(rename = "持仓市值")]
    pub market_value: f64,
    #[serde(rename = "持仓数量")]
    pub position_count: usize,
    #[serde(rename = "持仓列表")]
    pub positions: Vec<PositionView>,
    #[serde(rename = "总成交笔数")]
    pub order_count: usize,
    #[serde(rename = "已平仓盈亏")]
    pub realized_pnl: f64,
    #[serde(rename = "浮动盈亏")]
    pub unrealized_pnl: f64,
    #[serde(rename = "累计手续费")]
    pub total_fee: f64,
    #[serde(rename = "熔断状态")]
    pub halt_status: String,
    #[serde(rename = "熔断原因")]
    pub halt_reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PositionView {
    pub code: String,
    pub name: String,
    pub shares: i64,
    pub avg_cost: f64,
    pub last_price: f64,
    #[serde(rename = "盈亏%")]
    pub pnl_pct: f64,
    pub buy_date: Option<String>,
    pub available_date: Option<String>,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
}

pub struct Simulator {
    pub state: State,
    pub state_path: PathBuf,
    pub cal: Rc<Calendar>,
}

impl Simulator {
    pub fn open(paths: &Paths, capital: Option<f64>, cal: Rc<Calendar>) -> AppResult<Simulator> {
        let state = store::load(paths, capital);
        Ok(Simulator::at_state(state, paths.state(), cal))
    }

    pub fn at_state(state: State, state_path: PathBuf, cal: Rc<Calendar>) -> Simulator {
        let mut sim = Simulator {
            state,
            state_path,
            cal,
        };
        sim.state.config = sim.state.config.clone().sanitized();
        sim.backfill_available_date();
        sim
    }

    /// 备份旧账户与经验库后新建账户。
    pub fn reset(
        paths: &Paths,
        capital: Option<f64>,
        clear_experience: bool,
        cal: Rc<Calendar>,
    ) -> AppResult<Simulator> {
        store::backup_rotate(paths, "state")?;
        if clear_experience {
            store::backup_rotate(paths, "experience")?;
        }
        let sim = Simulator::open(paths, capital, cal)?;
        sim.save()?;
        util::log(&format!(
            "[状态] 模拟账户已重置: 初始资金 {:.0} 元",
            sim.state.initial_capital
        ));
        Ok(sim)
    }

    pub fn save(&self) -> AppResult<()> {
        store::save_to(&self.state_path, &self.state)
    }

    pub fn cfg(&self) -> &Config {
        &self.state.config
    }

    // ---------- 资产 ----------
    pub fn total_assets(&self) -> f64 {
        self.state.cash + self.market_value()
    }

    pub fn market_value(&self) -> f64 {
        self.state
            .positions
            .values()
            .map(|pos| pos.shares as f64 * pos.last_price)
            .sum()
    }

    pub fn total_return_pct(&self) -> f64 {
        let init = self.state.initial_capital;
        if init == 0.0 {
            0.0
        } else {
            (self.total_assets() - init) / init * 100.0
        }
    }

    // ---------- 费用 ----------
    pub fn commission(&self, amount: f64) -> f64 {
        (amount * self.cfg().commission_rate).max(self.cfg().min_commission)
    }

    pub fn transfer_fee(&self, amount: f64) -> f64 {
        amount * self.cfg().transfer_fee_rate
    }

    pub fn stamp_tax(&self, amount: f64) -> f64 {
        amount * self.cfg().stamp_tax_rate
    }

    /// 估算“买一手再卖出”的往返费用占比，含最低佣金。
    pub fn round_trip_cost_pct(&self, price: f64, code: &str) -> f64 {
        let lot = rules::min_buy_shares(code) as f64;
        let amount = price * lot;
        if amount <= 0.0 {
            return 999.0;
        }
        let buy_fee = self.commission(amount) + self.transfer_fee(amount);
        let sell_fee = self.commission(amount) + self.transfer_fee(amount) + self.stamp_tax(amount);
        (buy_fee + sell_fee) / amount * 100.0
    }

    // ---------- 风控 ----------
    pub fn check_buy_risk(&self, code: &str, price: f64, shares: i64) -> Result<(), String> {
        if self.state.positions.contains_key(code) {
            return Err(format!("已持有 {code}，不重复买入"));
        }
        if self.state.positions.len() >= self.cfg().max_positions as usize {
            return Err(format!("持仓已满 {} 只", self.cfg().max_positions));
        }
        if self.state.halted {
            return Err(format!("熔断中: {}", self.state.halt_reason));
        }
        let amount = price * shares as f64;
        let total_cost = amount + self.commission(amount) + self.transfer_fee(amount);
        if total_cost > self.state.cash + 1e-6 {
            return Err(format!(
                "现金不足: 需 {total_cost:.2}, 可用 {:.2}",
                self.state.cash
            ));
        }
        let total = self.total_assets();
        if total <= 0.0 {
            return Err("总资产为 0，无法下单".to_string());
        }
        let ratio = self.cfg().max_single_position_ratio;
        if total_cost / total > ratio + 1e-9 {
            let max_shares = (total * ratio / price) as i64;
            return Err(format!(
                "单票仓位超限({:.0}%), 最多可买 {max_shares} 股",
                ratio * 100.0
            ));
        }
        Ok(())
    }

    /// 回撤熔断：回撤恢复到 `halt_recover_pct` 或冷却期满自动解除。
    pub fn update_halt(&mut self, today: Option<NaiveDate>) -> AppResult<()> {
        let peak = if self.state.peak_assets == 0.0 {
            self.state.initial_capital
        } else {
            self.state.peak_assets
        };
        let total = self.total_assets();
        let today_date = today.unwrap_or_else(util::today);
        if total > peak {
            self.state.peak_assets = util::round_to(total, 2);
            if self.state.halted {
                self.clear_halt();
            }
        } else {
            let dd = if peak == 0.0 {
                0.0
            } else {
                (total - peak) / peak * 100.0
            };
            if !self.state.halted && dd <= self.cfg().drawdown_halt_pct {
                self.state.halted = true;
                self.state.halted_at = Some(util::ymd(today_date));
                self.state.halt_reason = format!(
                    "总资产回撤 {dd:.1}% 触发熔断；回撤恢复至 {}% 或 {} 个交易日后自动解除",
                    py_float(self.cfg().halt_recover_pct),
                    self.cfg().halt_cooldown_days
                );
            } else if self.state.halted {
                let recovered = dd >= self.cfg().halt_recover_pct;
                let mut cooled = false;
                if let Some(at) = self.state.halted_at.as_deref().and_then(util::parse_date) {
                    cooled = (today_date - at).num_days() >= self.cfg().halt_cooldown_days as i64;
                }
                if recovered || cooled {
                    self.clear_halt();
                }
            }
        }
        self.save()
    }

    pub fn check_min_hold(&self, code: &str, force: bool, now: NaiveDateTime) -> bool {
        if force {
            return self.held_minutes(code, now) >= 5.0;
        }
        for order in self.state.orders.iter().rev() {
            if order.code == code && order.action == "BUY" {
                return match util::parse_datetime(&order.time) {
                    Some(at) => {
                        (now - at).num_seconds() as f64 / 60.0 >= self.cfg().min_hold_minutes as f64
                    }
                    // 时间解析失败时宁可放行，否则持仓会被永久锁死
                    None => true,
                };
            }
        }
        true
    }

    pub fn held_minutes(&self, code: &str, now: NaiveDateTime) -> f64 {
        for order in self.state.orders.iter().rev() {
            if order.code == code && order.action == "BUY" {
                return match util::parse_datetime(&order.time) {
                    Some(at) => (now - at).num_seconds() as f64 / 60.0,
                    None => 10_000.0,
                };
            }
        }
        10_000.0
    }

    // ---------- 下单 ----------
    pub fn buy(&mut self, req: &BuyRequest<'_>) -> AppResult<TradeOutcome> {
        let code = req.code;
        let name = req.name;
        let price = req.price;
        if !price.is_finite() || price <= 0.0 {
            return Ok(TradeOutcome::reject("价格必须为正的有限数"));
        }
        let shares = rules::normalize_buy_shares(code, req.shares);
        if shares <= 0 {
            return Ok(TradeOutcome::reject(format!(
                "股数不足最低买入单位({}股)",
                rules::min_buy_shares(code)
            )));
        }
        if let Err(msg) = rules::check_price_limit(price, req.prev_close, code, name) {
            return Ok(TradeOutcome::reject(msg));
        }
        if rules::at_limit_up(price, req.prev_close, code, name) {
            return Ok(TradeOutcome::reject(format!(
                "涨停价 {price:.2} 通常买不进"
            )));
        }
        if let Err(msg) = self.check_buy_risk(code, price, shares) {
            return Ok(TradeOutcome::reject(msg));
        }

        let amount = price * shares as f64;
        let commission = self.commission(amount);
        let transfer_fee = self.transfer_fee(amount);
        let total_cost = amount + commission + transfer_fee;
        if total_cost > self.state.cash + 1e-6 {
            return Ok(TradeOutcome::reject(format!(
                "现金不足: 需 {total_cost:.2}(含费), 可用 {:.2}",
                self.state.cash
            )));
        }

        let now = util::now_local();
        let trade_day = req.trade_date.unwrap_or_else(|| now.date());
        if req.trade_date.is_some() && !self.cal.is_trading_day(trade_day) {
            return Ok(TradeOutcome::reject(format!("{trade_day} 不是交易日")));
        }

        let stop = req.stop_loss.and_then(|value| {
            let value = util::round_to(value, 2);
            if value > 0.0 && value < price {
                Some(value)
            } else {
                None
            }
        });
        let take = req.take_profit.and_then(|value| {
            let value = util::round_to(value, 2);
            if value > price {
                Some(value)
            } else {
                None
            }
        });

        self.state.cash = util::round_to(self.state.cash - total_cost, 2);
        let buy_day = util::ymd(trade_day);
        let available = rules::available_date(Some(&buy_day), &self.cal).unwrap_or(trade_day);
        self.state.positions.insert(
            code.to_string(),
            Position {
                name: name.to_string(),
                shares,
                avg_cost: total_cost / shares as f64,
                last_price: price,
                buy_date: Some(buy_day.clone()),
                available_date: Some(util::ymd(available)),
                stop_loss: stop,
                take_profit: take,
                open_time: util::now_iso(),
            },
        );

        let order = Order {
            id: self.state.next_order_id,
            time: format!("{buy_day}T{}", now.format("%H:%M:%S")),
            action: "BUY".to_string(),
            code: code.to_string(),
            name: name.to_string(),
            price,
            shares,
            amount,
            commission,
            transfer_fee,
            stamp_tax: 0.0,
            reason: req.reason.to_string(),
        };
        self.state.next_order_id += 1;
        self.state.orders.push(order.clone());
        self.save()?;
        Ok(TradeOutcome {
            success: true,
            msg: format!(
                "已买入 {name}({code}) {shares}股 @ {price:.2} (费用 {:.2})",
                commission + transfer_fee
            ),
            order: Some(order),
        })
    }

    pub fn sell(&mut self, req: &SellRequest<'_>) -> AppResult<TradeOutcome> {
        let code = req.code;
        let Some(pos) = self.state.positions.get(code) else {
            return Ok(TradeOutcome::reject(format!("未持有 {code}")));
        };
        let name = pos.name.clone();
        let held_shares = pos.shares;
        let buy_date = pos.buy_date.clone();
        let available = pos.available_date.clone();

        let price = req.price;
        if !price.is_finite() || price <= 0.0 {
            return Ok(TradeOutcome::reject("价格必须为正的有限数"));
        }

        let now = util::now_local();
        let trade_day = req.trade_date.unwrap_or_else(|| now.date());
        if req.trade_date.is_some() && !self.cal.is_trading_day(trade_day) {
            return Ok(TradeOutcome::reject(format!("{trade_day} 不是交易日")));
        }
        if !rules::is_t1_available(buy_date.as_deref(), trade_day) {
            return Ok(TradeOutcome::reject(format!(
                "T+1 限制: {} 买入，最早 {} 可卖",
                opt_text(&buy_date),
                opt_text(&available)
            )));
        }
        if let Err(msg) = rules::check_price_limit(price, req.prev_close, code, &name) {
            return Ok(TradeOutcome::reject(msg));
        }
        if rules::at_limit_down(price, req.prev_close, code, &name) {
            return Ok(TradeOutcome::reject(format!(
                "跌停价 {price:.2} 通常卖不出"
            )));
        }

        // 风控卖出（止损/止盈）不受最短持有时间限制
        if req.reason.contains("AI信号")
            && !req.reason.contains("止损")
            && !req.reason.contains("止盈")
            && !self.check_min_hold(code, false, now)
        {
            return Ok(TradeOutcome::reject(format!(
                "持有仅 {:.0} 分钟，未到最短持有 {} 分钟",
                self.held_minutes(code, now),
                self.cfg().min_hold_minutes
            )));
        }

        let shares = if req.shares == -1 || req.shares > held_shares {
            held_shares
        } else {
            req.shares
        };
        if shares <= 0 {
            return Ok(TradeOutcome::reject("股数必须为正"));
        }

        let amount = price * shares as f64;
        let commission = self.commission(amount);
        let stamp_tax = self.stamp_tax(amount);
        let transfer_fee = self.transfer_fee(amount);
        let net = amount - commission - stamp_tax - transfer_fee;

        self.state.cash = util::round_to(self.state.cash + net, 2);
        if shares >= held_shares {
            self.state.positions.remove(code);
        } else if let Some(pos) = self.state.positions.get_mut(code) {
            pos.shares = held_shares - shares;
            pos.last_price = price;
        }

        let sell_day = util::ymd(trade_day);
        let order = Order {
            id: self.state.next_order_id,
            time: format!("{sell_day}T{}", now.format("%H:%M:%S")),
            action: "SELL".to_string(),
            code: code.to_string(),
            name: name.clone(),
            price,
            shares,
            amount,
            commission,
            transfer_fee,
            stamp_tax,
            reason: req.reason.to_string(),
        };
        self.state.next_order_id += 1;
        self.state.orders.push(order.clone());
        self.save()?;
        Ok(TradeOutcome {
            success: true,
            msg: format!("已卖出 {code} {shares}股 @ {price:.2}"),
            order: Some(order),
        })
    }

    // ---------- 持仓 ----------
    pub fn update_prices(&mut self, prices: &HashMap<String, f64>) -> AppResult<()> {
        for (code, pos) in self.state.positions.iter_mut() {
            let Some(value) = prices.get(code).copied() else {
                continue;
            };
            if value.is_finite() && value > 0.0 {
                pos.last_price = value;
            }
        }
        self.save()
    }

    pub fn check_stop_loss_take_profit(&self, prices: &HashMap<String, f64>) -> Vec<RiskSignal> {
        let mut signals = Vec::new();
        for (code, pos) in self.state.positions.iter() {
            let Some(price) = prices.get(code).copied() else {
                continue;
            };
            if !price.is_finite() || price <= 0.0 {
                continue;
            }
            let avg = pos.avg_cost;
            if avg <= 0.0 {
                continue;
            }
            let pnl_pct = (price - avg) / avg * 100.0;
            let stop = pos
                .stop_loss
                .filter(|value| *value != 0.0)
                .unwrap_or(self.cfg().stop_loss_pct);
            let take = pos
                .take_profit
                .filter(|value| *value != 0.0)
                .unwrap_or(self.cfg().take_profit_pct);
            if pnl_pct <= stop {
                signals.push(RiskSignal {
                    code: code.clone(),
                    shares: -1,
                    price,
                    reason: format!("止损触发: 盈亏 {pnl_pct:+.1}% (阈值 {stop:.1}%)"),
                });
            } else if pnl_pct >= take {
                signals.push(RiskSignal {
                    code: code.clone(),
                    shares: -1,
                    price,
                    reason: format!("止盈触发: 盈亏 {pnl_pct:+.1}% (阈值 {take:.1}%)"),
                });
            }
        }
        signals
    }

    // ---------- 统计 ----------
    pub fn snapshot(&mut self, date: Option<&str>) -> AppResult<NetValue> {
        let day = match date {
            Some(text) if !text.is_empty() => text.to_string(),
            _ => util::ymd(util::today()),
        };
        self.update_halt(util::parse_date(&day))?;
        let record = NetValue {
            date: day.clone(),
            total_assets: util::round_to(self.total_assets(), 2),
            cash: util::round_to(self.state.cash, 2),
            market_value: util::round_to(self.market_value(), 2),
        };
        match self
            .state
            .daily_net_values
            .iter()
            .position(|item| item.date == day)
        {
            Some(index) => self.state.daily_net_values[index] = record.clone(),
            None => self.state.daily_net_values.push(record.clone()),
        }
        self.state
            .daily_net_values
            .sort_by(|a, b| a.date.cmp(&b.date));
        self.save()?;
        Ok(record)
    }

    pub fn log_signal(&mut self, input: &SignalLogInput<'_>) -> AppResult<()> {
        self.state.signal_log.push(SignalLogEntry {
            time: util::now_iso(),
            code: input.code.to_string(),
            name: input.name.to_string(),
            signal: input.signal.to_string(),
            confidence: input.confidence,
            reason: input.reason.to_string(),
            executed: input.executed,
            skip_reason: input.skip_reason.to_string(),
            note: input.note.to_string(),
            price: input.price,
            hallucination_risk: input.hallucination_risk,
        });
        if self.state.signal_log.len() > MAX_SIGNAL_LOG {
            let overflow = self.state.signal_log.len() - MAX_SIGNAL_LOG;
            self.state.signal_log.drain(0..overflow);
        }
        self.save()
    }

    pub fn add_ai_usage(
        &mut self,
        calls: u64,
        errors: u64,
        rule_skipped: u64,
        cost_cny: f64,
    ) -> AppResult<()> {
        let usage = &mut self.state.ai_usage;
        usage.calls += calls;
        usage.errors += errors;
        usage.rule_skipped += rule_skipped;
        usage.cost_cny = util::round_to(usage.cost_cny + cost_cny, 4);
        self.save()
    }

    /// FIFO 配对计算已平仓盈亏；未匹配的卖出（如手工导入）不计收益。
    pub fn realized_pnl(&self) -> f64 {
        let mut lots: HashMap<&str, std::collections::VecDeque<(i64, f64)>> = HashMap::new();
        let mut realized = 0.0f64;
        for order in &self.state.orders {
            if order.code.is_empty() || order.shares <= 0 {
                continue;
            }
            let shares = order.shares as f64;
            if order.action == "BUY" {
                let fee = order.commission + order.transfer_fee;
                let cost_per_share = (order.amount + fee) / shares;
                lots.entry(order.code.as_str())
                    .or_default()
                    .push_back((order.shares, cost_per_share));
            } else if order.action == "SELL" {
                let fee = order.commission + order.stamp_tax + order.transfer_fee;
                let net_per_share = (order.amount - fee) / shares;
                let queue = lots.entry(order.code.as_str()).or_default();
                let mut remaining = order.shares;
                let mut cost_basis = 0.0f64;
                let mut matched = 0i64;
                while remaining > 0 {
                    let Some(front) = queue.front_mut() else {
                        break;
                    };
                    let take = front.0.min(remaining);
                    cost_basis += take as f64 * front.1;
                    front.0 -= take;
                    remaining -= take;
                    matched += take;
                    if front.0 <= 0 {
                        queue.pop_front();
                    }
                }
                if matched > 0 {
                    realized += net_per_share * matched as f64 - cost_basis;
                }
            }
        }
        realized
    }

    pub fn summary(&self) -> Snapshot {
        let realized = self.realized_pnl();
        let mut unrealized = 0.0f64;
        let mut total_fee = 0.0f64;
        for pos in self.state.positions.values() {
            unrealized += (pos.last_price - pos.avg_cost) * pos.shares as f64;
        }
        for order in &self.state.orders {
            total_fee += order.commission + order.stamp_tax + order.transfer_fee;
        }
        let positions: Vec<PositionView> = self
            .state
            .positions
            .iter()
            .map(|(code, pos)| PositionView {
                code: code.clone(),
                name: pos.name.clone(),
                shares: pos.shares,
                avg_cost: util::round_to(pos.avg_cost, 3),
                last_price: util::round_to(pos.last_price, 3),
                pnl_pct: if pos.avg_cost != 0.0 {
                    util::round_to((pos.last_price - pos.avg_cost) / pos.avg_cost * 100.0, 2)
                } else {
                    0.0
                },
                buy_date: pos.buy_date.clone(),
                available_date: pos.available_date.clone(),
                stop_loss: pos.stop_loss,
                take_profit: pos.take_profit,
            })
            .collect();

        Snapshot {
            initial_capital: util::round_to(self.state.initial_capital, 2),
            total_assets: util::round_to(self.total_assets(), 2),
            total_return_pct: util::round_to(self.total_return_pct(), 2),
            cash: util::round_to(self.state.cash, 2),
            market_value: util::round_to(self.market_value(), 2),
            position_count: self.state.positions.len(),
            positions,
            order_count: self.state.orders.len(),
            realized_pnl: util::round_to(realized, 2),
            unrealized_pnl: util::round_to(unrealized, 2),
            total_fee: util::round_to(total_fee, 2),
            halt_status: if self.state.halted {
                "熔断中"
            } else {
                "正常"
            }
            .to_string(),
            halt_reason: self.state.halt_reason.clone(),
        }
    }

    fn clear_halt(&mut self) {
        self.state.halted = false;
        self.state.halt_reason.clear();
        self.state.halted_at = None;
    }

    /// 旧 state 里持仓缺 available_date 时用真实日历补齐（迁移阶段拿不到日历）。
    fn backfill_available_date(&mut self) {
        let cal = Rc::clone(&self.cal);
        for pos in self.state.positions.values_mut() {
            if pos.available_date.is_some() {
                continue;
            }
            let Some(buy_date) = pos.buy_date.as_deref() else {
                continue;
            };
            if let Some(day) = rules::available_date(Some(buy_date), &cal) {
                pos.available_date = Some(util::ymd(day));
            }
        }
    }
}

/// 浮点打印保留一位小数（-5.0 而非 -5），避免提示文案里出现突兀的整数阈值。
fn py_float(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

fn opt_text(value: &Option<String>) -> String {
    match value {
        Some(text) => text.clone(),
        None => "None".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::STATE_VERSION;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!(
            "quant-sim-sim-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("data")).expect("创建临时目录");
        Paths { base_dir: base }
    }

    fn open_sim(paths: &Paths) -> Simulator {
        Simulator::open(paths, Some(50_000.0), Rc::new(Calendar::empty())).expect("打开模拟账户")
    }

    fn date(text: &str) -> NaiveDate {
        util::parse_date(text).expect("测试日期合法")
    }

    fn buy_req<'a>(code: &'a str, name: &'a str, price: f64, shares: i64) -> BuyRequest<'a> {
        BuyRequest {
            code,
            name,
            price,
            shares,
            reason: "",
            trade_date: None,
            prev_close: None,
            stop_loss: None,
            take_profit: None,
        }
    }

    #[test]
    fn buy_sell_t1() {
        let paths = temp_paths("t1");
        let mut sim = open_sim(&paths);
        let mut req = buy_req("600519", "贵州茅台", 100.0, 100);
        req.reason = "AI信号: 买入(置信80%)";
        req.trade_date = Some(date("2026-10-23"));
        assert!(sim.buy(&req).expect("买入不应报错").success);

        let mut sell_req = SellRequest {
            code: "600519",
            price: 101.0,
            shares: -1,
            reason: "风控卖出",
            trade_date: Some(date("2026-10-23")),
            prev_close: None,
        };
        let blocked = sim.sell(&sell_req).expect("卖出不报错");
        assert!(!blocked.success);
        assert!(blocked.msg.contains("T+1"), "实际提示: {}", blocked.msg);

        sell_req.trade_date = Some(date("2026-10-26"));
        let sold = sim.sell(&sell_req).expect("卖出不报错");
        assert!(sold.success, "实际提示: {}", sold.msg);
        assert!(!sim.state.positions.contains_key("600519"));
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn lot_and_price_validation() {
        let paths = temp_paths("lot");
        let mut sim = open_sim(&paths);
        let small = sim
            .buy(&buy_req("688001", "华兴源创", 10.0, 100))
            .expect("买入不报错");
        assert!(!small.success);
        assert!(small.msg.contains("200"), "实际提示: {}", small.msg);
        assert!(
            sim.buy(&buy_req("688001", "华兴源创", 10.0, 200))
                .expect("买入不报错")
                .success
        );
        assert!(
            !sim.buy(&buy_req("600519", "贵州茅台", 0.0, 100))
                .expect("买入不报错")
                .success
        );
        assert_eq!(sim.summary().position_count, 1);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn price_limit_rejects_limit_up_buy() {
        let paths = temp_paths("limit");
        let mut sim = open_sim(&paths);
        let mut req = buy_req("600000", "浦发银行", 11.0, 100);
        req.prev_close = Some(10.0);
        let blocked = sim.buy(&req).expect("买入不报错");
        assert!(!blocked.success);
        assert!(blocked.msg.contains("涨停"), "实际提示: {}", blocked.msg);

        req.price = 10.5;
        assert!(sim.buy(&req).expect("买入不报错").success);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn config_is_single_source() {
        let paths = temp_paths("config");
        let mut sim = open_sim(&paths);
        sim.state.config.stop_loss_pct = -1.0;
        assert!(
            sim.buy(&buy_req("600000", "浦发银行", 10.0, 100))
                .expect("买入不报错")
                .success
        );
        let avg = sim.state.positions["600000"].avg_cost;
        let mut prices = HashMap::new();
        prices.insert("600000".to_string(), avg * 0.985);
        let signals = sim.check_stop_loss_take_profit(&prices);
        assert!(!signals.is_empty());
        assert!(
            signals[0].reason.contains("止损"),
            "实际提示: {}",
            signals[0].reason
        );
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn avg_cost_includes_fee() {
        let paths = temp_paths("avgcost");
        let mut sim = open_sim(&paths);
        assert!(
            sim.buy(&buy_req("600000", "浦发银行", 10.0, 100))
                .expect("买入不报错")
                .success
        );
        let avg = sim.state.positions["600000"].avg_cost;
        assert!(avg > 10.0);
        let summary = sim.summary();
        assert!((summary.unrealized_pnl - (10.0 - avg) * 100.0).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn fifo_partial_sell() {
        let paths = temp_paths("fifo");
        let mut sim = open_sim(&paths);
        let mut req = buy_req("600000", "浦发银行", 10.0, 200);
        req.trade_date = Some(date("2026-10-23"));
        assert!(sim.buy(&req).expect("买入不报错").success);
        let sold = sim
            .sell(&SellRequest {
                code: "600000",
                price: 11.0,
                shares: 100,
                reason: "",
                trade_date: Some(date("2026-10-26")),
                prev_close: None,
            })
            .expect("卖出不报错");
        assert!(sold.success, "实际提示: {}", sold.msg);
        let summary = sim.summary();
        assert_eq!(summary.position_count, 1);
        assert_eq!(summary.positions[0].shares, 100);
        assert!(
            summary.realized_pnl > 0.0,
            "已平仓盈亏 {}",
            summary.realized_pnl
        );
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn halt_cooldown_releases() {
        let paths = temp_paths("halt");
        let mut sim = open_sim(&paths);
        sim.state.peak_assets = 60_000.0;
        sim.update_halt(Some(date("2026-10-20"))).expect("熔断判断");
        assert!(sim.state.halted);
        assert!(sim.state.halt_reason.contains("回撤"));
        // 冷却 5 个自然日后自动解除
        sim.update_halt(Some(date("2026-10-27"))).expect("冷却解除");
        assert!(!sim.state.halted);
        assert!(sim.state.halt_reason.is_empty());
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn atomic_state_and_backup() {
        let paths = temp_paths("atomic");
        let sim = open_sim(&paths);
        sim.save().expect("第一次保存");
        sim.save().expect("第二次保存");
        assert!(paths.state().exists());
        assert!(paths.state().with_extension("json.bak").exists());
        let text = std::fs::read_to_string(paths.state()).expect("读回 state");
        serde_json::from_str::<serde_json::Value>(&text).expect("state 是合法 JSON");
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn corrupt_state_recovers_from_backup() {
        let paths = temp_paths("recover");
        {
            let sim = open_sim(&paths);
            sim.save().expect("第一次保存");
            sim.save().expect("第二次保存");
        }
        std::fs::write(paths.state(), "{broken").expect("写入坏文件");
        let recovered = open_sim(&paths);
        assert_eq!(recovered.state.cash, 50_000.0);
        assert_eq!(recovered.state.version, STATE_VERSION);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn min_hold_window_is_enforced() {
        let paths = temp_paths("minhold");
        let mut sim = open_sim(&paths);
        let mut req = buy_req("600000", "浦发银行", 10.0, 100);
        req.trade_date = Some(date("2026-10-23"));
        assert!(sim.buy(&req).expect("买入不报错").success);
        // 以订单自身时间为基准，避免依赖机器时钟
        let placed = util::parse_datetime(&sim.state.orders[0].time).expect("订单时间可解析");
        assert!(!sim.check_min_hold("600000", false, placed + chrono::Duration::minutes(10)));
        assert!(sim.check_min_hold("600000", false, placed + chrono::Duration::minutes(40)));
        // force 分支只要求 5 分钟
        assert!(!sim.check_min_hold("600000", true, placed + chrono::Duration::minutes(4)));
        assert!(sim.check_min_hold("600000", true, placed + chrono::Duration::minutes(6)));
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn empty_account_has_no_negative_zero() {
        let paths = temp_paths("negzero");
        let mut sim = open_sim(&paths);
        let snap = sim.snapshot(Some("2026-10-23")).expect("快照");
        assert!(snap.total_assets.is_sign_positive());
        assert!(snap.market_value.is_sign_positive());

        let summary = sim.summary();
        // 空仓时这些字段都应是正零，否则终端与看板会渲染成 "-0.00"
        assert!(summary.market_value.is_sign_positive());
        assert!(summary.unrealized_pnl.is_sign_positive());
        assert!(summary.realized_pnl.is_sign_positive());
        assert!(summary.total_return_pct.is_sign_positive());
        assert_eq!(format!("{:.2}", summary.market_value), "0.00");
        assert_eq!(format!("{:.2}", summary.unrealized_pnl), "0.00");
        let text = serde_json::to_string(&summary).expect("成绩单可序列化");
        assert!(!text.contains("-0.0"), "成绩单不应出现负零: {text}");
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn summary_exposes_chinese_keys() {
        let paths = temp_paths("summary");
        let sim = open_sim(&paths);
        let text = serde_json::to_string(&sim.summary()).expect("成绩单可序列化");
        for key in [
            "初始资金",
            "当前总资产",
            "总收益率%",
            "现金",
            "持仓市值",
            "持仓数量",
            "持仓列表",
            "总成交笔数",
            "已平仓盈亏",
            "浮动盈亏",
            "累计手续费",
            "熔断状态",
            "熔断原因",
        ] {
            assert!(text.contains(key), "缺少键 {key}");
        }
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn snapshot_upserts_by_date() {
        let paths = temp_paths("snapshot");
        let mut sim = open_sim(&paths);
        sim.snapshot(Some("2026-10-23")).expect("快照");
        sim.snapshot(Some("2026-10-23")).expect("重复快照");
        assert_eq!(sim.state.daily_net_values.len(), 1);
        sim.snapshot(Some("2026-10-22")).expect("补录早一天");
        assert_eq!(sim.state.daily_net_values.len(), 2);
        assert_eq!(sim.state.daily_net_values[0].date, "2026-10-22");
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn non_trading_day_rejects_order() {
        let paths = temp_paths("holiday");
        let mut sim = open_sim(&paths);
        let mut req = buy_req("600000", "浦发银行", 10.0, 100);
        req.trade_date = Some(date("2026-10-24"));
        let blocked = sim.buy(&req).expect("买入不报错");
        assert!(!blocked.success);
        assert!(
            blocked.msg.contains("不是交易日"),
            "实际提示: {}",
            blocked.msg
        );
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }
}
