//! 技术指标与规则打分。
//!
//! 口径必须固定：RSI/ATR 用 Wilder 平滑、MACD 柱等于 DIF-DEA、
//! 盘中量比按已走时段折算。任何“顺手优化”都会让回测与实盘对不上。

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::calendar::Calendar;
use crate::types::MarketSource;
use crate::util;

/// 少于这么多根 K 线时指标无意义，直接放弃。
const MIN_BARS: usize = 25;

#[derive(Clone, Debug, Serialize)]
pub struct Indicators {
    pub code: String,
    pub close: f64,
    pub is_intraday: bool,
    pub ma5: Option<f64>,
    pub ma10: Option<f64>,
    pub ma20: Option<f64>,
    pub ma60: Option<f64>,
    pub rsi14: f64,
    pub macd_dif: f64,
    pub macd_dea: f64,
    pub macd_hist: f64,
    pub atr14: f64,
    pub volume_ratio: f64,
    pub trend: String,
    pub signal_txt: String,
}

pub struct RuleScore {
    pub score: i32,
    pub grade: String,
    pub reasons: Vec<String>,
}

impl RuleScore {
    /// 指标不足时调用方（Bot/回测）用它占位，grade 固定为“数据不足”。
    pub fn insufficient() -> RuleScore {
        RuleScore {
            score: 0,
            grade: "数据不足".to_string(),
            reasons: vec!["K线数据不足".to_string()],
        }
    }
}

/// 取日 K 线并计算指标；数据不足或取不到数据时返回 None。
///
/// `as_of` 必须通过 `get_kline` 的 `end_date` 截断（而不是先取最近 N 根再过滤），
/// 否则回测里早期的指标会因为样本被截短而全空。
pub fn get_indicators(
    md: &mut dyn MarketSource,
    cal: &Calendar,
    symbol: &str,
    limit: usize,
    as_of: Option<&str>,
    now: NaiveDateTime,
) -> Option<Indicators> {
    let mut bars = md.get_kline(symbol, "1d", limit, as_of);
    if let Some(cut) = as_of {
        let cut: String = cut.chars().take(10).collect();
        bars.retain(|bar| bar.date.as_str() <= cut.as_str());
    }
    if bars.len() < MIN_BARS {
        return None;
    }

    let closes: Vec<f64> = bars.iter().map(|bar| bar.close).collect();
    let highs: Vec<f64> = bars.iter().map(|bar| bar.high).collect();
    let lows: Vec<f64> = bars.iter().map(|bar| bar.low).collect();
    let volumes: Vec<f64> = bars.iter().map(|bar| bar.volume).collect();
    let last = *closes.last()?;
    let today = util::ymd(now.date());
    let progress = cal.session_progress(now);
    let is_intraday = as_of.is_none()
        && bars.last().map(|bar| bar.date == today).unwrap_or(false)
        && cal.is_trading_day(now.date())
        && progress > 0.0
        && progress < 1.0;

    let ma5 = ma(&closes, 5);
    let ma10 = ma(&closes, 10);
    let ma20 = ma(&closes, 20);
    let ma60 = ma(&closes, 60);
    let rsi14 = rsi(&closes, 14);
    let (dif, dea, hist) = macd(&closes);
    let atr14 = atr(&highs, &lows, &closes, 14);
    let volume_ratio = volume_ratio(&volumes, is_intraday, cal, now);
    let trend_text = trend(ma5, ma10, ma20, ma60, &closes);

    let label = if is_intraday { "最新价" } else { "收盘" };
    let intraday_note = if is_intraday { "(盘中折算)" } else { "" };
    let signal_txt = format!(
        "{label} {last:.2} | MA5 {} MA10 {} MA20 {} MA60 {} | RSI14 {rsi14:.1} | \
         MACD DIF {dif:.3} DEA {dea:.3} 柱 {hist:.3} | ATR14 {atr14:.2} | \
         量比 {volume_ratio:.2}{intraday_note} | 趋势: {trend_text}",
        fmt_opt(ma5),
        fmt_opt(ma10),
        fmt_opt(ma20),
        fmt_opt(ma60),
    );

    Some(Indicators {
        code: symbol.to_string(),
        close: util::round_to(last, 2),
        is_intraday,
        ma5: round_opt(ma5),
        ma10: round_opt(ma10),
        ma20: round_opt(ma20),
        ma60: round_opt(ma60),
        rsi14: util::round_to(rsi14, 1),
        macd_dif: dif,
        macd_dea: dea,
        macd_hist: hist,
        atr14: util::round_to(atr14, 2),
        volume_ratio: util::round_to(volume_ratio, 2),
        trend: trend_text,
        signal_txt,
    })
}

