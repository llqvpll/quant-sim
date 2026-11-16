//! 通用工具：日志、时间、取整、URL 编码、文本清洗、短哈希，以及中文字符窗口搜索。
//!
//! 所有字符串截断都按"字符"而不是"字节"进行，与 Python 切片语义保持一致，
//! 否则中文会被截半并产生非法 UTF-8。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::sync::OnceLock;

use chrono::{Local, NaiveDate, NaiveDateTime};
use regex::Regex;

/// 统一日志：`[HH:MM:SS] 内容`，立即 flush，便于重定向到文件时实时可见。
pub fn log(msg: &str) {
    println!("[{}] {}", Local::now().format("%H:%M:%S"), msg);
    let _ = std::io::stdout().flush();
}

/// 本地当前时间。
pub fn now_local() -> NaiveDateTime {
    Local::now().naive_local()
}

/// ISO 时间戳（不含时区后缀），对齐 Python `isoformat(timespec="seconds")`。
pub fn now_iso() -> String {
    now_local().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// 带空格的日期时间，AI 信号的 `timestamp` 字段沿用 Python `strftime` 的写法。
pub fn now_stamp() -> String {
    now_local().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn today() -> NaiveDate {
    Local::now().date_naive()
}

pub fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// 四舍五入到指定位数，半远离零（Rust `f64::round` 即是该规则）。
///
/// 结果为零时统一归一为正零：IEEE 里 `(-0.0001 * 100.0).round()` 是负零，
/// `{:.2}` 会渲染成 "-0.00"，金额与百分比场景一律不该出现这种负数零。
pub fn round_to(v: f64, digits: u32) -> f64 {
    if !v.is_finite() {
        return v;
    }
    let factor = 10f64.powi(digits as i32);
    let rounded = (v * factor).round() / factor;
    if rounded == 0.0 {
        0.0
    } else {
        rounded
    }
}

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let text = s.trim();
    if text.is_empty() {
        return None;
    }
    for fmt in ["%Y-%m-%d", "%Y/%m/%d", "%Y%m%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(text, fmt) {
            return Some(d);
        }
    }
    None
}

pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let text = s.trim();
    if text.is_empty() {
        return None;
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(text, fmt) {
            return Some(d);
        }
    }
    // 带小数秒（Python isoformat 可能输出 6 位微秒）
    if let Ok(d) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(d);
    }
    parse_date(text).and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// 百分号编码（UTF-8 字节级）。未保留字符直接输出，空格编码为 `+`，
/// 与 Python `urllib.parse.urlencode` 的行为一致。
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 拼接查询串；`url` 已带 `?` 时用 `&` 续接。
pub fn build_url(url: &str, params: &[(&str, String)]) -> String {
    if params.is_empty() {
        return url.to_string();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    let query: Vec<String> = params
        .iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect();
    format!("{url}{sep}{}", query.join("&"))
}

fn tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[^>]+>").expect("HTML 标签正则为常量"))
}

pub fn strip_html(html: &str) -> String {
    tag_regex().replace_all(html, "").to_string()
}

/// 8 位十六进制短哈希。用途只是给回测状态文件起一个稳定文件名，
/// 不需要密码学强度，因此用标准库 `DefaultHasher` 而不是引入 md5。
pub fn hash8(s: &str) -> String {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    let hex = format!("{:016x}", hasher.finish());
    hex[..8].to_string()
}

/// 按字符数截断，超长时不加省略号。
pub fn take_chars(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    s.chars().take(n).collect()
}

/// JSON 值转文本：字符串原样返回，`null`/缺失为空串，其它按 JSON 字面量输出。
/// 对齐 Python `str(value or "")` 的用法（接口字段类型不稳定：有时是字符串，
/// 有时是数字）。
pub fn json_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// JSON 值转数字：数字直取，字符串按数字解析，空串/null 视为 0。
pub fn json_number(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Some(0.0);
            }
            trimmed.parse::<f64>().ok()
        }
        serde_json::Value::Null => Some(0.0),
        _ => None,
    }
}

