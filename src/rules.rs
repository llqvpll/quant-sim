//! A 股交易规则：板块、涨跌停、手数、T+1。
//!
//! 期货式“差不多”在撮合里会直接产生假收益，因此这里所有判断都按交易所口径实现。

use chrono::NaiveDate;

use crate::calendar::Calendar;
use crate::util;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Board {
    Main,
    Star,
    Chinext,
    Bse,
    Unknown,
}

/// 从 600519 / sh600519 / 600519.SH 中提取 6 位代码。
pub fn code_digits(code: &str) -> String {
    let text = code.trim().to_uppercase();
    let mut run = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            run.push(ch);
            if run.len() == 6 {
                return run;
            }
        } else {
            run.clear();
        }
    }
    String::new()
}

pub fn board(code: &str) -> Board {
    let digits = code_digits(code);
    if digits.is_empty() {
        return Board::Unknown;
    }
    if digits.starts_with("688") || digits.starts_with("689") {
        return Board::Star;
    }
    if digits.starts_with("300") || digits.starts_with("301") || digits.starts_with("302") {
        return Board::Chinext;
    }
    if digits.starts_with('4') || digits.starts_with('8') || digits.starts_with("920") {
        return Board::Bse;
    }
    if digits.starts_with('6') || digits.starts_with('0') || digits.starts_with('3') {
        return Board::Main;
    }
    Board::Unknown
}

/// ST/*ST/SST/S*ST 前缀判定；名称可能带空格，先清理再比对。
pub fn is_st_name(name: &str) -> bool {
    let text = name.trim().to_uppercase().replace(' ', "");
    text.starts_with("ST")
        || text.starts_with("*ST")
        || text.starts_with("SST")
        || text.starts_with("S*ST")
}

/// 按板块与 ST 状态返回涨跌停比例。
pub fn price_limit_ratio(code: &str, name: &str) -> f64 {
    match board(code) {
        Board::Bse => 0.30,
        Board::Star | Board::Chinext => 0.20,
        _ if is_st_name(name) => 0.05,
        _ => 0.10,
    }
}

/// 价格 tick 0.01，半远离零。
pub fn round_price(value: f64) -> f64 {
    util::round_to(value, 2)
}

/// 返回 (跌停价, 涨停价)；前收无效时返回 None。
pub fn limit_prices(prev_close: f64, code: &str, name: &str) -> Option<(f64, f64)> {
    if !prev_close.is_finite() || prev_close <= 0.0 {
        return None;
    }
    let ratio = price_limit_ratio(code, name);
    let upper = round_price(prev_close * (1.0 + ratio));
    let lower = round_price(prev_close * (1.0 - ratio));
    Some((lower.max(0.01), upper))
}

/// 委托价是否落在涨跌停区间；没有前收时放行。
pub fn check_price_limit(
    price: f64,
    prev_close: Option<f64>,
    code: &str,
    name: &str,
) -> Result<(), String> {
    let Some(prev) = prev_close else {
        return Ok(());
    };
    if !prev.is_finite() || prev <= 0.0 {
        return Ok(());
    }
    if !price.is_finite() {
        return Err("价格不是数字".to_string());
    }
    let Some((lower, upper)) = limit_prices(prev, code, name) else {
        return Ok(());
    };
    if upper <= 0.0 {
        return Ok(());
    }
    if price > upper + 1e-9 {
        return Err(format!("买入价 {price:.2} 高于涨停价 {upper:.2}"));
    }
    if price < lower - 1e-9 {
        return Err(format!("卖出价 {price:.2} 低于跌停价 {lower:.2}"));
    }
    Ok(())
}

/// 是否处于涨停价：涨停通常买不进。
pub fn at_limit_up(price: f64, prev_close: Option<f64>, code: &str, name: &str) -> bool {
    let Some(prev) = prev_close else {
        return false;
    };
    if !prev.is_finite() || prev <= 0.0 {
        return false;
    }
    match limit_prices(prev, code, name) {
        Some((_, upper)) => upper > 0.0 && price >= upper - 1e-9,
        None => false,
    }
}

/// 是否处于跌停价：跌停通常卖不出。
pub fn at_limit_down(price: f64, prev_close: Option<f64>, code: &str, name: &str) -> bool {
    let Some(prev) = prev_close else {
        return false;
    };
    if !prev.is_finite() || prev <= 0.0 {
        return false;
    }
    match limit_prices(prev, code, name) {
        Some((lower, _)) => lower > 0.0 && price <= lower + 1e-9,
        None => false,
    }
}

/// 按板块把买入股数规整为合法值；买不起最低单位时返回 0。
pub fn normalize_buy_shares(code: &str, shares: i64) -> i64 {
    if shares <= 0 {
        return 0;
    }
    match board(code) {
        // 科创板 200 股起，之后可 1 股递增
        Board::Star => {
            if shares >= 200 {
                shares
            } else {
                0
            }
        }
        // 北交所 100 股起，之后可 1 股递增
        Board::Bse => {
            if shares >= 100 {
                shares
            } else {
                0
            }
        }
        // 主板/创业板：100 股整数倍
        _ => (shares / 100) * 100,
    }
}

