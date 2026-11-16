//! 集中配置：默认值、旧 state 合并与非法值修正。
//!
//! state.json 允许用户手改，因此这里的任何一步都不得失败：读不懂的键回落默认，
//! 并在 `[配置校验]` 日志里说明改了什么。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATE_VERSION: u32 = 2;

/// 31 个可调参数，键名与 Python 版 state.json 完全一致。
#[derive(Clone, Debug, Serialize)]
pub struct Config {
    // 账户
    pub initial_capital: f64,
    // 仓位与风控
    pub max_positions: u32,
    pub max_single_position_ratio: f64,
    pub stop_loss_pct: f64,
    pub take_profit_pct: f64,
    pub drawdown_halt_pct: f64,
    pub halt_recover_pct: f64,
    pub halt_cooldown_days: u32,
    pub min_hold_minutes: u32,
    // 费用：佣金双边、印花税卖出单边、过户费双边
    pub commission_rate: f64,
    pub min_commission: f64,
    pub stamp_tax_rate: f64,
    pub transfer_fee_rate: f64,
    // 交易约束
    pub min_trade_amount: f64,
    pub max_one_lot_ratio: f64,
    pub max_round_trip_cost_pct: f64,
    pub min_price: f64,
    pub max_price: f64,
    // 策略
    pub scan_top_n: u32,
    pub ai_analyze_n: u32,
    pub rule_score_min: i32,
    pub scan_interval: u32,
    pub signal_cooldown_min: u32,
    pub confidence_buy_threshold: f64,
    pub confidence_base_ratio: f64,
    pub confidence_max_ratio: f64,
    // 回测
    pub backtest_days: u32,
    pub backtest_decision_every: u32,
    // AI
    pub ai_max_retries: u32,
    pub ai_timeout_seconds: u32,
    pub price_tolerance_pct: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            initial_capital: 50_000.0,
            max_positions: 3,
            max_single_position_ratio: 0.25,
            stop_loss_pct: -5.0,
            take_profit_pct: 15.0,
            drawdown_halt_pct: -10.0,
            halt_recover_pct: -5.0,
            halt_cooldown_days: 5,
            min_hold_minutes: 30,
            commission_rate: 0.0003,
            min_commission: 5.0,
            stamp_tax_rate: 0.0005,
            transfer_fee_rate: 0.00001,
            min_trade_amount: 3_000.0,
            max_one_lot_ratio: 0.25,
            max_round_trip_cost_pct: 1.5,
            min_price: 5.0,
            max_price: 500.0,
            scan_top_n: 10,
            ai_analyze_n: 5,
            rule_score_min: 20,
            scan_interval: 300,
            signal_cooldown_min: 30,
            confidence_buy_threshold: 0.65,
            confidence_base_ratio: 0.15,
            confidence_max_ratio: 0.25,
            backtest_days: 60,
            backtest_decision_every: 5,
            ai_max_retries: 3,
            ai_timeout_seconds: 180,
            price_tolerance_pct: 3.0,
        }
    }
}

/// 手写的反序列化让“怎么读都读得进来”：整个 Config 走 Value → 逐键合并，
/// 与 Python 的 merge_config 等价。单个坏键只会回落默认值，不会连累整份 state。
impl<'de> Deserialize<'de> for Config {
    fn deserialize<D>(deserializer: D) -> Result<Config, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        Ok(Config::merged(Some(&raw)))
    }
}

impl Config {
    /// 把旧 state 里的配置合并到默认值上，未知键丢弃。
    ///
    /// 非数字/非有限值直接回落默认并记一条 `[配置校验]`：Rust 的结构体字段
    /// 无法像 Python dict 那样暂存字符串，只能在此处就完成类型收敛。
    pub fn merged(raw: Option<&Value>) -> Config {
        let mut issues: Vec<String> = Vec::new();
        let cfg = Config::merge_from(Config::default(), raw, &mut issues);
        report(&issues);
        cfg
    }

