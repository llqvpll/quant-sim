//! A 股交易日历与连续竞价时段。
//!
//! 只排除周末；法定节假日从 `data/holidays.json` 读取，不内置易过期的日期表。

use std::collections::HashSet;

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

use crate::error::{AppError, AppResult};
use crate::store::Paths;

const MORNING_OPEN: (u32, u32) = (9, 30);
const MORNING_CLOSE: (u32, u32) = (11, 30);
const AFTERNOON_OPEN: (u32, u32) = (13, 0);
const AFTERNOON_CLOSE: (u32, u32) = (15, 0);

/// 一天的连续竞价总分钟数（上午 120 + 下午 120）。
const SESSION_MINUTES: f64 = 240.0;
/// 单日/前后搜索上限，避免节假日文件写坏时死循环。
const MAX_SEARCH_DAYS: i64 = 370;

pub struct Calendar {
    holidays: HashSet<NaiveDate>,
}

impl Calendar {
    /// 读取节假日文件；缺失或格式非法时退化为“只排除周末”，不报错。
    pub fn load(paths: &Paths) -> Calendar {
        let path = paths.holidays();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Calendar::empty();
        };
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Calendar::empty();
        };
        // 兼容 ["2026-10-01", ...] 与 {"holidays": [...]} 两种写法
        let items = match &raw {
            serde_json::Value::Array(items) => items.clone(),
            serde_json::Value::Object(map) => match map.get("holidays") {
                Some(serde_json::Value::Array(items)) => items.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let mut holidays = HashSet::new();
        for item in items {
            if let Some(text) = item.as_str() {
                // 只取前 10 个字符，容忍 "2026-10-01T00:00:00" 这类写法
                let text: String = text.chars().take(10).collect();
                if let Some(day) = crate::util::parse_date(&text) {
                    holidays.insert(day);
                }
            }
        }
        Calendar { holidays }
    }

    pub fn empty() -> Calendar {
        Calendar {
            holidays: HashSet::new(),
        }
    }

    pub fn is_trading_day(&self, day: NaiveDate) -> bool {
        if day.weekday().num_days_from_monday() >= 5 {
            return false;
        }
        !self.holidays.contains(&day)
    }

    /// 连续竞价时段：9:30-11:30、13:00-15:00，含边界。
    pub fn is_trading_time(&self, now: NaiveDateTime) -> bool {
        if !self.is_trading_day(now.date()) {
            return false;
        }
        let seconds = minute_of_day(now);
        (in_window(seconds, MORNING_OPEN, MORNING_CLOSE))
            || in_window(seconds, AFTERNOON_OPEN, AFTERNOON_CLOSE)
    }

    /// 当前交易时段已走过的比例（0~1），供盘中量比折算使用。
    pub fn session_progress(&self, now: NaiveDateTime) -> f64 {
        if !self.is_trading_day(now.date()) {
            return 1.0;
        }
        let seconds = minute_of_day(now);
        let morning_open = window_start(MORNING_OPEN);
        let morning_close = window_start(MORNING_CLOSE);
        let afternoon_open = window_start(AFTERNOON_OPEN);
        let afternoon_close = window_start(AFTERNOON_CLOSE);
        if seconds < morning_open {
            return 0.0;
        }
        if seconds <= morning_close {
            let elapsed = (seconds - morning_open) / 60.0;
            return (elapsed / SESSION_MINUTES).clamp(0.01, 1.0);
        }
        if seconds < afternoon_open {
            return 0.5;
        }
        if seconds <= afternoon_close {
            let elapsed = 120.0 + (seconds - afternoon_open) / 60.0;
            return (elapsed / SESSION_MINUTES).clamp(0.01, 1.0);
        }
        1.0
    }

    /// 下一个交易日；`include_today` 为真且当天可交易时返回当天。
    pub fn next_trading_day(&self, day: NaiveDate, include_today: bool) -> AppResult<NaiveDate> {
        let mut current = if include_today {
            day
        } else {
            day.succ_opt().unwrap_or(day)
        };
        for _ in 0..MAX_SEARCH_DAYS {
            if self.is_trading_day(current) {
                return Ok(current);
            }
            match current.succ_opt() {
                Some(next) => current = next,
                None => break,
            }
        }
        Err(AppError::Invalid(
            "无法找到下一个交易日，请检查节假日文件".to_string(),
        ))
    }
}

fn minute_of_day(now: NaiveDateTime) -> f64 {
    let time = now.time();
    (time.hour() * 3600 + time.minute() * 60 + time.second()) as f64
}

fn window_start(hm: (u32, u32)) -> f64 {
    (hm.0 * 3600 + hm.1 * 60) as f64
}