/// 纯打分，不碰网络，便于单测与回测复用。
pub fn rule_score(ind: &Indicators, change_pct: Option<f64>) -> RuleScore {
    let mut score = 0i32;
    let mut reasons: Vec<String> = Vec::new();
    let trend_text = ind.trend.clone();

    if trend_text.contains("多头") {
        score += 25;
        reasons.push(format!("趋势{trend_text}+25"));
    } else if trend_text.contains("偏多") {
        score += 15;
        reasons.push("趋势偏多+15".to_string());
    } else if trend_text.contains("空头") {
        score -= 25;
        reasons.push(format!("趋势{trend_text}-25"));
    } else if trend_text.contains("偏空") {
        score -= 10;
        reasons.push("趋势偏空-10".to_string());
    } else {
        reasons.push("趋势震荡0".to_string());
    }

    let rsi_value = ind.rsi14;
    if (40.0..=65.0).contains(&rsi_value) {
        score += 20;
        reasons.push(format!("RSI{rsi_value:.0}健康+20"));
    } else if rsi_value > 65.0 && rsi_value <= 75.0 {
        score += 10;
        reasons.push(format!("RSI{rsi_value:.0}强势+10"));
    } else if rsi_value > 75.0 && rsi_value <= 80.0 {
        score += 5;
        reasons.push(format!("RSI{rsi_value:.0}偏热+5"));
    } else if rsi_value > 80.0 {
        score -= 15;
        reasons.push(format!("RSI{rsi_value:.0}超买-15"));
    } else if rsi_value < 30.0 {
        score += 5;
        reasons.push(format!("RSI{rsi_value:.0}超卖+5"));
    } else {
        score += 5;
        reasons.push(format!("RSI{rsi_value:.0}偏弱+5"));
    }

    if ind.macd_hist > 0.0 {
        score += 15;
        reasons.push("MACD红柱+15".to_string());
    } else {
        score -= 10;
        reasons.push("MACD绿柱-10".to_string());
    }

    let vr = ind.volume_ratio;
    if (1.0..=3.0).contains(&vr) {
        score += 15;
        reasons.push(format!("量比{vr:.2}放量+15"));
    } else if (0.8..1.0).contains(&vr) {
        score += 5;
        reasons.push(format!("量比{vr:.2}平量+5"));
    } else if vr < 0.8 {
        score -= 5;
        reasons.push(format!("量比{vr:.2}缩量-5"));
    } else {
        score -= 10;
        reasons.push(format!("量比{vr:.2}异常放量-10"));
    }

    let chg = change_pct.unwrap_or(0.0);
    if (0.0..5.0).contains(&chg) {
        score += 15;
        reasons.push(format!("涨幅{chg:.1}%健康+15"));
    } else if (5.0..9.0).contains(&chg) {
        score += 10;
        reasons.push(format!("涨幅{chg:.1}%强势+10"));
    } else if chg >= 9.0 {
        score -= 5;
        reasons.push(format!("涨幅{chg:.1}%过高-5"));
    } else {
        score -= 10;
        reasons.push(format!("涨幅{chg:.1}%下跌-10"));
    }

    match ind.ma20 {
        Some(ma20) if ma20 > 0.0 && ind.close > ma20 => {
            score += 10;
            reasons.push("价>MA20 +10".to_string());
        }
        Some(ma20) if ma20 > 0.0 => {
            score -= 10;
            reasons.push("价<MA20 -10".to_string());
        }
        _ => {}
    }

    let grade = if score >= 60 {
        "A(强)"
    } else if score >= 30 {
        "B(中)"
    } else if score >= 10 {
        "C(弱)"
    } else {
        "D(差)"
    };

    RuleScore {
        score,
        grade: grade.to_string(),
        reasons,
    }
}

