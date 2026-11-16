//! 机器人主循环：候选筛选 → 规则预筛 → AI 分析 → 风控执行 → 净值与心跳。
//!
//! 数据源通过 `MarketSource` / `AiSource` / `NewsSource` 注入，这里没有任何直接
//! 网络代码，单测可以用假实现跑完整轮扫描。
//!
//! 以下几处行为经过反复调试才确定，改动前先看清对应测试：
//! - 信号冷却按 `(代码, 问题类型)` 缓存，`error` 绝不入缓存，否则一次超时会让该
//!   标的在整个冷却窗口里不再重试；
//! - 持仓里没进候选池的，取**浮亏最弱**的一只补进来，保证止损有人看；
//! - 买入被"仓位超限"拒绝时，按单票上限重算股数再下一次，而不是直接放弃。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::calendar::Calendar;
use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::http::HttpClient;
use crate::indicators::{self, RuleScore};
use crate::learner::Learner;
use crate::llm::{LlmClient, LlmConfig};
use crate::market::MarketData;
use crate::news::NewsAnalyzer;
use crate::rules;
use crate::signal::{AiSignal, AnalyzeRequest, SignalAction};
use crate::simulator::{
    BuyRequest, SellRequest, SignalLogInput, Simulator, Snapshot, TradeOutcome,
};
use crate::sizing;
use crate::store::Paths;
use crate::types::{
    AiSource, CachedSignal, Candidate, MarketSource, NewsSource, ScanAction, ScanReport,
    SkippedItem, StockInfo,
};
use crate::util::{log, now_iso, now_local, round_to, take_chars, today, ymd};

/// 行情短请求超时（秒）：行情接口本身很快，慢请求只会拖住整轮扫描。
const FAST_TIMEOUT_SECS: u64 = 10;
/// 大模型请求在配置超时之外额外留的余量（秒），避免卡在边界上。
const SLOW_TIMEOUT_MARGIN_SECS: u64 = 30;
/// 非交易时段的轮询间隔（秒）。
const IDLE_POLL_SECS: u64 = 60;
/// 循环间隔下限（秒）：低于 30 秒会被抬到 30，避免把行情接口打成限流。
const MIN_INTERVAL_SECS: u64 = 30;
/// 休眠切片（秒），保证 Ctrl+C 能在 1 秒内被感知。
const SLEEP_SLICE_SECS: u64 = 1;
/// 心跳间隔（秒）。必须明显小于看板的 180s 存活窗口。
const HEARTBEAT_SECS: u64 = 25;
/// 持仓股不参与规则筛选，用 999 表示"无需打分"。
const HELD_SCORE: i32 = 999;
/// 候选池最大长度：按涨幅取 `max(top_n*5, 30)` 只做二次核价。
const SHORTLIST_MIN: usize = 30;

pub struct Bot<M: MarketSource, A: AiSource, N: NewsSource> {
    pub sim: Simulator,
    pub md: M,
    pub ai: A,
    pub na: N,
    pub learner: Learner,
    pub paths: Paths,
    pub cal: Rc<Calendar>,
    signal_cache: HashMap<(String, String), CachedSignal>,
    experience_txt: String,
    pub force: bool,
    pub stop: Arc<AtomicBool>,
}

impl Bot<MarketData, LlmClient, NewsAnalyzer> {
    /// 生产装配：状态、行情、大模型、新闻各一份。
    ///
    /// 大模型配置缺失时返回可读错误。只有 `run` / `backtest` 走这里，
    /// `summary` / `dashboard` / `reset` 不构造 LLM，没有 Key 也能用。
    pub fn boot(
        paths: Paths,
        cal: Rc<Calendar>,
        capital: Option<f64>,
    ) -> AppResult<Bot<MarketData, LlmClient, NewsAnalyzer>> {
        let sim = Simulator::open(&paths, capital, Rc::clone(&cal))?;
        let cfg = sim.cfg().clone();
        let llm_cfg = LlmConfig::from_env(&cfg)?;
        let fast = std::time::Duration::from_secs(FAST_TIMEOUT_SECS);
        let slow = std::time::Duration::from_secs(
            cfg.ai_timeout_seconds as u64 + SLOW_TIMEOUT_MARGIN_SECS,
        );
        let ai = LlmClient::new(HttpClient::new(fast, slow)?, llm_cfg);
        let md = MarketData::new(HttpClient::new(fast, slow)?, Rc::clone(&cal));
        let na = NewsAnalyzer::new(HttpClient::new(fast, slow)?);
        let learner = Learner::open(paths.experience());
        Ok(Bot::with_parts(sim, md, ai, na, learner, paths, cal))
    }
}

