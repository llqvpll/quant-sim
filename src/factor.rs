//! 因子评估旁路：把离散的买卖信号折算成连续分数，再与未来收益做 IC 检验。
//!
//! 只读 `backtest_signals.jsonl` 与本地行情快照，不参与决策、撮合与仓位计算，
//! 因此不可能改变回测结果。标准差一律用总体方差（除以 N），与 `sharpe()`、
//! `walk::population_std` 的口径保持一致。
//!
//! 结论必须按样本如实标注：横截面宽度只有几只、有效期数不到 20 期时，
//! IC 的波动几乎全是噪声，这里不会为了报告好看而放宽门槛。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::calendar::Calendar;
use crate::error::{AppError, AppResult};
use crate::indicators;
use crate::snapshot;
use crate::store::Paths;
use crate::types::{Bar, MarketSource};
use crate::util;

/// 判定「统计上够用」的最小有效 IC 期数。
pub const MIN_IC_OBSERVATIONS: usize = 20;
/// 判定「统计上够用」的最小平均横截面宽度。
pub const MIN_MEAN_WIDTH: f64 = 5.0;
/// 分层数：低 / 中 / 高。
pub const LAYER_COUNT: usize = 3;

/// bootstrap 重抽样次数。1000 次足够稳定，且跑一次不到毫秒级。
pub const BOOTSTRAP_ITERATIONS: usize = 1000;

/// 显著性水平：p 低于该值且样本量达标，才把因子判为「显著」。
pub const SIGNIFICANCE_LEVEL: f64 = 0.05;
/// Pearson 至少要 3 对样本，2 个点必然落在一条直线上，相关系数没有意义。
const MIN_PEARSON_SAMPLES: usize = 3;
/// 读快照时一次取回的日 K 上限，够覆盖 `--days` 的常用取值。
const KLINE_LIMIT: usize = 400;
/// 计算指标时回看的日 K 根数，与回测保持一致（`backtest` 也用 120）。
const INDICATOR_LIMIT: usize = 120;

/// 面板里的一行：某日某标的的分数与该日之后的实际收益。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactorRow {
    pub date: String,
    pub symbol: String,
    /// 连续分数，取值 `[-1, 1]`。
    pub score: f64,
    /// 前向收益百分比，`(future_close / close - 1) * 100`。
    pub forward_return_pct: f64,
}

/// 分层统计：按分数升序切块后各层的均值。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayerReturn {
    pub layer: String,
    pub rows: usize,
    pub mean_score: f64,
    pub mean_forward_pct: f64,
}

/// 一次因子评估的完整结论，也是 `data/factor_report.json` 的主体。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactorReport {
    pub horizon_days: usize,
    pub rows: usize,
    pub dates: usize,
    /// 平均每期标的数（横截面宽度）。
    pub mean_width: f64,
    /// 有效 IC 的期数。
    pub ic_observations: usize,
    pub ic_mean: f64,
    pub ic_std: f64,
    /// `ic_mean / ic_std`，`ic_std` 为 0 时记 0。
    pub icir: f64,
    pub positive_ic_ratio_pct: f64,
    /// IC 序列的 t 统计量：`mean / (std / sqrt(n))`。
    pub ic_t: f64,
    /// 双尾 p 值（t 分布，自由度 `n-1`）。
    pub ic_p_value: f64,
    /// IC 均值的 bootstrap 95% 置信区间下界。
    pub ic_ci_low: f64,
    pub ic_ci_high: f64,
    /// 分层序号与各层平均前向收益的 Spearman 相关；±1 表示完全单调。
    pub layer_spearman: f64,
    /// 有效期内 p < 0.05 才算显著。样本不足时一律 false。
    pub significant: bool,
    pub layers: Vec<LayerReturn>,
    pub sufficient: bool,
    pub note: String,
}

/// 落盘用的包装：在 `FactorReport` 全字段之外补上快照 id。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactorReportFile {
    pub snapshot_id: String,
    #[serde(flatten)]
    pub report: FactorReport,
}

/// 离散信号 → 连续分数：buy 记 `+confidence`，sell 记 `-confidence`，
/// wait/hold/error 记 0。`confidence` 先 clamp 到 `[0, 1]`。
pub fn signal_to_score(action: &str, confidence: f64) -> f64 {
    let conf = if confidence.is_finite() {
        confidence.clamp(0.0, 1.0)
    } else {
        0.0
    };
    match action.trim().to_lowercase().as_str() {
        "buy" => conf,
        "sell" => -conf,
        _ => 0.0,
    }
}

/// Pearson 相关系数；样本少于 3 对、长度不一致或任一序列方差为 0 返回 `None`。
///
/// 分母写成 `sqrt(sxx * syy)` 而不是 `sqrt(sxx) * sqrt(syy)`：
/// 完全线性相关的输入能精确得到 `±1.0`，不会因两次开方留下 1e-16 的尾巴。
pub fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < MIN_PEARSON_SAMPLES || xs.len() != ys.len() {
        return None;
    }
    let count = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / count;
    let mean_y = ys.iter().sum::<f64>() / count;
    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    for (x, y) in xs.iter().zip(ys.iter()) {
        let dx = x - mean_x;
        let dy = y - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }
    if var_x <= 0.0 || var_y <= 0.0 {
        return None;
    }
    let r = cov / (var_x * var_y).sqrt();
    if r.is_finite() {
        Some(r.clamp(-1.0, 1.0))
    } else {
        None
    }
}

// ---------- 统计检验 ----------
//
// IC 均值单独看没有意义：21 期、标准差 0.27 时，均值 0.06 的 t 值只有 1.03，
// 远不到 5% 显著的 2.086。这里补齐 t 检验、bootstrap 区间与分层单调性，
// 让「有没有预测力」变成一个可以被拒绝的假设，而不是一句感觉。

