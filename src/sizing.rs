//! 仓位计算的唯一入口：置信度→比例、比例→目标股数。
//!
//! 实盘与回测只在数据源与成交时机上不同，仓位算法必须共用一份，否则同一信号
//! 会在两条路径上算出不同股数，结果无法互相解释。

use crate::config::Config;
use crate::rules;

/// 计算目标股数所需的输入。`volume_cap_shares` 只有回测会传，实盘为 None。
pub struct SizingInput<'a> {
    pub code: &'a str,
    pub price: f64,
    pub total_assets: f64,
    pub ratio: f64,
    pub volume_cap_shares: Option<i64>,
}

pub struct SizingOutcome {
    pub shares: i64,
    /// Some 表示不可下单，字符串可直接写进跳过原因日志。
    pub reason: Option<String>,
}

/// 置信度 → 仓位比例：门槛处从 base 起，满分封顶 max，线性插值。
pub fn confidence_ratio(confidence: f64, cfg: &Config) -> f64 {
    let threshold = cfg.confidence_buy_threshold;
    if confidence < threshold {
        return 0.0;
    }
    let base = cfg.confidence_base_ratio;
    let maximum = cfg.confidence_max_ratio;
    let span = (1.0 - threshold).max(1e-9);
    let ratio = base + (confidence - threshold) * (maximum - base) / span;
    ratio.clamp(0.0, maximum)
}

/// 在置信度比例上依次叠加模型给的仓位上限与全局上限。
/// `position_pct = Some(0)` 返回 0，由调用方保留各自的拒绝文案。
pub fn resolve_ratio(confidence: f64, position_pct: Option<u32>, cfg: &Config) -> f64 {
    let mut ratio = confidence_ratio(confidence, cfg);
    if let Some(pct) = position_pct {
        if pct == 0 {
            return 0.0;
        }
        ratio = ratio.min(pct as f64 / 100.0);
    }
    ratio.min(cfg.confidence_max_ratio)
}

/// 比例 → 目标股数：预算、板块手数规整、成交量上限、最低成交额。
/// 两条路径的拒绝文案在这里统一，便于日志逐字比对。
pub fn target_shares(input: &SizingInput<'_>, cfg: &Config) -> SizingOutcome {
    let budget = input.total_assets * input.ratio;
    let mut shares = if input.price > 0.0 {
        rules::normalize_buy_shares(input.code, (budget / input.price) as i64)
    } else {
        0
    };
    if let Some(cap) = input.volume_cap_shares {
        shares = shares.min(cap / 100 * 100);
    }
    if shares <= 0 {
        return SizingOutcome {
            shares,
            reason: Some(format!(
                "预算 {budget:.0} 元买不起最低 {} 股",
                rules::min_buy_shares(input.code)
            )),
        };
    }
    let amount = shares as f64 * input.price;
    if amount < cfg.min_trade_amount {
        return SizingOutcome {
            shares,
            reason: Some(format!(
                "单笔金额 {amount:.0} 元低于 {:.0} 元",
                cfg.min_trade_amount
            )),
        };
    }
    SizingOutcome {
        shares,
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(code: &'a str, price: f64, total_assets: f64, ratio: f64) -> SizingInput<'a> {
        SizingInput {
            code,
            price,
            total_assets,
            ratio,
            volume_cap_shares: None,
        }
    }

    #[test]
    fn ratio_at_threshold_and_cap() {
        let cfg = Config::default();
        assert_eq!(confidence_ratio(0.5, &cfg), 0.0);
        assert!((confidence_ratio(0.65, &cfg) - 0.15).abs() < 1e-9);
        assert!((confidence_ratio(1.0, &cfg) - 0.25).abs() < 1e-9);
        assert!(confidence_ratio(0.9, &cfg) <= cfg.confidence_max_ratio);
    }

    #[test]
    fn resolve_ratio_takes_the_smallest_cap() {
        let cfg = Config::default();
        // 0.9 置信度线性插值约 0.2214，被 position_pct=10% 压到 0.10
        assert!((resolve_ratio(0.9, Some(10), &cfg) - 0.10).abs() < 1e-9);
        // 不给 position_pct 时由全局上限 0.25 封顶，0.2214 保持不变
        assert!((resolve_ratio(0.9, Some(80), &cfg) - confidence_ratio(0.9, &cfg)).abs() < 1e-9);
        assert_eq!(resolve_ratio(1.0, Some(5), &cfg), 0.05);
        assert_eq!(resolve_ratio(0.9, Some(0), &cfg), 0.0);
        assert_eq!(resolve_ratio(0.4, Some(20), &cfg), 0.0);
    }

    #[test]
    fn target_shares_honours_board_lots() {
        let cfg = Config::default();
        // 主板：5000 元预算 / 10 元 → 500 股，100 整数倍
        let main = target_shares(&input("600000", 10.0, 50_000.0, 0.10), &cfg);
        assert!(main.reason.is_none());
        assert_eq!(main.shares, 500);
        // 科创板 200 股起，20000 元预算 / 10 元 → 2000 股可直接成交
        let star = target_shares(&input("688001", 10.0, 200_000.0, 0.10), &cfg);
        assert!(star.reason.is_none());
        assert_eq!(star.shares, 2000);
        // 预算不足 200 股时按科创板最低单位拒绝
        let star_small = target_shares(&input("688001", 10.0, 50_000.0, 0.01), &cfg);
        assert_eq!(star_small.shares, 0);
        assert_eq!(
            star_small.reason.as_deref(),
            Some("预算 500 元买不起最低 200 股")
        );
    }

    #[test]
    fn target_shares_applies_volume_cap_and_validation() {
        let cfg = Config::default();
        let mut capped = input("600000", 10.0, 50_000.0, 0.10);
        // cap=350 手数上限向下取整到 300 股
        capped.volume_cap_shares = Some(350);
        assert_eq!(target_shares(&capped, &cfg).shares, 300);

        let zero_price = target_shares(&input("600000", 0.0, 50_000.0, 0.10), &cfg);
        assert_eq!(zero_price.shares, 0);
        assert_eq!(
            zero_price.reason.as_deref(),
            Some("预算 5000 元买不起最低 100 股")
        );

        let too_small = target_shares(&input("600000", 1.0, 50_000.0, 0.05), &cfg);
        assert_eq!(too_small.shares, 2500);
        assert_eq!(
            too_small.reason.as_deref(),
            Some("单笔金额 2500 元低于 3000 元")
        );
    }

    /// 核心不变量：两条路径只差成交量上限，其余参数相同时股数必须完全一致。
    #[test]
    fn both_paths_agree_unless_volume_cap_binds() {
        let cfg = Config::default();
        let live = target_shares(&input("600000", 10.0, 50_000.0, 0.10), &cfg);
        let mut backtest = input("600000", 10.0, 50_000.0, 0.10);
        backtest.volume_cap_shares = Some(100_000);
        let backtest_large = target_shares(&backtest, &cfg);
        assert!(live.reason.is_none() && backtest_large.reason.is_none());
        assert_eq!(live.shares, 500);
        assert_eq!(backtest_large.shares, live.shares);

        backtest.volume_cap_shares = Some(350);
        let backtest_small = target_shares(&backtest, &cfg);
        assert_eq!(backtest_small.shares, 300);
        assert_eq!(live.shares, 500, "实盘路径不受回测成交量上限影响");
    }
}