    /// 修正明显不合法的配置。不返回错误，只修正并打日志。
    pub fn sanitized(mut self) -> Config {
        let d = Config::default();
        let mut issues: Vec<String> = Vec::new();

        // 有限性兜底：merged 已过滤一轮，但 NaN/inf 仍可能来自直接构造或旧版本序列化
        macro_rules! ensure_finite {
            ($($field:ident),* $(,)?) => {
                $(
                    if !self.$field.is_finite() {
                        issues.push(format!("{}={} 不是有限数，使用默认值", stringify!($field), self.$field));
                        self.$field = d.$field;
                    }
                )*
            };
        }
        ensure_finite!(
            initial_capital,
            max_single_position_ratio,
            stop_loss_pct,
            take_profit_pct,
            drawdown_halt_pct,
            halt_recover_pct,
            commission_rate,
            min_commission,
            stamp_tax_rate,
            transfer_fee_rate,
            min_trade_amount,
            max_one_lot_ratio,
            max_round_trip_cost_pct,
            min_price,
            max_price,
            confidence_buy_threshold,
            confidence_base_ratio,
            confidence_max_ratio,
            price_tolerance_pct,
        );

        if self.initial_capital <= 0.0 {
            issues.push("initial_capital 必须为正，使用默认值".to_string());
            self.initial_capital = d.initial_capital;
        }
        if !(self.max_single_position_ratio > 0.0 && self.max_single_position_ratio <= 1.0) {
            issues.push("max_single_position_ratio 必须在 (0,1]，修正为 0.25".to_string());
            self.max_single_position_ratio = 0.25;
        }
        if self.max_positions < 1 {
            issues.push("max_positions 至少为 1".to_string());
            self.max_positions = 1;
        }
        if self.stop_loss_pct >= 0.0 {
            issues.push("stop_loss_pct 必须为负，修正为 -5".to_string());
            self.stop_loss_pct = -5.0;
        }
        if self.take_profit_pct <= 0.0 {
            issues.push("take_profit_pct 必须为正，修正为 15".to_string());
            self.take_profit_pct = 15.0;
        }
        if self.drawdown_halt_pct >= 0.0 {
            issues.push("drawdown_halt_pct 必须为负，修正为 -10".to_string());
            self.drawdown_halt_pct = -10.0;
        }
        if self.halt_recover_pct <= self.drawdown_halt_pct {
            issues.push("halt_recover_pct 应高于 drawdown_halt_pct，修正为 -5".to_string());
            self.halt_recover_pct = -5.0;
        }
        if !(0.0..=1.0).contains(&self.confidence_buy_threshold) {
            issues.push("confidence_buy_threshold 必须在 [0,1]，修正为 0.65".to_string());
            self.confidence_buy_threshold = 0.65;
        }
        if self.confidence_base_ratio > self.confidence_max_ratio {
            issues.push("confidence_base_ratio 不能大于 max，修正".to_string());
            self.confidence_base_ratio = 0.15;
            self.confidence_max_ratio = 0.25;
        }
        if self.commission_rate < 0.0 || self.stamp_tax_rate < 0.0 {
            issues.push("费率不能为负，修正为默认值".to_string());
            self.commission_rate = d.commission_rate;
            self.stamp_tax_rate = d.stamp_tax_rate;
        }

        report(&issues);
        self
    }

