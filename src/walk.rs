//! 滚动窗口检验的纯逻辑：切窗、分布汇总、参数校验。
//!
//! 本策略没有可拟合参数，每个窗口天然都是样本外；这里只做滚动窗口稳健性检验，
//! 不对任何参数做选择，也不对结果给出投资建议。

use serde::Serialize;

use crate::util;

#[derive(Clone, Debug, Serialize)]
pub struct WalkSegment {
    pub index: usize,
    pub start: String,
    pub end: String,
    pub trading_days: usize,
    pub total_return_pct: f64,
    pub benchmark_return_pct: f64,
    pub excess_return_pct: f64,
    pub max_drawdown_pct: f64,
    pub sharpe: f64,
    pub sortino: f64,
    pub trades: usize,
    pub win_rate: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct WalkSummary {
    pub positive_excess_windows: usize,
    pub positive_ratio_pct: f64,
    pub mean_excess_pct: f64,
    pub std_excess_pct: f64,
    pub worst_excess_pct: f64,
    pub best_excess_pct: f64,
    pub mean_total_return_pct: f64,
    pub profitable_windows: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct WalkReport {
    pub snapshot_id: Option<String>,
    pub symbols: Vec<String>,
    pub benchmark: String,
    pub windows: usize,
    pub decision_every: usize,
    pub slippage_bps: f64,
    pub segments: Vec<WalkSegment>,
    pub summary: WalkSummary,
}

/// `--walk` 的参数校验：不传为单段，传入时必须至少 2 个窗口。
pub fn validate_walk_arg(value: Option<u32>) -> Result<usize, String> {
    match value {
        None => Ok(0),
        Some(windows) if windows < 2 => Err(format!("--walk 至少需要 2 个窗口，当前为 {windows}")),
        Some(windows) => Ok(windows as usize),
    }
}

/// 显式给出 --days 才按它截断；不传则取满 usable 个可用交易日。
pub fn walk_total(limit: Option<usize>, usable: usize) -> usize {
    limit.map(|value| value.min(usable)).unwrap_or(usable)
}

/// `--days` 是参与检验的总交易日上限：每段至少 min_days 天，
/// 因此总天数必须足够，否则参数本身就不成立。
pub fn validate_window_size(days: usize, windows: usize, min_days: usize) -> Result<(), String> {
    let needed = windows * min_days;
    if days < needed {
        return Err(format!("{windows} 段至少需要 {needed} 个交易日"));
    }
    Ok(())
}

/// 把可用交易日按数量等分成连续、互不重叠的窗口；多出来的天数分给靠前的段。
pub fn split_windows(
    dates: &[String],
    windows: usize,
    min_days: usize,
) -> Result<Vec<Vec<String>>, String> {
    let total = dates.len();
    if windows < 2 {
        return Err(format!("窗口数 {windows} 小于 2"));
    }
    if total < windows * min_days {
        return Err(format!(
            "可用交易日 {total} 天，不足以切出 {windows} 个各至少 {min_days} 天的窗口"
        ));
    }
    let base = total / windows;
    let extra = total % windows;
    let mut result: Vec<Vec<String>> = Vec::with_capacity(windows);
    let mut cursor: usize = 0;
    for index in 0..windows {
        let length = base + usize::from(index < extra);
        result.push(dates[cursor..cursor + length].to_vec());
        cursor += length;
    }
    Ok(result)
}

/// 总体标准差（除以 N），与 sharpe 的口径一致。
pub fn population_std(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let count = values.len() as f64;
    let mean = values.iter().sum::<f64>() / count;
    let variance = values
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f64>()
        / count;
    variance.sqrt()
}

/// 汇总各窗口的超额与总收益，所有比率保留 2 位。
pub fn summary(excess: &[f64], total_returns: &[f64]) -> WalkSummary {
    if excess.is_empty() {
        return WalkSummary::default();
    }
    let count = excess.len();
    let positive = excess.iter().filter(|value| **value > 0.0).count();
    let mean_excess = excess.iter().sum::<f64>() / count as f64;
    let mean_total = if total_returns.is_empty() {
        0.0
    } else {
        total_returns.iter().sum::<f64>() / total_returns.len() as f64
    };
    let worst = excess.iter().copied().fold(f64::INFINITY, f64::min);
    let best = excess.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    WalkSummary {
        positive_excess_windows: positive,
        positive_ratio_pct: util::round_to(positive as f64 / count as f64 * 100.0, 2),
        mean_excess_pct: util::round_to(mean_excess, 2),
        std_excess_pct: util::round_to(population_std(excess), 2),
        worst_excess_pct: util::round_to(worst, 2),
        best_excess_pct: util::round_to(best, 2),
        mean_total_return_pct: util::round_to(mean_total, 2),
        profitable_windows: total_returns.iter().filter(|value| **value > 0.0).count(),
    }
}

/// 只有「多窗口 + 实时行情」才需要提醒窗口边界会漂移。
pub fn needs_drift_warning(snapshot_id: &Option<String>, windows: usize) -> bool {
    windows >= 2 && snapshot_id.is_none()
}

/// 人可读汇总，中文、无 emoji、不给投资建议。
pub fn print_report(report: &WalkReport) {
    let summary = &report.summary;
    util::log(&format!(
        "[回测] 滚动窗口检验：{} 段 | 超额为正 {} 段 ({:.2}%)",
        report.windows, summary.positive_excess_windows, summary.positive_ratio_pct
    ));
    for segment in &report.segments {
        util::log(&format!(
            "[回测] 段{} {}~{} | 收益 {:.2}% | 基准 {:.2}% | 超额 {:.2}%",
            segment.index,
            segment.start,
            segment.end,
            segment.total_return_pct,
            segment.benchmark_return_pct,
            segment.excess_return_pct
        ));
    }
    util::log(&format!(
        "[回测] 超额均值 {:.2}% | 标准差 {:.2}% | 最差 {:.2}% | 最好 {:.2}%",
        summary.mean_excess_pct,
        summary.std_excess_pct,
        summary.worst_excess_pct,
        summary.best_excess_pct
    ));
    if summary.positive_excess_windows == report.windows {
        util::log("[回测] 各段均为正超额");
    } else {
        util::log("[回测] 存在为负的窗口，单段结果不足以说明策略稳定");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, NaiveDate};

    /// 151 个连续工作日，日期可预测，便于写死各段首末。
    fn dates(count: usize) -> Vec<String> {
        let mut result: Vec<String> = Vec::with_capacity(count);
        let mut day = NaiveDate::from_ymd_opt(2026, 1, 5).expect("固定日期");
        while result.len() < count {
            if day.weekday().num_days_from_monday() < 5 {
                result.push(util::ymd(day));
            }
            day = day.succ_opt().expect("日期递增");
        }
        result
    }

    #[test]
    fn split_windows_is_contiguous_and_covers_every_day() {
        let all = dates(151);
        let parts = split_windows(&all, 3, 10).expect("切窗");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 51);
        assert_eq!(parts[1].len(), 50);
        assert_eq!(parts[2].len(), 50);
        assert_eq!(parts[0].first().unwrap(), &all[0]);
        assert_eq!(parts[0].last().unwrap(), &all[50]);
        assert_eq!(parts[1].first().unwrap(), &all[51]);
        assert_eq!(parts[1].last().unwrap(), &all[100]);
        assert_eq!(parts[2].first().unwrap(), &all[101]);
        assert_eq!(parts[2].last().unwrap(), &all[150]);
        let covered: Vec<&String> = parts.iter().flat_map(|part| part.iter()).collect();
        assert_eq!(covered.len(), 151);
        for (index, date) in covered.iter().enumerate() {
            assert_eq!(*date, &all[index], "第 {index} 天必须与原序列一致");
        }
        // 段间首尾相接且不重叠
        assert!(parts[0].last().unwrap() < parts[1].first().unwrap());
        assert!(parts[1].last().unwrap() < parts[2].first().unwrap());
        let too_short = split_windows(&dates(25), 3, 10).unwrap_err();
        assert!(too_short.contains("可用交易日 25 天"));
    }

    #[test]
    fn walk_summary_matches_fixed_distribution() {
        let excess = [-5.0, 2.0, 3.0];
        let totals = [-1.0, 4.0, 2.0];
        let out = summary(&excess, &totals);
        assert_eq!(out.positive_excess_windows, 2);
        assert_eq!(out.positive_ratio_pct, 66.67);
        assert_eq!(out.mean_excess_pct, 0.0);
        assert_eq!(out.worst_excess_pct, -5.0);
        assert_eq!(out.best_excess_pct, 3.0);
        // 总体标准差 sqrt((25+4+9)/3) = sqrt(38/3)
        assert!((population_std(&excess) - (38.0f64 / 3.0).sqrt()).abs() < 1e-12);
        assert_eq!(out.std_excess_pct, 3.56);
        assert_eq!(out.mean_total_return_pct, 1.67);
        assert_eq!(out.profitable_windows, 2);
        assert_eq!(population_std(&[]), 0.0);
        assert_eq!(population_std(&[2.0, 2.0, 2.0]), 0.0);
    }

    #[test]
    fn validate_walk_arg_rejects_below_two() {
        assert_eq!(validate_walk_arg(None).unwrap(), 0);
        assert_eq!(validate_walk_arg(Some(3)).unwrap(), 3);
        assert_eq!(
            validate_walk_arg(Some(1)).unwrap_err(),
            "--walk 至少需要 2 个窗口，当前为 1"
        );
        assert_eq!(
            validate_walk_arg(Some(0)).unwrap_err(),
            "--walk 至少需要 2 个窗口，当前为 0"
        );
    }

    #[test]
    fn window_size_requires_ten_days_each() {
        assert!(validate_window_size(60, 3, 10).is_ok());
        assert!(validate_window_size(30, 3, 10).is_ok());
        assert_eq!(
            validate_window_size(29, 3, 10).unwrap_err(),
            "3 段至少需要 30 个交易日"
        );
        assert_eq!(
            validate_window_size(60, 7, 10).unwrap_err(),
            "7 段至少需要 70 个交易日"
        );
    }

    /// `--days 60 --walk 3` 必须得到 3 段各 20 天，且首尾相接。
    #[test]
    fn days_cap_splits_into_equal_windows() {
        let all = dates(151);
        let days = 60usize;
        let usable: Vec<String> = all[..days.min(all.len())].to_vec();
        assert_eq!(usable.len(), 60);
        assert!(validate_window_size(usable.len(), 3, 10).is_ok());
        let parts = split_windows(&usable, 3, 10).expect("切窗");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 20);
        assert_eq!(parts[1].len(), 20);
        assert_eq!(parts[2].len(), 20);
        assert_eq!(parts[0].first().unwrap(), &all[0]);
        assert_eq!(parts[0].last().unwrap(), &all[19]);
        assert_eq!(parts[1].first().unwrap(), &all[20]);
        assert_eq!(parts[1].last().unwrap(), &all[39]);
        assert_eq!(parts[2].first().unwrap(), &all[40]);
        assert_eq!(parts[2].last().unwrap(), &all[59]);
        let covered: Vec<&String> = parts.iter().flat_map(|part| part.iter()).collect();
        assert_eq!(covered.len(), 60);
        for (index, date) in covered.iter().enumerate() {
            assert_eq!(*date, &all[index]);
        }
    }

