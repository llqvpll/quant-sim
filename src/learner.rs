//! 交易经验学习：把成交流水还原为已平仓交易，并统计胜率/盈亏比。
//!
//! 买入成本含佣金与过户费、卖出净额扣三项费用，FIFO 配对，部分卖出按批次拆记录。

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AppResult;
use crate::store::Order;
use crate::util;

const REASON_MAX_CHARS: usize = 100;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClosedTrade {
    pub code: String,
    pub name: String,
    pub entry_time: String,
    pub exit_time: String,
    pub entry_price: f64,
    pub exit_price: f64,
    pub pnl_pct: f64,
    pub pnl_amount: f64,
    pub shares: i64,
    pub confidence: f64,
    pub hold_minutes: f64,
    pub reason: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfidenceStat {
    pub count: u32,
    pub wins: u32,
    pub win_rate: f64,
    pub avg_pnl: f64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperienceStats {
    pub total_trades: u32,
    pub win_rate: f64,
    pub avg_pnl: f64,
    pub profit_factor: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub best: f64,
    pub worst: f64,
    pub confidence_stats: BTreeMap<String, ConfidenceStat>,
    // 样本为空时不写 recent，与 Python 的空统计结构保持一致
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Experience {
    #[serde(default = "default_version")]
    pub version: u32,
    pub closed_trades: Vec<ClosedTrade>,
    pub stats: ExperienceStats,
    pub updated_at: String,
}

impl Default for Experience {
    fn default() -> Self {
        Experience {
            version: default_version(),
            closed_trades: Vec::new(),
            stats: ExperienceStats::default(),
            updated_at: String::new(),
        }
    }
}

fn default_version() -> u32 {
    2
}

pub struct Learner {
    pub experience: Experience,
    path: PathBuf,
}

impl Learner {
    /// 读取经验库；缺失或损坏时退回空经验，不影响交易主流程。
    pub fn open(path: PathBuf) -> Learner {
        let experience = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Experience>(&text).unwrap_or_default(),
            Err(_) => Experience::default(),
        };
        Learner { experience, path }
    }

    pub fn save(&self) -> AppResult<()> {
        let mut tmp = self.path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(&self.experience)?;
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 从成交流水重建已平仓交易与统计。部分卖出会拆成多条记录。
    pub fn learn_from_orders(&mut self, orders: &[Order]) -> AppResult<&ExperienceStats> {
        let mut lots: BTreeMap<String, VecDeque<OpenLot>> = BTreeMap::new();
        let mut closed: Vec<ClosedTrade> = Vec::new();

        for order in orders {
            if order.code.is_empty() || order.shares <= 0 {
                continue;
            }
            let shares = order.shares as f64;
            if order.action == "BUY" {
                let buy_fee = order.commission + order.transfer_fee;
                lots.entry(order.code.clone())
                    .or_default()
                    .push_back(OpenLot {
                        shares: order.shares,
                        cost_per_share: (order.amount + buy_fee) / shares,
                        time: order.time.clone(),
                        reason: order.reason.clone(),
                    });
            } else if order.action == "SELL" {
                let sell_fee = order.commission + order.stamp_tax + order.transfer_fee;
                let net_per_share = (order.amount - sell_fee) / shares;
                let queue = lots.entry(order.code.clone()).or_default();
                let mut remaining = order.shares;
                while remaining > 0 {
                    let Some(lot) = queue.front_mut() else {
                        break;
                    };
                    let take = lot.shares.min(remaining);
                    let cost = lot.cost_per_share;
                    let pnl_pct = if cost != 0.0 {
                        (net_per_share - cost) / cost * 100.0
                    } else {
                        0.0
                    };
                    closed.push(ClosedTrade {
                        code: order.code.clone(),
                        name: order.name.clone(),
                        entry_time: lot.time.clone(),
                        exit_time: order.time.clone(),
                        entry_price: util::round_to(cost, 4),
                        exit_price: util::round_to(net_per_share, 4),
                        pnl_pct: util::round_to(pnl_pct, 2),
                        pnl_amount: util::round_to((net_per_share - cost) * take as f64, 2),
                        shares: take,
                        confidence: extract_confidence(&lot.reason),
                        hold_minutes: util::round_to(hold_minutes(&lot.time, &order.time), 1),
                        reason: lot.reason.chars().take(REASON_MAX_CHARS).collect(),
                    });
                    lot.shares -= take;
                    remaining -= take;
                    if lot.shares <= 0 {
                        queue.pop_front();
                    }
                }
            }
        }

        // 同一批次可能被重复配对（例如手工补录流水），按三元组去重
        let mut seen = std::collections::HashSet::new();
        let mut unique: Vec<ClosedTrade> = Vec::new();
        for trade in closed {
            let key = (
                trade.code.clone(),
                trade.entry_time.clone(),
                trade.exit_time.clone(),
            );
            if seen.insert(key) {
                unique.push(trade);
            }
        }

        self.experience.version = default_version();
        self.experience.stats = compute_stats(&unique);
        self.experience.closed_trades = unique;
        self.experience.updated_at = util::now_iso();
        self.save()?;
        Ok(&self.experience.stats)
    }

    /// 供 prompt 的【历史经验】段落。
    pub fn summary_text(&self) -> String {
        let stats = &self.experience.stats;
        if stats.total_trades == 0 {
            return "暂无历史交易经验（系统刚启动，完成几笔交易后自动生成）。请基于技术面与消息面谨慎决策。"
                .to_string();
        }
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!(
            "【历史交易经验】已平仓 {} 笔，胜率 {}%，平均每笔 {:+.2}%，总盈亏比 {}，平均盈利 {:+.2}%，平均亏损 {:+.2}%，最好 {:+.2}% 最差 {:+.2}%",
            stats.total_trades,
            py_float(stats.win_rate),
            stats.avg_pnl,
            // Python 在无亏损时打印整数 999，这里同样不带小数
            stats.profit_factor,
            stats.avg_win,
            stats.avg_loss,
            stats.best,
            stats.worst
        ));
        for (bucket, item) in stats.confidence_stats.iter() {
            lines.push(format!(
                "  {bucket}: {}笔，胜率 {}%，均收益 {:+.2}%",
                item.count,
                py_float(item.win_rate),
                item.avg_pnl
            ));
        }
        let mut advice: Vec<String> = Vec::new();
        for (bucket, item) in stats.confidence_stats.iter() {
            if item.count >= 10 && item.win_rate >= 60.0 {
                advice.push(format!(
                    "{bucket} 样本 {} 笔、胜率 {}%，可维持当前权重",
                    item.count,
                    py_float(item.win_rate)
                ));
            } else if item.count >= 10 && item.win_rate <= 40.0 {
                advice.push(format!(
                    "{bucket} 样本 {} 笔、胜率 {}%，建议降低仓位或提高门槛",
                    item.count,
                    py_float(item.win_rate)
                ));
            }
        }
        if stats.total_trades < 10 {
            advice.push("样本不足 10 笔，统计仅供参考，不要据此重仓".to_string());
        }
        if !advice.is_empty() {
            lines.push(format!("  {}", advice.join("；")));
        }
        lines.join("\n")
    }
}

struct OpenLot {
    shares: i64,
    cost_per_share: f64,
    time: String,
    reason: String,
}

pub fn compute_stats(trades: &[ClosedTrade]) -> ExperienceStats {
    if trades.is_empty() {
        return ExperienceStats {
            message: Some("暂无已平仓交易，完成几笔后自动生成经验".to_string()),
            ..ExperienceStats::default()
        };
    }

    let wins: Vec<&ClosedTrade> = trades.iter().filter(|trade| trade.pnl_pct > 0.0).collect();
    let losses: Vec<&ClosedTrade> = trades.iter().filter(|trade| trade.pnl_pct <= 0.0).collect();
    let gross_win: f64 = wins.iter().map(|trade| trade.pnl_amount).sum();
    let gross_loss: f64 = losses
        .iter()
        .map(|trade| trade.pnl_amount)
        .sum::<f64>()
        .abs();
    let avg_win = if wins.is_empty() {
        0.0
    } else {
        wins.iter().map(|trade| trade.pnl_pct).sum::<f64>() / wins.len() as f64
    };
    let avg_loss = if losses.is_empty() {
        0.0
    } else {
        losses.iter().map(|trade| trade.pnl_pct).sum::<f64>() / losses.len() as f64
    };

    let mut buckets: BTreeMap<String, (u32, u32, f64)> = BTreeMap::new();
    for trade in trades {
        let conf = if trade.confidence == 0.0 {
            0.5
        } else {
            trade.confidence
        };
        let bucket = if conf >= 0.85 {
            "高置信(≥0.85)"
        } else if conf >= 0.70 {
            "中置信(0.70-0.85)"
        } else {
            "低置信(<0.70)"
        };
        let entry = buckets.entry(bucket.to_string()).or_insert((0, 0, 0.0));
        entry.0 += 1;
        if trade.pnl_pct > 0.0 {
            entry.1 += 1;
        }
        entry.2 += trade.pnl_pct;
    }
    let confidence_stats: BTreeMap<String, ConfidenceStat> = buckets
        .into_iter()
        .map(|(bucket, (count, wins, sum_pnl))| {
            (
                bucket,
                ConfidenceStat {
                    count,
                    wins,
                    win_rate: util::round_to(wins as f64 / count as f64 * 100.0, 1),
                    avg_pnl: util::round_to(sum_pnl / count as f64, 2),
                },
            )
        })
        .collect();

    let total_pnl: f64 = trades.iter().map(|trade| trade.pnl_pct).sum();
    let best = trades
        .iter()
        .map(|trade| trade.pnl_pct)
        .fold(f64::NEG_INFINITY, f64::max);
    let worst = trades
        .iter()
        .map(|trade| trade.pnl_pct)
        .fold(f64::INFINITY, f64::min);
    let profit_factor = if gross_loss > 0.0 {
        util::round_to(gross_win / gross_loss, 2)
    } else if gross_win > 0.0 {
        999.0
    } else {
        0.0
    };

    ExperienceStats {
        total_trades: trades.len() as u32,
        win_rate: util::round_to(wins.len() as f64 / trades.len() as f64 * 100.0, 1),
        avg_pnl: util::round_to(total_pnl / trades.len() as f64, 2),
        profit_factor,
        avg_win: util::round_to(avg_win, 2),
        avg_loss: util::round_to(avg_loss, 2),
        best: util::round_to(best, 2),
        worst: util::round_to(worst, 2),
        confidence_stats,
        recent: trades
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(|trade| {
                serde_json::json!({
                    "code": trade.code,
                    "name": trade.name,
                    "pnl_pct": trade.pnl_pct,
                    "exit_time": trade.exit_time.chars().take(16).collect::<String>(),
                })
            })
            .collect(),
        message: None,
    }
}

/// 解析 “置信 80%” 或 “confidence=0.8”，读不出来时给中性 0.5。
pub fn extract_confidence(reason: &str) -> f64 {
    if let Some(caps) = percent_re().captures(reason) {
        if let Some(value) = caps.get(1).and_then(|m| m.as_str().parse::<f64>().ok()) {
            return (value / 100.0).min(1.0);
        }
    }
    if let Some(caps) = ratio_re().captures(reason) {
        if let Some(value) = caps.get(1).and_then(|m| m.as_str().parse::<f64>().ok()) {
            return value;
        }
    }
    0.5
}

fn percent_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"置信\s*(\d+(?:\.\d+)?)\s*%").expect("置信度正则固定合法"))
}