/// 在字符切片中从 `from` 开始查找子串，返回字符下标（不是字节下标）。
pub fn find_sub(hay: &[char], needle: &str, from: usize) -> Option<usize> {
    let pat: Vec<char> = needle.chars().collect();
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    let last = hay.len() - pat.len();
    let mut i = from;
    while i <= last {
        if hay[i..i + pat.len()] == pat[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn contains_sub(hay: &[char], needle: &str) -> bool {
    find_sub(hay, needle, 0).is_some()
}

/// 字符切片是否以给定字符串结尾。
pub fn ends_with_str(hay: &[char], s: &str) -> bool {
    let pat: Vec<char> = s.chars().collect();
    if pat.len() > hay.len() {
        return false;
    }
    hay[hay.len() - pat.len()..] == pat[..]
}

/// 安装 SIGINT（Ctrl+C）处理：只置标志位，由主循环自己收尾（保存账户、释放锁）。
///
/// 没有 ctrlc 依赖，直接用 CRT 的 `signal()`。默认行为会直接结束进程，
/// 那样 `data/bot.lock` 会残留；置标志后主循环能在 1 秒内正常退出。
pub fn install_interrupt_handler() {
    let handler = on_sigint as extern "C" fn(i32) as usize;
    // SAFETY: 处理函数只做一次原子写，异步信号安全。
    unsafe {
        signal(SIGINT, handler);
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}

const SIGINT: i32 = 2;
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

extern "C" fn on_sigint(_signum: i32) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_to_is_half_away_from_zero() {
        assert_eq!(round_to(2.345, 2), 2.35);
        assert_eq!(round_to(-2.345, 2), -2.35);
        // 半远离零：2.5 / -2.5 都向远离零的方向进位，与 Python 银行家舍入不同。
        assert_eq!(round_to(2.5, 0), 3.0);
        assert_eq!(round_to(-2.5, 0), -3.0);
        assert_eq!(round_to(1295.0, 2), 1295.0);
        assert_eq!(round_to(0.8, 4), 0.8);
        // 1.005 在 f64 里其实是 1.00499999999999989，先乘后除只能得到 1.0。
        assert_eq!(round_to(1.005, 2), 1.0);
        // 负数零必须归一：否则金额会打印成 "-0.00"
        assert_eq!(round_to(-0.0001, 2), 0.0);
        assert!(round_to(-0.0001, 2).is_sign_positive());
        assert_eq!(round_to(-0.004, 1), 0.0);
        assert!(round_to(-0.004, 1).is_sign_positive());
        // 真负数不能被误伤
        assert_eq!(round_to(-0.006, 2), -0.01);
        assert!(round_to(-0.006, 2).is_sign_negative());
        assert!(round_to(f64::NAN, 2).is_nan());
    }

    #[test]
    fn parse_date_accepts_common_formats() {
        assert_eq!(
            parse_date("2026-10-21"),
            NaiveDate::from_ymd_opt(2026, 10, 21)
        );
        assert_eq!(
            parse_date("2026/10/21"),
            NaiveDate::from_ymd_opt(2026, 10, 21)
        );
        assert_eq!(
            parse_date("20261021"),
            NaiveDate::from_ymd_opt(2026, 10, 21)
        );
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("昨天"), None);
    }

    #[test]
    fn parse_datetime_accepts_iso_and_plain() {
        let expected = NaiveDate::from_ymd_opt(2026, 10, 21)
            .and_then(|d| d.and_hms_opt(16, 26, 0))
            .unwrap();
        assert_eq!(parse_datetime("2026-10-21T16:26:00"), Some(expected));
        assert_eq!(parse_datetime("2026-10-21 16:26:00"), Some(expected));
        assert_eq!(
            parse_datetime("2026-10-21"),
            Some(
                NaiveDate::from_ymd_opt(2026, 10, 21)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
            )
        );
    }

    #[test]
    fn encode_component_handles_chinese_and_space() {
        assert_eq!(encode_component("hs_a"), "hs_a");
        assert_eq!(
            encode_component("600519 贵州茅台"),
            "600519+%E8%B4%B5%E5%B7%9E%E8%8C%85%E5%8F%B0"
        );
        assert_eq!(encode_component("a,b"), "a%2Cb");
    }

    #[test]
    fn build_url_picks_separator() {
        let params = [("page", "1".to_string()), ("num", "100".to_string())];
        assert_eq!(
            build_url("https://a.com/x", &params),
            "https://a.com/x?page=1&num=100"
        );
        assert_eq!(
            build_url("https://a.com/x?k=1", &params),
            "https://a.com/x?k=1&page=1&num=100"
        );
        assert_eq!(build_url("https://a.com/x", &[]), "https://a.com/x");
    }

    #[test]
    fn strip_html_removes_tags() {
        assert_eq!(strip_html("<em>贵州</em>茅台"), "贵州茅台");
        assert_eq!(strip_html("无标签"), "无标签");
    }

    #[test]
    fn hash8_is_eight_hex_chars() {
        let h = hash8("600519");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash8("600519"), h);
        assert_ne!(hash8("600519"), hash8("000001"));
    }

    #[test]
    fn take_chars_counts_chars_not_bytes() {
        assert_eq!(take_chars("贵州茅台业绩", 4), "贵州茅台");
        assert_eq!(take_chars("abc", 10), "abc");
        assert_eq!(take_chars("abc", 0), "");
    }

    #[test]
    fn char_substring_helpers() {
        let chars: Vec<char> = "交易建议：不建议买入。".chars().collect();
        assert_eq!(find_sub(&chars, "买入", 0), Some(8));
        assert!(contains_sub(&chars, "不建议"));
        assert!(ends_with_str(&chars, "。"));
        assert!(!ends_with_str(&chars, "买入"));
    }

    #[test]
    fn json_helpers_tolerate_mixed_types() {
        let value: serde_json::Value = serde_json::json!({
            "code": "600519",
            "trade": "1295.00",
            "change": 1.17,
            "empty": "",
            "nothing": null,
        });
        assert_eq!(json_text(value.get("code")), "600519");
        assert_eq!(json_text(value.get("nothing")), "");
        assert_eq!(json_number(value.get("trade")), Some(1295.0));
        assert_eq!(json_number(value.get("change")), Some(1.17));
        assert_eq!(json_number(value.get("empty")), Some(0.0));
        assert_eq!(json_number(value.get("nothing")), Some(0.0));
        assert_eq!(json_number(value.get("missing")), None);
    }
}