impl<M: MarketSource, A: AiSource, N: NewsSource> Bot<M, A, N> {
    pub fn with_parts(
        sim: Simulator,
        md: M,
        ai: A,
        na: N,
        learner: Learner,
        paths: Paths,
        cal: Rc<Calendar>,
    ) -> Self {
        Bot {
            sim,
            md,
            ai,
            na,
            learner,
            paths,
            cal,
            signal_cache: HashMap::new(),
            experience_txt: String::new(),
            force: false,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cfg(&self) -> &Config {
        self.sim.cfg()
    }

    // ---------- 候选 ----------
    /// 全市场预筛 + 二次核价，返回按涨幅倒序的前 `top_n` 只。
    pub fn candidates(&mut self, top_n: usize) -> Vec<Candidate> {
        let stocks = self.md.get_all_a_stocks(false);
        if stocks.is_empty() {
            return Vec::new();
        }
        let total = self.sim.total_assets();
        if total <= 0.0 {
            return Vec::new();
        }
        let cfg = self.cfg().clone();
        let mut prefiltered: Vec<Candidate> = stocks
            .iter()
            .filter_map(|stock| prefilter(&self.sim, &cfg, total, stock))
            .collect();
        prefiltered.sort_by(|a, b| b.change_pct.total_cmp(&a.change_pct));
        prefiltered.truncate(top_n.saturating_mul(5).max(SHORTLIST_MIN));
        if prefiltered.is_empty() {
            return Vec::new();
        }

        let codes: Vec<String> = prefiltered.iter().map(|item| item.code.clone()).collect();
        let quotes = self.md.get_realtime_batch(&codes);
        let held: HashSet<String> = self.sim.state.positions.keys().cloned().collect();
        let mut candidates: Vec<Candidate> = Vec::new();
        for item in prefiltered {
            let quote = quotes.get(&item.code);
            let price = quote
                .map(|q| q.price)
                .filter(|value| *value > 0.0)
                .unwrap_or(item.price);
            if price <= 0.0 {
                continue;
            }
            let change_pct = quote.map(|q| q.change_pct).unwrap_or(item.change_pct);
            let name = quote
                .map(|q| q.name.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| item.name.clone());
            let prev_close = quote.map(|q| q.prev_close).filter(|value| *value > 0.0);
            // 涨停价通常买不进，未持仓的别再浪费一次 AI 调用；持仓的保留以便卖出。
            if !held.contains(&item.code)
                && rules::at_limit_up(price, prev_close, &item.code, &name)
            {
                continue;
            }
            let one_lot = price * rules::min_buy_shares(&item.code) as f64;
            if !lot_is_affordable(one_lot, total, &cfg) {
                continue;
            }
            if self.sim.round_trip_cost_pct(price, &item.code) > cfg.max_round_trip_cost_pct {
                continue;
            }
            candidates.push(Candidate {
                code: item.code,
                name,
                price,
                change_pct,
                prev_close,
                one_lot: round_to(one_lot, 2),
                ..Default::default()
            });
        }
        candidates.sort_by(|a, b| b.change_pct.total_cmp(&a.change_pct));
        candidates.truncate(top_n);
        candidates
    }

    // ---------- 一轮扫描 ----------
    pub fn scan_once(
        &mut self,
        top_n: Option<usize>,
        force: Option<bool>,
    ) -> AppResult<ScanReport> {
        let top_n = top_n.unwrap_or(self.cfg().scan_top_n as usize).max(1);
        let force = force.unwrap_or(self.force);
        let mut report = ScanReport {
            time: crate::util::now_stamp(),
            ..Default::default()
        };

        if !force && !self.md.market_is_open(now_local()) {
            report.market_closed = Some(true);
            log("当前非交易时段/休市，跳过扫描（如需强制运行请加 --force）");
            self.beat();
            self.sim.state.last_scan = serde_json::to_value(&report).unwrap_or_default();
            self.sim.save()?;
            return Ok(report);
        }

        let candidates = self.candidates(top_n);

        // 经验学习失败不能挡住开单，最多让 prompt 少一段历史统计。
        if self
            .learner
            .learn_from_orders(&self.sim.state.orders)
            .is_err()
        {
            self.experience_txt.clear();
            log("[警告] 经验学习失败，本轮 prompt 不带历史统计");
        } else {
            self.experience_txt = self.learner.summary_text();
        }

        let mut scan_list = candidates;
        self.append_weakest_holding(&mut scan_list);
        report.scanned = scan_list.len() as u32;
        log(&format!(
            "=== 扫描开始 {} | 候选 {} 只 ===",
            report.time, report.scanned
        ));

        let ai_list = self.rule_filter(&scan_list, &mut report);
        for scored in &ai_list {
            self.analyze_and_trade(scored, &mut report);
        }

        self.settle_positions(&mut report)?;

        let day = ymd(today());
        let record = self.sim.snapshot(Some(&day))?;
        self.sim.add_ai_usage(
            report.ai_calls as u64,
            report.error as u64,
            report.rule_skipped as u64,
            0.0,
        )?;
        report.halted = self.sim.state.halted;
        report.net_value = serde_json::to_value(&record).ok();
        self.sim.state.last_scan = serde_json::to_value(&report).unwrap_or_default();
        self.sim.save()?;
        self.beat();
        print_report(&self.sim, &report);
        Ok(report)
    }

    /// 持仓里没进候选池的，取浮亏最小的（最弱）一只补进来送去复核。
    fn append_weakest_holding(&mut self, scan_list: &mut Vec<Candidate>) {
        let in_list: HashSet<&str> = scan_list.iter().map(|item| item.code.as_str()).collect();
        let mut missing: Vec<(String, f64)> = self
            .sim
            .state
            .positions
            .iter()
            .filter(|(code, _)| !in_list.contains(code.as_str()))
            .map(|(code, pos)| {
                let avg = pos.avg_cost;
                let pnl = if avg > 0.0 {
                    (pos.last_price - avg) / avg * 100.0
                } else {
                    0.0
                };
                (code.clone(), pnl)
            })
            .collect();
        if missing.is_empty() {
            return;
        }
        missing.sort_by(|a, b| a.1.total_cmp(&b.1));
        let weakest = missing[0].0.clone();
        let Some(quote) = self.md.get_realtime(&weakest) else {
            return;
        };
        if quote.price <= 0.0 {
            return;
        }
        let fallback_name = self.sim.state.positions[&weakest].name.clone();
        scan_list.push(Candidate {
            code: weakest,
            name: if quote.name.is_empty() {
                fallback_name
            } else {
                quote.name
            },
            price: quote.price,
            change_pct: quote.change_pct,
            prev_close: Some(quote.prev_close).filter(|value| *value > 0.0),
            one_lot: 0.0,
            ..Default::default()
        });
    }

    /// 规则预筛：持仓直接放行，其余按打分排序，只有前 `ai_analyze_n` 只未持仓股
    /// 进 AI；被省掉的数量记进 `rule_skipped`（这是真正省 token 的地方）。
    fn rule_filter(&mut self, scan_list: &[Candidate], report: &mut ScanReport) -> Vec<Scored> {
        let cfg = self.cfg().clone();
        let held: HashSet<String> = self.sim.state.positions.keys().cloned().collect();
        let mut scored: Vec<Scored> = Vec::new();
        let mut skipped_rules = 0u32;
        for candidate in scan_list {
            if held.contains(&candidate.code) {
                scored.push(Scored {
                    cand: candidate.clone(),
                    score: HELD_SCORE,
                    grade: "持仓".to_string(),
                    reasons: String::new(),
                });
                continue;
            }
            let result = self.score_candidate(candidate);
            if result.score >= cfg.rule_score_min {
                scored.push(Scored {
                    cand: candidate.clone(),
                    score: result.score,
                    grade: result.grade.clone(),
                    reasons: take_chars(&result.reasons.join(" "), 80),
                });
            } else {
                skipped_rules += 1;
                report.skipped.push(SkippedItem {
                    code: candidate.code.clone(),
                    name: candidate.name.clone(),
                    reason: format!("规则筛选{}({}分)未达阈值", result.grade, result.score),
                });
            }
        }

        let mut non_held: Vec<Scored> = scored
            .iter()
            .filter(|item| !held.contains(&item.cand.code))
            .cloned()
            .collect();
        // 打分高的先送去分析
        non_held.sort_by_key(|item| std::cmp::Reverse(item.score));
        let held_only: Vec<Scored> = scored
            .into_iter()
            .filter(|item| held.contains(&item.cand.code))
            .collect();
        let limit = cfg.ai_analyze_n as usize;
        let over_limit = non_held.len().saturating_sub(limit) as u32;
        non_held.truncate(limit);
        report.rule_skipped = skipped_rules + over_limit;
        if skipped_rules > 0 {
            log(&format!(
                "规则预筛选: {} -> {} 只进 AI（免费跳过 {} 只）",
                scan_list.len(),
                held_only.len() + non_held.len(),
                report.rule_skipped
            ));
        }
        let mut ai_list = held_only;
        ai_list.extend(non_held);
        ai_list
    }

    fn score_candidate(&mut self, candidate: &Candidate) -> RuleScore {
        match indicators::get_indicators(
            &mut self.md,
            &self.cal,
            &candidate.code,
            120,
            None,
            now_local(),
        ) {
            Some(ind) => indicators::rule_score(&ind, Some(candidate.change_pct)),
            None => RuleScore::insufficient(),
        }
    }

    /// 成交后刷新持仓价，并按止损/止盈规则卖出。
    fn settle_positions(&mut self, report: &mut ScanReport) -> AppResult<()> {
        let positions: Vec<String> = self.sim.state.positions.keys().cloned().collect();
        if positions.is_empty() {
            return Ok(());
        }
        let quotes = self.md.get_realtime_batch(&positions);
        let prices: HashMap<String, f64> = quotes
            .iter()
            .filter(|(_, quote)| quote.price > 0.0)
            .map(|(code, quote)| (code.clone(), quote.price))
            .collect();
        self.sim.update_prices(&prices)?;

        for signal in self.sim.check_stop_loss_take_profit(&prices) {
            let quote = quotes.get(&signal.code);
            let price = quote
                .map(|item| item.price)
                .filter(|value| *value > 0.0)
                .unwrap_or(signal.price);
            if price <= 0.0 {
                report.skipped.push(SkippedItem {
                    code: signal.code.clone(),
                    name: signal.code.clone(),
                    reason: "止损/止盈无有效行情".to_string(),
                });
                continue;
            }
            let name = quote
                .map(|item| item.name.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| signal.code.clone());
            let prev_close = quote
                .map(|item| item.prev_close)
                .filter(|value| *value > 0.0);
            let outcome = self.sim.sell(&SellRequest {
                code: &signal.code,
                price,
                shares: -1,
                reason: &signal.reason,
                trade_date: None,
                prev_close,
            })?;
            let skip_reason = if outcome.success {
                ""
            } else {
                outcome.msg.as_str()
            };
            self.sim.log_signal(&SignalLogInput {
                code: &signal.code,
                name: &name,
                signal: "sell",
                confidence: 1.0,
                reason: &signal.reason,
                executed: outcome.success,
                note: "风控卖出",
                skip_reason,
                price: Some(price),
                hallucination_risk: false,
            })?;
            report.actions.push(ScanAction {
                code: signal.code.clone(),
                name: name.clone(),
                action: "SELL".to_string(),
                shares: serde_json::json!("全部"),
                price,
                msg: outcome.msg.clone(),
            });
            if outcome.success {
                report.sell += 1;
            } else {
                report.skipped.push(SkippedItem {
                    code: signal.code.clone(),
                    name,
                    reason: outcome.msg,
                });
            }
        }
        Ok(())
    }

    // ---------- 单只分析与下单 ----------
    fn analyze_and_trade(&mut self, scored: &Scored, report: &mut ScanReport) {
        let code = scored.cand.code.clone();
        let name = scored.cand.name.clone();
        let price = scored.cand.price;
        let held = self.sim.state.positions.contains_key(&code);
        let question_type = if held { "hold" } else { "buy" };
        let question = if held {
            "我已持有该股票，请分析是继续持有还是卖出？"
        } else {
            "现在适合买入吗？请给出明确的操作建议（买入/持有/观望/卖出）。"
        };

        let mut news_txt = String::new();
        let signal = match self.last_signal(&code, question_type) {
            Some(cached) => {
                report.cached += 1;
                log(&format!(
                    "[冷却] {name}({code}) 沿用 {question_type} 信号: {}",
                    cached.signal
                ));
                replay_cached(&code, &cached)
            }
            None => {
                let technical = self.technical_text(&code);
                news_txt = self.na.summary(&code, Some(&name)).txt;
                let rule_ctx = if scored.score < HELD_SCORE {
                    format!(
                        "【规则预筛】技术规则打分 {} 分（{}），依据: {}",
                        scored.score, scored.grade, scored.reasons
                    )
                } else {
                    String::new()
                };
                let question_full = if rule_ctx.is_empty() {
                    question.to_string()
                } else {
                    format!("{question}\n{rule_ctx}")
                };
                let account = self.account_context();
                log(&format!("[AI分析] {name}({code}) @ {price:.2} ..."));
                let mut request = AnalyzeRequest::new(&code, &question_full);
                request.technical = as_opt(&technical);
                request.news = as_opt(&news_txt);
                request.experience = as_opt(&self.experience_txt);
                request.extra = Some(&account);
                request.verify_price = Some(price);
                request.expected_name = Some(&name);
                let signal = self.ai.analyze(&request);
                report.ai_calls += 1;
                self.cache_signal(
                    &code,
                    question_type,
                    signal.signal.as_str(),
                    signal.confidence,
                    &signal.reason,
                );
                // AI 调用失败已经单独记过日志，不必再叠一行"幻觉风险"。
                if signal.hallucination_risk && !matches!(signal.signal, SignalAction::Error) {
                    log(&format!(
                        "  [警告] 幻觉风险 [{name}({code})]: {}",
                        signal.verify_notes
                    ));
                }
                signal
            }
        };

        let confidence = signal.confidence;
        if matches!(signal.signal, SignalAction::Error) {
            report.error += 1;
            let mut reason = if signal.reason.is_empty() {
                "AI错误".to_string()
            } else {
                signal.reason.clone()
            };
            if !signal.retryable {
                reason.push_str("（不可重试）");
            }
            self.log_signal_quiet(&SignalLogInput {
                code: &code,
                name: &name,
                signal: signal.signal.as_str(),
                confidence,
                reason: &reason,
                executed: false,
                note: "",
                skip_reason: &reason,
                price: Some(price),
                hallucination_risk: signal.hallucination_risk,
            });
            report.skipped.push(SkippedItem { code, name, reason });
            return;
        }

        let mut outcome: Option<TradeOutcome> = None;
        let mut skip_reason = String::new();
        match signal.signal {
            SignalAction::Buy if !held => {
                self.execute_buy(&signal, scored, &mut outcome, &mut skip_reason, report);
            }
            SignalAction::Sell if held => {
                let reason = format!("AI信号: 卖出(置信{:.0}%)", confidence * 100.0);
                match self.sim.sell(&SellRequest {
                    code: &code,
                    price,
                    shares: -1,
                    reason: &reason,
                    trade_date: None,
                    prev_close: scored.cand.prev_close,
                }) {
                    Ok(result) => {
                        if result.success {
                            report.sell += 1;
                        } else {
                            skip_reason = result.msg.clone();
                        }
                        report.actions.push(ScanAction {
                            code: code.clone(),
                            name: name.clone(),
                            action: "SELL".to_string(),
                            shares: serde_json::json!("全部"),
                            price,
                            msg: result.msg.clone(),
                        });
                        outcome = Some(result);
                    }
                    Err(err) => {
                        report.error += 1;
                        skip_reason = format!("执行失败: {err}");
                    }
                }
            }
            SignalAction::Sell => skip_reason = "AI建议卖出但未持仓".to_string(),
            other => skip_reason = format!("AI建议{}", other.as_str()),
        }

        if !skip_reason.is_empty() {
            report.wait += 1;
            report.skipped.push(SkippedItem {
                code: code.clone(),
                name: name.clone(),
                reason: skip_reason.clone(),
            });
        }

        let mut note = format!("价格{price:.2}");
        if !news_txt.is_empty() {
            note.push_str(&format!(
                " | 消息面:{}",
                take_chars(&news_txt.replace('\n', " "), 60)
            ));
        }
        self.log_signal_quiet(&SignalLogInput {
            code: &code,
            name: &name,
            signal: signal.signal.as_str(),
            confidence,
            reason: &take_chars(&signal.reason, 200),
            executed: outcome.as_ref().map(|item| item.success).unwrap_or(false),
            note: &note,
            skip_reason: &skip_reason,
            price: Some(price),
            hallucination_risk: signal.hallucination_risk,
        });
    }

    /// 买入分支：置信度门槛 → 仓位比例 → 手数规整 → 金额门槛 → 下单（含缩量重试）。
    fn execute_buy(
        &mut self,
        signal: &AiSignal,
        scored: &Scored,
        outcome: &mut Option<TradeOutcome>,
        skip_reason: &mut String,
        report: &mut ScanReport,
    ) {
        let candidate = &scored.cand;
        let code = candidate.code.as_str();
        let price = candidate.price;
        let confidence = signal.confidence;
        let cfg = self.cfg().clone();

        if confidence < cfg.confidence_buy_threshold {
            *skip_reason = format!(
                "置信度 {:.0}% 低于买入门槛 {:.0}%",
                confidence * 100.0,
                cfg.confidence_buy_threshold * 100.0
            );
            return;
        }
        if let Some(0) = signal.position_pct {
            *skip_reason = "AI 建议 0 仓位".to_string();
            return;
        }
        let ratio = sizing::resolve_ratio(confidence, signal.position_pct, &cfg);
        let sized = sizing::target_shares(
            &sizing::SizingInput {
                code,
                price,
                total_assets: self.sim.total_assets(),
                ratio,
                volume_cap_shares: None,
            },
            &cfg,
        );
        if let Some(reason) = sized.reason {
            *skip_reason = reason;
            return;
        }
        let mut shares = sized.shares;

        let mut result = match self.place_buy(signal, candidate, shares, confidence, false) {
            Ok(item) => item,
            Err(err) => {
                report.error += 1;
                *skip_reason = format!("执行失败: {err}");
                return;
            }
        };
        // 仓位超限时按单票上限缩量再试一次，比直接放弃更接近人工操作。
        if !result.success && result.msg.contains("仓位超限") {
            let sized = sizing::target_shares(
                &sizing::SizingInput {
                    code,
                    price,
                    total_assets: self.sim.total_assets(),
                    ratio: cfg.max_single_position_ratio,
                    volume_cap_shares: None,
                },
                &cfg,
            );
            if sized.reason.is_none() && sized.shares > 0 {
                let shares2 = sized.shares;
                match self.place_buy(signal, candidate, shares2, confidence, true) {
                    Ok(item) => {
                        result = item;
                        shares = shares2;
                    }
                    Err(err) => {
                        report.error += 1;
                        *skip_reason = format!("执行失败: {err}");
                        return;
                    }
                }
            }
        }
        if result.success {
            report.buy += 1;
        } else {
            *skip_reason = result.msg.clone();
        }
        report.actions.push(ScanAction {
            code: code.to_string(),
            name: candidate.name.clone(),
            action: "BUY".to_string(),
            shares: serde_json::json!(shares),
            price,
            msg: result.msg.clone(),
        });
        *outcome = Some(result);
    }

    fn place_buy(
        &mut self,
        signal: &AiSignal,
        candidate: &Candidate,
        shares: i64,
        confidence: f64,
        scaled: bool,
    ) -> AppResult<TradeOutcome> {
        let suffix = if scaled { "-缩量" } else { "" };
        let reason = format!("AI信号: 买入{suffix}(置信{:.0}%)", confidence * 100.0);
        self.sim.buy(&BuyRequest {
            code: &candidate.code,
            name: &candidate.name,
            price: candidate.price,
            shares,
            reason: &reason,
            trade_date: None,
            prev_close: candidate.prev_close,
            stop_loss: signal.stop_loss,
            take_profit: signal.take_profit,
        })
    }

    /// 信号日志写失败只记日志：账已经成交，不能让落库失败打断整轮扫描。
    fn log_signal_quiet(&mut self, input: &SignalLogInput<'_>) {
        if let Err(err) = self.sim.log_signal(input) {
            log(&format!("[错误] 信号日志写入失败: {err}"));
        }
    }

    fn technical_text(&mut self, code: &str) -> String {
        match indicators::get_indicators(&mut self.md, &self.cal, code, 120, None, now_local()) {
            Some(ind) => ind.signal_txt,
            None => String::new(),
        }
    }

    fn account_context(&self) -> String {
        let cfg = self.cfg();
        format!(
            "模拟账户总资产 {:.0} 元（小资金），佣金{:.2}‱、最低{:.0}元，卖出印花税{:.2}%，过户费{:.3}%；\
持仓上限 {} 只，单票仓位上限 {:.0}%，止损 {}%、止盈 {}%。",
            self.sim.total_assets(),
            cfg.commission_rate * 10000.0,
            cfg.min_commission,
            cfg.stamp_tax_rate * 100.0,
            cfg.transfer_fee_rate * 100.0,
            cfg.max_positions,
            cfg.max_single_position_ratio * 100.0,
            cfg.stop_loss_pct,
            cfg.take_profit_pct
        )
    }

    fn cache_signal(
        &mut self,
        code: &str,
        kind: &str,
        signal: &str,
        confidence: f64,
        reason: &str,
    ) {
        // error/unknown 不入缓存：否则一次网络抖动会让该标的整个冷却窗口不再重试。
        if signal == "error" || signal == "unknown" {
            return;
        }
        self.signal_cache.insert(
            (code.to_string(), kind.to_string()),
            CachedSignal {
                signal: signal.to_string(),
                confidence,
                reason: reason.to_string(),
                at: Instant::now(),
            },
        );
    }

    fn last_signal(&mut self, code: &str, kind: &str) -> Option<CachedSignal> {
        let key = (code.to_string(), kind.to_string());
        let item = self.signal_cache.get(&key)?;
        let ttl = self.cfg().signal_cooldown_min as u64 * 60;
        if item.at.elapsed().as_secs() >= ttl {
            self.signal_cache.remove(&key);
            return None;
        }
        Some(item.clone())
    }

    // ---------- 心跳与循环 ----------
    /// 写心跳文件；看板靠它判断存活，写失败绝对不能影响交易流程。
    pub fn beat(&self) {
        write_heartbeat(&self.paths);
    }

    pub fn run_loop(&mut self, interval: Option<u64>, top_n: Option<usize>) -> AppResult<()> {
        let cfg = self.cfg().clone();
        let interval = interval
            .unwrap_or(cfg.scan_interval as u64)
            .max(MIN_INTERVAL_SECS);
        let top_n = top_n.unwrap_or(cfg.scan_top_n as usize).max(1);
        log(&format!(
            "AI 量化交易机器人启动 | 间隔 {interval}s | 每轮候选 {top_n} 只 | 持仓≤{} | 单票≤{:.0}% | \
止损{}% 止盈{}% | 熔断{}%",
            cfg.max_positions,
            cfg.max_single_position_ratio * 100.0,
            cfg.stop_loss_pct,
            cfg.take_profit_pct,
            cfg.drawdown_halt_pct
        ));
        crate::util::install_interrupt_handler();
        // 心跳交给独立线程：单轮 AI 分析可能超过看板的 180s 存活窗口。
        let heartbeat = spawn_heartbeat(
            Paths {
                base_dir: self.paths.base_dir.clone(),
            },
            Arc::clone(&self.stop),
        );
        while !self.stopped() {
            if !self.force && !self.md.market_is_open(now_local()) {
                log("非交易时段，等待开盘（Ctrl+C 停止）");
                self.sleep(IDLE_POLL_SECS);
                continue;
            }
            if let Err(err) = self.scan_once(Some(top_n), None) {
                log(&format!("[错误] {err}"));
                self.sleep(IDLE_POLL_SECS);
                continue;
            }
            self.sleep(interval);
        }
        // 让心跳线程看到停止标志后退出，避免进程结束时留下后台线程。
        self.stop.store(true, Ordering::SeqCst);
        let _ = heartbeat.join();
        log("机器人已停止");
        Ok(())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || crate::util::interrupted()
    }

    /// 可中断休眠：按 1 秒切片，顺带每 30 秒补一次心跳。
    fn sleep(&self, seconds: u64) {
        let deadline = Instant::now() + std::time::Duration::from_secs(seconds);
        let mut last_beat = Instant::now();
        while Instant::now() < deadline {
            if self.stopped() {
                return;
            }
            if last_beat.elapsed().as_secs() >= 30 {
                self.beat();
                last_beat = Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_secs(SLEEP_SLICE_SECS));
        }
    }
}

/// 写心跳文件（tmp + rename 原子替换）。看板靠它判断存活，任何失败都静默吞掉：
/// 一次磁盘抖动不能让交易主流程崩。
fn write_heartbeat(paths: &Paths) {
    let path = paths.heartbeat();
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let payload = serde_json::json!({
        "last_beat": now_iso(),
        "pid": std::process::id(),
    });
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(payload.to_string().as_bytes())?;
        file.flush()
    });
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 独立的心跳线程：一轮扫描可能因为大模型慢响应拖几分钟，
/// 靠扫描间隙打点会让看板（180s 窗口）误判离线，所以心跳必须与扫描节奏解耦。
fn spawn_heartbeat(paths: Paths, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            write_heartbeat(&paths);
            // 按 1 秒切片检查退出条件，保证 Ctrl+C 后能立刻收尾。
            for _ in 0..HEARTBEAT_SECS {
                if stop.load(Ordering::SeqCst) || crate::util::interrupted() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    })
}