    /// State 的 `config` 字段反序列化入口：先取 Value 再逐键合并，
    /// 保证单个坏键不会让整份 state 读取失败。
    pub fn de_loose<'de, D>(deserializer: D) -> Result<Config, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        Ok(Config::merged(Some(&raw)))
    }

    fn merge_from(mut cfg: Config, raw: Option<&Value>, issues: &mut Vec<String>) -> Config {
        let Some(obj) = raw.and_then(|value| value.as_object()) else {
            return cfg;
        };
        cfg.initial_capital = read_f64(obj, "initial_capital", cfg.initial_capital, issues);
        cfg.max_positions = read_u32(obj, "max_positions", cfg.max_positions, issues);
        cfg.max_single_position_ratio = read_f64(
            obj,
            "max_single_position_ratio",
            cfg.max_single_position_ratio,
            issues,
        );
        cfg.stop_loss_pct = read_f64(obj, "stop_loss_pct", cfg.stop_loss_pct, issues);
        cfg.take_profit_pct = read_f64(obj, "take_profit_pct", cfg.take_profit_pct, issues);
        cfg.drawdown_halt_pct = read_f64(obj, "drawdown_halt_pct", cfg.drawdown_halt_pct, issues);
        cfg.halt_recover_pct = read_f64(obj, "halt_recover_pct", cfg.halt_recover_pct, issues);
        cfg.halt_cooldown_days =
            read_u32(obj, "halt_cooldown_days", cfg.halt_cooldown_days, issues);
        cfg.min_hold_minutes = read_u32(obj, "min_hold_minutes", cfg.min_hold_minutes, issues);
        cfg.commission_rate = read_f64(obj, "commission_rate", cfg.commission_rate, issues);
        cfg.min_commission = read_f64(obj, "min_commission", cfg.min_commission, issues);
        cfg.stamp_tax_rate = read_f64(obj, "stamp_tax_rate", cfg.stamp_tax_rate, issues);
        cfg.transfer_fee_rate = read_f64(obj, "transfer_fee_rate", cfg.transfer_fee_rate, issues);
        cfg.min_trade_amount = read_f64(obj, "min_trade_amount", cfg.min_trade_amount, issues);
        cfg.max_one_lot_ratio = read_f64(obj, "max_one_lot_ratio", cfg.max_one_lot_ratio, issues);
        cfg.max_round_trip_cost_pct = read_f64(
            obj,
            "max_round_trip_cost_pct",
            cfg.max_round_trip_cost_pct,
            issues,
        );
        cfg.min_price = read_f64(obj, "min_price", cfg.min_price, issues);
        cfg.max_price = read_f64(obj, "max_price", cfg.max_price, issues);
        cfg.scan_top_n = read_u32(obj, "scan_top_n", cfg.scan_top_n, issues);
        cfg.ai_analyze_n = read_u32(obj, "ai_analyze_n", cfg.ai_analyze_n, issues);
        cfg.rule_score_min = read_i32(obj, "rule_score_min", cfg.rule_score_min, issues);
        cfg.scan_interval = read_u32(obj, "scan_interval", cfg.scan_interval, issues);
        cfg.signal_cooldown_min =
            read_u32(obj, "signal_cooldown_min", cfg.signal_cooldown_min, issues);
        cfg.confidence_buy_threshold = read_f64(
            obj,
            "confidence_buy_threshold",
            cfg.confidence_buy_threshold,
            issues,
        );
        cfg.confidence_base_ratio = read_f64(
            obj,
            "confidence_base_ratio",
            cfg.confidence_base_ratio,
            issues,
        );
        cfg.confidence_max_ratio = read_f64(
            obj,
            "confidence_max_ratio",
            cfg.confidence_max_ratio,
            issues,
        );
        cfg.backtest_days = read_u32(obj, "backtest_days", cfg.backtest_days, issues);
        cfg.backtest_decision_every = read_u32(
            obj,
            "backtest_decision_every",
            cfg.backtest_decision_every,
            issues,
        );
        cfg.ai_max_retries = read_u32(obj, "ai_max_retries", cfg.ai_max_retries, issues);
        cfg.ai_timeout_seconds =
            read_u32(obj, "ai_timeout_seconds", cfg.ai_timeout_seconds, issues);
        cfg.price_tolerance_pct =
            read_f64(obj, "price_tolerance_pct", cfg.price_tolerance_pct, issues);
        cfg
    }
}

fn report(issues: &[String]) {
    if !issues.is_empty() {
        crate::util::log(&format!("[配置校验] {}", issues.join("；")));
    }
}

/// 取一个数值键；缺失用默认值，读不出来回落到默认并记录问题。
fn read_f64(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    default: f64,
    issues: &mut Vec<String>,
) -> f64 {
    let Some(value) = obj.get(key) else {
        return default;
    };
    match value_as_f64(value) {
        Some(number) if number.is_finite() => number,
        Some(number) => {
            issues.push(format!("{key}={number} 不是有限数，使用默认值"));
            default
        }
        None => {
            issues.push(format!("{key}={} 不是数字，使用默认值", py_repr(value)));
            default
        }
    }
}