fn ratio_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)confidence\s*[=:]\s*(0?\.\d+|1(?:\.0+)?)")
            .expect("confidence 正则固定合法")
    })
}

fn hold_minutes(start: &str, end: &str) -> f64 {
    let (Some(start), Some(end)) = (util::parse_datetime(start), util::parse_datetime(end)) else {
        return 0.0;
    };
    (end - start).num_seconds() as f64 / 60.0
}

/// Python 打印浮点时总会带小数位（60.0 而非 60），prompt 文案沿用。
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quant-sim-learn-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir.join("experience.json")
    }

    fn order(id: u64, action: &str, price: f64, shares: i64, time: &str, reason: &str) -> Order {
        let amount = price * shares as f64;
        let (commission, stamp_tax, transfer_fee) = if action == "BUY" {
            (5.0, 0.0, amount * 0.00001)
        } else {
            (5.0, amount * 0.0005, amount * 0.00001)
        };
        Order {
            id,
            time: time.to_string(),
            action: action.to_string(),
            code: "600000".to_string(),
            name: "浦发银行".to_string(),
            price,
            shares,
            amount,
            commission,
            transfer_fee,
            stamp_tax,
            reason: reason.to_string(),
        }
    }

    fn trade(pnl_pct: f64, pnl_amount: f64, confidence: f64) -> ClosedTrade {
        ClosedTrade {
            code: "600000".to_string(),
            name: "浦发银行".to_string(),
            entry_time: "2026-01-05T10:00:00".to_string(),
            exit_time: "2026-01-06T14:00:00".to_string(),
            entry_price: 10.0,
            exit_price: 11.0,
            pnl_pct,
            pnl_amount,
            shares: 100,
            confidence,
            hold_minutes: 1680.0,
            reason: "测试".to_string(),
        }
    }

    #[test]
    fn empty_sample_has_message() {
        let stats = compute_stats(&[]);
        assert_eq!(stats.total_trades, 0);
        assert_eq!(stats.win_rate, 0.0);
        assert_eq!(stats.profit_factor, 0.0);
        assert_eq!(
            stats.message.as_deref(),
            Some("暂无已平仓交易，完成几笔后自动生成经验")
        );
        // 空统计不应写出 recent 键
        let text = serde_json::to_string(&stats).expect("统计可序列化");
        assert!(!text.contains("recent"));
    }

    #[test]
    fn profit_factor_three_branches() {
        let win_only = compute_stats(&[trade(10.0, 100.0, 0.8)]);
        assert_eq!(win_only.profit_factor, 999.0);

        let both = compute_stats(&[trade(10.0, 100.0, 0.8), trade(-5.0, -50.0, 0.6)]);
        assert_eq!(both.profit_factor, 2.0);
        assert_eq!(both.total_trades, 2);
        assert_eq!(both.win_rate, 50.0);
        assert_eq!(both.avg_pnl, 2.5);
        assert_eq!(both.best, 10.0);
        assert_eq!(both.worst, -5.0);

        let loss_only = compute_stats(&[trade(-5.0, -50.0, 0.9)]);
        assert_eq!(loss_only.profit_factor, 0.0);
    }

    #[test]
    fn confidence_buckets_split_by_level() {
        let stats = compute_stats(&[
            trade(10.0, 100.0, 0.9),
            trade(-2.0, -20.0, 0.75),
            trade(-3.0, -30.0, 0.5),
        ]);
        assert_eq!(stats.confidence_stats.len(), 3);
        let high = stats
            .confidence_stats
            .get("高置信(≥0.85)")
            .expect("高置信桶");
        assert_eq!(high.count, 1);
        assert_eq!(high.wins, 1);
        assert_eq!(high.win_rate, 100.0);
        let mid = stats
            .confidence_stats
            .get("中置信(0.70-0.85)")
            .expect("中置信桶");
        assert_eq!(mid.win_rate, 0.0);
        let low = stats
            .confidence_stats
            .get("低置信(<0.70)")
            .expect("低置信桶");
        assert_eq!(low.avg_pnl, -3.0);
    }

    #[test]
    fn learn_from_orders_builds_fifo_trade() {
        let path = temp_path("fifo");
        let mut learner = Learner::open(path.clone());
        let orders = vec![
            order(
                1,
                "BUY",
                10.0,
                200,
                "2026-01-05T10:00:00",
                "AI信号: 买入(置信 85%)",
            ),
            order(
                2,
                "SELL",
                11.0,
                100,
                "2026-01-06T14:00:00",
                "AI信号: 卖出(置信 80%)",
            ),
        ];
        let stats = learner.learn_from_orders(&orders).expect("学习成功");
        assert_eq!(stats.total_trades, 1);
        assert_eq!(stats.win_rate, 100.0);
        assert!(stats.profit_factor == 999.0);
        let closed = &learner.experience.closed_trades[0];
        assert_eq!(closed.shares, 100);
        // 买入成本必须含佣金与过户费，卖出价必须是净价
        assert!(closed.entry_price > 10.0);
        assert!(closed.exit_price < 11.0);
        assert!(closed.pnl_pct > 0.0);
        assert_eq!(closed.confidence, 0.85);
        assert_eq!(closed.hold_minutes, 1680.0);
        assert_eq!(learner.experience.version, 2);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(path.parent().expect("有父目录"));
    }

    #[test]
    fn unmatched_sell_is_ignored() {
        let path = temp_path("unmatched");
        let mut learner = Learner::open(path.clone());
        let orders = vec![order(
            1,
            "SELL",
            11.0,
            100,
            "2026-01-06T14:00:00",
            "无持仓卖出",
        )];
        let stats = learner.learn_from_orders(&orders).expect("学习成功");
        assert_eq!(stats.total_trades, 0);
        assert!(stats.message.is_some());
        let _ = std::fs::remove_dir_all(path.parent().expect("有父目录"));
    }

    #[test]
    fn summary_text_shapes() {
        let path = temp_path("text");
        let mut learner = Learner::open(path.clone());
        assert!(learner.summary_text().contains("暂无历史交易经验"));

        learner.experience.stats = compute_stats(&[trade(10.0, 100.0, 0.9)]);
        let text = learner.summary_text();
        assert!(text.contains("已平仓 1 笔"));
        assert!(text.contains("胜率 100.0%"));
        assert!(text.contains("总盈亏比 999"));
        assert!(text.contains("样本不足 10 笔"));
        assert!(text.contains("高置信(≥0.85)"));
        let _ = std::fs::remove_dir_all(path.parent().expect("有父目录"));
    }

    #[test]
    fn confidence_extraction_variants() {
        assert_eq!(extract_confidence("AI信号: 买入(置信 80%)"), 0.8);
        assert_eq!(extract_confidence("AI信号: 买入(置信度 65.5%)"), 0.5);
        assert_eq!(extract_confidence("confidence=0.72"), 0.72);
        assert_eq!(extract_confidence("CONFIDENCE: 1.0"), 1.0);
        assert_eq!(extract_confidence("没有提到"), 0.5);
    }

    #[test]
    fn corrupt_experience_file_falls_back() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{broken").expect("写入坏文件");
        let learner = Learner::open(path.clone());
        assert_eq!(learner.experience.version, 2);
        assert!(learner.experience.closed_trades.is_empty());
        let _ = std::fs::remove_dir_all(path.parent().expect("有父目录"));
    }
}