/// 打分结果 + 候选，避免往 `Candidate` 里塞非行情字段。
#[derive(Clone)]
struct Scored {
    cand: Candidate,
    score: i32,
    grade: String,
    reasons: String,
}

fn as_opt(text: &str) -> Option<&str> {
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn lot_is_affordable(one_lot: f64, total: f64, cfg: &Config) -> bool {
    one_lot <= total && one_lot / total <= cfg.max_one_lot_ratio
}

/// 单只股票的第一层预筛；不满足条件返回 None。
fn prefilter(sim: &Simulator, cfg: &Config, total: f64, stock: &StockInfo) -> Option<Candidate> {
    let code = stock.code.trim().to_string();
    let name = stock.name.trim().to_string();
    let price = stock.price;
    if code.is_empty() || name.is_empty() || price <= 0.0 {
        return None;
    }
    if price < cfg.min_price || price > cfg.max_price {
        return None;
    }
    // ST、次新（N/C 开头）、退市股都不参与：涨跌停规则不同且波动不可控。
    if rules::is_st_name(&name)
        || name.starts_with('N')
        || name.starts_with('C')
        || name.contains('退')
    {
        return None;
    }
    let one_lot = price * rules::min_buy_shares(&code) as f64;
    if !lot_is_affordable(one_lot, total, cfg) {
        return None;
    }
    if sim.round_trip_cost_pct(price, &code) > cfg.max_round_trip_cost_pct {
        return None;
    }
    Some(Candidate {
        code,
        name,
        price,
        change_pct: stock.change_pct,
        prev_close: None,
        one_lot: round_to(one_lot, 2),
        ..Default::default()
    })
}

/// 冷却命中时用缓存构造等价信号，不重新调用大模型。
fn replay_cached(code: &str, cached: &CachedSignal) -> AiSignal {
    AiSignal {
        signal: SignalAction::parse(&cached.signal).unwrap_or(SignalAction::Wait),
        confidence: cached.confidence,
        symbol: code.to_string(),
        reason: cached.reason.clone(),
        ..Default::default()
    }
}

fn shares_text(shares: &serde_json::Value) -> String {
    match shares {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub fn print_report(sim: &Simulator, report: &ScanReport) {
    log(&format!(
        "=== 扫描完成 | 扫描 {} | 买 {} | 卖 {} | 观望 {} | 错误 {} | AI {} 次 ===",
        report.scanned, report.buy, report.sell, report.wait, report.error, report.ai_calls
    ));
    let summary = sim.summary();
    log(&format!(
        "总资产 {:.2} | 收益率 {}% | 持仓 {} 只 | {}",
        money(summary.total_assets),
        money(summary.total_return_pct),
        summary.position_count,
        summary.halt_status
    ));
    for item in &report.actions {
        log(&format!(
            "  [{}] {}({}) {}股 @ {:.2} | {}",
            item.action,
            item.name,
            item.code,
            shares_text(&item.shares),
            item.price,
            item.msg
        ));
    }
    for item in report.skipped.iter().take(8) {
        log(&format!(
            "  [跳过] {}({}): {}",
            item.name, item.code, item.reason
        ));
    }
    if report.halted {
        log(&format!("[警告] 熔断中: {}", sim.state.halt_reason));
    }
}

/// 仅把负零归一为正零，不做取整：金额取整请用 `util::round_to`。
/// f64 在 (-0.5, 0) 区间取整会得到负零，直接 `{:.2}` 会渲染成 "-0.00"，
/// `simulator::summary()` 已在源头归一，这里再兜一层防止将来新增字段漏处理。
fn money(value: f64) -> f64 {
    if value == 0.0 {
        0.0
    } else {
        value
    }
}

/// 中文成绩单。标签沿用 state 里的中文键，方便和看板对照。
pub fn print_summary(snapshot: &Snapshot) {
    let line = "=".repeat(56);
    println!("{line}");
    println!("AI 模拟交易成绩单");
    println!("{line}");
    println!("初始资金: {:.2}", money(snapshot.initial_capital));
    println!("当前总资产: {:.2}", money(snapshot.total_assets));
    println!("总收益率%: {}", money(snapshot.total_return_pct));
    println!("现金: {:.2}", money(snapshot.cash));
    println!("持仓市值: {:.2}", money(snapshot.market_value));
    println!("持仓数量: {}", snapshot.position_count);
    if snapshot.positions.is_empty() {
        println!("持仓: 空仓");
    } else {
        println!();
        println!("持仓:");
        for pos in &snapshot.positions {
            let available = pos.available_date.as_deref().unwrap_or("-");
            println!(
                "  {}({}) {}股 成本{:.3} 现价{:.2} 盈亏{}% 可卖{}",
                pos.name,
                pos.code,
                pos.shares,
                money(pos.avg_cost),
                money(pos.last_price),
                money(pos.pnl_pct),
                available
            );
        }
    }
    println!("总成交笔数: {}", snapshot.order_count);
    println!("已平仓盈亏: {:.2}", money(snapshot.realized_pnl));
    println!("浮动盈亏: {:.2}", money(snapshot.unrealized_pnl));
    println!("累计手续费: {:.2}", money(snapshot.total_fee));
    println!("熔断状态: {}", snapshot.halt_status);
    println!("熔断原因: {}", snapshot.halt_reason);
    println!("{line}");
}

// ---------- 单实例锁 ----------
/// 进程是否存活。Windows 用 tasklist 探活（`kill(pid, 0)` 在 Windows 上语义不同）；
/// 其它平台看 `/proc`，拿不到就当作已退出，让锁能被回收。
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match output {
            Ok(result) => String::from_utf8_lossy(&result.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

/// 抢占单实例锁。已存在且持有者存活时返回 false 并打提示；
/// 持有者已退出则删锁重试一次。
pub fn acquire_lock(paths: &Paths) -> bool {
    let path = paths.lock();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let _ = file.write_all(std::process::id().to_string().as_bytes());
                let _ = file.flush();
                return true;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                // Windows 下必须先关掉句柄再删，`read_to_string` 会自己关闭。
                let old_pid = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                if pid_alive(old_pid) {
                    log(&format!("已有机器人实例在运行 (PID {old_pid})，本次退出"));
                    return false;
                }
                if std::fs::remove_file(&path).is_err() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    false
}

/// 释放锁：只有内容仍指向自己时才删，避免误删别人的锁。
pub fn release_lock(paths: &Paths) {
    let path = paths.lock();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    if content.trim() == std::process::id().to_string() {
        let _ = std::fs::remove_file(&path);
    }
}

/// RAII 锁：正常返回路径与 Ctrl+C 都会释放，不给下次启动留垃圾。
pub struct LockGuard {
    /// 独立持有一份基目录副本，Drop 时直接复用 `release_lock` 的语义，
    /// 这样调用方仍可以把原来的 `Paths` 交给 `Bot::boot`。
    paths: Paths,
}

impl LockGuard {
    pub fn acquire(paths: &Paths) -> Option<LockGuard> {
        if acquire_lock(paths) {
            Some(LockGuard {
                // 只复制基目录，用来在 Drop 时复用 release_lock 的语义
                paths: Paths {
                    base_dir: paths.base_dir.clone(),
                },
            })
        } else {
            None
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        release_lock(&self.paths);
    }
}

/// 校验 CLI 数值参数。失败文案直接面向命令行用户，需要能一眼看懂该怎么改。
pub fn validate_run_args(
    capital: Option<f64>,
    scan: Option<u32>,
    interval: Option<u64>,
) -> AppResult<()> {
    if let Some(value) = capital {
        if value <= 0.0 {
            return Err(AppError::Invalid("--capital 必须为正数".to_string()));
        }
    }
    if let Some(value) = scan {
        if value < 1 {
            return Err(AppError::Invalid("--scan 必须 >= 1".to_string()));
        }
    }
    if let Some(value) = interval {
        if value < MIN_INTERVAL_SECS {
            return Err(AppError::Invalid(format!(
                "--interval 必须 >= {MIN_INTERVAL_SECS} 秒"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::news::NewsSummary;
    use crate::types::Quote;
    use std::cell::Cell;

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!(
            "quant-sim-bot-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        Paths { base_dir: base }
    }

    fn quote_of(code: &str) -> Quote {
        Quote {
            code: code.to_string(),
            name: "浦发银行".to_string(),
            price: 10.0,
            prev_close: 9.9,
            open: 10.0,
            high: 10.2,
            low: 9.8,
            volume: 1000.0,
            change_pct: 1.0,
            ..Default::default()
        }
    }

    struct FakeMarket {
        open: bool,
    }

    impl MarketSource for FakeMarket {
        fn market_is_open(&mut self, _now: chrono::NaiveDateTime) -> bool {
            self.open
        }

        fn get_all_a_stocks(&mut self, _force: bool) -> Vec<StockInfo> {
            vec![StockInfo {
                code: "600000".to_string(),
                name: "浦发银行".to_string(),
                price: 10.0,
                change_pct: 1.0,
            }]
        }

        fn get_realtime(&mut self, symbol: &str) -> Option<Quote> {
            Some(quote_of(symbol))
        }

        fn get_realtime_batch(&mut self, symbols: &[String]) -> HashMap<String, Quote> {
            symbols
                .iter()
                .map(|code| (code.clone(), quote_of(code)))
                .collect()
        }

        fn get_kline(
            &mut self,
            _symbol: &str,
            _period: &str,
            _limit: usize,
            _end_date: Option<&str>,
        ) -> Vec<crate::types::Bar> {
            Vec::new()
        }
    }

    struct FakeAi {
        result: AiSignal,
        calls: Cell<u32>,
    }

    impl FakeAi {
        fn buy() -> FakeAi {
            FakeAi {
                result: AiSignal {
                    signal: SignalAction::Buy,
                    confidence: 0.9,
                    reason: "test".to_string(),
                    stop_loss: Some(9.0),
                    take_profit: Some(12.0),
                    position_pct: Some(10),
                    ..Default::default()
                },
                calls: Cell::new(0),
            }
        }

        fn error() -> FakeAi {
            FakeAi {
                result: AiSignal::error("600000", "timeout"),
                calls: Cell::new(0),
            }
        }
    }

    impl AiSource for FakeAi {
        fn analyze(&self, req: &AnalyzeRequest<'_>) -> AiSignal {
            self.calls.set(self.calls.get() + 1);
            AiSignal {
                symbol: req.symbol.to_string(),
                ..self.result.clone()
            }
        }
    }

    struct FakeNews;

    impl NewsSource for FakeNews {
        fn summary(&mut self, code: &str, _name: Option<&str>) -> NewsSummary {
            NewsSummary {
                code: code.to_string(),
                txt: "近1条相关新闻, 消息面中性".to_string(),
                ..Default::default()
            }
        }
    }

    fn make_bot(
        tag: &str,
        ai: FakeAi,
        open: bool,
        rule_score_min: i32,
    ) -> Bot<FakeMarket, FakeAi, FakeNews> {
        let paths = temp_paths(tag);
        let cal = Rc::new(Calendar::empty());
        let mut sim = Simulator::open(&paths, Some(50_000.0), Rc::clone(&cal)).expect("建账户");
        // 假行情没有 K 线，指标为空时规则分为 0：把阈值降到 0 让候选能进 AI，
        // 这样测的是"扫描/下单/冷却"本身，而不是指标计算。
        sim.state.config.rule_score_min = rule_score_min;
        let learner = Learner::open(paths.experience());
        Bot::with_parts(sim, FakeMarket { open }, ai, FakeNews, learner, paths, cal)
    }

    /// 同一标的连续两轮扫描只下一单：第二次被"已持仓"拦下。
    #[test]
    fn scan_buys_once() {
        let mut bot = make_bot("buy-once", FakeAi::buy(), true, 0);
        let report = bot.scan_once(Some(1), Some(true)).expect("首轮扫描");
        assert_eq!(report.buy, 1);
        assert!(bot.sim.state.positions.contains_key("600000"));
        // 已持仓后同样的 buy 信号不会再开新仓
        let second = bot.scan_once(Some(1), Some(true)).expect("次轮扫描");
        assert_eq!(second.buy, 0);
    }

    /// 冷却命中时复用缓存信号、不再调用模型，但照常执行下单。
    #[test]
    fn cooldown_path_does_not_crash() {
        let mut bot = make_bot("cooldown", FakeAi::buy(), true, 0);
        bot.cache_signal("600000", "buy", "buy", 0.9, "cached");
        let report = bot.scan_once(Some(1), Some(true)).expect("扫描");
        assert_eq!(report.cached, 1);
        assert_eq!(report.buy, 1);
        // 命中冷却就不该再花钱调模型
        assert_eq!(bot.ai.calls.get(), 0);
    }

    /// error 信号不入缓存，下一轮仍会重新调用模型。
    #[test]
    fn error_not_cached() {
        let mut bot = make_bot("error", FakeAi::error(), true, 0);
        bot.scan_once(Some(1), Some(true)).expect("首轮扫描");
        let key = ("600000".to_string(), "buy".to_string());
        assert!(!bot.signal_cache.contains_key(&key));
        bot.scan_once(Some(1), Some(true)).expect("次轮扫描");
        assert_eq!(bot.ai.calls.get(), 2);
    }

    /// 休市且未加 --force 时跳过扫描：报告标记 market_closed，不调用模型。
    #[test]
    fn scan_closed_market_skips() {
        let mut bot = make_bot("closed", FakeAi::buy(), false, 0);
        let report = bot.scan_once(Some(1), Some(false)).expect("扫描");
        assert_eq!(report.market_closed, Some(true));
        assert_eq!(report.buy, 0);
        // 休市不应该调用模型
        assert_eq!(bot.ai.calls.get(), 0);
    }

    #[test]
    fn rule_threshold_blocks_ai_call() {
        // 默认阈值 20，假行情无 K 线 → 指标不足 → 0 分，候选被规则挡下
        let mut bot = make_bot("rule-block", FakeAi::buy(), true, 20);
        let report = bot.scan_once(Some(1), Some(true)).expect("扫描");
        assert_eq!(report.ai_calls, 0);
        assert_eq!(report.buy, 0);
        assert!(report.rule_skipped >= 1);
        assert!(report
            .skipped
            .iter()
            .any(|item| item.reason.contains("未达阈值")));
    }

    #[test]
    fn sizing_ratio_is_linear_and_capped() {
        let bot = make_bot("ratio", FakeAi::buy(), true, 0);
        let cfg = bot.cfg().clone();
        assert_eq!(
            sizing::confidence_ratio(cfg.confidence_buy_threshold - 0.01, &cfg),
            0.0
        );
        let at_threshold = sizing::confidence_ratio(cfg.confidence_buy_threshold, &cfg);
        assert!((at_threshold - cfg.confidence_base_ratio).abs() < 1e-9);
        assert!((sizing::confidence_ratio(1.0, &cfg) - cfg.confidence_max_ratio).abs() < 1e-9);
    }

    #[test]
    fn money_never_renders_negative_zero() {
        // f64 的 -0.0 会打印成 "-0.00"，成绩单里不能出现这种像 bug 的输出。
        assert_eq!(format!("{:.2}", money(-0.0)), "0.00");
        assert_eq!(format!("{:.2}", money(0.0)), "0.00");
        assert_eq!(format!("{:.2}", money(-12.5)), "-12.50");
        assert_eq!(format!("{:.2}", money(1295.0)), "1295.00");
    }

    #[test]
    fn heartbeat_thread_stops_on_flag() {
        let bot_dir = temp_paths("hb-thread");
        let stop = Arc::new(AtomicBool::new(false));
        let hb_paths = Paths {
            base_dir: bot_dir.base_dir.clone(),
        };
        let handle = spawn_heartbeat(hb_paths, Arc::clone(&stop));
        // 等第一次落盘
        let heartbeat = bot_dir.heartbeat();
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while !heartbeat.exists() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(heartbeat.exists(), "心跳线程应写出 heartbeat.json");
        stop.store(true, Ordering::SeqCst);
        // 线程按 1 秒切片检查标志，join 必须很快返回
        handle.join().expect("心跳线程应正常退出");
    }

    #[test]
    fn heartbeat_file_is_written() {
        let bot = make_bot("beat", FakeAi::buy(), true, 0);
        bot.beat();
        let text = std::fs::read_to_string(bot.paths.heartbeat()).expect("心跳文件");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value["last_beat"].as_str().unwrap().contains('T'));
        assert_eq!(value["pid"].as_u64().unwrap(), std::process::id() as u64);
    }

    /// 建锁后文件存在且写着本进程 PID；重复获取应失败；释放后文件消失。
    #[test]
    fn lock_acquire_release() {
        let paths = temp_paths("lock");
        assert!(acquire_lock(&paths));
        assert!(paths.lock().exists());
        // 自己还活着，第二次抢锁必须失败
        assert!(!acquire_lock(&paths));
        release_lock(&paths);
        assert!(!paths.lock().exists());
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let paths = temp_paths("stale-lock");
        std::fs::create_dir_all(paths.data_dir()).unwrap();
        // 不存在的 PID：锁应被回收
        std::fs::write(paths.lock(), "4294967290").unwrap();
        assert!(acquire_lock(&paths));
        release_lock(&paths);
        assert!(!paths.lock().exists());
    }

    #[test]
    fn release_lock_ignores_other_owner() {
        let paths = temp_paths("foreign-lock");
        std::fs::create_dir_all(paths.data_dir()).unwrap();
        std::fs::write(paths.lock(), "123456789").unwrap();
        release_lock(&paths);
        // 别人写的锁不能被我们删掉
        assert!(paths.lock().exists());
        let _ = std::fs::remove_file(paths.lock());
    }

    #[test]
    fn lock_guard_releases_on_drop() {
        let paths = temp_paths("guard");
        let lock_path = paths.lock();
        {
            let _guard = LockGuard::acquire(&paths).expect("抢锁");
            assert!(lock_path.exists());
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn validate_run_args_matches_argparse() {
        assert!(validate_run_args(Some(0.0), None, None).is_err());
        assert!(validate_run_args(Some(-1.0), None, None).is_err());
        assert!(validate_run_args(None, Some(0), None).is_err());
        assert!(validate_run_args(None, None, Some(29)).is_err());
        assert!(validate_run_args(Some(50_000.0), Some(1), Some(30)).is_ok());
        assert!(validate_run_args(None, None, None).is_ok());
    }

    #[test]
    fn prefilter_drops_st_and_expensive_lots() {
        let paths = temp_paths("prefilter");
        let cal = Rc::new(Calendar::empty());
        let sim = Simulator::open(&paths, Some(50_000.0), cal).expect("建账户");
        let cfg = sim.cfg().clone();
        let total = sim.total_assets();

        let ok = StockInfo {
            code: "600000".to_string(),
            name: "浦发银行".to_string(),
            price: 10.0,
            change_pct: 1.0,
        };
        assert!(prefilter(&sim, &cfg, total, &ok).is_some());

        let st = StockInfo {
            name: "*ST康德".to_string(),
            ..ok.clone()
        };
        assert!(prefilter(&sim, &cfg, total, &st).is_none());

        let new_stock = StockInfo {
            name: "N新锐".to_string(),
            ..ok.clone()
        };
        assert!(prefilter(&sim, &cfg, total, &new_stock).is_none());

        let too_expensive = StockInfo {
            price: 1000.0,
            ..ok.clone()
        };
        assert!(prefilter(&sim, &cfg, total, &too_expensive).is_none());

        // 一手 100 股 * 400 元 = 40000，占总资产 80% > 单票上限
        let big_lot = StockInfo {
            name: "贵价股".to_string(),
            price: 400.0,
            ..ok
        };
        assert!(prefilter(&sim, &cfg, total, &big_lot).is_none());
    }
}