/// Lanczos 近似 ln Γ(x)。
fn ln_gamma(x: f64) -> f64 {
    const COF: [f64; 6] = [
        76.180_091_729_471_46,
        -86.505_320_329_416_77,
        24.014_098_240_830_91,
        -1.231_739_572_450_155,
        0.120_865_097_386_617_9e-2,
        -0.539_523_938_495_3e-5,
    ];
    let mut y = x;
    let tmp = x + 5.5;
    let tmp = tmp - (x + 0.5) * tmp.ln();
    let mut ser = 1.000_000_000_190_015;
    for cof in COF {
        y += 1.0;
        ser += cof / y;
    }
    -tmp + (2.506_628_274_631_000_5 * ser / x).ln()
}

/// 不完全 Beta 函数的连分数展开（Numerical Recipes 的 betacf）。
fn betacf(a: f64, b: f64, x: f64) -> f64 {
    const MAX_ITER: usize = 200;
    const EPS: f64 = 3.0e-7;
    const FPMIN: f64 = 1.0e-30;

    let mut c = 1.0;
    let mut d = 1.0 - (a + b) * x / (a + 1.0);
    if d.abs() < FPMIN {
        d = FPMIN;
    }
    d = 1.0 / d;
    let mut h = d;
    for step in 1..=MAX_ITER {
        let m = step as f64;
        let m2 = 2.0 * m;
        let mut aa = m * (b - m) * x / ((a + m2 - 1.0) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        h *= d * c;
        aa = -(a + m) * (a + b + m) * x / ((a + m2) * (a + m2 + 1.0));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    h
}

/// 正则化不完全 Beta 函数 I_x(a, b)。
fn betai(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let bt = (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        bt * betacf(a, b, x) / a
    } else {
        1.0 - bt * betacf(b, a, 1.0 - x) / b
    }
}

/// t 分布双尾 p 值。
pub fn t_two_sided_p(t: f64, df: f64) -> f64 {
    if df <= 0.0 || !t.is_finite() {
        return 1.0;
    }
    betai(df / 2.0, 0.5, df / (df + t * t))
}

/// IC 均值的 bootstrap 置信区间。固定种子的 xorshift，同一份数据每次结果一致。
pub fn bootstrap_mean_ci(values: &[f64], iterations: usize) -> (f64, f64) {
    if values.is_empty() || iterations == 0 {
        return (0.0, 0.0);
    }
    let mut state: u64 = 0x5EED_1234_ABCD_0001;
    let mut means: Vec<f64> = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let mut sum = 0.0;
        for _ in 0..values.len() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let index = (state % values.len() as u64) as usize;
            sum += values[index];
        }
        means.push(sum / values.len() as f64);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let low = (iterations as f64 * 0.025) as usize;
    let high = ((iterations as f64 * 0.975) as usize).min(iterations - 1);
    (means[low], means[high])
}

/// Spearman 秩相关：两组值先转秩再算 Pearson。
pub fn spearman(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < 3 || xs.len() != ys.len() {
        return None;
    }
    fn ranks(values: &[f64]) -> Vec<f64> {
        let mut order: Vec<usize> = (0..values.len()).collect();
        order.sort_by(|a, b| {
            values[*a]
                .partial_cmp(&values[*b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut out = vec![0.0; values.len()];
        // 并列值必须取平均秩。否则常数序列会拿到 1、2、3，
        // 算出一个假的 +1，而它本该因为方差为零被判为无定义。
        let mut start = 0;
        while start < order.len() {
            let mut end = start;
            while end + 1 < order.len() && values[order[end + 1]] == values[order[start]] {
                end += 1;
            }
            let average = (start + end) as f64 / 2.0 + 1.0;
            for slot in start..=end {
                out[order[slot]] = average;
            }
            start = end + 1;
        }
        out
    }
    pearson(&ranks(xs), &ranks(ys))
}

/// 汇总面板为一份报告。
pub fn evaluate(rows: &[FactorRow], horizon: usize) -> FactorReport {
    // 按日期分组；组内至少 2 行才算一个可比的横截面。
    let mut by_date: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, row) in rows.iter().enumerate() {
        by_date.entry(row.date.as_str()).or_default().push(index);
    }
    let dates: BTreeSet<&str> = by_date.keys().copied().collect();

    let mut ics: Vec<f64> = Vec::new();
    for indices in by_date.values() {
        if indices.len() < 2 {
            continue;
        }
        let xs: Vec<f64> = indices.iter().map(|index| rows[*index].score).collect();
        let ys: Vec<f64> = indices
            .iter()
            .map(|index| rows[*index].forward_return_pct)
            .collect();
        if let Some(ic) = pearson(&xs, &ys) {
            ics.push(ic);
        }
    }

    let ic_mean = mean(&ics);
    let ic_std = crate::walk::population_std(&ics);
    let icir = if ic_std > 0.0 { ic_mean / ic_std } else { 0.0 };
    let positive = ics.iter().filter(|value| **value > 0.0).count();
    let positive_ic_ratio_pct = if ics.is_empty() {
        0.0
    } else {
        positive as f64 * 100.0 / ics.len() as f64
    };
    let mean_width = if dates.is_empty() {
        0.0
    } else {
        rows.len() as f64 / dates.len() as f64
    };

    let sufficient = ics.len() >= MIN_IC_OBSERVATIONS && mean_width >= MIN_MEAN_WIDTH;
    let note = if sufficient {
        format!(
            "横截面宽度 {mean_width:.1} 只、有效 {} 期，已达统计门槛（>=5 只 / >=20 期）",
            ics.len()
        )
    } else {
        format!(
            "横截面宽度 {mean_width:.1} 只、有效 {} 期，IC 统计意义有限（建议 >=5 只 / >=20 期）",
            ics.len()
        )
    };

    let layers = build_layers(rows);
    let ic_t = if ics.len() >= 2 && ic_std > 0.0 {
        ic_mean / (ic_std / (ics.len() as f64).sqrt())
    } else {
        0.0
    };
    let degrees = (ics.len().max(2) - 1) as f64;
    let ic_p_value = t_two_sided_p(ic_t, degrees);
    let (ic_ci_low, ic_ci_high) = bootstrap_mean_ci(&ics, BOOTSTRAP_ITERATIONS);
    let layer_ranks: Vec<f64> = (0..layers.len()).map(|index| index as f64).collect();
    let layer_returns: Vec<f64> = layers.iter().map(|item| item.mean_forward_pct).collect();
    let layer_spearman = spearman(&layer_ranks, &layer_returns).unwrap_or(0.0);
    let significant = ics.len() >= MIN_IC_OBSERVATIONS && ic_p_value < SIGNIFICANCE_LEVEL;

    FactorReport {
        horizon_days: horizon,
        rows: rows.len(),
        dates: dates.len(),
        mean_width: util::round_to(mean_width, 4),
        ic_observations: ics.len(),
        ic_mean: util::round_to(ic_mean, 6),
        ic_std: util::round_to(ic_std, 6),
        icir: util::round_to(icir, 6),
        positive_ic_ratio_pct: util::round_to(positive_ic_ratio_pct, 2),
        ic_t: util::round_to(ic_t, 4),
        ic_p_value: util::round_to(ic_p_value, 6),
        ic_ci_low: util::round_to(ic_ci_low, 6),
        ic_ci_high: util::round_to(ic_ci_high, 6),
        layer_spearman: util::round_to(layer_spearman, 4),
        significant,
        layers,
        sufficient,
        note,
    }
}

/// 读信号日志 + 快照 K 线，组装面板。返回 `(面板, 跳过的行数)`。
///
/// 每行可以带 `score`（`factor build` 产出的规则因子面板），也可以只带
/// `signal`+`confidence`（回测写出的模型信号日志）；两者都按快照重算前向收益，
/// 保证评估口径与行情数据一致。
///
/// 跳过计数只统计「解析成功但算不出前向收益」的行：快照里没有该标的、
/// 找不到该日期、`i + horizon` 越过最后一根 K 线，或价格非法。
pub fn build_panel(
    signals_path: &Path,
    snapshot_dir: &Path,
    horizon: usize,
) -> AppResult<(Vec<FactorRow>, usize)> {
    if horizon == 0 {
        return Err(AppError::Invalid("前向收益期数必须为正整数".to_string()));
    }
    let text = std::fs::read_to_string(signals_path).map_err(|err| {
        AppError::Business(format!(
            "信号日志不可读（{}）：{err}",
            signals_path.display()
        ))
    })?;

    let mut cache: BarCache = BTreeMap::new();
    let mut rows: Vec<FactorRow> = Vec::new();
    let mut skipped: usize = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // 半截行、被手改坏的行直接忽略，不占用跳过计数
            continue;
        };
        let date = util::json_text(value.get("date")).trim().to_string();
        let symbol = util::json_text(value.get("symbol")).trim().to_string();
        if date.is_empty() || symbol.is_empty() {
            skipped += 1;
            continue;
        }
        // 两种面板都认：规则因子面板自带 `score`（直接采用），
        // 模型信号日志只有 `signal`+`confidence`（按买卖方向折算）。
        let score = match util::json_number(value.get("score")) {
            Some(score) if score.is_finite() => score.clamp(-1.0, 1.0),
            _ => signal_to_score(
                &util::json_text(value.get("signal")),
                util::json_number(value.get("confidence")).unwrap_or(0.0),
            ),
        };

        let Some(bars) = load_bars(&mut cache, snapshot_dir, &symbol) else {
            skipped += 1;
            continue;
        };
        let Some(index) = bars.iter().position(|bar| bar.date == date) else {
            skipped += 1;
            continue;
        };
        let Some(future) = bars.get(index + horizon) else {
            skipped += 1;
            continue;
        };
        let base = bars[index].close;
        if !base.is_finite() || base <= 0.0 || !future.close.is_finite() {
            skipped += 1;
            continue;
        }
        let forward = (future.close / base - 1.0) * 100.0;
        if !forward.is_finite() {
            skipped += 1;
            continue;
        }
        rows.push(FactorRow {
            date,
            symbol,
            score,
            forward_return_pct: util::round_to(forward, 6),
        });
    }

    // 固定成 (日期, 标的) 顺序，同一份日志无论拼接顺序如何都得到同一份报告
    rows.sort_by(|left, right| {
        left.date
            .cmp(&right.date)
            .then_with(|| left.symbol.cmp(&right.symbol))
    });
    Ok((rows, skipped))
}

/// 规则分归一化到 `[-1, 1]`：除以 100 后 clamp。
///
/// `rule_score` 自身取值落在 `-70 ~ 100`，理论上不会越界；clamp 是防止上游
/// 打分口径变化后把因子值推到区间外，让 IC 比较失去意义。
pub fn rule_score_to_factor(score: i32) -> f64 {
    (score as f64 / 100.0).clamp(-1.0, 1.0)
}

/// 从快照逐日逐标的算技术规则分，组装成因子面板。
///
/// 全程只读磁盘上的日 K，不调用大模型、不联网，因此同一份快照永远得到同一份面板。
/// `days == 0` 或超过可用根数时用满快照区间。
pub fn build_rule_panel(
    paths: &Paths,
    snapshot_id: &str,
    symbols: &[String],
    days: usize,
    horizon: usize,
) -> AppResult<(Vec<FactorRow>, usize)> {
    if horizon == 0 {
        return Err(AppError::Invalid("前向收益期数必须为正整数".to_string()));
    }
    if symbols.is_empty() {
        return Err(AppError::Invalid("规则因子至少要一个标的".to_string()));
    }
    let calendar = Calendar::load(paths);
    let mut source = snapshot::SnapshotSource::open(paths, snapshot_id)?;
    let now = util::now_local();
    let mut rows: Vec<FactorRow> = Vec::new();
    let mut skipped: usize = 0;

    for symbol in symbols {
        let bars = source.get_kline(symbol, "1d", KLINE_LIMIT, None);
        if bars.is_empty() {
            util::log(&format!("[因子] 快照缺少 {symbol} 的日K，整个标的跳过"));
            skipped += 1;
            continue;
        }
        let start = if days == 0 {
            0
        } else {
            bars.len().saturating_sub(days)
        };
        for index in start..bars.len() {
            let Some(future) = bars.get(index + horizon) else {
                // 尾部 horizon 根没有未来收益，逐日计数
                skipped += 1;
                continue;
            };
            // as_of 必须传当日，否则指标会吃到未来数据
            let Some(ind) = indicators::get_indicators(
                &mut source,
                &calendar,
                symbol,
                INDICATOR_LIMIT,
                Some(bars[index].date.as_str()),
                now,
            ) else {
                // 该日之前不足 25 根 K 线，指标无定义
                skipped += 1;
                continue;
            };
            let base = bars[index].close;
            if !base.is_finite() || base <= 0.0 || !future.close.is_finite() {
                skipped += 1;
                continue;
            }
            let forward = (future.close / base - 1.0) * 100.0;
            if !forward.is_finite() {
                skipped += 1;
                continue;
            }
            rows.push(FactorRow {
                date: bars[index].date.clone(),
                symbol: symbol.clone(),
                score: rule_score_to_factor(indicators::rule_score(&ind, None).score),
                forward_return_pct: util::round_to(forward, 6),
            });
        }
    }

    rows.sort_by(|left, right| {
        left.date
            .cmp(&right.date)
            .then_with(|| left.symbol.cmp(&right.symbol))
    });
    Ok((rows, skipped))
}

/// 面板落盘为 JSONL，一行一个 `FactorRow`，供 `factor report --panel` 读回。
pub fn save_panel(path: &Path, rows: &[FactorRow]) -> AppResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(row)?);
        text.push('\n');
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// 报告落盘：`FactorReport` 全字段 + 快照 id，末尾带换行。
pub fn save_report(path: &Path, snapshot_id: &str, report: &FactorReport) -> AppResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let payload = FactorReportFile {
        snapshot_id: snapshot_id.to_string(),
        report: report.clone(),
    };
    let text = serde_json::to_string_pretty(&payload)?;
    std::fs::write(path, format!("{text}\n"))?;
    Ok(())
}