pub fn min_buy_shares(code: &str) -> i64 {
    if board(code) == Board::Star {
        200
    } else {
        100
    }
}

/// T+1：卖出日必须严格晚于买入日；旧数据没有买入日期时放行。
pub fn is_t1_available(buy_date: Option<&str>, sell_date: NaiveDate) -> bool {
    let Some(text) = buy_date else {
        return true;
    };
    if text.is_empty() {
        return true;
    }
    match parse_day(text) {
        Some(buy) => sell_date > buy,
        None => true,
    }
}

/// 买入后可卖的最早交易日。
pub fn available_date(buy_date: Option<&str>, cal: &Calendar) -> Option<NaiveDate> {
    let text = buy_date?;
    if text.is_empty() {
        return None;
    }
    let buy = parse_day(text)?;
    cal.next_trading_day(buy, false).ok()
}

/// 只取前 10 个字符解析日期，容忍旧数据里带时分秒的写法。
fn parse_day(text: &str) -> Option<NaiveDate> {
    let head: String = text.chars().take(10).collect();
    util::parse_date(&head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        util::parse_date(text).expect("测试日期合法")
    }

    #[test]
    fn board_and_st() {
        assert_eq!(board("600519"), Board::Main);
        assert_eq!(board("300750"), Board::Chinext);
        assert_eq!(board("688001"), Board::Star);
        assert_eq!(board("920285"), Board::Bse);
        assert_eq!(board("430047"), Board::Bse);
        assert_eq!(board(""), Board::Unknown);
        assert!(is_st_name("*ST海航"));
        assert!(is_st_name("ST板块"));
        assert!(is_st_name("S*ST前锋"));
        assert!(!is_st_name("贵州茅台"));
    }

    #[test]
    fn code_digits_strips_prefix_and_suffix() {
        assert_eq!(code_digits("600519"), "600519");
        assert_eq!(code_digits("sh600519"), "600519");
        assert_eq!(code_digits("600519.SH"), "600519");
        assert_eq!(code_digits("SZ000001"), "000001");
        assert_eq!(code_digits("abc"), "");
    }

    #[test]
    fn price_limits() {
        assert_eq!(limit_prices(10.0, "600000", ""), Some((9.0, 11.0)));
        assert_eq!(limit_prices(10.0, "600000", "*ST测试"), Some((9.5, 10.5)));
        assert_eq!(limit_prices(10.0, "300001", ""), Some((8.0, 12.0)));
        assert_eq!(limit_prices(10.0, "688001", ""), Some((8.0, 12.0)));
        assert_eq!(limit_prices(10.0, "920285", ""), Some((7.0, 13.0)));
        assert_eq!(price_limit_ratio("300001", ""), 0.20);
        assert_eq!(price_limit_ratio("920285", ""), 0.30);
        assert_eq!(limit_prices(0.0, "600000", ""), None);
        assert!(at_limit_up(11.0, Some(10.0), "600000", ""));
        assert!(at_limit_down(9.0, Some(10.0), "600000", ""));
        assert!(!at_limit_up(10.5, Some(10.0), "600000", ""));
        assert!(!at_limit_down(9.5, Some(10.0), "600000", ""));
        // 无前收时不做涨跌停判断
        assert!(!at_limit_up(11.0, None, "600000", ""));
        assert!(check_price_limit(10.5, Some(10.0), "600000", "").is_ok());
        assert!(check_price_limit(11.5, Some(10.0), "600000", "").is_err());
        assert!(check_price_limit(8.5, Some(10.0), "600000", "").is_err());
    }

    #[test]
    fn lot_rules() {
        assert_eq!(normalize_buy_shares("600000", 250), 200);
        assert_eq!(normalize_buy_shares("600000", 99), 0);
        assert_eq!(normalize_buy_shares("688001", 100), 0);
        assert_eq!(normalize_buy_shares("688001", 300), 300);
        assert_eq!(normalize_buy_shares("920285", 50), 0);
        assert_eq!(normalize_buy_shares("920285", 150), 150);
        assert_eq!(min_buy_shares("688001"), 200);
        assert_eq!(min_buy_shares("600000"), 100);
    }

    #[test]
    fn t1_and_available_date() {
        assert!(!is_t1_available(Some("2026-10-23"), date("2026-10-23")));
        assert!(is_t1_available(Some("2026-10-23"), date("2026-10-26")));
        assert!(is_t1_available(None, date("2026-10-26")));
        // 旧数据里日期写坏时放行，避免永久锁仓
        assert!(is_t1_available(Some("坏日期"), date("2026-10-26")));

        let cal = Calendar::empty();
        assert_eq!(
            available_date(Some("2026-10-23"), &cal),
            Some(date("2026-10-26"))
        );
        assert_eq!(available_date(None, &cal), None);
        assert_eq!(available_date(Some("坏日期"), &cal), None);
    }
}