pub fn ma(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period {
        return None;
    }
    let window = &values[values.len() - period..];
    Some(window.iter().sum::<f64>() / period as f64)
}

/// Wilder 平滑 RSI；没有下跌时定义为 100。
pub fn rsi(values: &[f64], period: usize) -> f64 {
    if period == 0 || values.len() <= period {
        return 50.0;
    }
    let mut gains: Vec<f64> = Vec::with_capacity(values.len() - 1);
    let mut losses: Vec<f64> = Vec::with_capacity(values.len() - 1);
    for pair in values.windows(2) {
        let diff = pair[1] - pair[0];
        gains.push(diff.max(0.0));
        losses.push((-diff).max(0.0));
    }
    let mut avg_gain = gains[..period].iter().sum::<f64>() / period as f64;
    let mut avg_loss = losses[..period].iter().sum::<f64>() / period as f64;
    let scale = (period - 1) as f64;
    for index in period..gains.len() {
        avg_gain = (avg_gain * scale + gains[index]) / period as f64;
        avg_loss = (avg_loss * scale + losses[index]) / period as f64;
    }
    if avg_loss == 0.0 {
        return 100.0;
    }
    let rs = avg_gain / avg_loss;
    100.0 - 100.0 / (1.0 + rs)
}

pub fn ema_series(values: &[f64], period: usize) -> Vec<f64> {
    if values.is_empty() || period == 0 {
        return Vec::new();
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut ema = values[0];
    let mut out = Vec::with_capacity(values.len());
    out.push(ema);
    for value in &values[1..] {
        ema = value * k + ema * (1.0 - k);
        out.push(ema);
    }
    out
}

/// 返回 (DIF, DEA, 柱)；柱就是 DIF-DEA，不乘 2。
pub fn macd(closes: &[f64]) -> (f64, f64, f64) {
    let (fast, slow, signal) = (12usize, 26usize, 9usize);
    if closes.len() < slow + signal {
        return (0.0, 0.0, 0.0);
    }
    let ema_fast = ema_series(closes, fast);
    let ema_slow = ema_series(closes, slow);
    let difs: Vec<f64> = ema_fast
        .iter()
        .zip(ema_slow.iter())
        .map(|(fast_value, slow_value)| fast_value - slow_value)
        .collect();
    let deas = ema_series(&difs, signal);
    let dif = *difs.last().unwrap_or(&0.0);
    let dea = *deas.last().unwrap_or(&0.0);
    (
        util::round_to(dif, 3),
        util::round_to(dea, 3),
        util::round_to(dif - dea, 3),
    )
}

/// Wilder 平滑 ATR。
pub fn atr(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> f64 {
    if period == 0
        || closes.len() <= period
        || highs.len() < closes.len()
        || lows.len() < closes.len()
    {
        return 0.0;
    }
    let mut trs: Vec<f64> = Vec::with_capacity(closes.len() - 1);
    for index in 1..closes.len() {
        let prev_close = closes[index - 1];
        trs.push(
            (highs[index] - lows[index])
                .max((highs[index] - prev_close).abs())
                .max((lows[index] - prev_close).abs()),
        );
    }
    let mut value = trs[..period].iter().sum::<f64>() / period as f64;
    let scale = (period - 1) as f64;
    for tr in &trs[period..] {
        value = (value * scale + tr) / period as f64;
    }
    value
}

/// 量比；盘中按已走时段折算，避免早盘被误判为缩量。
pub fn volume_ratio(volumes: &[f64], is_intraday: bool, cal: &Calendar, now: NaiveDateTime) -> f64 {
    if volumes.len() < 6 {
        return 1.0;
    }
    let recent = &volumes[volumes.len() - 6..volumes.len() - 1];
    let base = recent.iter().sum::<f64>() / 5.0;
    if base <= 0.0 {
        return 1.0;
    }
    let mut current = volumes[volumes.len() - 1];
    if is_intraday {
        let progress = cal.session_progress(now);
        current /= progress.max(0.05);
    }
    (current / base).max(0.01)
}

pub fn trend(
    ma5: Option<f64>,
    ma10: Option<f64>,
    ma20: Option<f64>,
    ma60: Option<f64>,
    closes: &[f64],
) -> String {
    let Some(last) = closes.last().copied() else {
        return "震荡".to_string();
    };
    if let (Some(v5), Some(v10), Some(v20), Some(v60)) = (ma5, ma10, ma20, ma60) {
        if v5 > v10 && v10 > v20 && v20 > v60 && last > v5 {
            return "多头排列(强势)".to_string();
        }
        if v5 < v10 && v10 < v20 && v20 < v60 && last < v5 {
            return "空头排列(弱势)".to_string();
        }
    }
    if let (Some(v5), Some(v10), Some(v20)) = (ma5, ma10, ma20) {
        if v5 > v10 && v10 > v20 && last > v5 {
            return "短期多头".to_string();
        }
        if v5 < v10 && v10 < v20 && last < v5 {
            return "短期空头".to_string();
        }
    }
    if let (Some(v5), Some(v10)) = (ma5, ma10) {
        if v5 > v10 {
            return "短期偏多".to_string();
        }
        if v5 < v10 {
            return "短期偏空".to_string();
        }
    }
    "震荡".to_string()
}

fn fmt_opt(value: Option<f64>) -> String {
    match value {
        Some(number) => format!("{number:.2}"),
        None => "--".to_string(),
    }
}

fn round_opt(value: Option<f64>) -> Option<f64> {
    value.map(|number| util::round_to(number, 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Bar;
    use chrono::{Datelike, NaiveDate};

    /// 假行情源：只实现 get_kline，其余方法返回空，保证指标层可离线单测。
    struct FakeMarket {
        rows: Vec<Bar>,
        calls: Vec<Option<String>>,
    }

    impl FakeMarket {
        fn new(rows: Vec<Bar>) -> FakeMarket {
            FakeMarket {
                rows,
                calls: Vec::new(),
            }
        }
    }

    impl MarketSource for FakeMarket {
        fn market_is_open(&mut self, _now: NaiveDateTime) -> bool {
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
        ) -> std::collections::HashMap<String, crate::types::Quote> {
            std::collections::HashMap::new()
        }

        fn get_kline(
            &mut self,
            _symbol: &str,
            _period: &str,
            limit: usize,
            end_date: Option<&str>,
        ) -> Vec<Bar> {
            self.calls.push(end_date.map(|text| text.to_string()));
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

    /// 造一段确定性行情：起点 2026-01-05，涨跌交替，跳过周末。
    fn make_rows(count: usize) -> Vec<Bar> {
        let mut rows = Vec::with_capacity(count);
        let mut day = NaiveDate::from_ymd_opt(2026, 1, 5).expect("固定日期");
        let mut price = 10.0f64;
        for index in 0..count {
            while day.weekday().num_days_from_monday() >= 5 {
                day = day.succ_opt().expect("日期递增");
            }
            let close = if index % 3 == 0 {
                price - 0.05
            } else {
                price + 0.1
            };
            rows.push(Bar {
                date: util::ymd(day),
                open: price,
                close,
                high: close + 0.2,
                low: price - 0.2,
                volume: 1000.0 + index as f64 * 10.0,
            });
            price = close;
            day = day.succ_opt().expect("日期递增");
        }
        rows
    }

    fn dt(text: &str) -> NaiveDateTime {
        util::parse_datetime(text).expect("测试时间合法")
    }

    #[test]
    fn indicators_shape_and_as_of() {
        let rows = make_rows(120);
        let cut = rows[rows.len() - 10].date.clone();
        let mut market = FakeMarket::new(rows);
        let ind = get_indicators(
            &mut market,
            &Calendar::empty(),
            "600519",
            120,
            Some(&cut),
            util::now_local(),
        )
        .expect("指标不为空");
        assert!((0.0..=100.0).contains(&ind.rsi14));
        assert!(ind.atr14 > 0.0);
        assert_eq!(
            ind.macd_hist,
            util::round_to(ind.macd_dif - ind.macd_dea, 3)
        );
        assert_eq!(market.calls.last().cloned().flatten(), Some(cut));
        assert!(!ind.is_intraday);
        assert!(ind.signal_txt.contains("收盘"));
        assert!(ind.ma20.is_some());
    }

    #[test]
    fn short_history_returns_empty() {
        let mut market = FakeMarket::new(make_rows(20));
        let result = get_indicators(
            &mut market,
            &Calendar::empty(),
            "600519",
            120,
            None,
            util::now_local(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn intraday_volume_ratio_is_annualized() {
        let mut rows = make_rows(120);
        let last = rows.len() - 1;
        let base: f64 = rows[last - 5..last]
            .iter()
            .map(|bar| bar.volume)
            .sum::<f64>()
            / 5.0;
        rows[last].volume = base / 4.0;
        let now = dt(&format!("{}T10:30:00", rows[last].date));
        let mut market = FakeMarket::new(rows);
        let ind = get_indicators(&mut market, &Calendar::empty(), "600519", 120, None, now)
            .expect("指标不为空");
        assert!(ind.is_intraday);
        assert!(ind.volume_ratio > 0.5);
        assert!(ind.signal_txt.contains("盘中折算"));
    }

    #[test]
    fn rule_score_ranges() {
        let mut market = FakeMarket::new(make_rows(120));
        let ind = get_indicators(
            &mut market,
            &Calendar::empty(),
            "600519",
            120,
            None,
            util::now_local(),
        )
        .expect("指标不为空");
        let result = rule_score(&ind, Some(2.0));
        assert!(result.grade.len() > 1);
        assert!(!result.reasons.is_empty());
        // 分数必须落在各条规则累加的可达范围内
        assert!(result.score >= -75 && result.score <= 100);
    }

    #[test]
    fn insufficient_score_placeholder() {
        let empty = RuleScore::insufficient();
        assert_eq!(empty.score, 0);
        assert_eq!(empty.grade, "数据不足");
        assert_eq!(empty.reasons, vec!["K线数据不足".to_string()]);
    }

    #[test]
    fn ma_and_rsi_basics() {
        let values: Vec<f64> = (1..=20).map(|v| v as f64).collect();
        assert_eq!(ma(&values, 5), Some(18.0));
        assert_eq!(ma(&values, 25), None);
        // 单调上涨没有下跌，RSI 定义为 100
        assert_eq!(rsi(&values, 14), 100.0);
        // 样本太短时取中性值
        assert_eq!(rsi(&values[..10], 14), 50.0);
    }

    #[test]
    fn macd_hist_is_dif_minus_dea() {
        let closes: Vec<f64> = make_rows(120).iter().map(|bar| bar.close).collect();
        let (dif, dea, hist) = macd(&closes);
        assert_eq!(hist, util::round_to(dif - dea, 3));
        // 样本不足 35 根时返回全零
        assert_eq!(macd(&closes[..20]), (0.0, 0.0, 0.0));
    }

    #[test]
    fn trend_labels() {
        let up = [10.0, 11.0, 12.0, 13.0];
        assert_eq!(
            trend(Some(12.0), Some(11.0), Some(10.0), Some(9.0), &up),
            "多头排列(强势)"
        );
        let down = [12.0, 11.0, 10.0, 8.0];
        assert_eq!(
            trend(Some(9.0), Some(10.0), Some(11.0), Some(12.0), &down),
            "空头排列(弱势)"
        );
        assert_eq!(
            trend(Some(12.0), Some(11.0), Some(10.0), None, &up),
            "短期多头"
        );
        assert_eq!(
            trend(Some(9.0), Some(10.0), Some(11.0), None, &down),
            "短期空头"
        );
        assert_eq!(trend(Some(11.0), Some(10.0), None, None, &up), "短期偏多");
        assert_eq!(trend(Some(10.0), Some(11.0), None, None, &down), "短期偏空");
        assert_eq!(trend(None, None, None, None, &up), "震荡");
    }
}
