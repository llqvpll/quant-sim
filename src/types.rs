//! 跨模块共享的数据结构与数据源 trait。
//!
//! 这里的类型只描述"数据长什么样"，不含任何行为；IO 实现（`MarketData` /
//! `LlmClient` / `NewsAnalyzer`）通过 trait 注入，方便上层用假实现做单测。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 实时行情快照。字段语义与 Python `_parse_quote` 返回的 dict 逐键对应。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Quote {
    pub code: String,
    pub name: String,
    pub price: f64,
    pub prev_close: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub volume: f64,
    pub change_pct: f64,
    pub time: String,
    pub turnover: f64,
    pub limit_up: f64,
    pub limit_down: f64,
    pub volume_ratio: f64,
}

/// 全市场列表中的一行（来自新浪列表接口）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StockInfo {
    pub code: String,
    pub name: String,
    pub price: f64,
    pub change_pct: f64,
}

/// 单根 K 线。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bar {
    pub date: String,
    pub open: f64,
    pub close: f64,
    pub high: f64,
    pub low: f64,
    pub volume: f64,
}

/// 预筛后的候选股，附带规则打分供 prompt 使用。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Candidate {
    pub code: String,
    pub name: String,
    pub price: f64,
    pub change_pct: f64,
    pub prev_close: Option<f64>,
    pub one_lot: f64,
    pub rule_score: i32,
    pub rule_grade: String,
    pub rule_reasons: String,
}

/// 止损/止盈触发的风险信号。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RiskSignal {
    pub code: String,
    pub shares: i64,
    pub price: f64,
    pub reason: String,
}

/// 扫描报告里的一条已执行动作。`shares` 既可能是股数也可能是 `"全部"`，
/// 因此用 `serde_json::Value` 承载，与 Python dict 的混合类型对齐。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanAction {
    pub code: String,
    pub name: String,
    pub action: String,
    pub shares: serde_json::Value,
    pub price: f64,
    pub msg: String,
}

/// 被跳过的标的及原因。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkippedItem {
    pub code: String,
    pub name: String,
    pub reason: String,
}

/// 一轮扫描的完整报告，序列化后写入 `state.last_scan`。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanReport {
    pub time: String,
    pub scanned: u32,
    pub buy: u32,
    pub sell: u32,
    pub wait: u32,
    pub error: u32,
    pub cached: u32,
    pub ai_calls: u32,
    pub rule_skipped: u32,
    pub actions: Vec<ScanAction>,
    pub skipped: Vec<SkippedItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_closed: Option<bool>,
    pub halted: bool,
    pub net_value: Option<serde_json::Value>,
}

/// 冷却窗口内复用的 AI 信号（error 不入缓存）。
#[derive(Clone, Debug)]
pub struct CachedSignal {
    pub signal: String,
    pub confidence: f64,
    pub reason: String,
    pub at: std::time::Instant,
}

/// 看板 `/api/state` 返回的机器人状态。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BotStatus {
    pub running: bool,
    pub last_beat: Option<String>,
    pub last_scan: Option<String>,
}

/// 行情数据源。所有方法都在失败时返回空值而不是抛错，
/// 因为一轮扫描不能因为单只股票的行情缺失而整体失败。
pub trait MarketSource {
    fn market_is_open(&mut self, now: chrono::NaiveDateTime) -> bool;
    fn get_all_a_stocks(&mut self, force: bool) -> Vec<StockInfo>;
    fn get_realtime(&mut self, symbol: &str) -> Option<Quote>;
    fn get_realtime_batch(&mut self, symbols: &[String]) -> HashMap<String, Quote>;
    fn get_kline(
        &mut self,
        symbol: &str,
        period: &str,
        limit: usize,
        end_date: Option<&str>,
    ) -> Vec<Bar>;
}

/// 大模型信号源。`&self` 而非 `&mut self`，便于回测时多个标的共享同一客户端。
pub trait AiSource {
    fn analyze(&self, req: &crate::signal::AnalyzeRequest<'_>) -> crate::signal::AiSignal;
}

/// 新闻数据源。
pub trait NewsSource {
    fn summary(&mut self, code: &str, name: Option<&str>) -> crate::news::NewsSummary;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_report_skips_market_closed_when_none() {
        let report = ScanReport::default();
        let value = serde_json::to_value(&report).unwrap();
        assert!(value.get("market_closed").is_none());
        assert_eq!(value["halted"], serde_json::Value::Bool(false));
        assert_eq!(value["actions"], serde_json::json!([]));
    }

    #[test]
    fn scan_report_keeps_market_closed_when_set() {
        let report = ScanReport {
            market_closed: Some(true),
            ..Default::default()
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["market_closed"], serde_json::Value::Bool(true));
    }

    #[test]
    fn scan_action_default_shares_is_null() {
        let action = ScanAction::default();
        let value = serde_json::to_value(&action).unwrap();
        assert!(value["shares"].is_null());
    }
}