fn read_u32(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    default: u32,
    issues: &mut Vec<String>,
) -> u32 {
    let base = default as f64;
    let number = read_f64(obj, key, base, issues);
    // 负数在 Rust 里饱和为 0，随后由 sanitized 的“至少为 1”规则兜住
    number.trunc() as u32
}

fn read_i32(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    default: i32,
    issues: &mut Vec<String>,
) -> i32 {
    let base = default as f64;
    read_f64(obj, key, base, issues).trunc() as i32
}

/// 与 Python `float()` 一致的宽松转换：数字与数字字符串都收。
fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 近似 Python `repr()`，只用于日志文案。
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => format!("{text:?}"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_merge() {
        let raw = json!({"stop_loss_pct": -3, "unknown": 1});
        let cfg = Config::merged(Some(&raw));
        assert_eq!(cfg.stop_loss_pct, -3.0);
        assert_eq!(cfg.stamp_tax_rate, 0.0005);
        // 未知键被丢弃：序列化结果里不应出现
        let text = serde_json::to_string(&cfg).expect("配置可序列化");
        assert!(!text.contains("unknown"));
        assert_eq!(text.matches(':').count(), 31);
    }

    #[test]
    fn validate_fixes_bad_values() {
        let raw = json!({
            "max_single_position_ratio": 2,
            "stop_loss_pct": 1,
            "take_profit_pct": -1,
            "initial_capital": -5,
            "confidence_buy_threshold": 2,
        });
        let cfg = Config::merged(Some(&raw)).sanitized();
        assert_eq!(cfg.max_single_position_ratio, 0.25);
        assert!(cfg.stop_loss_pct < 0.0);
        assert!(cfg.take_profit_pct > 0.0);
        assert_eq!(cfg.initial_capital, Config::default().initial_capital);
        assert!((0.0..=1.0).contains(&cfg.confidence_buy_threshold));
    }

    #[test]
    fn broken_field_falls_back_to_default() {
        let raw = json!({"stop_loss_pct": "abc", "take_profit_pct": null, "max_positions": "x"});
        let cfg = Config::merged(Some(&raw));
        assert_eq!(cfg.stop_loss_pct, -5.0);
        assert_eq!(cfg.take_profit_pct, 15.0);
        assert_eq!(cfg.max_positions, 3);
    }

    #[test]
    fn numeric_string_is_accepted() {
        let raw = json!({"stop_loss_pct": "-4.5", "max_positions": 2.9});
        let cfg = Config::merged(Some(&raw));
        assert_eq!(cfg.stop_loss_pct, -4.5);
        assert_eq!(cfg.max_positions, 2);
    }

    #[test]
    fn sanitize_orders_paired_thresholds() {
        let raw = json!({"confidence_base_ratio": 0.5, "confidence_max_ratio": 0.2});
        let cfg = Config::merged(Some(&raw)).sanitized();
        assert_eq!(cfg.confidence_base_ratio, 0.15);
        assert_eq!(cfg.confidence_max_ratio, 0.25);

        let raw = json!({"drawdown_halt_pct": -20, "halt_recover_pct": -30});
        let cfg = Config::merged(Some(&raw)).sanitized();
        assert_eq!(cfg.halt_recover_pct, -5.0);
    }

    #[test]
    fn loose_deserialize_never_fails() {
        let value = json!({"initial_capital": "bad", "max_positions": [], "junk": 1});
        let cfg: Config = serde_json::from_value(value).expect("容错反序列化");
        assert_eq!(cfg.initial_capital, 50_000.0);
        assert_eq!(cfg.max_positions, 3);

        // 非对象（如 null）也应回落默认值
        let cfg: Config = serde_json::from_value(Value::Null).expect("null 也能读");
        assert_eq!(cfg.min_price, 5.0);
    }
}