/// 边界按整分钟闭区间：11:30:00 仍算盘中，11:30:01 已收盘。
fn in_window(seconds: f64, start: (u32, u32), end: (u32, u32)) -> bool {
    seconds >= window_start(start) && seconds <= window_start(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 写一份 holidays.json 再走真实加载路径，比直接构造 HashSet 更贴近线上。
    fn cal_with_holidays(content: &str) -> Calendar {
        let base = std::env::temp_dir().join(format!(
            "quant-sim-cal-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("data")).expect("创建临时目录");
        std::fs::write(base.join("data").join("holidays.json"), content).expect("写节假日文件");
        let cal = Calendar::load(&crate::store::Paths {
            base_dir: base.clone(),
        });
        let _ = std::fs::remove_dir_all(&base);
        cal
    }

    fn day(text: &str) -> NaiveDate {
        crate::util::parse_date(text).expect("测试日期合法")
    }

    fn dt(text: &str) -> NaiveDateTime {
        crate::util::parse_datetime(text).expect("测试时间合法")
    }

    #[test]
    fn weekend_is_not_trading_day() {
        let cal = Calendar::empty();
        assert!(cal.is_trading_day(day("2026-10-23")));
        assert!(!cal.is_trading_day(day("2026-10-24")));
        assert!(!cal.is_trading_day(day("2026-10-25")));
    }

    #[test]
    fn holidays_file_overrides_weekday() {
        // 数组写法
        let cal = cal_with_holidays(r#"["2026-10-01", "2026-10-02T00:00:00"]"#);
        assert!(!cal.is_trading_day(day("2026-10-01")));
        // 带时分秒的写法也要能读
        assert!(!cal.is_trading_day(day("2026-10-02")));
        assert!(cal.is_trading_day(day("2026-10-05")));

        // {"holidays": [...]} 写法
        let cal = cal_with_holidays(r#"{"holidays": ["2026-10-01"]}"#);
        assert!(!cal.is_trading_day(day("2026-10-01")));

        // 文件坏掉时退化为“只排除周末”，不能报错也不能把工作日全拉黑
        let cal = cal_with_holidays("{broken");
        assert!(cal.is_trading_day(day("2026-10-01")));

        // 文件缺失同理
        let cal = cal_with_holidays("");
        assert!(cal.is_trading_day(day("2026-10-05")));
    }

    #[test]
    fn trading_time_boundaries() {
        let cal = Calendar::empty();
        assert!(cal.is_trading_time(dt("2026-10-23T09:30:00")));
        assert!(cal.is_trading_time(dt("2026-10-23T11:30:00")));
        assert!(!cal.is_trading_time(dt("2026-10-23T11:31:00")));
        assert!(cal.is_trading_time(dt("2026-10-23T13:00:00")));
        assert!(cal.is_trading_time(dt("2026-10-23T15:00:00")));
        assert!(!cal.is_trading_time(dt("2026-10-23T15:01:00")));
        assert!(!cal.is_trading_time(dt("2026-10-24T10:00:00")));
    }

    #[test]
    fn session_progress_hits_session_bounds() {
        let cal = Calendar::empty();
        assert_eq!(cal.session_progress(dt("2026-10-23T09:00:00")), 0.0);
        assert!((cal.session_progress(dt("2026-10-23T10:30:00")) - 0.25).abs() < 1e-9);
        assert_eq!(cal.session_progress(dt("2026-10-23T12:00:00")), 0.5);
        assert!((cal.session_progress(dt("2026-10-23T14:00:00")) - 0.75).abs() < 1e-9);
        assert_eq!(cal.session_progress(dt("2026-10-23T15:00:00")), 1.0);
        // 非交易日视作收盘
        assert_eq!(cal.session_progress(dt("2026-10-24T10:30:00")), 1.0);
        // 开盘第一分钟不能被算成 0 进度
        assert!(cal.session_progress(dt("2026-10-23T09:30:10")) > 0.0);
    }

    #[test]
    fn next_trading_day_skips_weekend_and_holiday() {
        // 10-24/25 是周末，10-26 是节假日 → 23(五) 的下一个交易日是 27(二)
        let cal = cal_with_holidays(r#"["2026-10-26"]"#);
        let friday = day("2026-10-23");
        assert_eq!(
            cal.next_trading_day(friday, false).unwrap(),
            day("2026-10-27")
        );
        assert_eq!(cal.next_trading_day(friday, true).unwrap(), friday);
        assert_eq!(
            cal.next_trading_day(friday, false).unwrap().to_string(),
            "2026-10-27"
        );
        // 周末本身也要能往前找
        assert_eq!(
            cal.next_trading_day(day("2026-10-24"), false).unwrap(),
            day("2026-10-27")
        );
    }
}