    /// 显式 --days 才截断；不传时用满可用区间。
    #[test]
    fn explicit_days_limits_walk_while_omitted_uses_all() {
        let all = dates(151);
        let available = all.len();
        assert_eq!(available, 151);
        let windows = 3usize;

        let explicit: Option<usize> = Some(60);
        let total = explicit
            .map(|limit| limit.min(available))
            .unwrap_or(available);
        assert_eq!(total, 60);
        let parts = split_windows(&all[..total], windows, 10).expect("切窗");
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            vec![20, 20, 20]
        );
        assert_eq!(parts.iter().map(|part| part.len()).sum::<usize>(), 60);

        let omitted: Option<usize> = None;
        let total_all = omitted
            .map(|limit| limit.min(available))
            .unwrap_or(available);
        assert_eq!(total_all, 151);
        let parts_all = split_windows(&all[..total_all], windows, 10).expect("切窗");
        assert_eq!(
            parts_all.iter().map(|part| part.len()).collect::<Vec<_>>(),
            vec![51, 50, 50]
        );
        assert_eq!(parts_all.iter().map(|part| part.len()).sum::<usize>(), 151);
        assert_eq!(parts_all[0].first().unwrap(), &all[0]);
        assert_eq!(parts_all[2].last().unwrap(), &all[150]);
    }

    /// 不传 --days 时用满 usable=152；显式 60 时截断到 60。
    #[test]
    fn walk_days_limit_none_uses_all_usable() {
        let usable = 152usize;
        let all = dates(usable);
        let windows = 3usize;

        let total_all = walk_total(None, usable);
        assert_eq!(total_all, 152);
        assert!(validate_window_size(total_all, windows, 10).is_ok());
        let parts_all = split_windows(&all[..total_all], windows, 10).expect("切窗");
        assert_eq!(
            parts_all.iter().map(|part| part.len()).collect::<Vec<_>>(),
            vec![51, 51, 50]
        );
        assert_eq!(parts_all.iter().map(|part| part.len()).sum::<usize>(), 152);

        let total_60 = walk_total(Some(60), usable);
        assert_eq!(total_60, 60);
        let parts_60 = split_windows(&all[..total_60], windows, 10).expect("切窗");
        assert_eq!(
            parts_60.iter().map(|part| part.len()).collect::<Vec<_>>(),
            vec![20, 20, 20]
        );
        assert_eq!(parts_60.iter().map(|part| part.len()).sum::<usize>(), 60);

        // 上限超过可用数时按可用数截断
        assert_eq!(walk_total(Some(999), usable), 152);
    }

    #[test]
    fn drift_warning_only_for_live_walk() {
        assert!(needs_drift_warning(&None, 3));
        assert!(!needs_drift_warning(
            &Some("20261116_192108".to_string()),
            3
        ));
        assert!(!needs_drift_warning(&None, 0));
        assert!(!needs_drift_warning(&None, 1));
    }
}