/// 每个标的的 K 线只读一次；读不到时缓存 `None`，避免对同一标的反复碰磁盘。
type BarCache = BTreeMap<String, Option<Vec<Bar>>>;

fn load_bars<'a>(
    cache: &'a mut BarCache,
    snapshot_dir: &Path,
    symbol: &str,
) -> Option<&'a Vec<Bar>> {
    if !cache.contains_key(symbol) {
        let path = snapshot_dir.join("kline").join(snapshot::file_name(symbol));
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Bar>>(&text).ok());
        cache.insert(symbol.to_string(), loaded);
    }
    cache.get(symbol).and_then(|item| item.as_ref())
}

/// 按分数升序切成低/中/高三等份；行数少于 3 时只出一层。
fn build_layers(rows: &[FactorRow]) -> Vec<LayerReturn> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&FactorRow> = rows.iter().collect();
    sorted.sort_by(|left, right| {
        left.score
            .partial_cmp(&right.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let count = if sorted.len() < LAYER_COUNT {
        1
    } else {
        LAYER_COUNT
    };
    let labels = layer_labels(count);
    let mut layers: Vec<LayerReturn> = Vec::with_capacity(count);
    for (index, label) in labels.iter().enumerate() {
        let start = index * sorted.len() / count;
        let end = (index + 1) * sorted.len() / count;
        let slice = &sorted[start..end];
        let scores: Vec<f64> = slice.iter().map(|row| row.score).collect();
        let forwards: Vec<f64> = slice.iter().map(|row| row.forward_return_pct).collect();
        layers.push(LayerReturn {
            layer: (*label).to_string(),
            rows: slice.len(),
            mean_score: util::round_to(mean(&scores), 6),
            mean_forward_pct: util::round_to(mean(&forwards), 6),
        });
    }
    layers
}

fn layer_labels(count: usize) -> &'static [&'static str] {
    match count {
        1 => &["全部"],
        2 => &["低", "高"],
        _ => &["低", "中", "高"],
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, NaiveDate};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "quant-sim-factor-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建临时目录");
        base
    }

    /// 带 `data/` 的运行目录，快照与报告都写在它下面。
    fn temp_paths(tag: &str) -> Paths {
        let base = temp_dir(tag);
        std::fs::create_dir_all(base.join("data")).expect("创建 data");
        Paths { base_dir: base }
    }

    /// 造 `count` 根连续工作日 K 线，收盘价按 `start + step * i` 线性变化。
    fn make_bars(count: usize, start: f64, step: f64) -> Vec<Bar> {
        let mut rows: Vec<Bar> = Vec::with_capacity(count);
        let mut day = NaiveDate::from_ymd_opt(2026, 1, 5).expect("固定日期");
        for index in 0..count {
            while day.weekday().num_days_from_monday() >= 5 {
                day = day.succ_opt().expect("日期递增");
            }
            let close = start + step * index as f64;
            rows.push(Bar {
                date: util::ymd(day),
                open: close,
                close,
                high: close,
                low: close,
                volume: 1000.0,
            });
            day = day.succ_opt().expect("日期递增");
        }
        rows
    }

    #[test]
    fn signal_to_score_maps_actions_to_signed_confidence() {
        assert_eq!(signal_to_score("buy", 0.8), 0.8);
        assert_eq!(signal_to_score("sell", 0.6), -0.6);
        assert_eq!(signal_to_score("wait", 0.9), 0.0);
        assert_eq!(signal_to_score("hold", 0.9), 0.0);
        assert_eq!(signal_to_score("error", 0.5), 0.0);
        // 越界的置信度先 clamp 到 [0,1]
        assert_eq!(signal_to_score("buy", 1.7), 1.0);
        assert_eq!(signal_to_score("BUY", 0.3), 0.3);
        assert_eq!(signal_to_score("sell", -0.4), -0.0);
    }

    #[test]
    fn pearson_matches_known_cases() {
        assert_eq!(pearson(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]), Some(1.0));
        assert_eq!(pearson(&[1.0, 2.0, 3.0], &[3.0, 2.0, 1.0]), Some(-1.0));
        // 任一序列方差为 0 时相关系数无定义
        assert_eq!(pearson(&[1.0, 1.0, 1.0], &[2.0, 3.0, 4.0]), None);
        assert_eq!(pearson(&[1.0, 2.0, 3.0], &[5.0, 5.0, 5.0]), None);
        // 两个点必然共线，不给结论
        assert_eq!(pearson(&[1.0, 2.0], &[3.0, 4.0]), None);
        assert_eq!(pearson(&[1.0, 2.0, 3.0], &[1.0, 2.0]), None);
        // 非平凡样本：手算 r = 5.5 / sqrt(5.0 * 8.75) = 0.831522…
        let r = pearson(&[1.0, 2.0, 3.0, 4.0], &[1.0, 3.0, 2.0, 5.0]).expect("有定义");
        assert_eq!(util::round_to(r, 6), 0.831522);
        // 另一种整齐样本：r = 8 / 10 = 0.8
        assert_eq!(
            pearson(&[1.0, 2.0, 3.0, 4.0, 5.0], &[2.0, 1.0, 4.0, 3.0, 5.0]),
            Some(0.8)
        );
    }

    #[test]
    fn evaluate_reports_perfect_ic_when_score_tracks_return() {
        let mut rows: Vec<FactorRow> = Vec::new();
        for day in 0..25 {
            for slot in 0..5 {
                let score = (slot as f64 - 2.0) / 2.0;
                rows.push(FactorRow {
                    date: format!("2026-01-{:02}", day + 1),
                    symbol: format!("60000{slot}"),
                    score,
                    forward_return_pct: score * 2.0,
                });
            }
        }
        let report = evaluate(&rows, 5);
        assert_eq!(report.horizon_days, 5);
        assert_eq!(report.rows, 125);
        assert_eq!(report.dates, 25);
        assert_eq!(report.mean_width, 5.0);
        assert_eq!(report.ic_observations, 25);
        assert_eq!(report.ic_mean, 1.0);
        assert_eq!(report.ic_std, 0.0);
        assert_eq!(report.icir, 0.0);
        assert_eq!(report.positive_ic_ratio_pct, 100.0);
        assert!(report.sufficient);
        assert!(report.note.contains("已达统计门槛"));
        assert_eq!(report.layers.len(), 3);
        // 125 行按 0/41/83/125 切开，末两层各多 1 行
        assert_eq!(report.layers[0].rows, 41);
        assert_eq!(report.layers[1].rows, 42);
        assert_eq!(report.layers[2].rows, 42);
    }

    #[test]
    fn evaluate_flags_narrow_panel_as_insufficient() {
        let mut rows: Vec<FactorRow> = Vec::new();
        for day in 0..3 {
            for slot in 0..2 {
                let score = if slot == 0 { -0.5 } else { 0.5 };
                rows.push(FactorRow {
                    date: format!("2026-03-0{}", day + 1),
                    symbol: format!("00000{slot}"),
                    score,
                    forward_return_pct: score,
                });
            }
        }
        let report = evaluate(&rows, 5);
        assert_eq!(report.rows, 6);
        assert_eq!(report.dates, 3);
        assert_eq!(report.mean_width, 2.0);
        // 每期只有 2 只，Pearson 需要 3 对样本，因此一期有效 IC 都没有
        assert_eq!(report.ic_observations, 0);
        assert_eq!(report.ic_mean, 0.0);
        assert_eq!(report.ic_std, 0.0);
        assert_eq!(report.icir, 0.0);
        assert_eq!(report.positive_ic_ratio_pct, 0.0);
        assert!(!report.sufficient);
        assert!(report.note.contains("横截面宽度 2.0 只"));
        assert!(report.note.contains("有效 0 期"));
        assert!(report.note.contains("IC 统计意义有限"));
    }

    #[test]
    fn layers_split_sorted_rows_into_three_parts() {
        let rows: Vec<FactorRow> = (0..9)
            .map(|index| FactorRow {
                date: format!("2026-01-{:02}", index + 1),
                symbol: "600000".to_string(),
                score: -1.0 + 0.25 * index as f64,
                forward_return_pct: index as f64,
            })
            .collect();
        let report = evaluate(&rows, 5);
        assert_eq!(report.layers.len(), 3);
        assert_eq!(report.layers[0].layer, "低");
        assert_eq!(report.layers[1].layer, "中");
        assert_eq!(report.layers[2].layer, "高");
        for layer in &report.layers {
            assert_eq!(layer.rows, 3);
        }
        assert_eq!(report.layers[0].mean_score, -0.75);
        assert_eq!(report.layers[1].mean_score, 0.0);
        assert_eq!(report.layers[2].mean_score, 0.75);
        // 前 3 / 中 3 / 后 3 的前向收益均值
        assert_eq!(report.layers[0].mean_forward_pct, 1.0);
        assert_eq!(report.layers[1].mean_forward_pct, 4.0);
        assert_eq!(report.layers[2].mean_forward_pct, 7.0);
    }

    #[test]
    fn build_panel_skips_out_of_range_and_keeps_exact_returns() {
        let root = temp_dir("panel");
        let dir = root.join("snap");
        std::fs::create_dir_all(dir.join("kline")).expect("创建 kline 目录");
        // 600000 收盘 10,11,12…；600519 收盘 20,19.5,19…
        let rising = make_bars(30, 10.0, 1.0);
        let falling = make_bars(30, 20.0, -0.5);
        for (code, bars) in [("600000", &rising), ("600519", &falling)] {
            let text = serde_json::to_string_pretty(bars).expect("序列化 K 线");
            std::fs::write(dir.join("kline").join(format!("{code}.json")), text).expect("写 K 线");
        }

        let signals = root.join("signals.jsonl");
        let entries = [
            serde_json::json!({
                "date": rising[0].date.as_str(),
                "symbol": "600000",
                "signal": "buy",
                "confidence": 0.8,
            }),
            serde_json::json!({
                "date": rising[0].date.as_str(),
                "symbol": "600519",
                "signal": "sell",
                "confidence": 0.6,
            }),
            // 下标 25 加 5 越过最后一根（共 30 根），应被跳过
            serde_json::json!({
                "date": rising[25].date.as_str(),
                "symbol": "600000",
                "signal": "buy",
                "confidence": 0.5,
            }),
            // 快照里没有这个日期，应被跳过
            serde_json::json!({
                "date": "2030-01-01",
                "symbol": "600000",
                "signal": "buy",
                "confidence": 0.5,
            }),
        ];
        let mut text = String::new();
        for entry in entries {
            text.push_str(&format!("{entry}\n"));
        }
        std::fs::write(&signals, text).expect("写信号日志");

        let (rows, skipped) = build_panel(&signals, &dir, 5).expect("组装面板");
        assert_eq!(rows.len(), 2);
        assert_eq!(skipped, 2);
        assert_eq!(rows[0].date, rising[0].date);
        assert_eq!(rows[0].symbol, "600000");
        assert_eq!(rows[0].score, 0.8);
        // close[5]=15.0 / close[0]=10.0 → 50.0%
        assert_eq!(rows[0].forward_return_pct, 50.0);
        assert_eq!(rows[1].symbol, "600519");
        assert_eq!(rows[1].score, -0.6);
        // close[5]=17.5 / close[0]=20.0 → -12.5%
        assert_eq!(rows[1].forward_return_pct, -12.5);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 规则因子面板自带 `score`，读回时必须用它，而不是按 signal 重新折算。
    #[test]
    fn build_panel_prefers_explicit_score_field() {
        let root = temp_dir("panel-score");
        let dir = root.join("snap");
        std::fs::create_dir_all(dir.join("kline")).expect("创建 kline 目录");
        let bars = make_bars(30, 10.0, 1.0);
        let text = serde_json::to_string_pretty(&bars).expect("序列化 K 线");
        std::fs::write(dir.join("kline").join("600000.json"), text).expect("写 K 线");

        let day = bars[0].date.clone();
        let signals = root.join("panel.jsonl");
        let entries = [
            // 显式 score 优先于 signal/confidence
            serde_json::json!({"date": day, "symbol": "600000", "score": -0.8, "signal": "buy", "confidence": 0.9}),
            // 没有 score 时退回按信号折算
            serde_json::json!({"date": day, "symbol": "600000", "signal": "sell", "confidence": 0.6}),
            // 越界的 score 被 clamp
            serde_json::json!({"date": day, "symbol": "600000", "score": 1.4}),
        ];
        let mut text = String::new();
        for entry in entries {
            text.push_str(&format!("{entry}\n"));
        }
        std::fs::write(&signals, text).expect("写面板");

        let (rows, skipped) = build_panel(&signals, &dir, 5).expect("组装面板");
        assert_eq!(skipped, 0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].score, -0.8);
        assert_eq!(rows[1].score, -0.6);
        assert_eq!(rows[2].score, 1.0);
        // 三条的前向收益都按快照重算：close[5]=15.0 / close[0]=10.0 → 50.0%
        assert_eq!(rows[0].forward_return_pct, 50.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 造一份只有日 K 的快照，供规则因子面板使用。
    fn write_snapshot(paths: &Paths, symbols: &[(&str, Vec<Bar>)]) -> String {
        let mut kline: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
        let mut codes: Vec<String> = Vec::new();
        for (code, bars) in symbols {
            kline.insert((*code).to_string(), bars.clone());
            codes.push((*code).to_string());
        }
        let meta = snapshot::SnapshotMeta {
            id: "20261116_202529".to_string(),
            created_at: "2026-11-16T20:25:29".to_string(),
            symbols: Vec::new(),
            kline_symbols: codes,
            benchmark: "sh000300".to_string(),
            benchmark_source: "index:sh000300".to_string(),
            note: "单测快照".to_string(),
        };
        snapshot::write_snapshot(paths, &meta, &kline, &[]).expect("写快照");
        meta.id
    }

    /// 一组日涨跌序列，使第 35 根 K 线（下标 34）的规则分恰好是 60：
    /// 短期多头+25、RSI79 偏热+5、MACD 绿柱-10、量比+15、涨幅+15、价>MA20+10。
    /// 用来锁定「规则分 → 因子分」的除以 100 归一化。
    const SIXTY_POINT_STEPS: [f64; 40] = [
        0.8, 0.8, 0.5, 1.5, -0.2, 1.2, -0.2, 1.0, 1.0, -0.2, 0.5, 0.2, -0.2, 0.5, 0.2, -0.6, 1.2,
        0.4, 1.2, 1.0, -0.2, -0.6, -0.2, -0.6, -0.2, -0.2, 0.2, 0.3, 0.1, 0.4, -0.6, 0.5, 0.2, 0.8,
        1.5, 0.5, 0.1, 0.5, -0.6, 1.2,
    ];

    #[test]
    fn build_rule_panel_normalises_sixty_point_score_to_half_scale() {
        let paths = temp_paths("rule-sixty");
        let mut price = 10.0f64;
        let bars: Vec<Bar> = SIXTY_POINT_STEPS
            .iter()
            .enumerate()
            .map(|(index, step)| {
                price += *step;
                Bar {
                    date: format!("2026-01-{:02}", index + 1),
                    open: price,
                    close: price,
                    high: price + 0.05,
                    low: price - 0.05,
                    volume: 1000.0,
                }
            })
            .collect();
        let id = write_snapshot(&paths, &[("600000", bars.clone())]);
        let (rows, skipped) =
            build_rule_panel(&paths, &id, &["600000".to_string()], 0, 5).expect("组装面板");
        // 前 24 根不够算指标、尾部 5 根没有未来收益：11 行、跳过 29 行
        assert_eq!(rows.len(), 11);
        assert_eq!(skipped, 29);
        let target = bars[34].date.clone();
        let row = rows
            .iter()
            .find(|row| row.date == target)
            .expect("该日样本应在面板里");
        // rule_score = 60 → 0.6
        assert_eq!(row.score, 0.6);
        // close[39]=23.699999999999996 / close[34]=21.999999999999996 → 7.727273%
        assert_eq!(row.forward_return_pct, 7.727273);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn rule_score_to_factor_normalises_and_clamps() {
        // 60 分的规则分正好落在 0.6
        assert_eq!(rule_score_to_factor(60), 0.6);
        assert_eq!(rule_score_to_factor(25), 0.25);
        assert_eq!(rule_score_to_factor(0), 0.0);
        assert_eq!(rule_score_to_factor(-25), -0.25);
        // 上游口径若把分数推到 100 以上，必须被 clamp 在 1.0
        assert_eq!(rule_score_to_factor(100), 1.0);
        assert_eq!(rule_score_to_factor(140), 1.0);
        assert_eq!(rule_score_to_factor(-120), -1.0);
    }

    #[test]
    fn build_rule_panel_scores_and_forward_returns_are_exact() {
        let paths = temp_paths("rule-panel");
        // 600000 逐日 +1 元；600519 逐日 -0.5 元，量能恒定
        let rising = make_bars(40, 10.0, 1.0);
        let falling = make_bars(40, 20.0, -0.5);
        let id = write_snapshot(
            &paths,
            &[("600000", rising.clone()), ("600519", falling.clone())],
        );
        let symbols = vec!["600000".to_string(), "600519".to_string()];
        let (rows, skipped) = build_rule_panel(&paths, &id, &symbols, 0, 5).expect("组装面板");

        // 指标需要前 25 根，尾部 5 根没有未来收益：每只 11 行、跳过 29 行
        assert_eq!(rows.len(), 22);
        assert_eq!(skipped, 58);
        assert_eq!(rows[0].date, rising[24].date);
        assert_eq!(rows[0].symbol, "600000");
        // 严格单调上涨：趋势+25、RSI 超买-15、MACD-10、量比+15、涨幅+15、价>MA20+10 = 40 分
        assert_eq!(rows[0].score, 0.4);
        // close[29]=39.0 / close[24]=34.0 → 14.705882%
        assert_eq!(rows[0].forward_return_pct, 14.705882);
        assert_eq!(rows[1].symbol, "600519");
        // 严格单调下跌：空头-25、RSI 超卖+5、MACD-10、量比+15、涨幅+15、价<MA20-10 = -10 分
        assert_eq!(rows[1].score, -0.1);
        // close[29]=5.5 / close[24]=8.0 → -31.25%
        assert_eq!(rows[1].forward_return_pct, -31.25);

        // --days 截断只看最后 10 根：i 落在 30..39，前 5 根还有未来收益
        let (tail, tail_skipped) =
            build_rule_panel(&paths, &id, &symbols, 10, 5).expect("组装面板");
        assert_eq!(tail.len(), 10);
        assert_eq!(tail_skipped, 10);
        assert_eq!(tail[0].date, rising[30].date);
        let _ = std::fs::remove_dir_all(&paths.base_dir);
    }

    #[test]
    fn save_panel_writes_one_row_per_line() {
        let root = temp_dir("panel-save");
        let path = root.join("data").join("factor_panel.jsonl");
        let rows = vec![
            FactorRow {
                date: "2026-04-03".to_string(),
                symbol: "600000".to_string(),
                score: 0.6,
                forward_return_pct: 1.25,
            },
            FactorRow {
                date: "2026-04-03".to_string(),
                symbol: "600519".to_string(),
                score: -0.25,
                forward_return_pct: -0.5,
            },
        ];
        save_panel(&path, &rows).expect("写面板");
        let text = std::fs::read_to_string(&path).expect("读回面板");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: FactorRow = serde_json::from_str(lines[0]).expect("首行合法");
        assert_eq!(first.score, 0.6);
        assert_eq!(first.forward_return_pct, 1.25);
        let second: FactorRow = serde_json::from_str(lines[1]).expect("次行合法");
        assert_eq!(second.symbol, "600519");
        assert!(text.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_report_writes_snapshot_id_and_all_fields() {
        let root = temp_dir("save");
        let path = root.join("data").join("factor_report.json");
        let report = evaluate(
            &[FactorRow {
                date: "2026-04-03".to_string(),
                symbol: "600000".to_string(),
                score: -0.4,
                forward_return_pct: 1.5,
            }],
            5,
        );
        save_report(&path, "20261116_192108", &report).expect("写报告");
        let text = std::fs::read_to_string(&path).expect("读回报告");
        assert!(text.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(&text).expect("报告合法");
        assert_eq!(value["snapshot_id"], "20261116_192108");
        assert_eq!(value["horizon_days"], 5);
        assert_eq!(value["rows"], 1);
        assert_eq!(value["ic_observations"], 0);
        assert_eq!(value["sufficient"], false);
        assert_eq!(value["layers"][0]["layer"], "全部");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn t_p_value_matches_textbook_critical_values() {
        // df=20 时双尾 5% 临界值 2.086、1% 临界值 2.845（任意统计教材附表）。
        assert!((t_two_sided_p(2.086, 20.0) - 0.05).abs() < 0.002);
        assert!((t_two_sided_p(2.845, 20.0) - 0.01).abs() < 0.002);
        // df=10 时 5% 临界值 2.228。
        assert!((t_two_sided_p(2.228, 10.0) - 0.05).abs() < 0.002);
        // t=0 时双尾概率为 1。
        assert!((t_two_sided_p(0.0, 20.0) - 1.0).abs() < 1e-9);
        // 自由度非法时退回 1，避免出现 NaN 混进报告。
        assert_eq!(t_two_sided_p(2.0, 0.0), 1.0);
    }

    #[test]
    fn bootstrap_ci_brackets_the_sample_mean() {
        let values = [-0.4, -0.2, 0.0, 0.1, 0.3, 0.5, 0.2, -0.1, 0.4, 0.05];
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let (low, high) = bootstrap_mean_ci(&values, 2000);
        assert!(
            low < mean && mean < high,
            "区间 [{low}, {high}] 应包含均值 {mean}"
        );
        // 同一份数据两次调用必须完全一致，否则「可复现」是假的。
        assert_eq!((low, high), bootstrap_mean_ci(&values, 2000));
        // 空输入不能 panic。
        assert_eq!(bootstrap_mean_ci(&[], 100), (0.0, 0.0));
    }

    #[test]
    fn spearman_detects_layer_monotonicity() {
        let layers = [0.0, 1.0, 2.0];
        assert_eq!(
            spearman(&layers, &[-1.0, 0.2, 1.5]),
            Some(1.0),
            "完全递增应为 +1"
        );
        assert_eq!(
            spearman(&layers, &[1.5, 0.2, -1.0]),
            Some(-1.0),
            "完全递减应为 -1"
        );
        assert_eq!(
            spearman(&layers, &[-1.0, 2.0, 0.5]),
            Some(0.5),
            "非单调的两个上升一个下降应为 0.5"
        );
        assert_eq!(spearman(&layers, &[1.0, 1.0, 1.0]), None, "常数序列无定义");
    }

    #[test]
    fn evaluate_exposes_significance_fields() {
        // 25 期 × 5 只、分数与收益完全同序：IC 恒为 1.0，t 应极大、p 接近 0。
        let mut rows = Vec::new();
        for day in 0..25 {
            for index in 0..5 {
                let score = index as f64 * 0.2;
                rows.push(FactorRow {
                    date: format!("2026-04-{day:02}"),
                    symbol: format!("60000{index}"),
                    score,
                    forward_return_pct: score * 3.0,
                });
            }
        }
        let report = evaluate(&rows, 5);
        assert_eq!(report.ic_observations, 25);
        assert!((report.ic_mean - 1.0).abs() < 1e-9);
        assert!(
            report.ic_t > 100.0,
            "ICIR 无穷大时 t 应极大，实际 {}",
            report.ic_t
        );
        assert!(
            report.ic_p_value < 1e-6,
            "p 应接近 0，实际 {}",
            report.ic_p_value
        );
        assert!(report.significant);
        assert!((report.layer_spearman - 1.0).abs() < 1e-9);
        // 置信区间同样应贴近 1.0。
        assert!(report.ic_ci_low > 0.99 && report.ic_ci_high <= 1.0);
    }

    #[test]
    fn evaluate_marks_insufficient_sample_as_not_significant() {
        // 3 期即使 IC 很高，也不能判为显著。
        let mut rows = Vec::new();
        for day in 0..3 {
            for index in 0..5 {
                let score = index as f64 * 0.2;
                rows.push(FactorRow {
                    date: format!("2026-04-{day:02}"),
                    symbol: format!("60000{index}"),
                    score,
                    forward_return_pct: score,
                });
            }
        }
        let report = evaluate(&rows, 5);
        assert_eq!(report.ic_observations, 3);
        assert!(!report.significant, "3 期样本不应判为显著");
        assert!(!report.sufficient);
    }
}
